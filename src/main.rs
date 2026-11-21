use btleplug::api::{Central, Manager as _, Peripheral, ScanFilter, WriteType};
use btleplug::platform::Manager;
use clap::Parser;
use futures::stream::StreamExt;
use stanley_charger::Args;
use stanley_charger::BatteryData;
use stanley_charger::charge_loop::{self, ControlOutcome};
use stanley_charger::decode::{BmsStream, FrameRecord, Frames, Outcome};
use std::error::Error;
use std::process::exit;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::Mutex;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinError;
use tokio::time::{interval, sleep, timeout};
use uuid::{Uuid, uuid};

const TARGET_MAC: &str = "A5:C2:37:02:49:87";
const BMS_RX_UUID: Uuid = uuid!("0000ff01-0000-1000-8000-00805f9b34fb");
const BMS_TX_UUID: Uuid = uuid!("0000ff02-0000-1000-8000-00805f9b34fb");
const BMS_SERVICE_UUID: Uuid = uuid!("0000ff00-0000-1000-8000-00805f9b34fb");
const QUERY_BATTERY: [u8; 7] = [0xdd, 0xa5, 0x03, 0x00, 0xff, 0xfd, 0x77];

/// How often the battery is asked for a report while watching it.
const QUERY_EVERY: Duration = Duration::from_secs(2);

/// How long to wait for the battery to finish an answer. The whole answer arrives well
/// inside this; the window is there so a silent battery is noticed rather than waited on.
const ANSWER_WINDOW: Duration = Duration::from_secs(2);

/// How long the supply is given to confirm the output is off after an interrupt.
/// A cycle that is already running finishes within about a second of serial time.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(15);

/// Exit code for a run that ended without the supply confirming its output is off.
/// Anyone supervising the program needs to hear about that case in more than the log.
const EXIT_UNCONFIRMED: i32 = 1;

/// How long one attempt to reach the battery may take. BlueZ gives up on a connect to a
/// device that is not answering only after a while, long enough for the program to look
/// hung, and killing it is how a link nobody owns gets left behind.
const CONNECT_BUDGET: Duration = Duration::from_secs(30);

/// How long the radio is given to answer a request to drop a leftover link. `btmgmt` can
/// sit waiting for an event that never arrives, and that must not hold up an exit.
const STALE_LINK_BUDGET: Duration = Duration::from_secs(5);

/// The battery link, so the process that ends can put the radio back.
///
/// BlueZ leaves the link up when a client's D-Bus connection simply disappears, and this
/// Venus OS build re-establishes it for a trusted device. A JBD BMS serves one central, so
/// a leftover link locks the phone app out until someone disconnects by hand.
type BatteryLink = Mutex<Option<btleplug::platform::Peripheral>>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();

    if args.watch_battery {
        return watch_only().await;
    }

    let (v_tx, v_rx) = mpsc::channel::<BatteryData>(10);

    // Interrupt handler for Ctrl-C and the signal a service manager sends on stop.
    // Built here because signal() needs a running Tokio reactor.
    let mut terminate = signal(SignalKind::terminate())?;

    // --- Task 1: Tenma PSU Controller ---
    let (stop_tx, stop_rx) = watch::channel(false);
    let mut psu_task = tokio::spawn(charge_loop::run_psu_task(v_rx, args, stop_rx.clone()));

    // --- Task 2: BMS Monitor (with Recovery) ---
    let battery_link: BatteryLink = Mutex::new(None);

    // Whether a connect attempt is in flight right now. One of these is dropped when an
    // interrupt wins the select below, and it can still complete afterwards, leaving a
    // link this process never held and therefore cannot hand back. Only that case needs
    // the kernel's help; a link the process does hold is released by name.
    let connecting = Arc::new(AtomicBool::new(false));
    let manager = Manager::new().await?;
    let adapter = manager
        .adapters()
        .await?
        .into_iter()
        .next()
        .ok_or("No BT adapter")?;

    loop {
        tokio::select! {
            ended = &mut psu_task => {
                println!("PSU task ended. Exiting application.");
                release_battery(&battery_link).await;
                leave(confirmed_off(ended));
            }
            _ = shutdown_on_request(&mut terminate) => {
                println!("Interrupted. Switching the supply output off.");
                // Closing the battery channel also releases the BMS task, and the
                // watch flag takes the charge loop out of its wait for the next report.
                drop(v_tx);
                let _ = stop_tx.send(true);

                let ended = timeout(SHUTDOWN_GRACE, &mut psu_task).await;
                release_battery(&battery_link).await;

                if connecting.load(Ordering::Relaxed) {
                    println!("A connect was still in flight, so a link may be left behind.");
                    clear_stale_link().await;
                }

                match ended {
                    Ok(joined) => leave(confirmed_off(joined)),
                    // The task is still inside a write, so nothing can vouch for the output.
                    Err(_elapsed) => leave(false),
                }
            }
            _ = async {
                let _ = adapter.start_scan(ScanFilter::default()).await;

                connecting.store(true, Ordering::Relaxed);
                let reached = reach_battery(&adapter).await;
                connecting.store(false, Ordering::Relaxed);

                match reached {
                    Ok(bms) => {
                        *battery_link.lock().await = Some(bms.clone());

                        // Notifications stream is created INSIDE this function
                        // and will be dropped immediately if it returns an error.
                        if let Err(e) =
                            run_monitor_loop(&bms, v_tx.clone(), stop_rx.clone()).await
                        {
                            eprintln!("BMS Error: {}. Cleaning up...", e);
                        }

                        // Only the holder of the link disconnects, so this and
                        // release_battery() never both drop the same connection.
                        let held = battery_link.lock().await.take();
                        if let Some(bms) = held {
                            release(&bms).await;
                        }
                    }
                    Err(e) => {
                        eprintln!("Connection failed: {e}. Trying again in a moment.");
                    }
                }
                sleep(Duration::from_secs(5)).await;
            } => {}
        }
    }
}

/// Talk to the battery and nothing else, so its frames can be read while the supply
/// stays exactly where it was left.
async fn watch_only() -> Result<(), Box<dyn Error>> {
    let manager = Manager::new().await?;
    let adapter = manager
        .adapters()
        .await?
        .into_iter()
        .next()
        .ok_or("No BT adapter")?;

    println!("Battery MAC: {TARGET_MAC}");
    adapter.start_scan(ScanFilter::default()).await?;

    // Ctrl-C has to work here too. Before this, a failed connect ran to its own slow
    // end while the signal went unwatched, and the only way out was a kill that leaves
    // the battery tied up.
    let mut terminate = signal(SignalKind::terminate())?;
    let bms = tokio::select! {
        reached = reach_battery(&adapter) => reached?,
        _ = shutdown_on_request(&mut terminate) => {
            println!("Stopped while still looking for the battery.");
            println!("The supply was never opened, so nothing needs switching off.");
            // A connect dropped mid-flight can still complete, which would leave a link
            // this process cannot hand back.
            sleep(Duration::from_secs(2)).await;
            clear_stale_link().await;
            return Ok(());
        }
    };

    let watched = watch_frames(&bms, &mut terminate).await;
    release(&bms).await;
    watched
}

/// Print every notification the battery sends, then what arrived in total.
async fn watch_frames(
    bms: &btleplug::platform::Peripheral,
    terminate: &mut Signal,
) -> Result<(), Box<dyn Error>> {
    let chars = bms.characteristics();
    let rx_char = chars
        .iter()
        .find(|c| c.uuid == BMS_RX_UUID)
        .ok_or("RX not found")?;
    let tx_char = chars
        .iter()
        .find(|c| c.uuid == BMS_TX_UUID)
        .ok_or("TX not found")?;

    let mut notifications = bms.notifications().await?;
    bms.subscribe(rx_char).await?;
    sleep(Duration::from_millis(500)).await;

    println!("Watching the battery. The serial port is not open, so the supply is untouched.");
    println!("Press Ctrl-C to stop and see what arrived.");

    let mut stream = BmsStream::default();
    let mut frames = Frames::default();
    let mut ask = interval(QUERY_EVERY);

    let reason = loop {
        tokio::select! {
            _ = ask.tick() => {
                if let Err(e) = bms.write(tx_char, &QUERY_BATTERY, WriteType::WithoutResponse).await {
                    break format!("the query could not be sent: {e}");
                }
            }
            arrived = notifications.next() => {
                match arrived {
                    Some(n) => show(frames.observe(&mut stream, &n.value), &n.value),
                    None => break "the notification stream closed".to_string(),
                }
            }
            // Covers Ctrl-C and the signal from a shell or a service manager. Watching
            // only Ctrl-C means `pkill`, which sends SIGTERM, appears to do nothing.
            _ = shutdown_on_request(terminate) => break "stopped".to_string(),
        }
    };

    println!("Stopped: {reason}.");
    print!("{}", frames.summary());
    Ok(())
}

/// One notification: what it was, what it came to, and its bytes.
fn show(record: FrameRecord, fragment: &[u8]) {
    let hex = fragment
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ");

    let verdict = if record.outcomes.is_empty() {
        "no whole frame yet".to_string()
    } else {
        record
            .outcomes
            .iter()
            .map(Outcome::label)
            .collect::<Vec<_>>()
            .join(", ")
    };

    println!(
        "{:>4}  {:>3} bytes  {:<24} {}",
        record.index,
        record.len,
        record.kind.describe(),
        verdict
    );
    println!("      {hex}");
}

/// Whether the charge task ended with the supply known to be off. A panicked task
/// proved nothing, so it counts as unconfirmed.
fn confirmed_off(joined: Result<ControlOutcome, JoinError>) -> bool {
    matches!(joined, Ok(outcome) if outcome.output_is_confirmed_off())
}

/// End the program, and let the exit code say whether someone still has to look at the supply.
fn leave(confirmed_off: bool) -> ! {
    if !confirmed_off {
        eprintln!(
            "WARNING: the supply never confirmed the output is off. \
              Check the supply's front panel before walking away."
        );
    }

    exit(if confirmed_off { 0 } else { EXIT_UNCONFIRMED })
}

/// Drop the battery link if this process still holds it, and say whether it worked.
async fn release_battery(link: &BatteryLink) {
    let held = link.lock().await.take();

    if let Some(bms) = held {
        release(&bms).await;
    }
}

/// Ask for a leftover link to the battery to be dropped, which is what a run that was
/// killed leaves behind, and what a connect dropped mid-flight can leave behind.
///
/// BlueZ is asked, because on this Cerbo it is BlueZ that holds the link. Its own
/// command answers, and the answer can be checked with `hcitool con`. `btmgmt` is the
/// fallback for a link BlueZ cannot see, and it needs the right adapter: this box keeps
/// the battery on `hci1`, and asking `hci0` does nothing useful and then sits waiting.
async fn clear_stale_link() {
    let asked = timeout(
        STALE_LINK_BUDGET,
        tokio::process::Command::new("bluetoothctl")
            .args(["--timeout", "4", "disconnect", TARGET_MAC])
            .kill_on_drop(true)
            .output(),
    )
    .await;

    match asked {
        Ok(Ok(answer)) if answer_matches("Successful", &answer.stdout) => {
            println!("Battery link dropped. A phone app can connect now.");
        }
        // BlueZ answers "Successful" only when there was a link to drop. Anything else
        // is it saying nothing of use, so claim nothing and point at the check.
        Ok(Ok(_)) => println!(
            "Asked BlueZ for the battery link; it did not confirm. Check with hcitool con."
        ),
        Ok(Err(e)) => eprintln!(
            "Could not ask BlueZ to drop the battery link: {e}.\n  \
              If a phone app cannot connect: bluetoothctl disconnect {TARGET_MAC}"
        ),
        Err(_elapsed) => eprintln!(
            "BlueZ did not answer in {}s.\n  \
              If a phone app cannot connect: bluetoothctl disconnect {TARGET_MAC}",
            STALE_LINK_BUDGET.as_secs()
        ),
    }
}

/// Whether a command's output holds the given word.
fn answer_matches(word: &str, output: &[u8]) -> bool {
    String::from_utf8_lossy(output).contains(word)
}

async fn release(bms: &btleplug::platform::Peripheral) {
    match bms.disconnect().await {
        Ok(()) => println!("Battery link released. A phone app can connect now."),
        Err(e) => eprintln!(
            "WARNING: the battery link is still up: {e}. A phone app cannot connect \
              until it is dropped: bluetoothctl disconnect {}",
            bms.address()
        ),
    }
}

/// Completes when the user presses Ctrl-C or the process is asked to stop by a
/// service manager.
async fn shutdown_on_request(terminate: &mut Signal) {
    tokio::select! {
        // Both calls re-arm the handler on completion, so the next wait installs a
        // fresh one. That makes this safe to hold inside a select loop.
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
}

/// Reach the battery within a bounded time, retrying once with the link cleared.
///
/// A leftover link from a run that was killed is the usual reason a connect fails, and
/// clearing it is cheap, so the retry costs nothing when the reason is something else.
async fn reach_battery(
    adapter: &btleplug::platform::Adapter,
) -> Result<btleplug::platform::Peripheral, Box<dyn Error>> {
    let attempt = async {
        match find_and_connect(adapter).await {
            Ok(bms) => Ok(bms),
            Err(e) => {
                clear_stale_link().await;
                find_and_connect(adapter)
                    .await
                    .map_err(|again| -> Box<dyn Error> {
                        format!("{e}; after clearing the link, {again}").into()
                    })
            }
        }
    };

    timeout(CONNECT_BUDGET, attempt)
        .await
        .map_err(|_| -> Box<dyn Error> {
            format!(
                "no answer within {}s. The battery serves one central at a time and stops \
                 advertising while it does, so close whatever app is holding it.",
                CONNECT_BUDGET.as_secs()
            )
            .into()
        })?
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

/// Read the battery until told to stop, or until the charge task stops listening.
///
/// Only a report that was actually decoded is passed on. Sending the last report again
/// would keep the charge task's idea of the battery's age quiet while telling it
/// nothing new, which is how one stale reading came to steer the supply.
async fn run_monitor_loop(
    bms: &btleplug::platform::Peripheral,
    v_tx: mpsc::Sender<BatteryData>,
    mut stop_rx: watch::Receiver<bool>,
) -> Result<(), Box<dyn Error>> {
    let mut stream = BmsStream::default();
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
        if *stop_rx.borrow_and_update() {
            return Ok(());
        }

        bms.write(tx_char, &QUERY_BATTERY, WriteType::WithoutResponse)
            .await?;

        // Read until the answer is complete. Fragments that complete nothing are
        // normal, and a window that runs out means the battery went quiet.
        let mut reported = false;
        while let Ok(Some(n)) = timeout(ANSWER_WINDOW, notifications.next()).await {
            for outcome in stream.observe(&n.value) {
                match outcome {
                    Outcome::Status(status) => {
                        println!(
                            "Battery Voltage: {:.2}V | Current: {:+.2}A | SoC: {}% | Probe: {:.1}C",
                            status.volts, status.amps, status.soc, status.degrees_c
                        );

                        if v_tx.send(status.reading()).await.is_err() {
                            // The charge task is gone, so there is nothing left to feed.
                            return Ok(());
                        }

                        reported = true;
                    }
                    Outcome::Corrupt { command } => {
                        eprintln!("Threw away a broken answer (command {command:02x}).")
                    }
                    Outcome::Resynced { dropped } => eprintln!(
                        "Lost part of an answer: {dropped} bytes dropped, \
                          the next report will be the first good one."
                    ),
                    Outcome::NotRead { .. } => {}
                }
            }

            if reported {
                break;
            }
        }

        tokio::select! {
            _ = sleep(Duration::from_secs(10)) => {}
            _ = stop_rx.changed() => {}
        }
    }
}
