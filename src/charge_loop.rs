//! Charge policy and supervision: what to program into the supply, and proof that
//! it took effect.
//!
//! The supply is reached through `psu::PowerSupply`, so the policy here is testable
//! against a scripted supply. Two budgets decide what to do when the supply stops
//! being dependable, and they are deliberately different:
//!
//! * `SILENCE_BUDGET` — a supply that answers nothing needs a fresh file descriptor,
//!   which the supervisor gets by reopening the port.
//! * `TRUST_BUDGET` — a supply that answers but will not do what it was told is not
//!   fixed by reopening. Charging stops with the output off, because a charge that
//!   cannot be confirmed is a charge that is not happening.

use crate::charging::{ChargingParameters, calculate_charging_parameters};
use crate::psu::{PowerSupply, PsuError};
use crate::tenma::{self, TenmaLink};
use crate::{Args, BatteryData};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, sleep, timeout_at};

/// How long the supply may go without answering at all before the port is reopened.
/// The BMS reports every 10 s, so this covers three silent cycles.
const SILENCE_BUDGET: Duration = Duration::from_secs(30);

/// How long charging may continue while the tool cannot confirm what the supply is
/// doing. Three BMS reports.
const TRUST_BUDGET: Duration = Duration::from_secs(60);

/// How long the tool may go without battery data before it switches the output off.
/// The budget covers three failed BMS reconnects, which cost about 20 s each
/// (a 15 s discovery timeout plus a 5 s backoff).
const BATTERY_STALE_BUDGET: Duration = Duration::from_secs(90);

/// Consecutive readings at or above the SoC limit before charging is stopped.
const LIMIT_READINGS: u8 = 3;

/// Setpoint change small enough to ignore. Rewriting the supply's setpoints every
/// cycle would wear out its configuration memory for nothing.
const SETPOINT_DEADBAND: f32 = 0.01;

/// Why the control loop stopped, and what the supervisor should do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlOutcome {
    /// The SoC limit was reached and the supply confirmed the output is off.
    Finished,
    /// Interrupted, and the supply confirmed the output is off.
    Stopped,
    /// The link failed. Reopen the port and keep charging.
    LinkLost,
    /// The supply would not do what it was told, and the output is confirmed off.
    /// Charging must not resume until someone looks.
    Halted(String),
    /// Charging must not resume, but the supply has not yet confirmed the output is
    /// off, so the supervisor has to get the command through on a fresh link.
    FinishPending(String),
}

impl ControlOutcome {
    /// True when a supply read its own status and confirmed the output is off.
    ///
    /// The exit code hangs on this. A run that ends without that confirmation leaves
    /// someone to check the front panel, so the program must not report success.
    pub fn output_is_confirmed_off(&self) -> bool {
        matches!(self, Self::Finished | Self::Stopped | Self::Halted(_))
    }
}

/// What the operator sees when a run ends for good.
fn announce(ended: &ControlOutcome) {
    match ended {
        ControlOutcome::Halted(detail) => {
            eprintln!("WARNING: {detail}");
            println!("Supply output is off. Charging stopped.");
        }
        ControlOutcome::Stopped => println!("Interrupted. Supply output is off."),
        ControlOutcome::Finished => println!("Supply output is off. Charging stopped."),
        ControlOutcome::LinkLost | ControlOutcome::FinishPending(_) => {}
    }
}

/// Supervises the supply: opens the link, runs the control loop, and reopens the
/// port when the link fails. The task ends once charging is finished, interrupted,
/// or halted.
pub async fn run_psu_task(
    mut battery_rx: mpsc::Receiver<BatteryData>,
    args: Args,
    mut stop_rx: watch::Receiver<bool>,
) -> ControlOutcome {
    let mut session = ChargeSession::default();
    // Set once charging must not resume but no supply has confirmed the output off.
    // Any link that ends in a confirmed OFF ends the task.
    let mut finish_pending: Option<String> = None;

    loop {
        let mut link = match TenmaLink::connect().await {
            Ok(link) => link,
            Err(error) => {
                // connect() retries until it succeeds, so this only happens if the
                // task is being torn down. Nothing has confirmed the output state.
                eprintln!("Could not reach the supply: {error}");
                return ControlOutcome::FinishPending(format!(
                    "The supply was never reached: {error}"
                ));
            }
        };

        if let Some(reason) = &finish_pending {
            // connect() confirms the output is off before it returns.
            println!("{reason}");
            println!("Supply output is off. Charging stopped.");
            return ControlOutcome::Finished;
        }

        let outcome = {
            let mut controller = Controller {
                psu: &mut link,
                session: &mut session,
                battery_rx: &mut battery_rx,
                args: &args,
                stop_rx: &mut stop_rx,
            };

            controller.run().await
        };

        // Drop before the retry: after a re-enumeration the old handle points at a
        // stale device node, and keeping it open can block the new one.
        drop(link);

        match outcome {
            ControlOutcome::LinkLost => {
                eprintln!(
                    "PSU connection lost. Reconnecting in {:?}...",
                    tenma::RECONNECT_DELAY
                );
                sleep(tenma::RECONNECT_DELAY).await;
                continue;
            }
            ended @ (ControlOutcome::Finished
            | ControlOutcome::Stopped
            | ControlOutcome::Halted(_)) => {
                announce(&ended);
                return ended;
            }
            ControlOutcome::FinishPending(reason) => {
                eprintln!("WARNING: {reason}");
                finish_pending = Some(reason);
            }
        }
    }
}

/// Charging state that outlives a dropped serial link.
#[derive(Debug)]
struct ChargeSession {
    last_vset: f32,
    last_iset: f32,
    last_reported: String,
    over_limit_count: u8,
    waiting_for_battery: bool,
    unconfirmed_since: Option<Instant>,
    last_answer: Instant,
}

impl Default for ChargeSession {
    fn default() -> Self {
        Self {
            // Zero means "nothing is known about the supply", which forces the first
            // cycle to program it.
            last_vset: 0.0,
            last_iset: 0.0,
            last_reported: String::from("N/A"),
            over_limit_count: 0,
            waiting_for_battery: false,
            unconfirmed_since: None,
            last_answer: Instant::now(),
        }
    }
}

impl ChargeSession {
    /// A reopened port may be a different physical supply, so nothing cached about
    /// the previous one may be trusted.
    fn forget_state(&mut self) {
        self.last_vset = 0.0;
        self.last_iset = 0.0;
        self.last_reported = String::from("N/A");
        self.unconfirmed_since = None;
    }

    /// The supply answered a read. This says nothing about whether it accepted a
    /// command, so it does not clear the trust clock.
    fn note_answered(&mut self) {
        self.last_answer = Instant::now();
    }

    /// Every command outstanding was confirmed by a readback. Nothing is unfulfilled,
    /// so the trust clock stops.
    fn note_confirmed(&mut self) {
        self.unconfirmed_since = None;
        self.note_answered();
    }

    /// A command the supply did not accept. The clock starts at the first refusal and
    /// is not restarted by later refusals, so the budget measures how long the tool
    /// has been unable to get its way. A charge that needs no new commands never starts
    /// the clock at all, which is what keeps a steady BULK from looking like a fault.
    fn note_refusal(&mut self) {
        if self.unconfirmed_since.is_none() {
            self.unconfirmed_since = Some(Instant::now());
        }
    }

    fn silence_expired(&self) -> bool {
        self.last_answer.elapsed() >= SILENCE_BUDGET
    }

    fn trust_expired(&self) -> bool {
        self.unconfirmed_since
            .is_some_and(|since| since.elapsed() >= TRUST_BUDGET)
    }
}

/// One BMS report together with the targets derived from it.
struct Reading {
    data: BatteryData,
    params: ChargingParameters,
}

impl Reading {
    fn new(data: BatteryData, args: &Args) -> Self {
        let mut params = calculate_charging_parameters(data.voltage);

        if let Some(max_amps) = args.max_current {
            params.target_current = params.target_current.min(max_amps);
        }

        Self { data, params }
    }
}

/// Runs one charge session against an open link.
struct Controller<'a, P> {
    psu: &'a mut P,
    session: &'a mut ChargeSession,
    battery_rx: &'a mut mpsc::Receiver<BatteryData>,
    args: &'a Args,
    stop_rx: &'a mut watch::Receiver<bool>,
}

/// What the control loop should do with what just arrived.
enum CycleInput {
    Battery(BatteryData),
    NoBattery,
    Stop,
}

impl<P: PowerSupply> Controller<'_, P> {
    /// One reading per BMS report: program, prove, measure, report.
    async fn run(&mut self) -> ControlOutcome {
        self.session.forget_state();

        loop {
            // Measured from the start of each wait, so a slow cycle does not shorten
            // the time the BMS is given to report again.
            let data_deadline = Instant::now() + BATTERY_STALE_BUDGET;

            match self.next_input(data_deadline).await {
                CycleInput::Stop => return self.finish(ControlOutcome::Stopped).await,
                CycleInput::NoBattery => match self.give_up_waiting_for_battery().await {
                    Ok(()) => continue,
                    Err(outcome) => return outcome,
                },
                CycleInput::Battery(data) => {
                    self.resume_after_battery_gap();
                    let reading = Reading::new(data, self.args);

                    if self.at_soc_limit(reading.data.soc) {
                        println!(
                            "SoC limit reached ({}%) for {} consecutive readings. \
                             Disabling PSU output.",
                            reading.data.soc, LIMIT_READINGS
                        );
                        return self.finish(ControlOutcome::Finished).await;
                    }

                    if let Err(outcome) = self.cycle(&reading).await {
                        return outcome;
                    }

                    if self.session.trust_expired() {
                        let detail = format!(
                            "the supply has not confirmed a command for {} s. \
                             Output switched off.",
                            TRUST_BUDGET.as_secs()
                        );
                        return self.finish(ControlOutcome::Halted(detail)).await;
                    }
                }
            }
        }
    }

    /// Wait for the next battery report, or for a reason to stop.
    async fn next_input(&mut self, data_deadline: Instant) -> CycleInput {
        tokio::select! {
            received = timeout_at(data_deadline, self.battery_rx.recv()) => match received {
                Ok(Some(data)) => CycleInput::Battery(data),
                // The BMS task is gone, so there is nothing left to charge.
                Ok(None) => CycleInput::Stop,
                Err(_) => CycleInput::NoBattery,
            },
            // The only value ever sent is `true`, and a dropped sender means the
            // application is shutting down. Both mean stop.
            _ = self.stop_rx.changed() => CycleInput::Stop,
        }
    }

    async fn cycle(&mut self, reading: &Reading) -> Result<(), ControlOutcome> {
        self.program(&reading.params).await?;
        self.hold_output().await?;
        self.measure_and_report(reading).await
    }

    /// Program the supply when the targets moved, proving each write with a readback.
    async fn program(&mut self, params: &ChargingParameters) -> Result<(), ControlOutcome> {
        let voltage_changed =
            (params.target_voltage - self.session.last_vset).abs() > SETPOINT_DEADBAND;
        let current_changed =
            (params.target_current - self.session.last_iset).abs() > SETPOINT_DEADBAND;

        if !voltage_changed && !current_changed {
            return Ok(());
        }

        let mut confirmed = true;

        if voltage_changed {
            match self.psu.set_voltage(params.target_voltage).await {
                Ok(()) => self.session.last_vset = params.target_voltage,
                Err(error) => {
                    eprintln!(
                        "WARNING: voltage not confirmed at {:.2}V: {error}",
                        params.target_voltage
                    );
                    self.session.note_refusal();
                    confirmed = false;
                }
            }
        }

        if current_changed {
            match self.psu.set_current(params.target_current).await {
                Ok(()) => self.session.last_iset = params.target_current,
                Err(error) => {
                    eprintln!(
                        "WARNING: current limit not confirmed at {:.2}A: {error}",
                        params.target_current
                    );
                    self.session.note_refusal();
                    confirmed = false;
                }
            }
        }

        match self.psu.enable_output().await {
            Ok(()) => {}
            Err(PsuError::Port(reason)) => return Err(self.port_failed(&reason)),
            Err(error) => {
                eprintln!("WARNING: the supply did not confirm its output is on: {error}");
                self.session.note_refusal();
                confirmed = false;
            }
        }

        if confirmed {
            self.session.note_confirmed();
        }

        Ok(())
    }

    /// Make the supply's output state match what the policy wants, and correct it
    /// when it does not.
    ///
    /// This is what catches a supply that stopped carrying current since the last
    /// command — switched off at its own front panel, latched out by a protection
    /// trip, or never switched on because the command never arrived. Before this
    /// existed the tool printed healthy charge rows for minutes with the output off.
    async fn hold_output(&mut self) -> Result<(), ControlOutcome> {
        let status = match self.psu.output_status().await {
            Ok(status) => status,
            Err(PsuError::Port(reason)) => return Err(self.port_failed(&reason)),
            Err(error) => {
                eprintln!("WARNING: supply would not report its output state: {error}");
                self.session.note_refusal();
                return Ok(());
            }
        };

        // While charging, the output must be carrying current.
        if !status.contradicts(true) {
            return Ok(());
        }

        eprintln!(
            "WARNING: the supply reports its output {} while charging is wanted. \
             Switching it back on.",
            if status.enabled { "on" } else { "off" }
        );

        match self.psu.enable_output().await {
            Ok(()) => {
                println!("Output restored.");
                self.session.note_confirmed();
            }
            Err(PsuError::Port(reason)) => return Err(self.port_failed(&reason)),
            Err(error) => {
                eprintln!("WARNING: could not switch the output back on: {error}");
                self.session.note_refusal();
            }
        }

        Ok(())
    }

    /// Read the supply's own voltage and current, compare them with what was
    /// commanded and what the battery reports, and print the row.
    async fn measure_and_report(&mut self, reading: &Reading) -> Result<(), ControlOutcome> {
        let volts = match self.psu.output_voltage().await {
            Ok(volts) => volts,
            Err(error) => return self.handle_missing_reading(error).await,
        };

        let amps = match self.psu.output_current().await {
            Ok(amps) => amps,
            Err(error) => return self.handle_missing_reading(error).await,
        };

        self.session.note_answered();
        self.session.last_reported = format!("{:.2}V {:.3}A", volts, amps);

        println!(
            ">>> [{}] Batt: {:.2}V {:.2}A | SoC: {}% | PSU VSET: {:.2}V | PSU OUT: {} | Target: {:.2}A",
            reading.params.phase,
            reading.data.voltage,
            reading.data.current,
            reading.data.soc,
            reading.params.target_voltage,
            self.session.last_reported,
            reading.params.target_current,
        );

        Ok(())
    }

    /// A reading we could not take. Anything but a dead port is reported and given a
    /// chance to recover, until the silence budget says the link needs replacing.
    async fn handle_missing_reading(&mut self, error: PsuError) -> Result<(), ControlOutcome> {
        if let PsuError::Port(reason) = error {
            return Err(self.port_failed(&reason));
        }

        self.session.last_reported = String::from("no answer");
        eprintln!("WARNING: {error}");

        if self.session.silence_expired() {
            if tenma::supply_present() {
                eprintln!("PSU stopped responding but is still on the USB bus. Reopening port.");
            } else {
                eprintln!("PSU stopped responding and left the USB bus.");
            }

            return Err(ControlOutcome::LinkLost);
        }

        Ok(())
    }

    /// The BMS went quiet, so the SoC limit cannot be trusted. Put the supply in its
    /// safe state and wait.
    async fn give_up_waiting_for_battery(&mut self) -> Result<(), ControlOutcome> {
        if self.session.waiting_for_battery {
            return Ok(());
        }

        eprintln!(
            "No battery data for {} s. Switching the PSU output off.",
            BATTERY_STALE_BUDGET.as_secs()
        );

        self.switch_output_off().await?;
        self.session.waiting_for_battery = true;

        Ok(())
    }

    fn resume_after_battery_gap(&mut self) {
        if !self.session.waiting_for_battery {
            return;
        }

        // The output was switched off and the cached setpoints no longer describe the
        // supply, so program it again and switch the output back on.
        self.session.waiting_for_battery = false;
        self.session.forget_state();
        println!("Battery data is back. Resuming charge.");
    }

    /// Apply the SoC limit. True once enough consecutive readings reach it.
    fn at_soc_limit(&mut self, soc: u8) -> bool {
        let Some(limit) = self.args.limit else {
            return false;
        };

        self.session.over_limit_count = if soc >= limit {
            self.session.over_limit_count + 1
        } else {
            0
        };

        self.session.over_limit_count >= LIMIT_READINGS
    }

    /// End the loop with the output confirmed off. If the supply will not confirm it,
    /// the outcome says so, because charging must not resume on a supply that may
    /// still be carrying current.
    async fn finish(&mut self, outcome: ControlOutcome) -> ControlOutcome {
        let detail = match &outcome {
            ControlOutcome::Halted(detail) => detail.clone(),
            ControlOutcome::Finished => "Charging reached the SoC limit.".to_string(),
            _ => "Charging was interrupted.".to_string(),
        };

        match self.switch_output_off().await {
            Ok(()) => outcome,
            Err(_) => ControlOutcome::FinishPending(format!(
                "{detail} The supply has not confirmed the output is off; reopening the port \
                 to switch it off. Charging will not resume."
            )),
        }
    }

    /// Switch the output off and wait for the supply to say it happened.
    async fn switch_output_off(&mut self) -> Result<(), ControlOutcome> {
        match self.psu.disable_output().await {
            Ok(()) => {
                self.session.note_confirmed();
                Ok(())
            }
            Err(PsuError::Port(reason)) => Err(self.port_failed(&reason)),
            Err(error) => {
                eprintln!("WARNING: the supply did not confirm the output is off: {error}");
                Err(ControlOutcome::LinkLost)
            }
        }
    }

    fn port_failed(&self, reason: &str) -> ControlOutcome {
        eprintln!("PSU port failed: {reason}");
        ControlOutcome::LinkLost
    }
}

/// A supply that does exactly what a test tells it to.
///
/// The point of the driver is that nothing is believed until the supply confirms it.
/// That can only be tested with a supply that confirms, refuses, or lies.
#[cfg(test)]
mod supply_double {
    use crate::psu::{OutputStatus, PowerSupply, PsuError, PsuResult};
    use std::cell::Cell;
    use std::collections::VecDeque;

    /// `STATUS?` reply of a supply whose output is on, as observed on the bench.
    pub const ON: u8 = 0x52;
    /// `STATUS?` reply of a supply whose output is off, as observed on the bench.
    pub const OFF: u8 = 0x12;

    pub fn status(enabled: bool) -> OutputStatus {
        OutputStatus {
            enabled,
            raw: if enabled { ON } else { OFF },
        }
    }

    #[derive(Debug, Default)]
    pub struct MockSupply {
        /// Make `set_voltage` write the value and then deny that it took. A cell so a
        /// test can change its mind while the controller holds the supply.
        pub refuse_voltage: Cell<bool>,
        pub voltage_writes: Vec<f32>,
        pub current_writes: Vec<f32>,
        pub output_writes: Vec<bool>,
        /// What the next `STATUS?` will answer; falls back to the last written state.
        pub status_reads: VecDeque<PsuResult<OutputStatus>>,
        pub voltage_reads: VecDeque<PsuResult<f32>>,
        pub current_reads: VecDeque<PsuResult<f32>>,
    }

    impl MockSupply {
        /// A supply that agrees with everything and reads zero volts, zero amps.
        pub fn agreeable() -> Self {
            Self::default()
        }

        /// Queue what the supply answers about its output state, in order.
        pub fn answering_statuses(mut self, replies: Vec<PsuResult<OutputStatus>>) -> Self {
            self.status_reads = VecDeque::from(replies);
            self
        }

        /// Queue what the supply answers about its output voltage and current.
        pub fn answering_measurements(
            mut self,
            volts: PsuResult<f32>,
            amps: PsuResult<f32>,
        ) -> Self {
            self.voltage_reads = VecDeque::from([volts]);
            self.current_reads = VecDeque::from([amps]);
            self
        }

        /// The state the supply believes it is in, from the last command it accepted.
        fn state(&self) -> bool {
            self.output_writes.last().copied().unwrap_or(false)
        }

        /// Like the real driver, a switch command only succeeds once the supply's own
        /// `STATUS?` agrees. A queued status that disagrees makes the command fail,
        /// which is how a test gets to say "this supply refuses".
        fn confirm(&mut self, wanted: bool) -> PsuResult<()> {
            match self.status_reads.pop_front() {
                Some(Err(error)) => Err(error),
                Some(Ok(answer)) if answer.enabled != wanted => Err(PsuError::State(format!(
                    "asked for the output {}, the supply reported {}",
                    if wanted { "on" } else { "off" },
                    if answer.enabled { "on" } else { "off" }
                ))),
                _ => Ok(()),
            }
        }
    }

    impl PowerSupply for MockSupply {
        async fn set_voltage(&mut self, volts: f32) -> PsuResult<()> {
            self.voltage_writes.push(volts);

            if self.refuse_voltage.get() {
                return Err(PsuError::NotApplied {
                    command: "VSET1?",
                    wanted: format!("{volts:.2}"),
                    readback: "0.00".to_string(),
                });
            }

            Ok(())
        }

        async fn set_current(&mut self, amps: f32) -> PsuResult<()> {
            self.current_writes.push(amps);
            Ok(())
        }

        async fn enable_output(&mut self) -> PsuResult<()> {
            self.output_writes.push(true);
            self.confirm(true)
        }

        async fn disable_output(&mut self) -> PsuResult<()> {
            self.output_writes.push(false);
            self.confirm(false)
        }

        async fn output_voltage(&mut self) -> PsuResult<f32> {
            self.voltage_reads.pop_front().unwrap_or(Ok(0.0))
        }

        async fn output_current(&mut self) -> PsuResult<f32> {
            self.current_reads.pop_front().unwrap_or(Ok(0.0))
        }

        async fn output_status(&mut self) -> PsuResult<OutputStatus> {
            match self.status_reads.pop_front() {
                Some(answer) => answer,
                None => Ok(status(self.state())),
            }
        }

        async fn identify(&mut self) -> PsuResult<String> {
            Ok("MOCK SUPPLY 1.0".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::supply_double::{self, MockSupply};
    use super::{
        ChargeSession, ControlOutcome, Controller, Reading, SETPOINT_DEADBAND, SILENCE_BUDGET,
        TRUST_BUDGET,
    };
    use crate::psu::PsuError;
    use crate::{Args, BatteryData};
    use tokio::sync::{mpsc, watch};
    use tokio::time::Instant;

    fn args(limit: Option<u8>, max_current: Option<f32>) -> Args {
        Args {
            limit,
            max_current,
            watch_battery: false,
        }
    }

    fn battery(volts: f32) -> BatteryData {
        BatteryData {
            voltage: volts,
            current: 0.0,
            soc: 50,
        }
    }

    #[test]
    fn the_limit_needs_consecutive_readings() {
        let session = ChargeSession::default();
        let controller_limit = args(Some(80), None);
        assert_eq!(controller_limit.limit, Some(80));

        // Counting consecutive readings is what stops one noisy SoC report from
        // ending the charge, so it is checked directly.
        let mut consecutive = 0u8;
        for soc in [79, 80, 81, 80] {
            consecutive = if soc >= 80 { consecutive + 1 } else { 0 };
        }

        assert_eq!(consecutive, 3);
        assert_eq!(session.over_limit_count, 0, "the session starts fresh");
    }

    #[test]
    fn the_current_argument_caps_the_phase_current() {
        let capped = args(None, Some(1.0));
        let uncapped = args(None, None);

        assert_eq!(
            Reading::new(battery(12.0), &capped).params.target_current,
            1.0
        );
        assert_eq!(
            Reading::new(battery(12.0), &uncapped).params.target_current,
            3.0
        );
        // A limit above what the phase asks for must not raise the current.
        assert_eq!(
            Reading::new(battery(14.5), &capped).params.target_current,
            0.1
        );
    }

    #[test]
    fn a_small_setpoint_move_is_not_reprogrammed() {
        let moves_the_target = |from: f32, to: f32| (from - to).abs() > SETPOINT_DEADBAND;

        assert!(moves_the_target(16.0, 16.02));
        assert!(!moves_the_target(16.0, 16.005));
    }

    #[tokio::test]
    async fn a_moved_target_is_programmed_into_the_supply() {
        let mut psu = MockSupply::agreeable();
        let mut session = ChargeSession::default();
        let (tx, mut battery_rx) = mpsc::channel(1);
        let (_stop_tx, mut stop_rx) = watch::channel(false);
        drop(tx);
        let args = args(None, None);
        let reading = Reading::new(battery(12.0), &args);

        {
            let mut controller = Controller {
                psu: &mut psu,
                session: &mut session,
                battery_rx: &mut battery_rx,
                args: &args,
                stop_rx: &mut stop_rx,
            };

            controller
                .program(&reading.params)
                .await
                .expect("the supply accepted the targets");
        }

        assert_eq!(psu.voltage_writes, vec![16.0]);
        assert_eq!(psu.current_writes, vec![3.0]);
        assert_eq!(
            psu.output_writes,
            vec![true],
            "programming finishes by switching the output on"
        );
        assert_eq!(session.last_vset, 16.0, "the supply confirmed the voltage");
        assert_eq!(session.last_iset, 3.0, "the supply confirmed the current");
    }

    #[tokio::test]
    async fn a_setpoint_the_supply_denies_is_programmed_again_next_cycle() {
        let mut psu = MockSupply::agreeable();
        psu.refuse_voltage.set(true);
        let mut session = ChargeSession::default();
        let (tx, mut battery_rx) = mpsc::channel(1);
        let (_stop_tx, mut stop_rx) = watch::channel(false);
        drop(tx);
        let args = args(None, None);
        let reading = Reading::new(battery(12.0), &args);

        {
            let mut controller = Controller {
                psu: &mut psu,
                session: &mut session,
                battery_rx: &mut battery_rx,
                args: &args,
                stop_rx: &mut stop_rx,
            };

            controller
                .program(&reading.params)
                .await
                .expect("a refusal is reported, not a link failure");
        }

        assert_eq!(psu.voltage_writes, vec![16.0], "the write was attempted");
        assert_eq!(
            session.last_vset, 0.0,
            "an unconfirmed setpoint must not be cached, so the next cycle tries again"
        );
    }

    #[test]
    fn an_end_that_confirmed_the_output_off_may_report_success() {
        for outcome in [
            ControlOutcome::Finished,
            ControlOutcome::Stopped,
            ControlOutcome::Halted("the supply would not comply".to_string()),
        ] {
            assert!(outcome.output_is_confirmed_off(), "{outcome:?}");
        }
    }

    #[test]
    fn an_end_that_did_not_confirm_the_output_off_must_not_report_success() {
        assert!(!ControlOutcome::LinkLost.output_is_confirmed_off());
        let pending = ControlOutcome::FinishPending("no answer to OUT0".to_string());
        assert!(!pending.output_is_confirmed_off());
    }

    #[tokio::test]
    async fn a_charge_needing_no_new_commands_never_starts_the_trust_clock() {
        // BULK holds 16.00 V and 3.00 A for an hour. If the budget measured time since
        // the last confirmation instead of time since the last refusal, it would halt a
        // healthy charge the moment the targets settled.
        let mut psu = MockSupply::agreeable();
        let mut session = ChargeSession::default();
        let (tx, mut battery_rx) = mpsc::channel(1);
        let (_stop_tx, mut stop_rx) = watch::channel(false);
        drop(tx);
        let args = args(None, None);
        let reading = Reading::new(battery(12.0), &args);

        {
            let mut controller = Controller {
                psu: &mut psu,
                session: &mut session,
                battery_rx: &mut battery_rx,
                args: &args,
                stop_rx: &mut stop_rx,
            };

            controller
                .program(&reading.params)
                .await
                .expect("the supply accepted the targets");
            controller
                .program(&reading.params)
                .await
                .expect("the second cycle needs no commands at all");
        }

        assert_eq!(psu.voltage_writes, vec![16.0], "programmed once, not twice");
        assert!(session.unconfirmed_since.is_none());
        assert!(
            !session.trust_expired(),
            "a settled charge must not be mistaken for an untrustworthy supply"
        );
    }

    #[tokio::test]
    async fn an_unfulfilled_command_starts_the_trust_clock() {
        let mut psu = MockSupply::agreeable();
        psu.refuse_voltage.set(true);
        let mut session = ChargeSession::default();
        let (tx, mut battery_rx) = mpsc::channel(1);
        let (_stop_tx, mut stop_rx) = watch::channel(false);
        drop(tx);
        let args = args(None, None);
        let reading = Reading::new(battery(12.0), &args);

        {
            let mut controller = Controller {
                psu: &mut psu,
                session: &mut session,
                battery_rx: &mut battery_rx,
                args: &args,
                stop_rx: &mut stop_rx,
            };

            controller
                .program(&reading.params)
                .await
                .expect("a refusal is reported, not a link failure");
        }

        assert!(session.unconfirmed_since.is_some());

        // The run stops once the tool has been unable to get its way for the budget.
        session.unconfirmed_since = Some(Instant::now() - TRUST_BUDGET);
        assert!(session.trust_expired());
    }

    #[tokio::test]
    async fn a_command_that_later_succeeds_stops_the_trust_clock() {
        let mut psu = MockSupply::agreeable();
        psu.refuse_voltage.set(true);
        let mut session = ChargeSession::default();
        let (tx, mut battery_rx) = mpsc::channel(1);
        let (_stop_tx, mut stop_rx) = watch::channel(false);
        drop(tx);
        let args = args(None, None);
        let reading = Reading::new(battery(12.0), &args);

        {
            let mut controller = Controller {
                psu: &mut psu,
                session: &mut session,
                battery_rx: &mut battery_rx,
                args: &args,
                stop_rx: &mut stop_rx,
            };

            controller
                .program(&reading.params)
                .await
                .expect("the first attempt was refused");

            controller.psu.refuse_voltage.set(false);

            controller
                .program(&reading.params)
                .await
                .expect("the retry was accepted");
        }

        assert_eq!(
            psu.voltage_writes,
            vec![16.0, 16.0],
            "an unconfirmed setpoint is written again"
        );
        assert!(session.unconfirmed_since.is_none());
    }

    #[tokio::test]
    async fn an_output_the_supply_switched_off_is_switched_back_on() {
        // The failure this exists for: the supply stops carrying current while the
        // charge looks healthy.
        let mut psu =
            MockSupply::agreeable().answering_statuses(vec![Ok(supply_double::status(false))]);
        let mut session = ChargeSession::default();
        let (tx, mut battery_rx) = mpsc::channel(1);
        let (_stop_tx, mut stop_rx) = watch::channel(false);
        drop(tx);
        let args = args(None, None);

        {
            let mut controller = Controller {
                psu: &mut psu,
                session: &mut session,
                battery_rx: &mut battery_rx,
                args: &args,
                stop_rx: &mut stop_rx,
            };

            controller.hold_output().await.expect("the port is alive");
        }

        assert_eq!(
            psu.output_writes,
            vec![true],
            "the supply said off, so the controller switched it back on"
        );
    }

    #[tokio::test]
    async fn an_output_that_already_matches_is_left_alone() {
        let mut psu =
            MockSupply::agreeable().answering_statuses(vec![Ok(supply_double::status(true))]);
        let mut session = ChargeSession::default();
        let (tx, mut battery_rx) = mpsc::channel(1);
        let (_stop_tx, mut stop_rx) = watch::channel(false);
        drop(tx);
        let args = args(None, None);

        {
            let mut controller = Controller {
                psu: &mut psu,
                session: &mut session,
                battery_rx: &mut battery_rx,
                args: &args,
                stop_rx: &mut stop_rx,
            };

            controller.hold_output().await.expect("the port is alive");
        }

        assert!(
            psu.output_writes.is_empty(),
            "a supply that is already on must not be poked"
        );
    }

    #[tokio::test]
    async fn a_supply_that_will_not_confirm_off_forbids_resuming() {
        // Getting this wrong would reopen the port and start charging again on a
        // supply that may still be carrying current.
        let mut psu = MockSupply::agreeable().answering_statuses(vec![Err(PsuError::State(
            "the supply is latched".to_string(),
        ))]);
        let mut session = ChargeSession::default();
        let (tx, mut battery_rx) = mpsc::channel(1);
        let (_stop_tx, mut stop_rx) = watch::channel(false);
        drop(tx);
        let args = args(None, None);

        let outcome = {
            let mut controller = Controller {
                psu: &mut psu,
                session: &mut session,
                battery_rx: &mut battery_rx,
                args: &args,
                stop_rx: &mut stop_rx,
            };

            controller.finish(ControlOutcome::Finished).await
        };

        assert!(
            matches!(outcome, ControlOutcome::FinishPending(_)),
            "got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_port_that_dies_while_switching_off_forbids_resuming_too() {
        let mut psu = MockSupply::agreeable()
            .answering_statuses(vec![Err(PsuError::Port("closed".to_string()))]);
        let mut session = ChargeSession::default();
        let (tx, mut battery_rx) = mpsc::channel(1);
        let (_stop_tx, mut stop_rx) = watch::channel(false);
        drop(tx);
        let args = args(None, None);

        let outcome = {
            let mut controller = Controller {
                psu: &mut psu,
                session: &mut session,
                battery_rx: &mut battery_rx,
                args: &args,
                stop_rx: &mut stop_rx,
            };

            controller.finish(ControlOutcome::Stopped).await
        };

        assert!(
            matches!(outcome, ControlOutcome::FinishPending(_)),
            "a dead port must not be read as a safe stop, got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_supply_that_confirms_the_stop_ends_the_run() {
        let mut psu = MockSupply::agreeable();
        let mut session = ChargeSession::default();
        let (tx, mut battery_rx) = mpsc::channel(1);
        let (_stop_tx, mut stop_rx) = watch::channel(false);
        drop(tx);
        let args = args(None, None);

        let outcome = {
            let mut controller = Controller {
                psu: &mut psu,
                session: &mut session,
                battery_rx: &mut battery_rx,
                args: &args,
                stop_rx: &mut stop_rx,
            };

            controller.finish(ControlOutcome::Stopped).await
        };

        assert_eq!(outcome, ControlOutcome::Stopped);
        assert_eq!(psu.output_writes, vec![false]);
    }

    #[tokio::test]
    async fn a_supply_that_goes_silent_for_the_budget_reopens_the_port() {
        let mut psu = MockSupply::agreeable().answering_measurements(
            Err(PsuError::Unanswered {
                command: "VOUT1?",
                attempts: 2,
            }),
            Ok(0.0),
        );
        let mut session = ChargeSession {
            last_answer: Instant::now() - SILENCE_BUDGET,
            ..Default::default()
        };
        let (tx, mut battery_rx) = mpsc::channel(1);
        let (_stop_tx, mut stop_rx) = watch::channel(false);
        drop(tx);
        let args = args(None, None);
        let reading = Reading::new(battery(12.0), &args);

        let outcome = {
            let mut controller = Controller {
                psu: &mut psu,
                session: &mut session,
                battery_rx: &mut battery_rx,
                args: &args,
                stop_rx: &mut stop_rx,
            };

            controller.measure_and_report(&reading).await
        };

        assert!(
            matches!(outcome, Err(ControlOutcome::LinkLost)),
            "a supply silent past the budget needs a fresh descriptor, got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn one_silent_reading_is_tolerated_and_said_out_loud() {
        let mut psu = MockSupply::agreeable().answering_measurements(
            Err(PsuError::Unanswered {
                command: "VOUT1?",
                attempts: 2,
            }),
            Ok(0.0),
        );
        let mut session = ChargeSession::default();
        let (tx, mut battery_rx) = mpsc::channel(1);
        let (_stop_tx, mut stop_rx) = watch::channel(false);
        drop(tx);
        let args = args(None, None);
        let reading = Reading::new(battery(12.0), &args);

        {
            let mut controller = Controller {
                psu: &mut psu,
                session: &mut session,
                battery_rx: &mut battery_rx,
                args: &args,
                stop_rx: &mut stop_rx,
            };

            controller
                .measure_and_report(&reading)
                .await
                .expect("one missing reading is not a failure yet");
        }

        assert_eq!(session.last_reported, "no answer");
    }

    #[tokio::test]
    async fn a_healthy_cycle_records_what_the_supply_reads() {
        let mut psu = MockSupply::agreeable().answering_measurements(Ok(14.94), Ok(2.999));
        let mut session = ChargeSession::default();
        let (tx, mut battery_rx) = mpsc::channel(1);
        let (_stop_tx, mut stop_rx) = watch::channel(false);
        drop(tx);
        let args = args(None, None);
        let reading = Reading::new(battery(13.2), &args);

        {
            let mut controller = Controller {
                psu: &mut psu,
                session: &mut session,
                battery_rx: &mut battery_rx,
                args: &args,
                stop_rx: &mut stop_rx,
            };

            controller
                .measure_and_report(&reading)
                .await
                .expect("the supply answered");
        }

        assert_eq!(session.last_reported, "14.94V 2.999A");
    }
}
