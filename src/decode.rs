//! Reading the battery over Bluetooth.
//!
//! One status query gets one answer. The link chops that answer into notifications of
//! the negotiated size, and nothing in a notification says where the answer began or
//! that it is a first or a last piece. So the bytes are buffered, and frames are taken
//! by the length each one declares.
//!
//! Captured on the bench, one answer arriving as 20 + 20 + 1 bytes:
//!
//! ```text
//! dd 03 00 22 05 2d 00 d0 0a ab 29 04 00 9f 2e aa 00 00 00 00
//! 00 00 25 1b 03 04 01 0a dd 00 00 00 28 32 0a ab 00 00 fa 45
//! 77
//! ```
//!
//! Read that as `dd`, command `0x03`, a 16-bit data length of `0x22` = 34, 34 data
//! bytes, one byte that is `fa` in every frame captured so far, one sum check, then
//! `77`. The check is the two's complement of the sum of the length bytes and the data
//! bytes. `1b` at data offset 19 is 27%, and `00 d0` at data offset 2 is 2.08 A flowing
//! into the battery. The vendor app reported the same state of charge and the same
//! current, which is what makes these offsets evidence rather than a guess.
//!
//! A previous version of this module matched fragments against a fixed order instead:
//! first frame, then a piece with no header, then a frame headed `dd 04`. That third
//! one is the answer to a query we never send, so the module decoded one report and
//! then refused every report after it, and the charger ran on a frozen reading.

use crate::BatteryData;
use binary_layout::binary_layout;

// A status answer, from its first byte to the temperature, which is the last field read
// here. What follows is counted from the end of the frame instead, because its length
// follows the declared data length. The macro takes no doc comments.
binary_layout!(status_frame, BigEndian, {
    marker: u8,
    command: u8,
    data_length: u16,
    // Centivolts across the whole pack.
    total_voltage: u16,
    // Centiamps. Positive is current into the battery.
    current: i16,
    // Centiamp-hours left in the pack.
    remaining_capacity: u16,
    // Centiamp-hours the pack is rated at.
    rated_capacity: u16,
    cycles: u16,
    // 11946 in the captured frame. Not named yet.
    spare_14: u16,
    // Six zero bytes in the captured frame.
    spare_16: [u8; 6],
    // 37 in the captured frame. Not named yet.
    spare_22: u8,
    // Percent.
    soc: u8,
    mosfet_state: u8,
    number_of_cells: u8,
    number_of_temp_sensors: u8,
    // Tenths of kelvin.
    temp1: i16,
});

/// Length in bytes of a layout that the macro says is fixed size.
const fn layout_size(size: Option<usize>) -> usize {
    match size {
        Some(size) => size,
        None => panic!("BMS frame layouts must be fixed size"),
    }
}

/// How far into a frame the fields above reach. A frame shorter than this cannot
/// answer a status query, and the layout views do not bounds-check.
const STATUS_FRAME_BYTES: usize = layout_size(status_frame::SIZE);

/// A frame starts with this.
const FRAME_START: u8 = 0xdd;

/// A frame ends with this.
const FRAME_END: u8 = 0x77;

/// The command whose answer this module reads.
pub const STATUS_COMMAND: u8 = 0x03;

/// `dd`, command, 16-bit length.
const HEADER_BYTES: usize = 4;

/// Unexplained byte, sum check, end marker.
const TRAILER_BYTES: usize = 3;

/// Longest data a frame may declare and still be believed. The status answer carries
/// 34, so this leaves room for a longer model while rejecting a corrupt length, which
/// would otherwise make the buffer wait for bytes that never come.
const MAX_DATA_BYTES: usize = 64;

/// Most a partly arrived frame may hold. A link that loses fragments must not be able
/// to grow this without limit.
const MAX_BUFFER_BYTES: usize = 256;

/// What the battery said in one status frame.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct Status {
    /// Volts across the pack.
    #[serde(rename = "voltage")]
    pub volts: f32,
    /// Amperes. Positive is current into the battery.
    #[serde(rename = "current")]
    pub amps: f32,
    /// Amp-hours left.
    #[serde(rename = "charge")]
    pub remaining_amp_hours: f32,
    /// Amp-hours the pack is rated at.
    pub rated_amp_hours: f32,
    pub cycles: u16,
    /// Percent.
    pub soc: u8,
    pub mosfet_state: u8,
    pub cells: u8,
    /// Degrees C at the probe the BMS reports first.
    #[serde(rename = "temperature")]
    pub degrees_c: f32,
}

impl Status {
    /// The three numbers the charger acts on.
    pub fn reading(&self) -> BatteryData {
        BatteryData {
            voltage: self.volts,
            current: self.amps,
            soc: self.soc,
        }
    }

    /// Watts flowing into the battery, or out of it when negative.
    pub fn watts(&self) -> f32 {
        self.volts * self.amps
    }
}

/// How a notification starts, judged on its own bytes. Useful in a capture, where it
/// shows which piece of an answer arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// `dd a5 <command> <length>`, the header the outgoing query uses.
    QueryHeader { command: u8 },
    /// `dd <command> <length>`, the header an answer uses.
    AnswerHeader { command: u8 },
    /// Does not begin with `dd`, so it continues the answer before it.
    Continuation,
    /// Too short to say.
    Indeterminate,
}

impl FrameKind {
    pub fn describe(&self) -> String {
        match self {
            FrameKind::QueryHeader { command } => format!("query header dd a5 {command:02x}"),
            FrameKind::AnswerHeader { command } => format!("answer header dd {command:02x}"),
            FrameKind::Continuation => "continuation".to_string(),
            FrameKind::Indeterminate => "too short to name".to_string(),
        }
    }
}

pub fn frame_kind(fragment: &[u8]) -> FrameKind {
    match fragment {
        [0xdd, 0xa5, command, ..] => FrameKind::QueryHeader { command: *command },
        [0xdd, command, ..] => FrameKind::AnswerHeader { command: *command },
        [first, ..] if *first != 0xdd => FrameKind::Continuation,
        _ => FrameKind::Indeterminate,
    }
}

/// What the bytes fed to [`BmsStream`] came to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Outcome {
    /// A whole status frame that added up.
    Status(Status),
    /// A whole frame whose answer this module does not read, the cell voltages for
    /// example. Real bytes, correctly framed, nothing to do.
    NotRead { command: u8 },
    /// A whole frame that failed its end marker or its sum check.
    Corrupt { command: u8 },
    /// Bytes thrown away in front of a frame, which is what a lost fragment leaves.
    Resynced { dropped: usize },
}

impl Outcome {
    /// Words for a log.
    pub fn label(&self) -> String {
        match self {
            Outcome::Status(status) => format!(
                "report {:.2}V {:+.2}A {}%",
                status.volts, status.amps, status.soc
            ),
            Outcome::NotRead { command } => format!("frame {command:02x} not read"),
            Outcome::Corrupt { command } => {
                format!("THREW AWAY frame {command:02x}, does not add up")
            }
            Outcome::Resynced { dropped } => format!("dropped {dropped} bytes of lost frame"),
        }
    }
}

/// What is at the head of the buffer.
enum Head {
    /// Too few bytes to know yet.
    Waiting,
    /// This many bytes precede the next possible frame start.
    Skip(usize),
    /// A start marker whose declared length cannot be true.
    FalseStart,
    /// A whole frame of this many bytes.
    Ready(usize),
}

/// Turns notifications into frames.
///
/// Feed every notification, in order, and read what comes back. Nothing is assumed
/// about how many notifications an answer needs, so a link that changes its negotiated
/// size, or a battery that answers with a different length, needs no change here.
#[derive(Debug, Default)]
pub struct BmsStream {
    buffer: Vec<u8>,
}

impl BmsStream {
    /// Take the next answer apart. Returns nothing until the notification that
    /// completes a frame arrives, which is normal.
    pub fn observe(&mut self, fragment: &[u8]) -> Vec<Outcome> {
        let mut outcomes = Vec::new();
        self.buffer.extend_from_slice(fragment);

        loop {
            match self.head() {
                Head::Waiting => break,
                Head::Skip(skipped) => {
                    self.buffer.drain(..skipped);
                    outcomes.push(Outcome::Resynced { dropped: skipped });
                }
                Head::FalseStart => {
                    self.buffer.remove(0);
                    outcomes.push(Outcome::Resynced { dropped: 1 });
                }
                Head::Ready(bytes) => {
                    let frame = self.buffer[..bytes].to_vec();
                    self.buffer.drain(..bytes);
                    outcomes.push(read_frame(&frame));
                }
            }
        }

        self.forget_overflow();
        outcomes
    }

    /// Bytes held while an answer is incomplete.
    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }

    fn head(&self) -> Head {
        let Some(start) = self.buffer.iter().position(|byte| *byte == FRAME_START) else {
            // Nothing here can begin a frame. Empty means wait for bytes; bytes that
            // cannot start anything are dropped, and never zero of them, which would
            // turn this loop into one that never ends.
            return if self.buffer.is_empty() {
                Head::Waiting
            } else {
                Head::Skip(self.buffer.len())
            };
        };

        if start > 0 {
            return Head::Skip(start);
        }

        if self.buffer.len() < HEADER_BYTES {
            return Head::Waiting;
        }

        let declared = u16::from_be_bytes([self.buffer[2], self.buffer[3]]) as usize;
        if declared > MAX_DATA_BYTES {
            return Head::FalseStart;
        }

        let whole = HEADER_BYTES + declared + TRAILER_BYTES;
        if self.buffer.len() < whole {
            return Head::Waiting;
        }

        Head::Ready(whole)
    }

    /// Drop a stalled partial frame so a link that keeps losing fragments cannot fill
    /// the buffer. The next frame start is found on the next notification.
    fn forget_overflow(&mut self) {
        if self.buffer.len() > MAX_BUFFER_BYTES {
            self.buffer.clear();
        }
    }
}

/// Read one whole frame.
fn read_frame(frame: &[u8]) -> Outcome {
    let command = frame.get(1).copied().unwrap_or_default();

    if frame.last() != Some(&FRAME_END) {
        return Outcome::Corrupt { command };
    }

    let declared = u16::from_be_bytes([frame[2], frame[3]]) as usize;
    let data_end = HEADER_BYTES + declared;
    if frame.len() < data_end + TRAILER_BYTES || !sum_check_adds_up(frame, data_end) {
        return Outcome::Corrupt { command };
    }

    if command != STATUS_COMMAND {
        return Outcome::NotRead { command };
    }

    if frame.len() < STATUS_FRAME_BYTES {
        // A status answer too short to hold the fields. Better a counted no than a
        // panic: this runs on the task that reads the battery.
        return Outcome::Corrupt { command };
    }

    let view = status_frame::View::new(frame);

    Outcome::Status(Status {
        volts: view.total_voltage().read() as f32 / 100.0,
        amps: view.current().read() as f32 / 100.0,
        remaining_amp_hours: view.remaining_capacity().read() as f32 / 100.0,
        rated_amp_hours: view.rated_capacity().read() as f32 / 100.0,
        cycles: view.cycles().read(),
        soc: view.soc().read(),
        mosfet_state: view.mosfet_state().read(),
        cells: view.number_of_cells().read(),
        degrees_c: kelvin_tenths_to_celsius(view.temp1().read()),
    })
}

/// The check byte is the two's complement of the sum of the length bytes and the data
/// bytes, so a frame is whole when the total comes to zero.
fn sum_check_adds_up(frame: &[u8], data_end: usize) -> bool {
    let summed = frame
        .get(2..data_end)
        .map(|bytes| bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)));

    match (summed, frame.get(frame.len().saturating_sub(2))) {
        (Some(sum), Some(check)) => sum.wrapping_add(*check) == 0,
        _ => false,
    }
}

const fn kelvin_tenths_to_celsius(kelvin_tenths: i16) -> f32 {
    kelvin_tenths as f32 / 10.0 - 273.15
}

/// A notification and what it came to.
#[derive(Debug)]
pub struct FrameRecord {
    pub index: usize,
    pub len: usize,
    pub kind: FrameKind,
    pub outcomes: Vec<Outcome>,
}

impl FrameRecord {
    /// The report this notification completed, if it completed one.
    pub fn report(&self) -> Option<Status> {
        self.outcomes.iter().find_map(|outcome| match outcome {
            Outcome::Status(status) => Some(*status),
            _ => None,
        })
    }
}

/// Count of what arrived, for `--watch-battery`.
///
/// This exists because a decoder that reads nothing looks exactly like a battery that
/// sends nothing, and telling them apart is the whole job of a capture.
#[derive(Debug, Default)]
pub struct Frames {
    pub notifications: usize,
    pub statuses: usize,
    pub corrupt: usize,
    pub not_read: usize,
    pub resynced_bytes: usize,
    /// Notifications that completed nothing, which is normal for the pieces before the
    /// last one, and is the only number that grows when the battery has stopped
    /// answering.
    pub unfinished: usize,
    pub smallest: Option<usize>,
    pub largest: Option<usize>,
    /// Command byte to frames carrying it.
    pub commands: std::collections::BTreeMap<u8, usize>,
}

impl Frames {
    /// Feed one notification to the stream and keep score.
    pub fn observe(&mut self, stream: &mut BmsStream, fragment: &[u8]) -> FrameRecord {
        self.notifications += 1;
        let kind = frame_kind(fragment);

        if let FrameKind::QueryHeader { command } | FrameKind::AnswerHeader { command } = kind {
            *self.commands.entry(command).or_default() += 1;
        }

        self.smallest = Some(
            self.smallest
                .map_or(fragment.len(), |smallest| smallest.min(fragment.len())),
        );
        self.largest = Some(
            self.largest
                .map_or(fragment.len(), |largest| largest.max(fragment.len())),
        );

        let outcomes = stream.observe(fragment);

        if outcomes.is_empty() {
            self.unfinished += 1;
        }

        for outcome in &outcomes {
            match outcome {
                Outcome::Status(_) => self.statuses += 1,
                Outcome::Corrupt { .. } => self.corrupt += 1,
                Outcome::NotRead { .. } => self.not_read += 1,
                Outcome::Resynced { dropped } => self.resynced_bytes += dropped,
            }
        }

        FrameRecord {
            index: self.notifications,
            len: fragment.len(),
            kind,
            outcomes,
        }
    }

    /// What a capture amounted to.
    pub fn summary(&self) -> String {
        use std::fmt::Write as _;

        let mut text = format!(
            "{} notifications: {} reports, {} corrupt, {} not read, {} bytes dropped resyncing\n",
            self.notifications, self.statuses, self.corrupt, self.not_read, self.resynced_bytes
        );

        if self.notifications == 0 {
            text.push_str("nothing arrived over the link\n");
            return text;
        }

        let _ = writeln!(
            text,
            "sizes: {} to {} bytes, {} completed nothing",
            self.smallest.unwrap_or(0),
            self.largest.unwrap_or(0),
            self.unfinished
        );

        let seen: Vec<String> = self
            .commands
            .iter()
            .map(|(command, count)| format!("{command:02x}:{count}"))
            .collect();
        let _ = writeln!(
            text,
            "frame starts: {}",
            if seen.is_empty() {
                "none".to_string()
            } else {
                seen.join(" ")
            }
        );

        if self.statuses == 0 && self.notifications > 0 {
            text.push_str("NOT ONE COMPLETE REPORT: the battery answered, nothing was read\n");
        }

        text
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BmsStream, FrameKind, Frames, Outcome, STATUS_COMMAND, frame_kind, kelvin_tenths_to_celsius,
    };
    use crate::BatteryData;

    /// The answer to one query, exactly as the link delivered it on 2026-11-21:
    /// twenty bytes, twenty more, then the last byte on its own.
    fn captured() -> Vec<Vec<u8>> {
        let pieces = [
            "dd 03 00 22 05 2d 00 d0 0a ab 29 04 00 9f 2e aa 00 00 00 00",
            "00 00 25 1b 03 04 01 0a dd 00 00 00 28 32 0a ab 00 00 fa 45",
            "77",
        ];

        pieces
            .iter()
            .map(|piece| {
                piece
                    .split_whitespace()
                    .map(|byte| u8::from_str_radix(byte, 16).unwrap())
                    .collect()
            })
            .collect()
    }

    /// The same answer as one block of bytes, for tests that do not care about pieces.
    fn captured_frame() -> Vec<u8> {
        captured().concat()
    }

    /// Change the current in a captured frame and make the sum check fit again.
    fn with_current(mut frame: Vec<u8>, amps: f32) -> Vec<u8> {
        let centiamps = (amps * 100.0).round() as i16;
        let bytes = centiamps.to_be_bytes();
        frame[6] = bytes[0];
        frame[7] = bytes[1];

        let declared = u16::from_be_bytes([frame[2], frame[3]]) as usize;
        let data_end = 4 + declared;
        let sum = frame[2..data_end]
            .iter()
            .fold(0u8, |sum, byte| sum.wrapping_add(*byte));
        let last = frame.len() - 2;
        frame[last] = sum.wrapping_neg();
        frame
    }

    fn feed_all(stream: &mut BmsStream) -> Vec<Vec<Outcome>> {
        captured()
            .iter()
            .map(|piece| stream.observe(piece))
            .collect()
    }

    #[test]
    fn the_captured_answer_says_what_the_vendor_app_said() {
        let mut stream = BmsStream::default();
        let outcomes = feed_all(&mut stream);
        let last = outcomes.last().expect("three pieces");

        assert!(
            outcomes[0].is_empty() && outcomes[1].is_empty(),
            "the first two pieces hold no whole frame: {outcomes:?}"
        );
        assert_eq!(
            last.len(),
            1,
            "one answer, so one outcome, not one per piece"
        );

        let Outcome::Status(status) = last[0] else {
            panic!("expected a report, got {:?}", last[0]);
        };

        assert_eq!(status.volts, 13.25);
        assert_eq!(status.amps, 2.08, "current into the battery is positive");
        assert_eq!(status.soc, 27, "percent, as the app showed");
        assert_eq!(status.cells, 4);
        assert_eq!(status.mosfet_state, 3);
        assert_eq!(status.cycles, 159);
        assert_eq!(status.remaining_amp_hours, 27.31);
        assert_eq!(status.rated_amp_hours, 105.0);
        assert!(
            (status.degrees_c - 5.0).abs() < 0.1,
            "got {} C",
            status.degrees_c
        );
        assert_eq!(status.watts(), 13.25 * 2.08);
        assert_eq!(stream.buffered(), 0, "the whole answer was used up");
    }

    #[test]
    fn a_report_carries_the_three_numbers_the_charger_needs() {
        let mut stream = BmsStream::default();
        let outcomes = feed_all(&mut stream);
        let Outcome::Status(status) = outcomes[2][0] else {
            panic!("expected a report");
        };

        assert_eq!(
            status.reading(),
            BatteryData {
                voltage: 13.25,
                current: 2.08,
                soc: 27,
            }
        );
    }

    #[test]
    fn a_discharge_current_keeps_its_sign() {
        let mut stream = BmsStream::default();
        let frame = with_current(captured_frame(), -2.50);

        let outcomes = stream.observe(&frame);
        let Outcome::Status(status) = outcomes[0] else {
            panic!("expected a report, got {outcomes:?}");
        };

        assert_eq!(status.amps, -2.50);
        assert!(status.watts() < 0.0, "power out must read negative");
    }

    #[test]
    fn the_second_answer_arrives_whole_after_the_first() {
        // The fault this replaces decoded one report and then refused every report
        // after it. Asking twice is the shortest test of that.
        let mut stream = BmsStream::default();
        let mut reports = 0;

        for round in 0..3 {
            for piece in captured() {
                if stream
                    .observe(&piece)
                    .iter()
                    .any(|outcome| matches!(outcome, Outcome::Status(_)))
                {
                    reports += 1;
                }
            }

            assert_eq!(reports, round + 1, "every answer must be read");
        }
    }

    #[test]
    fn a_frame_that_does_not_add_up_is_counted_and_forgotten() {
        let mut stream = BmsStream::default();
        let mut broken = captured_frame();
        let spot = broken.len() - 8;
        broken[spot] = broken[spot].wrapping_add(1);

        let outcomes = stream.observe(&broken);

        assert_eq!(outcomes, vec![super::Outcome::Corrupt { command: 0x03 }]);
        assert_eq!(stream.buffered(), 0, "it must not be held");

        // The next answer still reads, so one bad frame costs one report.
        let mut next = 0;
        for piece in captured() {
            next += stream
                .observe(&piece)
                .iter()
                .filter(|outcome| matches!(outcome, Outcome::Status(_)))
                .count();
        }
        assert_eq!(next, 1, "the stream recovers on its own");
    }

    #[test]
    fn a_lost_fragment_costs_one_answer_and_no_more() {
        let mut stream = BmsStream::default();

        // The middle piece of one answer never arrives, so its end lands in the middle
        // of nowhere and the frame that comes out does not add up.
        let pieces = captured();
        stream.observe(&pieces[0]);
        stream.observe(&pieces[2]);

        let mut reports = 0;
        for _ in 0..2 {
            for piece in captured() {
                reports += stream
                    .observe(&piece)
                    .iter()
                    .filter(|outcome| matches!(outcome, Outcome::Status(_)))
                    .count();
            }
        }

        assert_eq!(
            reports, 1,
            "the answer in progress is lost, the one after it is read"
        );
    }

    #[test]
    fn bytes_that_cannot_begin_a_frame_are_dropped_once() {
        let mut stream = BmsStream::default();

        assert_eq!(
            stream.observe(&[0x00, 0x11, 0x22]),
            vec![super::Outcome::Resynced { dropped: 3 }]
        );
        assert_eq!(stream.buffered(), 0);

        // With nothing held there is nothing to drop. This must return quietly rather
        // than report work to do, which is how a stream can spin without end.
        assert!(stream.observe(&[]).is_empty());
        assert_eq!(stream.buffered(), 0);
    }

    #[test]
    fn bytes_before_a_frame_are_dropped_and_said() {
        let mut stream = BmsStream::default();
        let frame = captured_frame();
        let (head, body) = frame.split_at(5);

        let mut outcomes = stream.observe(&[0x00, 0x11]);
        assert_eq!(outcomes, vec![super::Outcome::Resynced { dropped: 2 }]);

        outcomes = stream.observe(head);
        assert!(
            outcomes.iter().all(|outcome| matches!(
                outcome,
                super::Outcome::Resynced { .. } | super::Outcome::Corrupt { .. }
            )),
            "a frame start in the middle of a frame cannot be read: {outcomes:?}"
        );

        for piece in [body, &captured()[1], &captured()[2]] {
            let read = stream.observe(piece);
            let _ = read;
        }

        assert!(
            stream
                .observe(&captured_frame())
                .iter()
                .any(|outcome| matches!(outcome, Outcome::Status(_))),
            "a clean frame after the mess must read"
        );
    }

    #[test]
    fn an_answer_we_do_not_read_is_named_not_swallowed() {
        // The cell voltage answer, which this tool could ask for but does not.
        let mut cell_answer = vec![0xdd, 0x04, 0x00, 0x08];
        cell_answer.extend_from_slice(&[0x0c, 0xc4, 0x0c, 0xc4, 0x0c, 0xc4, 0x0c, 0xc4]);
        cell_answer.push(0xfa);
        let sum = cell_answer[2..12]
            .iter()
            .fold(0u8, |sum, b| sum.wrapping_add(*b));
        cell_answer.push(sum.wrapping_neg());
        cell_answer.push(0x77);

        let mut stream = BmsStream::default();

        assert_eq!(
            stream.observe(&cell_answer),
            vec![super::Outcome::NotRead { command: 0x04 }]
        );
    }

    #[test]
    fn an_impossible_length_cannot_grow_the_buffer() {
        let mut stream = BmsStream::default();

        // A corrupt length of 65535 would otherwise leave the stream waiting for bytes
        // that will never come, holding everything after it.
        let outcomes = stream.observe(&[0xdd, 0x03, 0xff, 0xff, 0x01, 0x02, 0x03]);

        assert!(
            outcomes.contains(&super::Outcome::Resynced { dropped: 1 }),
            "{outcomes:?}"
        );
        assert!(stream.buffered() < 8, "{}", stream.buffered());
    }

    #[test]
    fn a_notification_we_cannot_name_yet_is_just_a_wait() {
        let mut stream = BmsStream::default();

        assert!(stream.observe(&[0xdd]).is_empty());
        assert!(stream.observe(&[0x03]).is_empty());
        assert_eq!(stream.buffered(), 2);
    }

    #[test]
    fn a_fragment_is_named_from_its_own_bytes() {
        assert_eq!(
            frame_kind(&[0xdd, 0xa5, 0x03, 0x00, 0xff, 0xfd, 0x77]),
            FrameKind::QueryHeader { command: 0x03 },
            "the query we send"
        );
        assert_eq!(
            frame_kind(&captured()[0]),
            FrameKind::AnswerHeader { command: 0x03 }
        );
        assert_eq!(frame_kind(&captured()[1]), FrameKind::Continuation);
        assert_eq!(frame_kind(&[0x77]), FrameKind::Continuation);
        assert_eq!(frame_kind(&[]), FrameKind::Indeterminate);
    }

    #[test]
    fn the_tally_reports_reports() {
        let mut stream = BmsStream::default();
        let mut frames = Frames::default();

        let records: Vec<_> = captured()
            .iter()
            .map(|piece| frames.observe(&mut stream, piece))
            .collect();

        assert_eq!(records[2].len, 1);
        let report = records[2].report().expect("the last piece completes it");
        assert_eq!(report.soc, 27);

        assert_eq!(frames.statuses, 1);
        assert_eq!(frames.corrupt, 0);
        assert_eq!(
            frames.unfinished, 2,
            "the first two pieces complete nothing"
        );
        assert_eq!(frames.smallest, Some(1));
        assert_eq!(frames.largest, Some(20));
        assert_eq!(frames.commands.get(&STATUS_COMMAND), Some(&1));
        assert!(
            !frames.summary().contains("NOT ONE COMPLETE REPORT"),
            "{}",
            frames.summary()
        );
    }

    #[test]
    fn a_capture_with_no_reports_says_so_loudly() {
        let mut stream = BmsStream::default();
        let mut frames = Frames::default();

        frames.observe(&mut stream, &captured()[0]);

        assert!(
            frames.summary().contains("NOT ONE COMPLETE REPORT"),
            "{}",
            frames.summary()
        );
    }

    #[test]
    fn tenths_of_kelvin_become_degrees() {
        assert!(
            (kelvin_tenths_to_celsius(2981) - 24.95).abs() < 0.01,
            "the value in the captured frame"
        );
        assert!(
            (kelvin_tenths_to_celsius(2731) + 0.05).abs() < 0.01,
            "2731 tenths of kelvin is just under freezing"
        );
    }
}
