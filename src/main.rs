mod charging;
mod decode;

use btleplug::api::{Central, Manager as _, Peripheral, ScanFilter, WriteType};
use btleplug::platform::Manager;
use charging::calculate_charging_parameters;
use clap::Parser;
use decode::Decoder;
use futures::stream::StreamExt;
use std::error::Error;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};
use tokio_serial::SerialPortBuilderExt;
use uuid::{Uuid, uuid};

const TARGET_MAC: &str = "A5:C2:37:02:49:87";
const BMS_RX_UUID: Uuid = uuid!("0000ff01-0000-1000-8000-00805f9b34fb");
const BMS_TX_UUID: Uuid = uuid!("0000ff02-0000-1000-8000-00805f9b34fb");
const BMS_SERVICE_UUID: Uuid = uuid!("0000ff00-0000-1000-8000-00805f9b34fb");
const QUERY_BATTERY: [u8; 7] = [0xdd, 0xa5, 0x03, 0x00, 0xff, 0xfd, 0x77];

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Maximum battery charge percentage (SoC) limit
    #[arg(short, long)]
    limit: Option<u8>,
}

#[derive(Debug, Clone, Copy)]
struct BatteryData {
    voltage: f32,
    soc: u8,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();
    let (v_tx, mut v_rx) = mpsc::channel::<BatteryData>(10);

    // --- Task 1: Tenma PSU Controller ---
    tokio::spawn(async move {
        // lsusb for Tenma 72-2605: Bus 004 Device 005: ID 0416:5011 Nuvoton USB Virtual COM
        let vid = 0x0416;
        let pid = 0x5011;

        // Wait until the Tenma is physically plugged in and turned on
        let port_path = loop {
            if let Some(path) = find_port_path(vid, pid) {
                println!("Tenma found on {}", path);
                break path;
            }
            println!("Searching for Tenma (ID {:04x}:{:04x})...", vid, pid);
            tokio::time::sleep(Duration::from_secs(2)).await;
        };

        let mut port = tokio_serial::new(&port_path, 9600)
            .open_native_async()
            .expect("Could not open Tenma Serial Port");

        // Initial safety state: Output OFF
        let _ = port.try_write(b"OUT0\n");

        // Track state to avoid redundant commands
        let mut last_vset = 0.0_f32;
        let mut last_iset = 0.0_f32;
        let mut actual_v = "N/A".to_string();
        let mut over_limit_count = 0;
        let mut is_disconnected = false;

        while let Some(data) = v_rx.recv().await {
            if let Some(limit) = args.limit {
                if data.soc >= limit {
                    over_limit_count += 1;
                } else {
                    over_limit_count = 0;
                }

                if over_limit_count >= 3 && !is_disconnected {
                    println!(
                        "SoC limit reached ({}%) for 3 consecutive readings. Disconnecting PSU output.",
                        data.soc
                    );
                    let _ = port.try_write(b"OUT0\n");
                    is_disconnected = true;
                }
            }

            if is_disconnected {
                println!(
                    ">>> [STOPPED] Batt: {:.2}V | SoC: {}% | PSU output disconnected",
                    data.voltage, data.soc
                );
                continue;
            }

            let params = calculate_charging_parameters(data.voltage);

            let v_changed = (params.target_voltage - last_vset).abs() > 0.01;
            let i_changed = (params.target_current - last_iset).abs() > 0.01;

            if v_changed || i_changed {
                // --- CONTROL MODE ---
                // If we are changing settings, we don't query.
                // This gives the PSU 100% of its CPU to update the hardware.
                if v_changed {
                    let _ =
                        port.try_write(format!("VSET1:{:.2}\n", params.target_voltage).as_bytes());
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    last_vset = params.target_voltage;
                }
                if i_changed {
                    let _ =
                        port.try_write(format!("ISET1:{:.2}\n", params.target_current).as_bytes());
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    last_iset = params.target_current;
                }
                // make sure output is enabled
                let _ = port.try_write(b"OUT1\n");
                actual_v = "UPDATING".to_string();
            } else {
                // --- QUERY MODE ---
                // Everything is steady, safe to ask for data.
                let mut discard = [0u8; 64];
                while let Ok(n) = port.try_read(&mut discard) {
                    if n == 0 {
                        break;
                    }
                }

                let _ = port.try_write(b"VOUT1?\n");
                tokio::time::sleep(Duration::from_millis(250)).await; // Hefty delay for slow hardware

                let mut response_buf = [0u8; 16];
                if let Ok(n) = port.try_read(&mut response_buf) {
                    if n > 0 {
                        actual_v = String::from_utf8_lossy(&response_buf[..n])
                            .trim()
                            .to_string();
                    }
                }
            }

            let display_vout = if actual_v == "UPDATING" || actual_v == "N/A" {
                actual_v.clone()
            } else {
                format!("{}V", actual_v)
            };

            println!(
                ">>> [{}] Batt: {:.2}V | PSU VSET: {:.2}V | PSU VOUT: {} | Target: {:.2}A",
                params.phase,
                data.voltage,
                params.target_voltage,
                display_vout,
                params.target_current
            );
        }
    });

    // --- Task 2: BMS Monitor (with Recovery) ---
    let manager = Manager::new().await?;
    let adapter = manager
        .adapters()
        .await?
        .into_iter()
        .next()
        .ok_or("No BT adapter")?;

    loop {
        let _ = adapter.start_scan(ScanFilter::default()).await;

        match find_and_connect(&adapter).await {
            Ok(bms) => {
                // Notifications stream is created INSIDE this function
                // and will be dropped immediately if it returns an error.
                if let Err(e) = run_monitor_loop(&bms, v_tx.clone()).await {
                    eprintln!("BMS Error: {}. Cleaning up...", e);
                }
                let _ = bms.disconnect().await;
            }
            Err(e) => {
                eprintln!("Connection failed: {}. Resetting adapter...", e);
                // Optional: trigger system 'bluetoothctl remove' here
            }
        }
        sleep(Duration::from_secs(5)).await;
    }
}

fn find_port_path(vid: u16, pid: u16) -> Option<String> {
    tokio_serial::available_ports()
        .ok()?
        .into_iter()
        .find_map(|p| {
            if let tokio_serial::SerialPortType::UsbPort(info) = p.port_type {
                if info.vid == vid && info.pid == pid {
                    return Some(p.port_name);
                }
            }
            None
        })
}

async fn find_and_connect(
    adapter: &btleplug::platform::Adapter,
) -> Result<btleplug::platform::Peripheral, Box<dyn Error>> {
    let bms = timeout(Duration::from_secs(15), async {
        loop {
            if let Some(p) = adapter
                .peripherals()
                .await
                .unwrap()
                .into_iter()
                .find(|p| p.address().to_string().to_uppercase().contains(TARGET_MAC))
            {
                return p;
            }
            sleep(Duration::from_secs(1)).await;
        }
    })
    .await
    .map_err(|_| "Discovery timeout")?;

    // Trick from battery.rs: Connect while scanning
    bms.connect().await?;

    // Explicitly check for the BMS service UUID like your old code
    bms.discover_services().await?;
    let has_service = bms.services().iter().any(|s| s.uuid == BMS_SERVICE_UUID);

    if !has_service {
        return Err("BMS Service not found on device".into());
    }

    let _ = adapter.stop_scan().await;
    Ok(bms)
}

async fn run_monitor_loop(
    bms: &btleplug::platform::Peripheral,
    v_tx: mpsc::Sender<BatteryData>,
) -> Result<(), Box<dyn Error>> {
    let mut decoder = Decoder::default();
    let mut notifications = bms.notifications().await?;

    let chars = bms.characteristics();
    let rx_char = chars
        .iter()
        .find(|c| c.uuid == BMS_RX_UUID)
        .ok_or("RX not found")?;
    let tx_char = chars
        .iter()
        .find(|c| c.uuid == BMS_TX_UUID)
        .ok_or("TX not found")?;

    bms.subscribe(rx_char).await?;
    // A small delay here helps the BMS process the subscription before the first write
    sleep(Duration::from_millis(500)).await;

    loop {
        bms.write(tx_char, &QUERY_BATTERY, WriteType::WithoutResponse)
            .await?;

        // Feed the decoder packets until it has enough to report SoC
        while let Ok(Some(n)) = timeout(Duration::from_secs(2), notifications.next()).await {
            // Process the incoming fragment
            if let Err(e) = decoder.decode(&n.value) {
                eprintln!("Decode error: {}", e);
                continue;
            }

            // If Packet 2 was just received, we now have the SoC!
            if decoder.is_soc_ready() {
                println!(
                    "Battery Voltage: {:.2}V | Current: {:.2}A | SoC: {}%",
                    decoder.voltage, decoder.current, decoder.soc
                );
                let _ = v_tx
                    .send(BatteryData {
                        voltage: decoder.voltage,
                        soc: decoder.soc,
                    })
                    .await;
                break; // We have what we need for the charger
            }
        }
        sleep(Duration::from_secs(10)).await;
    }
}
