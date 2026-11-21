//! Tenma 72-2940 bench supply on USB serial, Korad/RAVNI "Serial Protocol V2.0".
//!
//! This module owns the wire and nothing else: keeping the tty configured the way
//! the supply needs, and turning commands into state the supply has confirmed.
//! Charge policy lives in `charge_loop`, which sees only `psu::PowerSupply`.
//!
//! Protocol facts measured on the unit in use (firmware V6.0, `*IDN?` answers
//! `TENMA 72-2940 V6.0 SN:00105270`):
//!
//! * Queries answer with a bare, unterminated number (`14.94`, `2.999`), so a reply
//!   cannot be delimited and must be read as a burst. See `parse_measurement`.
//! * `VOUT1?`, `IOUT1?`, `VSET1?`, `ISET1?` and `*IDN?` answer. `VOUT?`, `IOUT?`
//!   and `OUT?` do not, so the output on/off state comes only from `STATUS?`.
//! * Set commands (`VSET1:`, `ISET1:`, `OUT0`, `OUT1`) answer nothing at all. They
//!   are confirmed here by reading the value, or `STATUS?`, back.

use crate::psu::{OutputStatus, PowerSupply, PsuError, PsuResult, decode_status};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{sleep, timeout};
use tokio_serial::{DataBits, Parity, SerialPortBuilderExt, SerialStream, StopBits};

// The supply enumerates as a Nuvoton "USB Virtual COM" (lsusb: 0416:5011).
const TENMA_VID: u16 = 0x0416;
const TENMA_PID: u16 = 0x5011;
const TENMA_BAUD_RATE: u32 = 9600;

pub const RECONNECT_DELAY: Duration = Duration::from_secs(2);
const DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(2);
const COMMAND_SETTLE_TIME: Duration = Duration::from_millis(100);
/// Give the supply the bus to update its output before asking it about it.
const READ_SETTLE_TIME: Duration = Duration::from_millis(250);
/// Silence this long ends a reply burst.
const READ_GAP: Duration = Duration::from_millis(200);
const DRAIN_GAP: Duration = Duration::from_millis(10);
const DRAIN_ROUNDS: u8 = 16;
const MAX_REPLY_BYTES: usize = 32;

const QUERY_ATTEMPTS: u8 = 2;
/// The handshake gets more patience: the supply may still be settling after a USB
/// re-enumeration, and without a handshake nothing else can be trusted.
const HANDSHAKE_ATTEMPTS: u8 = 5;
/// The output transistor and the supply's control loop need a moment to settle
/// into a new state.
const STATE_ATTEMPTS: u8 = 4;
const STATE_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// `VSET1?`/`ISET1?` readback tolerance. This firmware family can answer one
/// least-significant digit away from what was written.
const VOLTAGE_TOLERANCE: f32 = 0.05;
const CURRENT_TOLERANCE: f32 = 0.02;

/// A measured voltage above this is a parsing artifact, not a reading. The highest
/// setpoint this tool programs is 16.00 V, and a stray byte from an earlier reply
/// lands in front of the real value, making the reading several times too large.
const MAX_PLAUSIBLE_VOLTS: f32 = 20.0;
/// Same for current. The highest limit this tool programs is 3.00 A, so a bound of
/// 5 A also turns a stray `1` in front of `0.000` back into a zero reading.
const MAX_PLAUSIBLE_AMPS: f32 = 5.0;

/// An open, verified connection to the supply.
pub struct TenmaLink {
    port: SerialStream,
    path: String,
    /// Line settings taken from the port as `serialport` configured it. Everything
    /// the supply needs, and what the tty is put back to if another process changes
    /// it underneath us.
    line: line_settings::LineSettings,
}

impl TenmaLink {
    /// Wait for the supply on the USB bus, then open it until the link verifies.
    ///
    /// The returned link is in the tool's safe state: the handshake proved that
    /// commands reach the supply and answers come back, and the output is confirmed
    /// off. A transient USB or permissions problem must not end charging, so this
    /// retries instead of failing out.
    pub async fn connect() -> PsuResult<Self> {
        loop {
            let path = wait_for_port_path().await;

            match Self::open_verified(&path).await {
                Ok(link) => {
                    println!("Tenma found on {}; output off.", link.path);
                    return Ok(link);
                }
                Err(e) => eprintln!("Tenma on {} unusable: {}. Retrying...", path, e),
            }

            sleep(RECONNECT_DELAY).await;
        }
    }

    /// Open the port, shake hands, and switch the output off. Every step is
    /// confirmed, so a supply that cannot be trusted never reaches the charge loop.
    async fn open_verified(path: &str) -> PsuResult<Self> {
        // serialport-rs claims the port exclusively (TIOCEXCL) and puts it in raw
        // mode as it opens. Exclusivity only binds non-root processes: Venus OS can
        // and does reopen this tty as root, which is why `line` is re-asserted before
        // every exchange.
        let port = tokio_serial::new(path, TENMA_BAUD_RATE)
            .data_bits(DataBits::Eight)
            .parity(Parity::None)
            .stop_bits(StopBits::One)
            .open_native_async()
            .map_err(|e| PsuError::Port(e.to_string()))?;

        let line = line_settings::capture(&port)?;
        let mut link = Self {
            port,
            path: path.to_string(),
            line,
        };

        let identity = link.identify().await?;
        println!("Supply answered the handshake: {}", identity.trim());

        link.disable_output().await?;
        Ok(link)
    }

    /// Put the tty back the way the supply needs it, and say so when it had to be
    /// done. See `line_settings`.
    fn ensure_line_settings(&mut self) {
        if let Some(report) = line_settings::ensure_usable(&self.port, &self.line) {
            eprintln!("WARNING: {report}");
        }
    }

    /// Send a command. Set commands are not answered, so success only means the
    /// bytes reached the driver, never that the supply acted on them.
    async fn write_command(&mut self, command: &str) -> PsuResult<()> {
        self.port
            .write_all(command.as_bytes())
            .await
            .map_err(|e| PsuError::Port(e.to_string()))?;

        self.port
            .flush()
            .await
            .map_err(|e| PsuError::Port(e.to_string()))
    }

    /// Ask a question and collect the answer, retrying while the supply stays quiet.
    ///
    /// Stale bytes are dropped first: an answer that arrives late, or that another
    /// process pulled half off the wire, must not be read as the answer to this
    /// question.
    async fn query(
        &mut self,
        command: &str,
        label: &'static str,
        attempts: u8,
    ) -> PsuResult<Vec<u8>> {
        for attempt in 1..=attempts {
            self.ensure_line_settings();
            drain_input(&mut self.port).await;
            self.write_command(command).await?;
            sleep(READ_SETTLE_TIME).await;

            let reply = read_reply_burst(&mut self.port).await;
            if !reply.is_empty() {
                return Ok(reply);
            }

            if attempt < attempts {
                sleep(COMMAND_SETTLE_TIME).await;
            }
        }

        Err(PsuError::Unanswered {
            command: label,
            attempts,
        })
    }

    /// Ask a question whose answer must be a measurement.
    async fn query_measurement(
        &mut self,
        command: &str,
        label: &'static str,
        plausible_max: f32,
    ) -> PsuResult<f32> {
        let reply = self.query(command, label, QUERY_ATTEMPTS).await?;

        parse_measurement(&reply, plausible_max).ok_or_else(|| PsuError::Unreadable {
            command: label,
            reply: describe_reply(&reply),
        })
    }

    /// Program a setpoint and confirm it against the supply's own readback.
    async fn set_and_verify(&mut self, target: &Setpoint) -> PsuResult<()> {
        self.write_command(&target.write).await?;
        sleep(COMMAND_SETTLE_TIME).await;

        let read_value = self
            .query_measurement(target.readback, target.readback, target.plausible_max)
            .await?;

        if setpoint_applied(target.wanted, read_value, target.tolerance) {
            return Ok(());
        }

        Err(PsuError::NotApplied {
            command: target.readback,
            wanted: format!("{:.3}", target.wanted),
            readback: format!("{read_value:.3}"),
        })
    }

    /// Switch the output and wait for the supply to report it there.
    async fn switch_output(&mut self, wanted: Output) -> PsuResult<()> {
        self.write_command(wanted.command()).await?;
        self.await_output_state(wanted).await
    }

    /// Poll `STATUS?` until the supply reports the state it was asked for.
    async fn await_output_state(&mut self, wanted: Output) -> PsuResult<()> {
        let mut last = String::from("no answer");

        for _ in 1..=STATE_ATTEMPTS {
            sleep(STATE_POLL_INTERVAL).await;

            match self.read_output_status().await {
                Ok(status) if status.enabled == wanted.is_on() => return Ok(()),
                Ok(status) => last = format!("STATUS? = {:#04x}", status.raw),
                Err(e) => last = e.to_string(),
            }
        }

        Err(PsuError::State(format!(
            "asked for the output {}, the supply last reported {}",
            wanted.described(),
            last
        )))
    }

    async fn read_output_status(&mut self) -> PsuResult<OutputStatus> {
        // The answer is a single raw byte, so unlike the numeric replies it cannot be
        // told apart from stray bytes. `query` retries, and callers that must land on
        // a state poll it themselves.
        let reply = self.query("STATUS?\n", "STATUS?", QUERY_ATTEMPTS).await?;

        Ok(decode_status(reply[0]))
    }
}

impl PowerSupply for TenmaLink {
    async fn set_voltage(&mut self, volts: f32) -> PsuResult<()> {
        self.set_and_verify(&Setpoint::volts(volts)).await
    }

    async fn set_current(&mut self, amps: f32) -> PsuResult<()> {
        self.set_and_verify(&Setpoint::amps(amps)).await
    }

    async fn enable_output(&mut self) -> PsuResult<()> {
        self.switch_output(Output::On).await
    }

    async fn disable_output(&mut self) -> PsuResult<()> {
        self.switch_output(Output::Off).await
    }

    async fn output_voltage(&mut self) -> PsuResult<f32> {
        self.query_measurement("VOUT1?\n", "VOUT1?", MAX_PLAUSIBLE_VOLTS)
            .await
    }

    async fn output_current(&mut self) -> PsuResult<f32> {
        self.query_measurement("IOUT1?\n", "IOUT1?", MAX_PLAUSIBLE_AMPS)
            .await
    }

    async fn output_status(&mut self) -> PsuResult<OutputStatus> {
        self.read_output_status().await
    }

    async fn identify(&mut self) -> PsuResult<String> {
        let reply = self.query("*IDN?\n", "*IDN?", HANDSHAKE_ATTEMPTS).await?;

        Ok(String::from_utf8_lossy(&reply).trim().to_string())
    }
}

/// Which way the output switch should be, and how to ask for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Output {
    On,
    Off,
}

impl Output {
    fn command(self) -> &'static str {
        match self {
            Self::On => "OUT1\n",
            Self::Off => "OUT0\n",
        }
    }

    fn is_on(self) -> bool {
        self == Self::On
    }

    fn described(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Off => "off",
        }
    }
}

/// A value to program into the supply, with the query that proves it landed.
struct Setpoint {
    write: String,
    readback: &'static str,
    wanted: f32,
    tolerance: f32,
    plausible_max: f32,
}

impl Setpoint {
    fn volts(volts: f32) -> Self {
        Self {
            write: format!("VSET1:{:.2}\n", volts),
            readback: "VSET1?",
            wanted: volts,
            tolerance: VOLTAGE_TOLERANCE,
            plausible_max: MAX_PLAUSIBLE_VOLTS,
        }
    }

    fn amps(amps: f32) -> Self {
        Self {
            write: format!("ISET1:{:.3}\n", amps),
            readback: "ISET1?",
            wanted: amps,
            tolerance: CURRENT_TOLERANCE,
            plausible_max: MAX_PLAUSIBLE_AMPS,
        }
    }
}

/// Read one reply burst: bytes until the wire goes quiet.
///
/// A reply cannot be delimited because this firmware does not terminate the numbers
/// it sends. Bytes are read as they arrive and the burst ends on a gap. A reply that
/// still has bytes after the gap is logged by the caller, which shows the garbled
/// exchanges that the supply used to answer with.
async fn read_reply_burst(port: &mut SerialStream) -> Vec<u8> {
    let mut reply = Vec::new();
    let mut byte = [0u8; 1];

    while reply.len() < MAX_REPLY_BYTES {
        match timeout(READ_GAP, port.read(&mut byte)).await {
            Ok(Ok(1)) => reply.push(byte[0]),
            _ => break,
        }
    }

    reply
}

/// Throw away anything already sitting in the driver buffers.
async fn drain_input(port: &mut SerialStream) {
    let mut byte = [0u8; 1];

    for _ in 0..DRAIN_ROUNDS {
        match timeout(DRAIN_GAP, port.read(&mut byte)).await {
            Ok(Ok(1)) => {}
            _ => return,
        }
    }
}

/// Wait until the supply appears on the USB bus and return its device path.
async fn wait_for_port_path() -> String {
    let mut announced = false;

    loop {
        if let Some(path) = find_port_path() {
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

/// True while the supply is on the USB bus. Used to tell a supply that left the bus
/// apart from one that has only wedged, which are different problems.
pub fn supply_present() -> bool {
    find_port_path().is_some()
}

/// Device path of the supply, or `None` when it is not on the USB bus.
fn find_port_path() -> Option<String> {
    tokio_serial::available_ports()
        .ok()?
        .into_iter()
        .find_map(|port| {
            if let tokio_serial::SerialPortType::UsbPort(info) = port.port_type
                && info.vid == TENMA_VID
                && info.pid == TENMA_PID
            {
                return Some(port.port_name);
            }
            None
        })
}

/// Pull the measured number out of a reply burst.
///
/// A reply is normally a bare number such as `14.94` or `0.000`, sometimes with a
/// line ending or a trailing null. A stray byte can arrive in front of it, which is
/// caught here: any reading that is not plausible for this supply is dropped so the
/// caller retries or reports the link as untrusted, rather than acting on a number
/// that was never measured.
fn parse_measurement(reply: &[u8], plausible_max: f32) -> Option<f32> {
    // Drop line endings, padding, and any text the supply puts in front of the value.
    // The value a query answers with is always at the end of the reply.
    let text = String::from_utf8_lossy(reply);
    let text = text.trim_end_matches(['\0', '\r', '\n', ' ']);
    let digits = trailing_number(text);

    if digits.is_empty() {
        return None;
    }

    let value = digits.parse::<f32>().ok()?;

    if value.is_finite() && value >= 0.0 && value <= plausible_max {
        Some(value)
    } else {
        None
    }
}

/// The number at the end of `text`, or an empty slice when it ends in something else.
fn trailing_number(text: &str) -> &str {
    let bytes = text.as_bytes();
    let mut start = bytes.len();

    while start > 0 && is_number_byte(bytes[start - 1]) {
        start -= 1;
    }

    // The scan stops at the first byte that cannot be part of a number, so
    // "ISET1?3.000" yields "3.000" and "-1.50" keeps its sign and is rejected as
    // impossible for this supply.
    &text[start..]
}

fn is_number_byte(byte: u8) -> bool {
    byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'0'..=b'9')
}

/// Whether a setpoint readback is close enough to count as applied.
fn setpoint_applied(wanted: f32, readback: f32, tolerance: f32) -> bool {
    (wanted - readback).abs() <= tolerance
}

/// Short printable form of a raw reply, for error messages.
fn describe_reply(reply: &[u8]) -> String {
    if reply.is_empty() {
        return "no reply".to_string();
    }

    let printable: String = reply
        .iter()
        .map(|&b| if b.is_ascii_graphic() { b as char } else { '?' })
        .collect();

    format!("{:?} ({})", reply, printable)
}

/// The tty settings the supply needs, and the repair for when they are changed
/// underneath us.
///
/// `serialport` puts the port in raw mode as it opens, but nothing keeps it there.
/// This is a live Cerbo GX, and anything running as root can reopen `/dev/ttyACM0`
/// and leave it in canonical mode, where the driver withholds the supply's
/// unterminated replies from `read()` — the supply then looks dead although the
/// charge is still running.
///
/// Measured on this machine while `gps-dbus` and `vedirect-interface` were attached
/// to the same port: `lflag = 0x2` (ICANON on) and a baud rate of 115200. With raw
/// mode re-applied the same port answered `*IDN?` on the first try.
mod line_settings {
    use super::TENMA_BAUD_RATE;
    use crate::psu::PsuResult;
    use nix::sys::termios::{self, BaudRate, SetArg, SpecialCharacterIndices};
    use std::os::fd::{AsRawFd, BorrowedFd};

    /// Non-canonical reads with no inter-character timeout, which is what makes the
    /// reply-burst reading in this module work.
    const NO_BUFFERING: u8 = 0;

    /// The settings a supply needs, taken from the port as it was opened, and put back
    /// whenever something changes them underneath us.
    #[derive(Debug, Clone)]
    pub struct LineSettings {
        settings: termios::Termios,
    }

    impl LineSettings {
        /// How the port is configured, in the words an operator needs.
        fn describe(&self) -> String {
            let mut causes = Vec::new();

            if self
                .settings
                .local_flags
                .contains(termios::LocalFlags::ICANON)
            {
                causes.push("canonical mode holds back the supply's unterminated replies");
            }

            if !self.settings.input_flags.is_empty() || !self.settings.output_flags.is_empty() {
                causes.push("terminal processing is switched on");
            }

            if self.settings.control_chars[SpecialCharacterIndices::VMIN as usize] != NO_BUFFERING {
                causes.push("reads are waiting for a character count");
            }

            if causes.is_empty() {
                causes.push("line settings differ");
            }

            causes.join(", ")
        }
    }

    /// Take a snapshot of a freshly opened port, having first made sure it is raw at
    /// the supply's baud rate. Later drift is repaired against this snapshot.
    pub fn capture<FD: AsRawFd>(port: &FD) -> PsuResult<LineSettings> {
        let fd = borrow(port);
        let mut settings = termios::tcgetattr(fd)?;

        termios::cfmakeraw(&mut settings);
        termios::cfsetispeed(&mut settings, BaudRate::B9600)?;
        termios::cfsetospeed(&mut settings, BaudRate::B9600)?;
        settings.control_chars[SpecialCharacterIndices::VMIN as usize] = NO_BUFFERING;
        settings.control_chars[SpecialCharacterIndices::VTIME as usize] = NO_BUFFERING;

        termios::tcsetattr(fd, SetArg::TCSANOW, &settings)?;

        // Keep what the kernel accepted rather than what was asked for, so drift is
        // measured against the real configuration.
        Ok(LineSettings {
            settings: termios::tcgetattr(fd)?,
        })
    }

    /// Re-assert the captured settings when the tty has drifted, describing what was
    /// found. `None` means the port was already correct, which is the normal case.
    ///
    /// This runs before every exchange with the supply, which costs two `ioctl` calls.
    /// They are worth it: one canonical-mode setting left behind by another process
    /// stops every read while the output keeps carrying current.
    ///
    /// A process that only changes the baud rate, and leaves the flags alone, is not
    /// caught here; it shows up as silence, and the supervisor's reopen fixes it
    /// because `capture` runs again.
    pub fn ensure_usable<FD: AsRawFd>(port: &FD, wanted: &LineSettings) -> Option<String> {
        let fd = borrow(port);
        let current = match termios::tcgetattr(fd) {
            Ok(current) => current,
            // The port is gone rather than misconfigured. Let the read report that.
            Err(_) => return None,
        };

        if !has_drifted(&current, &wanted.settings) {
            return None;
        }

        let found = LineSettings { settings: current }.describe();

        match termios::tcsetattr(fd, SetArg::TCSANOW, &wanted.settings) {
            Ok(()) => Some(format!(
                "the serial port had been reconfigured ({found}); raw {TENMA_BAUD_RATE} baud \
                 was restored"
            )),
            Err(error) => Some(format!(
                "the serial port had been reconfigured ({found}); restoring it failed: {error}"
            )),
        }
    }

    fn has_drifted(current: &termios::Termios, wanted: &termios::Termios) -> bool {
        current.local_flags != wanted.local_flags
            || current.input_flags != wanted.input_flags
            || current.output_flags != wanted.output_flags
            || current.control_flags != wanted.control_flags
            || current.control_chars != wanted.control_chars
            || current.line_discipline != wanted.line_discipline
    }

    /// Wrap the port's file descriptor for `nix`, which takes `AsFd` while
    /// `SerialStream` only offers `AsRawFd`.
    ///
    /// # Safety
    ///
    /// The borrow cannot outlive the port it was taken from: it is created here and
    /// used only inside the calling termios call.
    fn borrow<FD: AsRawFd>(port: &FD) -> BorrowedFd<'_> {
        // SAFETY: `port` is open for the whole call, so the descriptor it hands out
        // stays valid for the lifetime this returns.
        unsafe { BorrowedFd::borrow_raw(port.as_raw_fd()) }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_PLAUSIBLE_AMPS, MAX_PLAUSIBLE_VOLTS, Output, Setpoint, describe_reply,
        parse_measurement, setpoint_applied, trailing_number,
    };

    fn reply(text: &str) -> Vec<u8> {
        text.as_bytes().to_vec()
    }

    fn volts(text: &str) -> Option<f32> {
        parse_measurement(&reply(text), MAX_PLAUSIBLE_VOLTS)
    }

    fn amps(text: &str) -> Option<f32> {
        parse_measurement(&reply(text), MAX_PLAUSIBLE_AMPS)
    }

    #[test]
    fn parses_the_unterminated_number_the_supply_sends() {
        // Measured on the bench: VOUT1? answers the five bytes "14.94" and nothing else.
        assert_eq!(volts("14.94"), Some(14.94));
        assert_eq!(amps("2.999"), Some(2.999));
        assert_eq!(volts("00.00"), Some(0.0), "the output-off reading");
    }

    #[test]
    fn parses_a_number_ended_by_either_line_ending() {
        assert_eq!(volts("14.94\r\n"), Some(14.94));
        assert_eq!(volts("14.94\n"), Some(14.94));
        assert_eq!(volts("14.94\r"), Some(14.94));
    }

    #[test]
    fn ignores_the_trailing_null_this_firmware_adds() {
        assert_eq!(volts("14.94\0"), Some(14.94));
        assert_eq!(amps("2.999\0\0"), Some(2.999));
    }

    #[test]
    fn drops_a_stray_leading_digit_instead_of_trusting_it() {
        // A stray byte in front of the real value is the failure this bound exists
        // for: "1" then "0.000" must not be read as a current of ten amps.
        assert_eq!(
            amps("10.000"),
            None,
            "over the plausible limit, so not a reading"
        );
        // Same shape on the voltage side, where the bound is looser.
        assert_eq!(volts("114.94"), None);
        assert_eq!(volts("14.94"), Some(14.94));
    }

    #[test]
    fn refuses_replies_that_hold_no_number() {
        assert_eq!(volts("ERROR"), None);
        assert_eq!(volts(""), None);
        assert_eq!(volts("\r\n"), None);
        assert_eq!(amps("-"), None);
    }

    #[test]
    fn negative_readings_are_not_possible_from_this_supply() {
        assert_eq!(amps("-1.50"), None);
        assert_eq!(volts("-0.01"), None);
    }

    #[test]
    fn readback_tolerance_covers_the_firmwares_last_digit_error() {
        assert!(setpoint_applied(16.00, 15.99, 0.05));
        assert!(setpoint_applied(3.000, 3.010, 0.02));
        assert!(
            !setpoint_applied(16.00, 14.40, 0.05),
            "a different setpoint"
        );
        assert!(!setpoint_applied(3.000, 0.100, 0.02));
    }

    #[test]
    fn trailing_number_stops_at_the_first_non_number() {
        assert_eq!(trailing_number("ISET1?3.000"), "3.000");
        assert_eq!(trailing_number("14.94"), "14.94");
        assert_eq!(trailing_number("no digits here!"), "");
    }

    #[test]
    fn a_setpoint_names_its_own_readback_and_format() {
        // The supply takes volts to two decimals and amps to three, and answers a
        // different query for each.
        let volts = Setpoint::volts(16.0);
        assert_eq!(volts.write, "VSET1:16.00\n");
        assert_eq!(volts.readback, "VSET1?");

        let amps = Setpoint::amps(2.5);
        assert_eq!(amps.write, "ISET1:2.500\n");
        assert_eq!(amps.readback, "ISET1?");
    }

    #[test]
    fn the_output_commands_are_the_pair_the_firmware_answers() {
        assert_eq!(Output::On.command(), "OUT1\n");
        assert_eq!(Output::Off.command(), "OUT0\n");
        assert!(Output::On.is_on());
        assert!(!Output::Off.is_on());
        assert_eq!(Output::Off.described(), "off");
    }

    #[test]
    fn the_plausible_limits_bracket_what_the_supply_can_be_told() {
        // The highest setpoints this tool programs are 16.00 V and 3.00 A, and the
        // limits must sit above them with room for the firmware's rounding.
        const _: () = assert!(MAX_PLAUSIBLE_VOLTS > 16.0 && MAX_PLAUSIBLE_VOLTS < 100.0);
        const _: () = assert!(MAX_PLAUSIBLE_AMPS > 3.0 && MAX_PLAUSIBLE_AMPS < 10.0);
        assert!(amps("3.000").is_some());
        assert!(amps("30.00").is_none());
    }

    #[test]
    fn describe_reply_keeps_printable_text_and_escapes_the_rest() {
        // STATUS? answers a raw control byte, which must still read clearly in a log.
        assert_eq!(describe_reply(&[0x12]), "[18] (?)");
        assert_eq!(describe_reply(b"14.94"), "[49, 52, 46, 57, 52] (14.94)");
        assert_eq!(describe_reply(&[]), "no reply");
    }
}
