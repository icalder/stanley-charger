use crate::{Args, BatteryData};
use crate::charging::calculate_charging_parameters;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_serial::SerialPortBuilderExt;

pub async fn run_psu_task(mut v_rx: mpsc::Receiver<BatteryData>, args: Args) {
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

    let mut port = match tokio_serial::new(&port_path, 9600).open_native_async() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Could not open Tenma Serial Port: {}", e);
            return;
        }
    };

    // Initial safety state: Output OFF
    if let Err(e) = port.write_all(b"OUT0\n").await {
        eprintln!("Initial PSU command failed: {}. Exiting.", e);
        return;
    }

    // Track state to avoid redundant commands
    let mut last_vset = 0.0_f32;
    let mut last_iset = 0.0_f32;
    let mut actual_v = "N/A".to_string();
    let mut over_limit_count = 0;
    let mut missing_responses = 0;

    while let Some(data) = v_rx.recv().await {
        if let Some(limit) = args.limit {
            if data.soc >= limit {
                over_limit_count += 1;
            } else {
                over_limit_count = 0;
            }

            if over_limit_count >= 3 {
                println!(
                    "SoC limit reached ({}%) for 3 consecutive readings. Disconnecting PSU output.",
                    data.soc
                );
                let _ = port.write_all(b"OUT0\n").await;
                tokio::time::sleep(Duration::from_millis(200)).await;
                return;
            }
        }

        let params = calculate_charging_parameters(data.voltage);

        let v_changed = (params.target_voltage - last_vset).abs() > 0.01;
        let i_changed = (params.target_current - last_iset).abs() > 0.01;

        if v_changed || i_changed {
            // --- CONTROL MODE ---
            // If we are changing settings, we don't query.
            // This gives the PSU 100% of its CPU to update the hardware.
            if v_changed {
                if let Err(e) = port.write_all(format!("VSET1:{:.2}\n", params.target_voltage).as_bytes()).await {
                    eprintln!("PSU Write Error (VSET): {}. Exiting.", e);
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
                last_vset = params.target_voltage;
            }
            if i_changed {
                if let Err(e) = port.write_all(format!("ISET1:{:.2}\n", params.target_current).as_bytes()).await {
                    eprintln!("PSU Write Error (ISET): {}. Exiting.", e);
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
                last_iset = params.target_current;
            }
            if let Err(e) = port.write_all(b"OUT1\n").await {
                eprintln!("PSU Write Error (OUT1): {}. Exiting.", e);
                return;
            }
            actual_v = "UPDATING".to_string();
        } else {
            // --- QUERY MODE ---
            // Everything is steady, safe to ask for data.
            // Clear any lingering data in the buffer
            let mut discard = [0u8; 128];
            while let Ok(Ok(n)) = timeout(Duration::from_millis(10), port.read(&mut discard)).await {
                if n == 0 { break; }
            }

            if let Err(e) = port.write_all(b"VOUT1?\n").await {
                eprintln!("PSU write error (VOUT1?): {}. Exiting.", e);
                return;
            }

            // Hefty delay for slow hardware
            tokio::time::sleep(Duration::from_millis(250)).await;

            let mut response_buf = [0u8; 16];
            match timeout(Duration::from_millis(200), port.read(&mut response_buf)).await {
                Ok(Ok(n)) if n > 0 => {
                    actual_v = String::from_utf8_lossy(&response_buf[..n]).trim().to_string();
                    missing_responses = 0;
                }
                _ => {
                    // If we didn't get a valid response, we keep the last actual_v
                    // but increment missing_responses to detect power-off.
                    missing_responses += 1;
                }
            }

            if missing_responses >= 10 {
                // Only exit if the device is physically gone from the USB bus.
                // If it's still there, it might just be a transient glitch or the PSU is busy.
                if find_port_path(vid, pid).is_none() {
                    eprintln!("Tenma PSU disconnected or powered off (USB device gone). Exiting.");
                    return;
                }

                eprintln!("Warning: Tenma PSU not responding (10 cycles), but device is still on bus. Retrying...");
                missing_responses = 0;
                
                // Optional: Try to clear the buffer again
                let mut discard = [0u8; 128];
                while let Ok(Ok(n)) = timeout(Duration::from_millis(10), port.read(&mut discard)).await {
                    if n == 0 { break; }
                }
            }
        }

        let display_vout = if actual_v == "UPDATING" || actual_v == "N/A" {
            actual_v.clone()
        } else {
            format!("{}V", actual_v)
        };

        println!(
            ">>> [{}] Batt: {:.2}V | SoC: {}% | PSU VSET: {:.2}V | PSU VOUT: {} | Target: {:.2}A",
            params.phase,
            data.voltage,
            data.soc,
            params.target_voltage,
            display_vout,
            params.target_current
        );
    }
}

pub fn find_port_path(vid: u16, pid: u16) -> Option<String> {
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
