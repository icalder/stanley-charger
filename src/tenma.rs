use crate::charging::calculate_charging_parameters;
use crate::{Args, BatteryData};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep, timeout, timeout_at};
use tokio_serial::{SerialPortBuilderExt, SerialStream};

// Tenma 72-2605: lsusb reports "0416:5011 Nuvoton USB Virtual COM".
const TENMA_VID: u16 = 0x0416;
const TENMA_PID: u16 = 0x5011;
const TENMA_BAUD_RATE: u32 = 9600;

const DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(2);
const RECONNECT_DELAY: Duration = Duration::from_secs(2);
const COMMAND_SETTLE_TIME: Duration = Duration::from_millis(100);
const READ_SETTLE_TIME: Duration = Duration::from_millis(250);
const READ_TIMEOUT: Duration = Duration::from_millis(200);
const DRAIN_TIMEOUT: Duration = Duration::from_millis(10);

/// How long the supply may stay silent before the supervisor reopens the port.
/// The BMS reports every 10 s, so this covers three silent readings.
const NO_RESPONSE_BUDGET: Duration = Duration::from_secs(30);

/// How long the tool may go without battery data before it switches the output off.
/// The budget covers three failed BMS reconnects in main.rs, which cost about 20 s
/// each (15 s discovery timeout plus a 5 s backoff).
const BATTERY_STALE_BUDGET: Duration = Duration::from_secs(90);

/// How many times the tool sends OUT0 before it gives up on the current port.
const OFF_WRITE_ATTEMPTS: u8 = 3;

const LIMIT_READINGS: u8 = 3;

/// Answer to a `VOUT1?` query. Silent and LinkError need different handling: a quiet
/// device may still recover, a device that rejects the write needs a new port handle.
#[derive(Debug)]
enum VoltageProbe {
    Answered(String),
    Silent,
    LinkError,
}

/// Why the control loop stopped. The supervisor uses this to choose between
/// retrying the port and finishing the task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlOutcome {
    /// SoC limit reached and the supply accepted the OFF command.
    OutputOff,
    /// SoC limit reached, but no OFF command reached the supply. The supervisor
    /// must reopen the port and switch the output off before it stops.
    OutputOffPending,
    /// The serial link failed. Reopen the port and keep charging.
    LinkLost,
    /// The BMS sender went away. Nothing is left to charge.
    ChannelClosed,
}

/// Supervises the PSU: opens the port, runs the control loop, and reopens the
/// port whenever the link is lost. The task only returns when charging is done.
pub async fn run_psu_task(mut v_rx: mpsc::Receiver<BatteryData>, args: Args) {
    // True once the SoC limit is reached but the supply has not accepted the OFF
    // command. Charging must not resume, so the next open that lands OUT0 ends the task.
    let mut stop_pending = false;

    loop {
        let mut port = connect_psu().await;

        if stop_pending {
            // connect_psu writes OUT0 before it returns, so the supply is now off.
            report_charging_stopped();
            return;
        }

        let outcome = run_control_loop(&mut port, &mut v_rx, &args).await;

        // Drop before the retry: after a re-enumeration the old handle points at a
        // stale device node, and keeping it open can block the new one.
        drop(port);

        match outcome {
            ControlOutcome::OutputOff => {
                report_charging_stopped();
                return;
            }
            ControlOutcome::OutputOffPending => {
                stop_pending = true;
                eprintln!(
                    "WARNING: the supply did not accept the OFF command.\
                     Reopening the port to switch it off. Charging will not resume."
                );
            }
            ControlOutcome::LinkLost => {
                eprintln!(
                    "PSU connection lost. Reconnecting in {:?}...",
                    RECONNECT_DELAY
                );
            }
            ControlOutcome::ChannelClosed => {
                eprintln!("Battery data channel closed. Stopping PSU task.");
                return;
            }
        }

        sleep(RECONNECT_DELAY).await;
    }
}

fn report_charging_stopped() {
    println!("PSU output is off. Charging stopped.");
}

/// Waits for the PSU to appear on the USB bus, then opens it with the output off.
/// Retries forever; a transient USB or permission problem must not stop charging.
async fn connect_psu() -> SerialStream {
    loop {
        let path = wait_for_device().await;

        match tokio_serial::new(&path, TENMA_BAUD_RATE).open_native_async() {
            Ok(mut port) => {
                if write_command(&mut port, "OUT0", b"OUT0\n").await {
                    println!("Tenma found on {}; output off.", path);
                    return port;
                }
                eprintln!(
                    "Tenma opened on {} but did not accept OUT0. Retrying...",
                    path
                );
            }
            Err(e) => eprintln!("Could not open Tenma on {}: {}. Retrying...", path, e),
        }

        sleep(RECONNECT_DELAY).await;
    }
}

async fn wait_for_device() -> String {
    let mut announced = false;
    loop {
        if let Some(path) = find_port_path(TENMA_VID, TENMA_PID) {
            return path;
        }
        if !announced {
            println!(
                "Searching for Tenma (ID {:04x}:{:04x})...",
                TENMA_VID, TENMA_PID
            );
            announced = true;
        }
        sleep(DEVICE_POLL_INTERVAL).await;
    }
}

/// Switch the supply output off, retrying the command. Returns false when no
/// attempt reached the device, which tells the caller to reopen the port.
async fn disable_output(port: &mut SerialStream) -> bool {
    for attempt in 1..=OFF_WRITE_ATTEMPTS {
        if write_command(port, "OUT0", b"OUT0\n").await {
            sleep(COMMAND_SETTLE_TIME).await;
            return true;
        }
        eprintln!(
            "Could not switch the PSU output off (attempt {}/{}).",
            attempt, OFF_WRITE_ATTEMPTS
        );
        sleep(COMMAND_SETTLE_TIME).await;
    }

    false
}

/// Ask the supply for its output voltage. Clears stale bytes first, because a late
/// answer to an earlier query would otherwise be read as the current one.
async fn query_output_voltage(port: &mut SerialStream) -> VoltageProbe {
    drain_input(port).await;

    if !write_command(port, "VOUT1?", b"VOUT1?\n").await {
        return VoltageProbe::LinkError;
    }

    // Hefty delay for slow hardware.
    sleep(READ_SETTLE_TIME).await;

    let mut response_buf = [0u8; 16];
    match timeout(READ_TIMEOUT, port.read(&mut response_buf)).await {
        Ok(Ok(n)) if n > 0 => VoltageProbe::Answered(
            String::from_utf8_lossy(&response_buf[..n])
                .trim()
                .to_string(),
        ),
        _ => VoltageProbe::Silent,
    }
}

async fn write_command(port: &mut SerialStream, label: &str, command: &[u8]) -> bool {
    match port.write_all(command).await {
        Ok(()) => true,
        Err(e) => {
            eprintln!("PSU write error ({label}): {}", e);
            false
        }
    }
}

async fn drain_input(port: &mut SerialStream) {
    let mut discard = [0u8; 128];
    while let Ok(Ok(n)) = timeout(DRAIN_TIMEOUT, port.read(&mut discard)).await {
        if n == 0 {
            return;
        }
    }
}

async fn run_control_loop(
    port: &mut SerialStream,
    v_rx: &mut mpsc::Receiver<BatteryData>,
    args: &Args,
) -> ControlOutcome {
    // Cached setpoints start at zero so the first reading always programs the PSU.
    let mut last_vset = 0.0_f32;
    let mut last_iset = 0.0_f32;
    let mut actual_v = String::from("N/A");
    let mut over_limit_count = 0u8;
    let mut last_response = Instant::now();
    let mut data_deadline = Instant::now() + BATTERY_STALE_BUDGET;
    let mut waiting_for_battery = false;

    loop {
        let data = match timeout_at(data_deadline, v_rx.recv()).await {
            Ok(Some(data)) => data,
            Ok(None) => return ControlOutcome::ChannelClosed,
            Err(_) => {
                // No measurement, so the SoC limit cannot be trusted. Put the supply
                // in its safe state and wait for the BMS to report again.
                if !waiting_for_battery {
                    eprintln!(
                        "No battery data for {} s. Switching the PSU output off.",
                        BATTERY_STALE_BUDGET.as_secs()
                    );
                    if !disable_output(port).await {
                        return ControlOutcome::LinkLost;
                    }
                    waiting_for_battery = true;
                }

                data_deadline = Instant::now() + BATTERY_STALE_BUDGET;
                continue;
            }
        };

        data_deadline = Instant::now() + BATTERY_STALE_BUDGET;

        if waiting_for_battery {
            // The output is off and the cached setpoints no longer describe the
            // supply, so program them again and switch the output back on.
            last_vset = 0.0;
            last_iset = 0.0;
            waiting_for_battery = false;
            println!("Battery data is back. Resuming charge.");
        }

        if let Some(limit) = args.limit {
            over_limit_count = if data.soc >= limit {
                over_limit_count + 1
            } else {
                0
            };

            if over_limit_count >= LIMIT_READINGS {
                println!(
                    "SoC limit reached ({}%) for {} consecutive readings. Disabling PSU output.",
                    data.soc, LIMIT_READINGS
                );

                if disable_output(port).await {
                    return ControlOutcome::OutputOff;
                }

                eprintln!(
                    "No OUT0 command reached the supply after {} attempts.",
                    OFF_WRITE_ATTEMPTS
                );
                return ControlOutcome::OutputOffPending;
            }
        }

        let mut params = calculate_charging_parameters(data.voltage);
        if let Some(max_i) = args.max_current {
            params.target_current = params.target_current.min(max_i);
        }

        let v_changed = (params.target_voltage - last_vset).abs() > 0.01;
        let i_changed = (params.target_current - last_iset).abs() > 0.01;

        if v_changed || i_changed {
            // --- CONTROL MODE ---
            // Give the PSU the bus to update its output instead of querying it.
            if v_changed {
                if !write_command(
                    port,
                    "VSET",
                    format!("VSET1:{:.2}\n", params.target_voltage).as_bytes(),
                )
                .await
                {
                    return ControlOutcome::LinkLost;
                }
                sleep(COMMAND_SETTLE_TIME).await;
                last_vset = params.target_voltage;
            }
            if i_changed {
                if !write_command(
                    port,
                    "ISET",
                    format!("ISET1:{:.2}\n", params.target_current).as_bytes(),
                )
                .await
                {
                    return ControlOutcome::LinkLost;
                }
                sleep(COMMAND_SETTLE_TIME).await;
                last_iset = params.target_current;
            }
            if !write_command(port, "OUT1", b"OUT1\n").await {
                return ControlOutcome::LinkLost;
            }
            actual_v = String::from("UPDATING");

            // Control mode never queries, which would let a wedged supply hide behind a
            // run of setpoint changes. Check once the silence budget runs out.
            if last_response.elapsed() >= NO_RESPONSE_BUDGET {
                match query_output_voltage(port).await {
                    VoltageProbe::Answered(v) => {
                        actual_v = v;
                        last_response = Instant::now();
                    }
                    VoltageProbe::Silent => {
                        eprintln!("PSU gave no answer to the liveness check. Reopening port.");
                        return ControlOutcome::LinkLost;
                    }
                    VoltageProbe::LinkError => return ControlOutcome::LinkLost,
                }
            }
        } else {
            // --- QUERY MODE ---
            // Everything is steady, safe to ask for data.
            match query_output_voltage(port).await {
                VoltageProbe::Answered(v) => {
                    actual_v = v;
                    last_response = Instant::now();
                }
                // Keep the last reading on screen. The budget below decides whether
                // the supply is still there.
                VoltageProbe::Silent => {}
                VoltageProbe::LinkError => return ControlOutcome::LinkLost,
            }

            // A supply that stays silent this long needs a fresh file descriptor,
            // whether it left the USB bus or only wedged. The supervisor reopens it.
            if last_response.elapsed() >= NO_RESPONSE_BUDGET {
                if find_port_path(TENMA_VID, TENMA_PID).is_none() {
                    eprintln!("PSU stopped responding and left the USB bus.");
                } else {
                    eprintln!(
                        "PSU stopped responding but is still on the USB bus. Reopening port."
                    );
                }
                return ControlOutcome::LinkLost;
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
            if let tokio_serial::SerialPortType::UsbPort(info) = p.port_type
                && info.vid == vid
                && info.pid == pid
            {
                return Some(p.port_name);
            }
            None
        })
}
