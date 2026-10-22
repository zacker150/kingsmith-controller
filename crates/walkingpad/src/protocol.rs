//! The WalkingPad wire protocol: frames, CRC, commands, status and last-record
//! decoding, speed clamping and unit conversion.
//!
//! Pure functions and plain data only: no I/O, no clocks, no dependencies.
//! This is the only module that knows bytes (design §4).
//!
//! Frames are `f7 <type> <payload> <crc> fd` to the pad and
//! `f8 <type> <payload> <crc> fd` from it, where the CRC is the sum of every
//! byte between the header and the CRC, modulo 256. Multi-byte numbers are
//! 3-byte big-endian.

use std::fmt;

/// First byte of a frame sent to the pad.
pub const COMMAND_HEADER: u8 = 0xf7;
/// First byte of a frame sent by the pad.
pub const REPLY_HEADER: u8 = 0xf8;
/// Last byte of every frame.
pub const TRAILER: u8 = 0xfd;

/// Status request/reply and the speed, mode and start/pause commands.
pub const TYPE_STATUS: u8 = 0xa2;
/// Last-record request/reply.
pub const TYPE_RECORD: u8 = 0xa7;
/// Preference write.
pub const TYPE_PREFERENCE: u8 = 0xa6;

/// Length of `f8 a2` and `f8 a7` frames.
pub const REPLY_LEN: usize = 20;

/// Pad speed units (0.1 km/h) per mph: 1 mph = 1.609344 km/h.
pub const PAD_UNITS_PER_MPH: f64 = 16.093_44;

// ---------------------------------------------------------------------------
// CRC and framing
// ---------------------------------------------------------------------------

/// The CRC of a frame body (every byte between header and CRC).
#[must_use]
pub fn checksum(body: &[u8]) -> u8 {
    body.iter().fold(0u8, |sum, b| sum.wrapping_add(*b))
}

fn frame(header: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 3);
    out.push(header);
    out.extend_from_slice(body);
    out.push(checksum(body));
    out.push(TRAILER);
    out
}

/// Why a frame was rejected. Rejected frames are counted and logged, never
/// applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    Empty,
    /// Shorter than the smallest possible frame (header, type, CRC, trailer).
    TooShort(usize),
    BadHeader(u8),
    BadTrailer(u8),
    UnknownType(u8),
    WrongLength {
        frame_type: u8,
        expected: usize,
        found: usize,
    },
    BadCrc {
        expected: u8,
        found: u8,
    },
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "empty frame"),
            Self::TooShort(n) => write!(f, "frame too short ({n} bytes)"),
            Self::BadHeader(b) => write!(f, "bad header {b:02x}"),
            Self::BadTrailer(b) => write!(f, "bad trailer {b:02x}"),
            Self::UnknownType(t) => write!(f, "unknown frame type {t:02x}"),
            Self::WrongLength {
                frame_type,
                expected,
                found,
            } => write!(
                f,
                "frame type {frame_type:02x} must be {expected} bytes, got {found}"
            ),
            Self::BadCrc { expected, found } => {
                write!(f, "bad CRC {found:02x}, expected {expected:02x}")
            }
        }
    }
}

impl std::error::Error for FrameError {}

/// Checks header, trailer and CRC and returns the body (type and payload).
fn unframe(bytes: &[u8], header: u8) -> Result<&[u8], FrameError> {
    if bytes.is_empty() {
        return Err(FrameError::Empty);
    }
    if bytes.len() < 4 {
        return Err(FrameError::TooShort(bytes.len()));
    }
    if bytes[0] != header {
        return Err(FrameError::BadHeader(bytes[0]));
    }
    let last = bytes[bytes.len() - 1];
    if last != TRAILER {
        return Err(FrameError::BadTrailer(last));
    }
    let body = &bytes[1..bytes.len() - 2];
    let expected = checksum(body);
    let found = bytes[bytes.len() - 2];
    if expected != found {
        return Err(FrameError::BadCrc { expected, found });
    }
    Ok(body)
}

fn u24(b: &[u8]) -> u32 {
    (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2])
}

fn put_u24(out: &mut [u8], v: u32) {
    out[0] = (v >> 16) as u8;
    out[1] = (v >> 8) as u8;
    out[2] = v as u8;
}

// ---------------------------------------------------------------------------
// Units and speed
// ---------------------------------------------------------------------------

/// Lowest speed the app sends: 0.5 km/h.
pub const MIN_SPEED_UNITS: u8 = 5;
/// Highest speed the app sends: 6.0 km/h, the walking-mode maximum with the
/// handrail down (the R1 Pro manual confirms it).
pub const MAX_SPEED_UNITS: u8 = 60;

/// How the app shows speeds and distances. The wire is always 0.1 km/h and
/// 10 m units, whatever the pad's own display shows (KS Fit converts in the
/// app too).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub enum DisplayUnits {
    #[default]
    Miles,
    Km,
}

/// What clamping did to a requested speed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clamp {
    None,
    RaisedToMin,
    CappedAtMax,
}

/// A non-zero belt speed in pad units (0.1 km/h), inside 0.5–6.0 km/h. The
/// only way to build one is [`Speed::clamped`], so an out-of-range speed
/// frame can't be built. Zero is not a `Speed`: it is only reachable through
/// [`Command::SpeedZero`], which only Stop sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Speed(u8);

impl Speed {
    /// Clamps a requested speed (pad units) into the allowed range. Requests
    /// below the minimum, including zero and negative ones, become the minimum.
    #[must_use]
    pub fn clamped(units: i64) -> (Self, Clamp) {
        if units < i64::from(MIN_SPEED_UNITS) {
            (Self(MIN_SPEED_UNITS), Clamp::RaisedToMin)
        } else if units > i64::from(MAX_SPEED_UNITS) {
            (Self(MAX_SPEED_UNITS), Clamp::CappedAtMax)
        } else {
            (Self(units as u8), Clamp::None)
        }
    }

    /// The speed in pad units.
    #[must_use]
    pub const fn units(self) -> u8 {
        self.0
    }
}

fn div_round(n: u64, d: u64) -> u64 {
    (n + d / 2) / d
}

/// Converts a speed in tenths of the display unit (0.1 mph or 0.1 km/h) to
/// pad units (0.1 km/h), rounding to the nearest unit. Not clamped.
#[must_use]
pub fn display_tenths_to_pad(tenths: u32, display: DisplayUnits) -> u32 {
    let t = u64::from(tenths);
    let units = match display {
        DisplayUnits::Km => t,
        // 0.1 mph → 0.1 km/h: × 1.609344
        DisplayUnits::Miles => div_round(t * 1_609_344, 1_000_000),
    };
    units.min(u64::from(u32::MAX)) as u32
}

/// Converts pad units to tenths of the display unit, rounding to the nearest
/// tenth. For every 0.1 mph step this is the inverse of
/// [`display_tenths_to_pad`], so speed steps never drift.
#[must_use]
pub fn pad_to_display_tenths(units: u8, display: DisplayUnits) -> u32 {
    let u = u64::from(units);
    (match display {
        DisplayUnits::Km => u,
        // ÷ 1.609344 (× 0.6213712)
        DisplayUnits::Miles => div_round(u * 1_000_000, 1_609_344),
    }) as u32
}

/// A pad speed in the display unit per hour (mph or km/h), unrounded.
#[must_use]
pub fn pad_to_display_speed(units: u8, display: DisplayUnits) -> f64 {
    let kmh = f64::from(units) / 10.0;
    match display {
        DisplayUnits::Km => kmh,
        DisplayUnits::Miles => kmh * 10.0 / PAD_UNITS_PER_MPH,
    }
}

/// A distance from the status frame (units of 10 m) in miles or km.
#[must_use]
pub fn distance_to_display(tens_of_meters: u32, display: DisplayUnits) -> f64 {
    let km = f64::from(tens_of_meters) / 100.0;
    match display {
        DisplayUnits::Km => km,
        DisplayUnits::Miles => km / 1.609_344,
    }
}

// ---------------------------------------------------------------------------
// Commands (app → pad)
// ---------------------------------------------------------------------------

/// Pad mode, as set by [`Command::SetMode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub enum Mode {
    Auto,
    Manual,
    Standby,
}

impl Mode {
    #[must_use]
    pub const fn byte(self) -> u8 {
        match self {
            Self::Auto => 0,
            Self::Manual => 1,
            Self::Standby => 2,
        }
    }

    #[must_use]
    pub const fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Auto),
            1 => Some(Self::Manual),
            2 => Some(Self::Standby),
            _ => None,
        }
    }
}

/// Which last record to ask for (Phase 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordQuery {
    Latest,
    Next,
}

/// A preference write (Phase 2). The value is 24 bits; [`Preference::new`]
/// rejects anything larger so the frame can't overflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preference {
    key: u8,
    value: u32,
}

impl Preference {
    /// Max speed (value in pad units).
    pub const MAX_SPEED: u8 = 3;

    #[must_use]
    pub const fn new(key: u8, value: u32) -> Option<Self> {
        if value > 0x00ff_ffff {
            None
        } else {
            Some(Self { key, value })
        }
    }
}

/// Everything the app can send. Encoding can't fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// `f7 a2 00 00 a2 fd`
    AskStatus,
    /// `f7 a2 01 <speed> <crc> fd`
    SetSpeed(Speed),
    /// `f7 a2 01 00 a3 fd`: ph4's stop. One of the two Stop commands.
    SpeedZero,
    /// `f7 a2 02 <mode> <crc> fd`
    SetMode(Mode),
    /// `f7 a2 04 01 a7 fd`. **A toggle** on R1 Pro-family pads: starts a
    /// stopped belt and pauses a running one. Only ever sent by Start.
    StartPause,
    /// `f7 a7 aa <ff|00> <crc> fd`
    AskRecord(RecordQuery),
    /// `f7 a6 <key> 00 <value:3> <crc> fd`
    SetPreference(Preference),
}

impl Command {
    /// The frame for this command.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        match *self {
            Self::AskStatus => frame(COMMAND_HEADER, &[TYPE_STATUS, 0x00, 0x00]),
            Self::SetSpeed(s) => frame(COMMAND_HEADER, &[TYPE_STATUS, 0x01, s.units()]),
            Self::SpeedZero => frame(COMMAND_HEADER, &[TYPE_STATUS, 0x01, 0x00]),
            Self::SetMode(m) => frame(COMMAND_HEADER, &[TYPE_STATUS, 0x02, m.byte()]),
            Self::StartPause => frame(COMMAND_HEADER, &[TYPE_STATUS, 0x04, 0x01]),
            Self::AskRecord(q) => {
                let which = match q {
                    RecordQuery::Latest => 0xff,
                    RecordQuery::Next => 0x00,
                };
                frame(COMMAND_HEADER, &[TYPE_RECORD, 0xaa, which])
            }
            Self::SetPreference(p) => {
                let mut body = [TYPE_PREFERENCE, p.key, 0x00, 0, 0, 0];
                put_u24(&mut body[3..6], p.value);
                frame(COMMAND_HEADER, &body)
            }
        }
    }

    /// What the pad sees when it receives this command.
    #[must_use]
    pub fn wire(&self) -> WireCommand {
        match *self {
            Self::AskStatus => WireCommand::AskStatus,
            Self::SetSpeed(s) => WireCommand::SetSpeed(s.units()),
            Self::SpeedZero => WireCommand::SetSpeed(0),
            Self::SetMode(m) => WireCommand::SetMode(m.byte()),
            Self::StartPause => WireCommand::StartPause,
            Self::AskRecord(RecordQuery::Latest) => WireCommand::AskRecord(0xff),
            Self::AskRecord(RecordQuery::Next) => WireCommand::AskRecord(0x00),
            Self::SetPreference(p) => WireCommand::SetPreference {
                key: p.key,
                value: p.value,
            },
        }
    }

    /// True for commands that can start or speed up the belt. (The toggle can
    /// also stop it, which is why Stop only uses it under guard.)
    #[must_use]
    pub const fn can_move_belt(&self) -> bool {
        matches!(
            self,
            Self::SetSpeed(_) | Self::StartPause | Self::SetMode(Mode::Auto)
        )
    }
}

/// A command frame as decoded on the pad's side, with raw values. Used by the
/// fake pad and by tests; the app builds frames only through [`Command`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireCommand {
    AskStatus,
    SetSpeed(u8),
    SetMode(u8),
    StartPause,
    AskRecord(u8),
    SetPreference {
        key: u8,
        value: u32,
    },
    /// A well-formed frame this module doesn't model (e.g. qdomyos-zwift's
    /// init sequence). Holds the body: type and payload.
    Other(Vec<u8>),
}

/// Decodes a command frame (`f7 …`). Strict like [`decode_reply`].
pub fn decode_command(bytes: &[u8]) -> Result<WireCommand, FrameError> {
    let body = unframe(bytes, COMMAND_HEADER)?;
    Ok(match body {
        [TYPE_STATUS, 0x00, 0x00] => WireCommand::AskStatus,
        [TYPE_STATUS, 0x01, s] => WireCommand::SetSpeed(*s),
        [TYPE_STATUS, 0x02, m] => WireCommand::SetMode(*m),
        [TYPE_STATUS, 0x04, 0x01] => WireCommand::StartPause,
        [TYPE_RECORD, 0xaa, which] => WireCommand::AskRecord(*which),
        [TYPE_PREFERENCE, key, 0x00, v @ ..] if v.len() == 3 => WireCommand::SetPreference {
            key: *key,
            value: u24(v),
        },
        other => WireCommand::Other(other.to_vec()),
    })
}

// ---------------------------------------------------------------------------
// Replies (pad → app)
// ---------------------------------------------------------------------------

/// Status byte 2, as KS Fit 6.6.0 reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeltState {
    /// 0, "ready".
    Idle,
    /// 1.
    Running,
    /// 2.
    Paused,
    /// 3.
    Stopped,
    /// 4. The fault code is in byte 15.
    Fault,
    /// 5, "sleep": the manual's Standby, where "the motor and sensor stop
    /// responding". The pad drops into it after ~15 min idle; switching to
    /// manual wakes it. Shown as "Standby".
    Sleep,
    /// 6–9: the start countdown (this pad sends 9 right after a start).
    Countdown(u8),
    /// 11 or 0xff, "lock_off".
    LockOff(u8),
    /// 10 ("other") and anything not listed.
    Other(u8),
}

impl BeltState {
    #[must_use]
    pub const fn from_byte(b: u8) -> Self {
        match b {
            0 => Self::Idle,
            1 => Self::Running,
            2 => Self::Paused,
            3 => Self::Stopped,
            4 => Self::Fault,
            5 => Self::Sleep,
            6..=9 => Self::Countdown(b),
            11 | 0xff => Self::LockOff(b),
            other => Self::Other(other),
        }
    }

    #[must_use]
    pub const fn byte(self) -> u8 {
        match self {
            Self::Idle => 0,
            Self::Running => 1,
            Self::Paused => 2,
            Self::Stopped => 3,
            Self::Fault => 4,
            Self::Sleep => 5,
            Self::Countdown(b) | Self::LockOff(b) | Self::Other(b) => b,
        }
    }

    #[must_use]
    pub const fn is_countdown(self) -> bool {
        matches!(self, Self::Countdown(_))
    }
}

/// Byte 16: the last key pressed on the radio remote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteKey {
    None,
    Mode,
    Up,
    Stop,
    Down,
    ModeLongPress,
    Other(u8),
}

impl RemoteKey {
    #[must_use]
    pub const fn from_byte(b: u8) -> Self {
        match b {
            0 => Self::None,
            1 => Self::Mode,
            2 => Self::Up,
            3 => Self::Stop,
            4 => Self::Down,
            0x81 => Self::ModeLongPress,
            other => Self::Other(other),
        }
    }
}

/// Status byte 4. Unknown values decode instead of failing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeState {
    Known(Mode),
    /// 3: a newer mode KS Fit knows about; Start switches to manual first.
    Newer,
    Unknown(u8),
}

impl ModeState {
    #[must_use]
    pub const fn from_byte(b: u8) -> Self {
        match Mode::from_byte(b) {
            Some(m) => Self::Known(m),
            None if b == 3 => Self::Newer,
            None => Self::Unknown(b),
        }
    }

    #[must_use]
    pub const fn byte(self) -> u8 {
        match self {
            Self::Known(m) => m.byte(),
            Self::Newer => 3,
            Self::Unknown(b) => b,
        }
    }
}

/// A status frame (`f8 a2`, 20 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    /// Byte 2.
    pub belt: BeltState,
    /// Byte 3: current speed in pad units.
    pub speed: u8,
    /// Byte 4.
    pub mode: ModeState,
    /// Bytes 5–7, seconds.
    pub time_s: u32,
    /// Bytes 8–10, units of 10 m.
    pub distance_10m: u32,
    /// Bytes 11–13.
    pub steps: u32,
    /// Byte 14: unused (KS Fit ignores it). Kept so frames re-encode.
    pub byte14: u8,
    /// Byte 15: fault code, shown as E01–E15.
    pub fault_code: u8,
    /// Byte 16: last remote key, see [`RemoteKey`].
    pub remote_key: u8,
    /// Byte 17: unused (KS Fit ignores it). Kept so frames re-encode.
    pub byte17: u8,
}

impl Status {
    /// KS Fit's running test: the state byte can lag, so a non-zero speed
    /// also counts as moving.
    #[must_use]
    pub fn is_moving(&self) -> bool {
        self.belt == BeltState::Running || self.speed > 0
    }

    /// Moving, or about to (counting down).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.is_moving() || self.belt.is_countdown()
    }

    /// The fault to show, if any: when byte 15 is non-zero or the belt state
    /// is 4. Formatted like the pad's display (`E03`, `E12`).
    #[must_use]
    pub fn fault(&self) -> Option<String> {
        (self.fault_code != 0 || self.belt == BeltState::Fault)
            .then(|| format!("E{:02}", self.fault_code))
    }

    #[must_use]
    pub fn remote_key(&self) -> RemoteKey {
        RemoteKey::from_byte(self.remote_key)
    }

    /// Encodes the frame the pad would send. For the fake pad and tests.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut body = [0u8; REPLY_LEN - 3];
        body[0] = TYPE_STATUS;
        body[1] = self.belt.byte();
        body[2] = self.speed;
        body[3] = self.mode.byte();
        put_u24(&mut body[4..7], self.time_s.min(0x00ff_ffff));
        put_u24(&mut body[7..10], self.distance_10m.min(0x00ff_ffff));
        put_u24(&mut body[10..13], self.steps.min(0x00ff_ffff));
        body[13] = self.byte14;
        body[14] = self.fault_code;
        body[15] = self.remote_key;
        body[16] = self.byte17;
        frame(REPLY_HEADER, &body)
    }
}

/// A last-record frame (`f8 a7`, 20 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    /// Bytes 8–10, seconds.
    pub time_s: u32,
    /// Bytes 11–13, units of 10 m.
    pub distance_10m: u32,
    /// Bytes 14–16.
    pub steps: u32,
    /// Bytes 2–7, meaning unknown.
    pub head: [u8; 6],
    /// Byte 17, meaning unknown.
    pub unknown17: u8,
}

/// A decoded frame from the pad.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    Status(Status),
    Record(Record),
}

/// Decodes one notification. One notification is one frame (MTU 23).
///
/// Strict: checks the `f8` header, the `fd` trailer, the frame type, the
/// length (20 bytes for `a2` and `a7`) and the CRC.
pub fn decode_reply(bytes: &[u8]) -> Result<Reply, FrameError> {
    if bytes.is_empty() {
        return Err(FrameError::Empty);
    }
    if bytes.len() < 4 {
        return Err(FrameError::TooShort(bytes.len()));
    }
    if bytes[0] != REPLY_HEADER {
        return Err(FrameError::BadHeader(bytes[0]));
    }
    let frame_type = bytes[1];
    if frame_type != TYPE_STATUS && frame_type != TYPE_RECORD {
        return Err(FrameError::UnknownType(frame_type));
    }
    if bytes.len() != REPLY_LEN {
        return Err(FrameError::WrongLength {
            frame_type,
            expected: REPLY_LEN,
            found: bytes.len(),
        });
    }
    unframe(bytes, REPLY_HEADER)?;
    let b = bytes;
    Ok(if frame_type == TYPE_STATUS {
        Reply::Status(Status {
            belt: BeltState::from_byte(b[2]),
            speed: b[3],
            mode: ModeState::from_byte(b[4]),
            time_s: u24(&b[5..8]),
            distance_10m: u24(&b[8..11]),
            steps: u24(&b[11..14]),
            byte14: b[14],
            fault_code: b[15],
            remote_key: b[16],
            byte17: b[17],
        })
    } else {
        Reply::Record(Record {
            time_s: u24(&b[8..11]),
            distance_10m: u24(&b[11..14]),
            steps: u24(&b[14..17]),
            head: [b[2], b[3], b[4], b[5], b[6], b[7]],
            unknown17: b[17],
        })
    })
}

// ---------------------------------------------------------------------------
// Advertisement
// ---------------------------------------------------------------------------

/// The pad's manufacturer data looks like its MAC address (design §5.2): this
/// pad advertises `57 4c 4d 0f 62 75`, and pads in ph4's issues have MACs
/// starting `57:4C:4D` / `57:4C:4E`. Returns `AA:BB:CC:DD:EE:FF` for a 6-byte
/// value that isn't all zeros or all ones.
#[must_use]
pub fn mac_from_manufacturer_data(data: &[u8]) -> Option<String> {
    if data.len() != 6 || data.iter().all(|b| *b == 0) || data.iter().all(|b| *b == 0xff) {
        return None;
    }
    Some(
        data.iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(":"),
    )
}

/// Normalizes a MAC address string (`aa:bb:…` or `AA-BB-…`) to `AA:BB:…`.
/// Returns `None` for anything that isn't six hex bytes.
#[must_use]
pub fn normalize_mac(s: &str) -> Option<String> {
    let parts: Vec<&str> = s.split([':', '-']).collect();
    if parts.len() != 6 || parts.iter().any(|p| p.len() != 2) {
        return None;
    }
    let bytes: Option<Vec<u8>> = parts
        .iter()
        .map(|p| u8::from_str_radix(p, 16).ok())
        .collect();
    mac_from_manufacturer_data(&bytes?)
}

#[cfg(test)]
mod tests;
