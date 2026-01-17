mod decode;

use btleplug::api::{Central, Manager as _, Peripheral, ScanFilter, WriteType};
use btleplug::platform::Manager;
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let (v_tx, mut v_rx) = mpsc::channel::<f32>(10);

    // --- Task 1: Tenma PSU Controller ---
    tokio::spawn(async move {
        println!("Initializing Tenma PSU on /dev/ttyACM0...");
        let mut port = tokio_serial::new("/dev/ttyACM0", 9600)
            .open_native_async()
            .expect("Could not open Tenma Serial Port");

        // Initial safety state: Output OFF
        let _ = port.try_write(b"OUT0\n");

        // Track state to avoid redundant commands
        let mut last_vset = 0.0_f32;
        let mut last_iset = 0.0_f32;
        let mut actual_v = "N/A".to_string();

        while let Some(voltage) = v_rx.recv().await {
            let target_v = if voltage < 13.6 {
                // BULK: High push to overcome wire resistance
                16.00
            } else if voltage >= 14.2 {
                // FLOAT: Battery is full, drop to a resting voltage
                13.60
            } else {
                // ABSORPTION/TAPER: Close to full, move to standard charging voltage
                14.40
            };
            let target_i = if voltage < 13.6 {
                5.0
            } else if voltage >= 14.2 {
                0.1
            } else {
                (5.0 - ((voltage - 13.6) / 0.6) * 4.5).clamp(0.1, 5.0)
            };

            let v_changed = (target_v - last_vset).abs() > 0.01;
            let i_changed = (target_i - last_iset).abs() > 0.01;

            if v_changed || i_changed {
                // --- CONTROL MODE ---
                // If we are changing settings, we don't query.
                // This gives the PSU 100% of its CPU to update the hardware.
                if v_changed {
                    let _ = port.try_write(format!("VSET1:{:.2}\n", target_v).as_bytes());
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    last_vset = target_v;
                }
                if i_changed {
                    let _ = port.try_write(format!("ISET1:{:.2}\n", target_i).as_bytes());
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    last_iset = target_i;
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

            let phase = if target_v == 16.0 {
                "BULK"
            } else if target_v == 13.6 {
                "FLOAT"
            } else {
                "TAPER"
            };

            let display_vout = if actual_v == "UPDATING" || actual_v == "N/A" {
                actual_v.clone()
            } else {
                format!("{}V", actual_v)
            };

            println!(
                ">>> [{}] Batt: {:.2}V | PSU VSET: {:.2}V | PSU VOUT: {} | Target: {:.2}A",
                phase, voltage, target_v, display_vout, target_i
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
    v_tx: mpsc::Sender<f32>,
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
                let _ = v_tx.send(decoder.voltage).await;
                break; // We have what we need for the charger
            }
        }
        sleep(Duration::from_secs(10)).await;
    }
}
