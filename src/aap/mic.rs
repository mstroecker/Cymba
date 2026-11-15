//! The microphone over AAP, opcode 0x58: the packets that start and stop it,
//! their answers, and the audio packets the pods send meanwhile, each split
//! into its access units of AAC-ELD.
//!
//! The packets are LibrePods'; four runs on a Pro 3 confirmed them (#211).
//! The layouts, and what is known and not of each byte, are in
//! `docs/protocol/aap/microphone.md`; what the app does with them in
//! `docs/app/mic.md`.

use crate::aap::message;
#[cfg(any(test, feature = "harness"))]
use crate::aap::message::HEADER;

/// The microphone's opcode, for control packets and audio alike.
pub const OPCODE: u8 = 0x58;

/// Where a 0x58 packet says which kind it is: `00 00` control, `01 00` audio.
const KIND: usize = 6;
const CONTROL: [u8; 2] = [0x00, 0x00];
const AUDIO: [u8; 2] = [0x01, 0x00];

/// Where a control packet's payload starts, after its length: the start's
/// `00`, the stop's `03`, and their answers' `01` and `04`.
const PAYLOAD: usize = 10;

/// The packet that starts the stream, LibrePods'. What its payload selects
/// is not known (`microphone.md`, Open questions).
pub const START: [u8; 19] = [
    0x04, 0x00, 0x04, 0x00, 0x58, 0x00, 0x00, 0x00, 0x09, 0x00, 0x00, 0x01, 0x82, 0x00, 0x00, 0x00,
    0x04, 0x96, 0x00,
];

/// The packet that stops it, LibrePods'.
pub const STOP: [u8; 12] = [
    0x04, 0x00, 0x04, 0x00, 0x58, 0x00, 0x00, 0x00, 0x02, 0x00, 0x03, 0x01,
];

/// The first payload byte of the answer to the start, and of the answer to
/// the stop: one more than the request's.
const STARTED: u8 = 0x01;
const STOPPED: u8 = 0x04;

/// The bytes of an audio packet before its first access unit.
pub const AUDIO_HEADER_LEN: usize = 22;

/// What comes before each access unit's frame: a 4-byte timestamp and a
/// length byte.
const UNIT_HEADER_LEN: usize = 5;

/// The AudioSpecificConfig the frames decode with, LibrePods': AAC-ELD, mono,
/// 480 samples a frame, and 48 kHz, though the frames come at 64 kHz's pace
/// (`microphone.md`, The audio).
pub const AUDIO_SPECIFIC_CONFIG: [u8; 4] = [0xF8, 0xE6, 0x30, 0x00];

/// The samples in a frame, as the AudioSpecificConfig says.
pub const FRAME_SAMPLES: usize = 480;

/// The rate the decoded samples are played at: the pace the frames came at
/// on a Pro 3, not the rate the AudioSpecificConfig gives. Whether they sound
/// right at it is open (`microphone.md`, Open questions).
pub const SAMPLE_RATE: u32 = 64_000;

/// What the pods answered a control packet with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// The answer to the start: first payload byte `01`.
    Started,
    /// The answer to the stop: first payload byte `04`.
    Stopped,
    /// A control packet with another first payload byte, or none.
    Other(Option<u8>),
}

/// True for any 0x58 packet: the AAP header and the opcode.
pub fn is_mic_packet(packet: &[u8]) -> bool {
    message::opcode(packet) == Some(OPCODE)
}

/// True for an audio packet: the AAP header, 0x58, and `01 00` at bytes 6-7.
/// Cheap, for the read loop to route audio by before anything else.
pub fn is_audio_packet(packet: &[u8]) -> bool {
    is_mic_packet(packet) && packet.get(KIND..KIND + 2) == Some(&AUDIO)
}

/// True for a control packet: the AAP header, 0x58, and `00 00` at bytes
/// 6-7.
pub fn is_control_packet(packet: &[u8]) -> bool {
    is_mic_packet(packet) && packet.get(KIND..KIND + 2) == Some(&CONTROL)
}

/// Reads what a control packet answers, by its first payload byte.
pub(super) fn read_answer(packet: &[u8]) -> Answer {
    match packet.get(PAYLOAD) {
        Some(&STARTED) => Answer::Started,
        Some(&STOPPED) => Answer::Stopped,
        other => Answer::Other(other.copied()),
    }
}

/// [`read_answer`] for any packet: `None` for one that is no control
/// packet.
pub fn parse_answer(packet: &[u8]) -> Option<Answer> {
    is_control_packet(packet).then(|| read_answer(packet))
}

/// One access unit of an audio packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccessUnit<'a> {
    /// Its timestamp, which went up by 180 a unit in every run.
    pub timestamp: u32,
    /// The AAC-ELD frame.
    pub frame: &'a [u8],
}

/// The access units of an audio packet, in order: see [`access_units`].
#[derive(Debug, Clone)]
pub struct AccessUnits<'a> {
    packet: &'a [u8],
    offset: usize,
}

impl<'a> Iterator for AccessUnits<'a> {
    type Item = AccessUnit<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let header = self
            .packet
            .get(self.offset..self.offset + UNIT_HEADER_LEN)?;
        let timestamp = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        let start = self.offset + UNIT_HEADER_LEN;
        let frame = self.packet.get(start..start + usize::from(header[4]))?;
        self.offset = start + frame.len();
        Some(AccessUnit { timestamp, frame })
    }
}

impl AccessUnits<'_> {
    /// The bytes after the last whole unit read so far: none in every
    /// packet of the runs, once all units are read.
    pub fn leftover(&self) -> usize {
        self.packet.len().saturating_sub(self.offset)
    }
}

/// The access units of an audio packet, from byte 22 on, each a timestamp,
/// a length byte and that many bytes of frame. A unit cut short ends them.
/// Nothing for a packet that is not audio.
pub fn access_units(packet: &[u8]) -> AccessUnits<'_> {
    let offset = if is_audio_packet(packet) {
        AUDIO_HEADER_LEN
    } else {
        packet.len()
    };
    AccessUnits { packet, offset }
}

/// The answer the pods send to the start, or with `stopped`, to the stop,
/// as the runs printed them.
#[cfg(any(test, feature = "harness"))]
pub fn answer_packet(stopped: bool) -> Vec<u8> {
    if stopped {
        vec![
            0x04, 0x00, 0x04, 0x00, 0x58, 0x00, 0x00, 0x00, 0x02, 0x00, STOPPED, 0x01,
        ]
    } else {
        vec![
            0x04, 0x00, 0x04, 0x00, 0x58, 0x00, 0x00, 0x00, 0x03, 0x00, STARTED, 0x01, 0x01,
        ]
    }
}

/// An audio packet as the pods send it: the header, with `clock` in bytes
/// 18-21 and both lengths, and `units` after it.
#[cfg(any(test, feature = "harness"))]
pub fn audio_packet(clock: u32, units: &[AccessUnit<'_>]) -> Vec<u8> {
    let mut packet = HEADER.to_vec();
    packet.extend_from_slice(&[OPCODE, 0x00, AUDIO[0], AUDIO[1], 0, 0]);
    packet.extend_from_slice(&[OPCODE, 0x00, AUDIO[0], AUDIO[1], 0, 0]);
    // Bytes 16-17, the same in every packet of the runs; their meaning is
    // open.
    packet.extend_from_slice(&[0x18, 0x4B]);
    packet.extend_from_slice(&clock.to_le_bytes());
    for unit in units {
        packet.extend_from_slice(&unit.timestamp.to_le_bytes());
        packet.push(u8::try_from(unit.frame.len()).expect("a frame of 255 bytes at most"));
        packet.extend_from_slice(unit.frame);
    }
    let after = |at: usize| u16::try_from(packet.len() - at).expect("a short packet");
    let (outer, inner) = (after(10), after(16));
    packet[8..10].copy_from_slice(&outer.to_le_bytes());
    packet[14..16].copy_from_slice(&inner.to_le_bytes());
    packet
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The header of run 1's first audio packet, 343 bytes
    /// (`microphone.md`, Run 1).
    const RUN_1_HEADER: &str = "04 00 04 00 58 00 01 00 4d 01 58 00 01 00 47 01 18 4b 4f 9b ee 01";

    /// Run 1's first packet: its header, then units of the timestamps and
    /// lengths the run printed, their frames made up.
    fn run_1_packet() -> Vec<u8> {
        let mut packet: Vec<u8> = RUN_1_HEADER
            .split(' ')
            .map(|b| u8::from_str_radix(b, 16).unwrap())
            .collect();
        for (timestamp, len) in [(900u32, 74u8), (1080, 75), (1260, 74), (1440, 78)] {
            packet.extend_from_slice(&timestamp.to_le_bytes());
            packet.push(len);
            packet.extend(std::iter::repeat_n(len, usize::from(len)));
        }
        packet
    }

    /// The answers as the runs printed them; the start and the stop
    /// themselves, which the pods never send, read as other.
    #[test]
    fn reads_the_answers_of_the_runs() {
        let started = [
            0x04, 0x00, 0x04, 0x00, 0x58, 0x00, 0x00, 0x00, 0x03, 0x00, 0x01, 0x01, 0x01,
        ];
        let stopped = [
            0x04, 0x00, 0x04, 0x00, 0x58, 0x00, 0x00, 0x00, 0x02, 0x00, 0x04, 0x01,
        ];
        assert_eq!(parse_answer(&started), Some(Answer::Started));
        assert_eq!(parse_answer(&stopped), Some(Answer::Stopped));
        assert_eq!(answer_packet(false), started);
        assert_eq!(answer_packet(true), stopped);
        assert_eq!(parse_answer(&START), Some(Answer::Other(Some(0x00))));
        assert_eq!(parse_answer(&STOP), Some(Answer::Other(Some(0x03))));
        assert_eq!(parse_answer(&started[..10]), Some(Answer::Other(None)));
    }

    /// Bytes 6-7 tell audio from control; another opcode or header is
    /// neither.
    #[test]
    fn tells_audio_from_control() {
        let audio = run_1_packet();
        assert!(is_audio_packet(&audio));
        assert!(!is_control_packet(&audio));
        assert_eq!(parse_answer(&audio), None);
        assert!(is_control_packet(&START));
        assert!(!is_audio_packet(&START));

        let mut other_kind = audio.clone();
        other_kind[6] = 0x02;
        assert!(!is_audio_packet(&other_kind) && !is_control_packet(&other_kind));
        let mut other_opcode = audio.clone();
        other_opcode[4] = 0x59;
        assert!(!is_audio_packet(&other_opcode));
        let mut no_header = audio;
        no_header[0] = 0x01;
        assert!(!is_audio_packet(&no_header));
        // Cut before bytes 6-7 are whole.
        assert!(!is_audio_packet(&[
            0x04, 0x00, 0x04, 0x00, 0x58, 0x00, 0x01
        ]));
    }

    /// Run 1's first packet splits into the four units it printed, with
    /// nothing left over.
    #[test]
    fn splits_the_units_of_run_1() {
        let packet = run_1_packet();
        assert_eq!(packet.len(), 343);
        let mut units = access_units(&packet);
        let read: Vec<(u32, usize)> = units
            .by_ref()
            .map(|unit| (unit.timestamp, unit.frame.len()))
            .collect();
        assert_eq!(read, [(900, 74), (1080, 75), (1260, 74), (1440, 78)]);
        assert_eq!(units.leftover(), 0);
        // Each frame is its own bytes, made up above as its length.
        assert!(access_units(&packet).all(|unit| {
            unit.frame
                .iter()
                .all(|&b| usize::from(b) == unit.frame.len())
        }));
    }

    /// A unit cut short ends the units, and is left over.
    #[test]
    fn a_unit_cut_short_ends_them() {
        let packet = run_1_packet();
        let cut = &packet[..packet.len() - 10];
        let mut units = access_units(cut);
        assert_eq!(units.by_ref().count(), 3);
        assert_eq!(units.leftover(), 5 + 78 - 10);
        // A header with no unit, and a control packet, have none.
        assert_eq!(access_units(&packet[..AUDIO_HEADER_LEN]).count(), 0);
        assert_eq!(access_units(&START).count(), 0);
    }

    /// The builder gives run 1's header for run 1's units and clock.
    #[test]
    fn builds_the_header_of_run_1() {
        let packet = run_1_packet();
        let units: Vec<AccessUnit<'_>> = access_units(&packet).collect();
        let built = audio_packet(0x01EE_9B4F, &units);
        assert_eq!(crate::hex(&built[..AUDIO_HEADER_LEN]), RUN_1_HEADER);
        assert_eq!(built, packet);
    }
}
