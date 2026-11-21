//! The role the charge controller needs from a bench power supply.
//!
//! Every setter here is *verified*: it reports `Ok` only after the supply has
//! answered with the value or state that was asked for. A write to a serial port
//! always "succeeds" as far as the driver is concerned, so an unverified write
//! proves nothing about the hardware. Without this the tool happily printed
//! healthy charge rows while the supply had its output switched off.
//!
//! The charge policy is written against this trait, so it can be tested with a
//! scripted supply instead of a bench.

use std::future::Future;

/// `STATUS?` bit set while the output transistor conducts.
const STATUS_OUTPUT_ON: u8 = 0x40;

/// What the supply reports about its own output, decoded from the `STATUS?` byte.
///
/// Only the `enabled` bit is used for control decisions. The regulation mode is
/// informational: sigrok reports that most bits of this reply vary between
/// firmware revisions of the Korad/RAVNI family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputStatus {
    /// True when the supply says its output is switched on.
    pub enabled: bool,
    /// The undecoded reply, kept for logging. Bit 0 of this firmware family is said
    /// to separate constant-voltage from constant-current, but nothing measured on
    /// this unit identifies which way round it works, so it is never interpreted.
    pub raw: u8,
}

impl OutputStatus {
    /// True when the supply disagrees with the state the controller asked for.
    pub fn contradicts(&self, wanted_on: bool) -> bool {
        self.enabled != wanted_on
    }
}

/// Decode the single byte a Korad/RAVNI-protocol supply answers to `STATUS?`.
pub fn decode_status(status: u8) -> OutputStatus {
    OutputStatus {
        enabled: status & STATUS_OUTPUT_ON != 0,
        raw: status,
    }
}

/// Why a command did not reach or move the supply.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PsuError {
    /// The port handle itself failed, so it has to be reopened.
    #[error("serial port failed: {0}")]
    Port(String),
    /// The tty stopped being the port we opened, or refused a line setting.
    #[error("line settings could not be applied: {0}")]
    Line(#[from] nix::Error),
    /// The supply answered nothing. It may be wedged, gone from the bus, or being
    /// talked to at the wrong baud rate by another process.
    #[error("the supply answered nothing to {command} ({attempts} attempts)")]
    Unanswered { command: &'static str, attempts: u8 },
    /// The supply answered with text that is not the number we asked for.
    #[error("the supply gave an unreadable answer to {command}: {reply}")]
    Unreadable {
        command: &'static str,
        reply: String,
    },
    /// The supply answered, but not with the value that was programmed.
    #[error("{command} did not take effect: asked for {wanted}, the supply reads {readback}")]
    NotApplied {
        command: &'static str,
        wanted: String,
        readback: String,
    },
    /// The supply would not reach the requested output state.
    #[error("the supply would not reach the state it was asked for: {0}")]
    State(String),
}

impl PsuError {
    /// Only a dead port handle is worth reopening the device for. Everything else is
    /// a supply that is answering, or refusing, and needs reporting.
    pub fn is_port_failure(&self) -> bool {
        matches!(self, Self::Port(_))
    }
}

pub type PsuResult<T> = Result<T, PsuError>;

/// A bench supply the charge controller can drive. Split from the charge policy
/// so the policy can be tested without hardware; a supply has one role here, so
/// configuration, measurement, and status stay together.
pub trait PowerSupply {
    /// Program the output voltage and confirm it by reading `VSET1?` back.
    fn set_voltage(&mut self, volts: f32) -> impl Future<Output = PsuResult<()>>;

    /// Program the current limit and confirm it by reading `ISET1?` back.
    fn set_current(&mut self, amps: f32) -> impl Future<Output = PsuResult<()>>;

    /// Switch the output on and confirm it with `STATUS?`.
    fn enable_output(&mut self) -> impl Future<Output = PsuResult<()>>;

    /// Switch the output off and confirm it with `STATUS?`.
    fn disable_output(&mut self) -> impl Future<Output = PsuResult<()>>;

    /// Voltage the supply measures at its own terminals.
    fn output_voltage(&mut self) -> impl Future<Output = PsuResult<f32>>;

    /// Current the supply is delivering.
    fn output_current(&mut self) -> impl Future<Output = PsuResult<f32>>;

    /// The supply's own account of its output state.
    fn output_status(&mut self) -> impl Future<Output = PsuResult<OutputStatus>>;

    /// Ask the supply who it is. Used as the open handshake: it is the one command
    /// that proves writes and reads both work.
    fn identify(&mut self) -> impl Future<Output = PsuResult<String>>;
}

#[cfg(test)]
mod tests {
    use super::{OutputStatus, PsuError, STATUS_OUTPUT_ON, decode_status};

    // Byte observed on a Tenma 72-2940 V6.0 while it delivered 2.999 A into the pack.
    const OBSERVED_CHARGING: u8 = 0x52;
    // The same supply right after `OUT0`, when VOUT1? read 00.00 and IOUT1? 0.000.
    const OBSERVED_IDLE: u8 = 0x12;

    #[test]
    fn the_status_seen_while_charging_reads_as_output_on() {
        let status = decode_status(OBSERVED_CHARGING);

        assert!(status.enabled, "{OBSERVED_CHARGING:#04x} must read as on");
        assert_eq!(status.raw, OBSERVED_CHARGING);
    }

    #[test]
    fn the_status_seen_with_the_output_off_reads_as_off() {
        let status = decode_status(OBSERVED_IDLE);

        assert!(!status.enabled, "{OBSERVED_IDLE:#04x} must read as off");
        assert_eq!(status.raw, OBSERVED_IDLE);
        assert_ne!(
            OBSERVED_CHARGING & STATUS_OUTPUT_ON,
            OBSERVED_IDLE & STATUS_OUTPUT_ON,
            "the two observed readings must disagree in the output bit"
        );
    }

    #[test]
    fn the_regulation_mode_is_only_passed_through() {
        // Bit 0 is not interpreted, so whatever it holds survives into the log.
        assert_eq!(decode_status(0x41).raw, 0x41);
        assert_eq!(decode_status(0x40).raw, 0x40);
    }

    #[test]
    fn contradiction_is_reported_in_both_directions() {
        let on = decode_status(OBSERVED_CHARGING);
        let off = decode_status(OBSERVED_IDLE);

        assert!(on.contradicts(false), "supply is on, we wanted off");
        assert!(off.contradicts(true), "supply is off, we wanted on");
        assert!(!on.contradicts(true));
        assert!(!off.contradicts(false));
    }

    #[test]
    fn errors_name_the_command_that_failed() {
        let error = PsuError::Unanswered {
            command: "VOUT1?",
            attempts: 2,
        };

        let text = error.to_string();
        assert!(text.contains("VOUT1?"), "got {text}");
        assert!(text.contains("answered nothing"), "got {text}");
        assert!(text.contains("2 attempts"), "got {text}");

        let mismatch = PsuError::NotApplied {
            command: "ISET1?",
            wanted: "3.000 A".to_string(),
            readback: "0.000 A".to_string(),
        };

        let text = mismatch.to_string();
        assert!(text.contains("did not take effect"), "got {text}");
        assert!(text.contains("3.000 A"), "got {text}");
        assert!(text.contains("0.000 A"), "got {text}");
    }

    #[test]
    fn only_a_dead_port_is_worth_reopening_the_device_for() {
        assert!(PsuError::Port("closed".to_string()).is_port_failure());

        // A supply that answers nothing, or answers wrongly, is not a dead handle.
        // Reopening the port would hide a fault the operator has to see.
        assert!(
            !PsuError::Unanswered {
                command: "VOUT1?",
                attempts: 2,
            }
            .is_port_failure()
        );
        assert!(!PsuError::State("stuck".to_string()).is_port_failure());
    }

    #[test]
    fn errors_can_be_used_as_errors() {
        // Any caller that collects Box<dyn Error> needs this, and it is what lets the
        // driver's failures be reported by `main` unchanged.
        let error: Box<dyn std::error::Error> = Box::new(PsuError::State("stuck".to_string()));

        assert!(error.to_string().contains("stuck"));
    }

    #[test]
    fn an_output_status_is_a_value() {
        let status = OutputStatus {
            enabled: true,
            raw: OBSERVED_CHARGING,
        };

        assert_eq!(status, status);
    }
}
