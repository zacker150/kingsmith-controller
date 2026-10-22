//! The session: one async task that owns the transport and decides what to
//! send and when (design §5.2–5.4, §7).
//!
//! - **Discovery and connection.** Scans with the `fe00` filter while the app
//!   is visible and connects to the remembered pad when it appears; the user
//!   picks a pad the first time. Reconnects with backoff after an unexpected
//!   disconnect.
//! - **Handshake.** Asks for status every 300 ms after subscribing until the
//!   first valid status (3 s timeout), then polls every 750 ms.
//! - **Single writer.** Writes are at least 700 ms apart. A queued command
//!   takes the next slot instead of a poll and is followed by a poll. Speed
//!   commands collapse to the latest one.
//! - **Safety (§7).**
//!   - Stop pre-empts and clears the queue and cancels a pending Start. With
//!     the idempotent strategies (standby, the default, or speed 0; see
//!     [`StopMethod`]) it re-sends every other slot, with a status request in
//!     between, until a status confirms it. The start/pause toggle is only
//!     used by the opt-in [`StopMethod::ToggleGuarded`]: once per press, only
//!     while the belt moves at a steady speed, never retried.
//!   - Start is sent at most once per press, only from a fresh stopped
//!     status, and never retried.
//!   - Nothing that can move the belt is sent unless the user asked for it in
//!     this link; the queue is dropped on every disconnect.
//!   - Speeds go through [`Speed::clamped`] (0.5–6.0 km/h).

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::protocol::{
    BeltState, Clamp, Command, DisplayUnits, MAX_SPEED_UNITS, MIN_SPEED_UNITS, Mode, ModeState,
    Reply, Speed, Status, decode_reply, display_tenths_to_pad, distance_to_display,
    pad_to_display_tenths,
};
use crate::transport::{
    Advertisement, EventReceiver, LinkId, Transport, TransportError, TransportEvent,
};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// How Stop stops the belt. Supervised test 1 picks one.
///
/// KS Fit 6.6.0 stops with the start/pause toggle, sent only while the belt
/// moves; but a toggle sent to a stopped or slowing belt starts it again
/// (qdomyos-zwift), so the toggle is opt-in and guarded here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub enum StopMethod {
    /// `f7 a2 02 02 a6 fd`: switch to standby. Idempotent; re-sent until
    /// confirmed.
    #[default]
    Standby,
    /// `f7 a2 01 00 a3 fd` (ph4's stop). Idempotent; re-sent until confirmed.
    SpeedZero,
    /// `f7 a2 04 01 a7 fd`, the toggle: only while the belt moves at a steady
    /// speed and no earlier stop is in progress, at most once per press,
    /// never re-sent. While the belt slows down, presses send nothing.
    ToggleGuarded,
}

/// A pad the app remembers (design §5.2): the platform ID, plus the MAC when
/// known, which is the same on every platform.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct KnownPad {
    pub id: String,
    #[cfg_attr(feature = "serde", serde(default))]
    pub mac: Option<String>,
    #[cfg_attr(feature = "serde", serde(default))]
    pub name: Option<String>,
}

impl KnownPad {
    #[must_use]
    pub fn from_ad(ad: &Advertisement) -> Self {
        Self {
            id: ad.id.clone(),
            mac: ad.mac.clone(),
            name: ad.name.clone(),
        }
    }

    /// The pad's identity is its MAC (confirmed by the spike: Android's
    /// address equals the manufacturer data). When both sides know a MAC,
    /// only the MAC counts; the platform ID is the fallback.
    #[must_use]
    pub fn matches(&self, ad: &Advertisement) -> bool {
        match (&self.mac, &ad.mac) {
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
            _ => ad.id == self.id,
        }
    }
}

/// A speed preset, stored in hundredths of a km/h so it survives switching
/// the display between mph and km/h without drifting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct Preset(pub u16);

impl Preset {
    /// From tenths of the display unit.
    #[must_use]
    pub fn from_display_tenths(tenths: u32, display: DisplayUnits) -> Self {
        let h = match display {
            DisplayUnits::Km => u64::from(tenths) * 10,
            DisplayUnits::Miles => (u64::from(tenths) * 1_609_344 + 50_000) / 100_000,
        };
        Self(h.min(u64::from(u16::MAX)) as u16)
    }

    /// Tenths of the display unit (exact for presets set in that unit).
    #[must_use]
    pub fn display_tenths(self, display: DisplayUnits) -> u32 {
        let h = u64::from(self.0);
        (match display {
            DisplayUnits::Km => (h + 5) / 10,
            DisplayUnits::Miles => (h * 100_000 + 804_672) / 1_609_344,
        }) as u32
    }
}

/// The design's timings. Tests use the defaults.
#[derive(Debug, Clone)]
pub struct Timing {
    /// Minimum gap between two writes (ph4 uses 0.69 s).
    pub write_spacing: Duration,
    /// Status poll interval once ready.
    pub poll_interval: Duration,
    /// Status request interval during the handshake.
    pub handshake_retry: Duration,
    /// No valid status this long after subscribing: give up on the link.
    pub handshake_timeout: Duration,
    /// Missed status replies in a row before the link counts as stale.
    pub stale_after_missed: u32,
    /// Stop not confirmed this long after the press: warn.
    pub stop_confirm: Duration,
    /// Start not confirmed this long after sending: give up, never retry.
    pub start_confirm: Duration,
    /// The status Start is sent from must be at most this old.
    pub status_fresh: Duration,
    /// After an unexpected disconnect, how long the app shows "reconnecting".
    pub reconnect_window: Duration,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    /// Watchdog for a transport connect that never returns.
    pub connect_watchdog: Duration,
    pub write_timeout: Duration,
    /// Retry interval for a failed scan start (adapter off, …).
    pub scan_retry: Duration,
    /// The pad's start countdown, for the estimate shown in the UI.
    pub countdown: Duration,
    /// After this long, a speed target the pad never reached is forgotten.
    pub target_hold: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            write_spacing: Duration::from_millis(700),
            poll_interval: Duration::from_millis(750),
            handshake_retry: Duration::from_millis(300),
            handshake_timeout: Duration::from_secs(3),
            stale_after_missed: 3,
            stop_confirm: Duration::from_secs(5),
            start_confirm: Duration::from_secs(5),
            status_fresh: Duration::from_millis(1000),
            reconnect_window: Duration::from_secs(60),
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(15),
            connect_watchdog: Duration::from_secs(45),
            write_timeout: Duration::from_secs(2),
            scan_retry: Duration::from_secs(5),
            countdown: Duration::from_secs(3),
            target_hold: Duration::from_secs(10),
        }
    }
}

pub type LogFn = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Clone)]
pub struct Config {
    pub display_units: DisplayUnits,
    pub stop_method: StopMethod,
    pub remembered: Option<KnownPad>,
    pub presets: [Preset; 3],
    pub timing: Timing,
    /// Receives one line per notable event (rejected frames, writes, errors).
    pub log: Option<LogFn>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            display_units: DisplayUnits::Miles,
            stop_method: StopMethod::Standby,
            remembered: None,
            // 1.0, 2.0 and 3.0 mph
            presets: [Preset(161), Preset(322), Preset(483)],
            timing: Timing::default(),
            log: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Intents and snapshot
// ---------------------------------------------------------------------------

/// What the UI can ask for. Speeds are in tenths of the display unit.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "type", rename_all = "camelCase"))]
pub enum Intent {
    /// Connect to a pad from the scan list and remember it.
    Connect {
        id: String,
    },
    /// Drop the link and stop searching (to hand the pad to another device).
    Disconnect,
    /// Start searching again (after Disconnect or a permission error).
    Search,
    /// Forget the remembered pad.
    Forget,
    Start,
    Stop,
    SetMode {
        mode: Mode,
    },
    /// Change the speed by this many 0.1 steps of the display unit.
    AdjustSpeed {
        steps: i32,
    },
    SetSpeed {
        tenths: u32,
    },
    ApplyPreset {
        index: usize,
    },
    SetPreset {
        index: usize,
        tenths: u32,
    },
    SetDisplayUnits {
        units: DisplayUnits,
    },
    /// Whether the app is in the foreground. Scanning stops in the background.
    SetVisible {
        visible: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub enum LinkPhase {
    /// Not searching (after Disconnect, or in the background).
    Idle,
    /// Scanning; connects to the remembered pad when it appears.
    Searching,
    /// Searching within a minute of an unexpected disconnect.
    Reconnecting,
    Connecting,
    /// Connected, waiting for the first status.
    Handshake,
    Ready,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub enum BeltView {
    Idle,
    Running,
    Paused,
    Stopped,
    Fault,
    /// State 5, "sleep"; labelled Standby.
    Standby,
    Countdown,
    LockOff,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub enum ModeView {
    Auto,
    Manual,
    Standby,
    Newer,
    Unknown,
}

/// Live stats in display units.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct Stats {
    pub belt: BeltView,
    pub mode: ModeView,
    /// Rounded to 0.1.
    pub speed: f64,
    /// The speed the app last asked for, while the belt ramps to it.
    pub target_speed: Option<f64>,
    pub time_s: u32,
    /// Rounded to 0.01.
    pub distance: f64,
    pub steps: u32,
    /// Estimated seconds left in the start countdown.
    pub countdown_s: Option<u32>,
    /// `E01`–`E15` when the pad reports a fault.
    pub fault: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct UnitsView {
    pub display: DisplayUnits,
    pub speed_unit: &'static str,
    pub distance_unit: &'static str,
    pub min_speed: f64,
    pub max_speed: f64,
}

/// Which controls make sense right now. The UI only renders these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct Controls {
    /// Always true while a link exists.
    pub stop: bool,
    pub start: bool,
    pub speed: bool,
    pub manual: bool,
    pub auto: bool,
    pub standby: bool,
    pub disconnect: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub enum Level {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct Notice {
    pub level: Level,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub enum ProblemKind {
    PermissionDenied,
    AdapterOff,
    Other,
}

/// Why the app can't search right now.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct Problem {
    pub kind: ProblemKind,
    pub text: String,
}

/// Everything the UI renders. Published after every change.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct Snapshot {
    pub link: LinkPhase,
    /// A link exists (connected, handshaking or ready): Stop is enabled.
    pub has_link: bool,
    /// Three status replies in a row were missed: grey out the stats.
    pub stale: bool,
    /// The last link was lost unexpectedly and hasn't come back.
    pub link_lost: bool,
    pub scanning: bool,
    /// The pad being connected or connected.
    pub pad: Option<KnownPad>,
    pub remembered: Option<KnownPad>,
    pub discovered: Vec<Advertisement>,
    pub stats: Option<Stats>,
    pub units: UnitsView,
    pub controls: Controls,
    pub stopping: bool,
    pub stop_unconfirmed: bool,
    /// The belt is slowing down after a stop: "Slowing down…".
    pub slowing: bool,
    pub starting: bool,
    pub notice: Option<Notice>,
    pub problem: Option<Problem>,
    pub stop_method: StopMethod,
    /// Presets in the display unit.
    pub presets: [f64; 3],
    /// The presets as stored (for the settings file; not sent to the UI).
    #[cfg_attr(feature = "serde", serde(skip))]
    pub preset_store: [Preset; 3],
    pub bad_frames: u32,
}

// ---------------------------------------------------------------------------
// Handle
// ---------------------------------------------------------------------------

/// The app's side of a running session.
#[derive(Clone)]
pub struct SessionHandle {
    intents: mpsc::UnboundedSender<Intent>,
    state: watch::Receiver<Snapshot>,
}

impl SessionHandle {
    /// Queues an intent. Returns false if the session has ended.
    pub fn send(&self, intent: Intent) -> bool {
        self.intents.send(intent).is_ok()
    }

    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        self.state.borrow().clone()
    }

    /// A receiver that wakes on every published snapshot.
    #[must_use]
    pub fn watch(&self) -> watch::Receiver<Snapshot> {
        self.state.clone()
    }
}

/// Builds a session; run it with [`Session::run`] on a tokio runtime.
#[must_use]
pub fn new<T: Transport>(
    transport: T,
    events: EventReceiver,
    config: Config,
) -> (Session<T>, SessionHandle) {
    let (intent_tx, intent_rx) = mpsc::unbounded_channel();
    let (done_tx, done_rx) = mpsc::unbounded_channel();
    let now = Instant::now();
    let session = Session {
        t: Arc::new(transport),
        events,
        intents: intent_rx,
        done_tx,
        done_rx,
        state_tx: watch::channel(placeholder_snapshot()).0,
        phase: Phase::Searching,
        visible: true,
        scanning: false,
        scan_retry_at: None,
        problem: None,
        discovered: Vec::new(),
        target: config.remembered.clone(),
        remembered: config.remembered.clone(),
        attempt_seq: 0,
        next_attempt: now,
        backoff: config.timing.backoff_min,
        lost_at: None,
        link: LinkState::default(),
        notice: None,
        bad_frames: 0,
        cfg: config,
    };
    let first = session.snapshot(now);
    session.state_tx.send_replace(first);
    let handle = SessionHandle {
        intents: intent_tx,
        state: session.state_tx.subscribe(),
    };
    (session, handle)
}

/// Builds a session and spawns it on the current tokio runtime.
pub fn spawn<T: Transport>(transport: T, events: EventReceiver, config: Config) -> SessionHandle {
    let (session, handle) = new(transport, events, config);
    tokio::spawn(session.run());
    handle
}

fn placeholder_snapshot() -> Snapshot {
    Snapshot {
        link: LinkPhase::Idle,
        has_link: false,
        stale: false,
        link_lost: false,
        scanning: false,
        pad: None,
        remembered: None,
        discovered: Vec::new(),
        stats: None,
        units: UnitsView {
            display: DisplayUnits::Miles,
            speed_unit: "mph",
            distance_unit: "mi",
            min_speed: 0.0,
            max_speed: 0.0,
        },
        controls: Controls::default(),
        stopping: false,
        stop_unconfirmed: false,
        slowing: false,
        starting: false,
        notice: None,
        problem: None,
        stop_method: StopMethod::Standby,
        presets: [0.0; 3],
        preset_store: [Preset(0); 3],
        bad_frames: 0,
    }
}

// ---------------------------------------------------------------------------
// Session internals
// ---------------------------------------------------------------------------

/// What one write slot carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Write {
    Ask,
    /// The idempotent stop command, or the guarded toggle.
    Stop,
    /// Switch to manual to wake the pad from standby before a Start.
    Wake,
    StartPause,
    Mode(Mode),
    Speed(Speed),
}

impl Write {
    fn command(self, stop: StopKind) -> Command {
        match self {
            Self::Ask => Command::AskStatus,
            Self::Stop => match stop {
                StopKind::Idempotent(c) => c,
                StopKind::Toggle => Command::StartPause,
            },
            Self::Wake => Command::SetMode(Mode::Manual),
            Self::StartPause => Command::StartPause,
            Self::Mode(m) => Command::SetMode(m),
            Self::Speed(s) => Command::SetSpeed(s),
        }
    }
}

enum Phase {
    Idle,
    Searching,
    Connecting {
        attempt: u64,
        pad: KnownPad,
        started: Instant,
    },
    Handshake {
        link: LinkId,
        pad: KnownPad,
        started: Instant,
    },
    Ready {
        link: LinkId,
        pad: KnownPad,
    },
}

enum Done {
    Linked {
        attempt: u64,
        result: Result<LinkId, TransportError>,
    },
    Written {
        link: LinkId,
        write: Write,
        result: Result<(), TransportError>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopKind {
    /// Re-sent every other slot until confirmed.
    Idempotent(Command),
    /// The start/pause toggle: sent once, never re-sent.
    Toggle,
}

#[derive(Debug)]
struct StopState {
    kind: StopKind,
    pressed: Instant,
    /// Belt speed when Stop was pressed; a lower speed confirms it.
    ref_speed: Option<u8>,
    first_sent: Option<Instant>,
    sends: u32,
    warned: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartPhase {
    /// Switching to manual to leave standby.
    Waking { sent: bool },
    /// Waiting for a fresh idle status to send the toggle from.
    Ready,
    /// Sent once; waiting for the countdown.
    Sent { at: Instant },
}

#[derive(Debug)]
struct StartFlow {
    pressed: Instant,
    phase: StartPhase,
}

/// Everything that belongs to one link. Replaced wholesale on disconnect, so
/// nothing queued survives into the next link.
#[derive(Debug, Default)]
struct LinkState {
    last_write: Option<(Instant, Write)>,
    writing: bool,
    status: Option<Status>,
    status_at: Option<Instant>,
    missed: u32,
    poll_outstanding: bool,
    queued_mode: Option<Mode>,
    queued_speed: Option<Speed>,
    target_speed: Option<(u8, Instant)>,
    stop: Option<StopState>,
    /// A stop was confirmed and the belt is still slowing down.
    slowing: bool,
    /// The speed in the status before the current one.
    prev_speed: Option<u8>,
    start: Option<StartFlow>,
    countdown_since: Option<Instant>,
}

pub struct Session<T: Transport> {
    t: Arc<T>,
    events: EventReceiver,
    intents: mpsc::UnboundedReceiver<Intent>,
    done_tx: mpsc::UnboundedSender<Done>,
    done_rx: mpsc::UnboundedReceiver<Done>,
    state_tx: watch::Sender<Snapshot>,
    cfg: Config,
    phase: Phase,
    visible: bool,
    scanning: bool,
    scan_retry_at: Option<Instant>,
    problem: Option<Problem>,
    discovered: Vec<Advertisement>,
    /// The pad to connect to when it appears.
    target: Option<KnownPad>,
    remembered: Option<KnownPad>,
    attempt_seq: u64,
    next_attempt: Instant,
    backoff: Duration,
    lost_at: Option<Instant>,
    link: LinkState,
    notice: Option<Notice>,
    bad_frames: u32,
}

fn notice(level: Level, text: impl Into<String>) -> Option<Notice> {
    Some(Notice {
        level,
        text: text.into(),
    })
}

const SAFETY: &str = "Use the remote, pull the safety key, or step off.";

impl<T: Transport> Session<T> {
    /// Runs until every [`SessionHandle`] is dropped, then disconnects.
    pub async fn run(mut self) {
        loop {
            let now = Instant::now();
            self.tick(now).await;
            self.publish(Instant::now());
            let wake = self.next_wake(Instant::now());
            tokio::select! {
                biased;
                intent = self.intents.recv() => match intent {
                    Some(i) => self.on_intent(i, Instant::now()),
                    None => break,
                },
                Some(done) = self.done_rx.recv() => self.on_done(done, Instant::now()),
                Some(ev) = self.events.recv() => self.on_event(ev, Instant::now()),
                () = tokio::time::sleep_until(wake) => {}
            }
        }
        if let Some(link) = self.current_link() {
            let _ = tokio::time::timeout(Duration::from_secs(3), self.t.disconnect(link)).await;
        }
        let _ = tokio::time::timeout(Duration::from_secs(1), self.t.stop_scan()).await;
    }

    fn log(&self, msg: &str) {
        if let Some(log) = &self.cfg.log {
            log(msg);
        }
    }

    fn current_link(&self) -> Option<LinkId> {
        match &self.phase {
            Phase::Handshake { link, .. } | Phase::Ready { link, .. } => Some(*link),
            _ => None,
        }
    }

    fn is_ready(&self) -> bool {
        matches!(self.phase, Phase::Ready { .. })
    }

    fn stale(&self) -> bool {
        self.link.missed >= self.cfg.timing.stale_after_missed
    }

    // --- scheduling ------------------------------------------------------

    fn next_wake(&self, now: Instant) -> Instant {
        let t = &self.cfg.timing;
        let far = now + Duration::from_secs(3600);
        let mut wake = far;
        let mut at = |i: Instant| wake = wake.min(i);
        match &self.phase {
            Phase::Idle => {}
            Phase::Searching => {
                if let Some(r) = self.scan_retry_at
                    && self.visible
                    && !self.scanning
                {
                    at(r);
                }
                if self.target_visible().is_some() {
                    at(self.next_attempt);
                }
                if let Some(lost) = self.lost_at
                    && lost + t.reconnect_window > now
                {
                    at(lost + t.reconnect_window);
                }
            }
            Phase::Connecting { started, .. } => at(*started + t.connect_watchdog),
            Phase::Handshake { started, .. } => {
                at(*started + t.handshake_timeout);
                if !self.link.writing
                    && let Some((w, _)) = self.next_write(now)
                {
                    at(w);
                }
            }
            Phase::Ready { .. } => {
                if !self.link.writing
                    && let Some((w, _)) = self.next_write(now)
                {
                    at(w);
                }
                if let Some(s) = &self.link.stop
                    && !s.warned
                {
                    at(s.pressed + t.stop_confirm);
                }
                if let Some(f) = &self.link.start {
                    match f.phase {
                        StartPhase::Sent { at: sent } => at(sent + t.start_confirm),
                        _ => at(f.pressed + t.start_confirm),
                    }
                }
                if let Some(since) = self.link.countdown_since {
                    // refresh the countdown estimate
                    let elapsed = now.saturating_duration_since(since);
                    let next =
                        Duration::from_millis(((elapsed.as_millis() / 250 + 1) * 250) as u64);
                    at(since + next);
                }
                if let Some((_, set)) = self.link.target_speed {
                    at(set + t.target_hold);
                }
            }
        }
        // Everything due by `now` was done by the tick before this, so a
        // deadline in the past can only mean nothing to do yet; never spin.
        wake.max(now + Duration::from_millis(1))
    }

    /// The next write in the current link: when, and what.
    fn next_write(&self, now: Instant) -> Option<(Instant, Write)> {
        let t = &self.cfg.timing;
        let l = &self.link;
        match &self.phase {
            Phase::Handshake { .. } => {
                let Some((last_at, last)) = l.last_write else {
                    return Some((now, Write::Ask));
                };
                if l.stop
                    .as_ref()
                    .is_some_and(|s| matches!(s.kind, StopKind::Idempotent(_)))
                    && last != Write::Stop
                {
                    return Some((last_at + t.write_spacing, Write::Stop));
                }
                let gap = if last == Write::Ask {
                    t.handshake_retry
                } else {
                    t.write_spacing
                };
                Some((last_at + gap, Write::Ask))
            }
            Phase::Ready { .. } => {
                let Some((last_at, last)) = l.last_write else {
                    return Some((now, Write::Ask));
                };
                let slot = last_at + t.write_spacing;
                // Stop pre-empts everything. The idempotent command alternates
                // with status requests until confirmed; the guarded toggle goes
                // once, from a status newer than the last write.
                if let Some(stop) = &l.stop {
                    match stop.kind {
                        StopKind::Idempotent(_) if last != Write::Stop => {
                            return Some((slot, Write::Stop));
                        }
                        StopKind::Toggle
                            if stop.sends == 0 && self.status_fresh_since(last_at, slot) =>
                        {
                            return Some((slot, Write::Stop));
                        }
                        _ => {}
                    }
                }
                // A status request follows every command.
                let poll = last_at + t.poll_interval;
                if last != Write::Ask {
                    return Some((poll, Write::Ask));
                }
                if let Some(m) = l.queued_mode {
                    return Some((slot, Write::Mode(m)));
                }
                if let Some(f) = &l.start {
                    match f.phase {
                        StartPhase::Waking { sent: false } => return Some((slot, Write::Wake)),
                        StartPhase::Ready if self.status_fresh_since(last_at, slot) => {
                            return Some((slot, Write::StartPause));
                        }
                        _ => {}
                    }
                }
                if let Some(s) = l.queued_speed {
                    return Some((slot, Write::Speed(s)));
                }
                Some((poll, Write::Ask))
            }
            _ => None,
        }
    }

    /// Whether a status arrived after `since` and will be fresh at `at`.
    fn status_fresh_since(&self, since: Instant, at: Instant) -> bool {
        self.link.status_at.is_some_and(|s| {
            s >= since && at.saturating_duration_since(s) <= self.cfg.timing.status_fresh
        })
    }

    async fn tick(&mut self, now: Instant) {
        self.check_timeouts(now);
        match &self.phase {
            Phase::Idle => self.ensure_scan(false, now).await,
            Phase::Searching => {
                self.ensure_scan(self.visible, now).await;
                self.maybe_connect(now).await;
            }
            Phase::Connecting { .. } => {}
            Phase::Handshake { .. } | Phase::Ready { .. } => {
                if !self.link.writing
                    && let Some((at, w)) = self.next_write(now)
                    && at <= now
                {
                    self.write(w, now);
                }
            }
        }
    }

    fn check_timeouts(&mut self, now: Instant) {
        let t = self.cfg.timing.clone();
        match &self.phase {
            Phase::Connecting { started, .. } if now >= *started + t.connect_watchdog => {
                self.log("connect watchdog fired");
                self.connect_failed("Connecting took too long.".into(), now);
            }
            Phase::Handshake { link, started, .. } if now >= *started + t.handshake_timeout => {
                let link = *link;
                self.log("handshake timed out: no status reply");
                self.drop_link(link);
                self.connect_failed("The pad connected but didn't answer.".into(), now);
            }
            _ => {}
        }
        if let Some(stop) = &mut self.link.stop
            && !stop.warned
            && now >= stop.pressed + t.stop_confirm
        {
            stop.warned = true;
            if stop.kind == StopKind::Toggle {
                // Never re-sent; a new press may try again under guard.
                self.link.stop = None;
            }
            self.notice = notice(
                Level::Error,
                "Stop not confirmed — use the remote, the pad's button or the safety key.",
            );
            self.log("stop not confirmed within 5 s");
        }
        if let Some(flow) = &self.link.start {
            let (expired, text) = match flow.phase {
                StartPhase::Sent { at } => (
                    now >= at + t.start_confirm,
                    "Start not confirmed. Press Start again if the belt should start.",
                ),
                StartPhase::Waking { .. } => (
                    now >= flow.pressed + t.start_confirm,
                    "The pad didn't wake from standby. Touch M on the pad, then press Start.",
                ),
                StartPhase::Ready => (
                    now >= flow.pressed + t.start_confirm,
                    "Start not sent: no fresh status from the pad.",
                ),
            };
            if expired {
                self.link.start = None;
                self.notice = notice(Level::Warning, text);
                self.log(text);
            }
        }
        if let Some((_, set)) = self.link.target_speed
            && now >= set + t.target_hold
        {
            self.link.target_speed = None;
        }
    }

    fn write(&mut self, w: Write, now: Instant) {
        let Some(link) = self.current_link() else {
            return;
        };
        let kind = self
            .link
            .stop
            .as_ref()
            .map_or(StopKind::Idempotent(Command::SetMode(Mode::Standby)), |s| {
                s.kind
            });
        let cmd = w.command(kind);
        let stale_limit = self.cfg.timing.stale_after_missed;
        match w {
            Write::Ask => {
                if self.link.poll_outstanding {
                    self.link.missed += 1;
                    if self.link.missed == stale_limit {
                        self.log("link stale: 3 status replies missed");
                    }
                }
                self.link.poll_outstanding = true;
            }
            Write::Stop => {
                if kind == StopKind::Toggle {
                    // Re-check right before the toggle, from the fresh status:
                    // sent to a stopped or slowing belt it would start it.
                    if let Err(why) = self.toggle_stop_ok() {
                        self.link.stop = None;
                        self.notice = notice(Level::Info, why);
                        self.log(&format!("toggle stop not sent: {why}"));
                        return;
                    }
                }
                if let Some(s) = &mut self.link.stop {
                    s.sends += 1;
                    s.first_sent.get_or_insert(now);
                }
            }
            Write::Wake => {
                if let Some(f) = &mut self.link.start {
                    f.phase = StartPhase::Waking { sent: true };
                }
            }
            Write::StartPause => {
                // Re-check the belt from the fresh status right before the
                // toggle: it must be stopped in manual or auto, or the toggle
                // would do something else.
                let ok = self.link.status.is_some_and(|s| {
                    can_start_from(&s)
                        && matches!(s.mode, ModeState::Known(Mode::Manual | Mode::Auto))
                }) && self.link.missed < stale_limit;
                if !ok {
                    self.link.start = None;
                    self.notice =
                        notice(Level::Warning, "Start cancelled: the belt isn't stopped.");
                    self.log("start cancelled before sending: belt not stopped");
                    return;
                }
                if let Some(f) = &mut self.link.start {
                    f.phase = StartPhase::Sent { at: now };
                }
            }
            Write::Mode(m) => {
                if self.link.queued_mode == Some(m) {
                    self.link.queued_mode = None;
                }
            }
            Write::Speed(s) => {
                if self.link.queued_speed == Some(s) {
                    self.link.queued_speed = None;
                }
            }
        }
        self.link.last_write = Some((now, w));
        self.link.writing = true;
        if w != Write::Ask {
            self.log(&format!("send {cmd:?}"));
        }
        let t = Arc::clone(&self.t);
        let done = self.done_tx.clone();
        let frame = cmd.encode();
        let timeout = self.cfg.timing.write_timeout;
        tokio::spawn(async move {
            let result = match tokio::time::timeout(timeout, t.write(link, &frame)).await {
                Ok(r) => r,
                Err(_) => Err(TransportError::Timeout),
            };
            let _ = done.send(Done::Written {
                link,
                write: w,
                result,
            });
        });
    }

    // --- discovery and connection -------------------------------------------

    async fn ensure_scan(&mut self, want: bool, now: Instant) {
        if want == self.scanning {
            return;
        }
        if want {
            if self.scan_retry_at.is_some_and(|r| now < r) {
                return;
            }
            if self
                .problem
                .as_ref()
                .is_some_and(|p| p.kind == ProblemKind::PermissionDenied)
                && self.scan_retry_at.is_none()
            {
                // Asking again needs a user action (Search intent).
                return;
            }
            match tokio::time::timeout(Duration::from_secs(10), self.t.start_scan()).await {
                Ok(Ok(())) => {
                    self.scanning = true;
                    self.problem = None;
                    self.scan_retry_at = None;
                }
                Ok(Err(e)) => self.scan_failed(e, now),
                Err(_) => self.scan_failed(TransportError::Timeout, now),
            }
        } else {
            self.scanning = false;
            self.discovered.clear();
            if let Err(e) = self.t.stop_scan().await {
                self.log(&format!("stop scan failed: {e}"));
            }
        }
    }

    fn scan_failed(&mut self, e: TransportError, now: Instant) {
        self.log(&format!("scan failed: {e}"));
        let (kind, text, retry) = match e {
            TransportError::PermissionDenied => (
                ProblemKind::PermissionDenied,
                "Bluetooth permission is needed to find the pad.".to_string(),
                None,
            ),
            TransportError::AdapterOff => (
                ProblemKind::AdapterOff,
                "Bluetooth is off. Turn it on to find the pad.".to_string(),
                Some(now + self.cfg.timing.scan_retry),
            ),
            other => (
                ProblemKind::Other,
                format!("Can't search for the pad: {other}"),
                Some(now + self.cfg.timing.scan_retry),
            ),
        };
        self.problem = Some(Problem { kind, text });
        self.scan_retry_at = retry;
    }

    fn target_visible(&self) -> Option<&Advertisement> {
        let target = self.target.as_ref()?;
        self.discovered.iter().find(|ad| target.matches(ad))
    }

    async fn maybe_connect(&mut self, now: Instant) {
        if !self.visible || !self.scanning || now < self.next_attempt {
            return;
        }
        let Some(ad) = self.target_visible().cloned() else {
            return;
        };
        self.connect(&ad, now).await;
    }

    async fn connect(&mut self, ad: &Advertisement, now: Instant) {
        self.attempt_seq += 1;
        let attempt = self.attempt_seq;
        // Connecting stops the scan (blec does it anyway).
        self.ensure_scan(false, now).await;
        self.phase = Phase::Connecting {
            attempt,
            pad: KnownPad::from_ad(ad),
            started: now,
        };
        self.log(&format!("connecting to {} ({:?})", ad.id, ad.mac));
        let t = Arc::clone(&self.t);
        let done = self.done_tx.clone();
        let id = ad.id.clone();
        tokio::spawn(async move {
            let result = async {
                let link = t.connect(&id).await?;
                if let Err(e) = t.subscribe(link).await {
                    let _ = t.disconnect(link).await;
                    return Err(e);
                }
                Ok(link)
            }
            .await;
            let _ = done.send(Done::Linked { attempt, result });
        });
    }

    fn connect_failed(&mut self, why: String, now: Instant) {
        self.phase = Phase::Searching;
        self.link = LinkState::default();
        self.next_attempt = now + self.backoff;
        self.backoff = (self.backoff * 2).min(self.cfg.timing.backoff_max);
        self.notice = notice(Level::Warning, why);
    }

    /// Ends the current link from our side, without waiting.
    fn drop_link(&mut self, link: LinkId) {
        let t = Arc::clone(&self.t);
        tokio::spawn(async move {
            let _ = tokio::time::timeout(Duration::from_secs(5), t.disconnect(link)).await;
        });
    }

    fn on_link_lost(&mut self, now: Instant) {
        self.log("link lost");
        // Every queued command, pending Stop and Start flow goes with it.
        self.link = LinkState::default();
        self.phase = Phase::Searching;
        self.lost_at = Some(now);
        self.next_attempt = now;
        self.backoff = self.cfg.timing.backoff_min;
        self.notice = None;
    }

    // --- inputs ----------------------------------------------------------

    fn on_done(&mut self, done: Done, now: Instant) {
        match done {
            Done::Linked { attempt, result } => {
                let current = match &self.phase {
                    Phase::Connecting {
                        attempt: a, pad, ..
                    } if *a == attempt => Some(pad.clone()),
                    _ => None,
                };
                match (current, result) {
                    (Some(pad), Ok(link)) => {
                        self.log(&format!("link {link} up, handshaking"));
                        self.link = LinkState::default();
                        self.phase = Phase::Handshake {
                            link,
                            pad,
                            started: now,
                        };
                    }
                    (Some(_), Err(e)) => {
                        self.log(&format!("connect failed: {e}"));
                        let why = match e {
                            TransportError::Incompatible(_) => {
                                "That device isn't a compatible pad.".to_string()
                            }
                            other => format!("Couldn't connect: {other}"),
                        };
                        self.connect_failed(why, now);
                    }
                    (None, Ok(link)) => {
                        // An attempt we gave up on (Disconnect, watchdog): close it.
                        self.log(&format!("closing late link {link}"));
                        self.drop_link(link);
                    }
                    (None, Err(_)) => {}
                }
            }
            Done::Written {
                link,
                write,
                result,
            } => {
                if Some(link) == self.current_link() {
                    self.link.writing = false;
                }
                if let Err(e) = result {
                    self.log(&format!("write {write:?} failed: {e}"));
                }
            }
        }
    }

    fn on_event(&mut self, ev: TransportEvent, now: Instant) {
        match ev {
            TransportEvent::ScanResults(ads) => {
                if self.scanning {
                    self.discovered = ads;
                }
            }
            TransportEvent::Notification { link, data } => {
                if Some(link) != self.current_link() {
                    return;
                }
                match decode_reply(&data) {
                    Ok(Reply::Status(s)) => self.on_status(s, now),
                    Ok(Reply::Record(r)) => self.log(&format!("last record: {r:?}")),
                    Err(e) => {
                        self.bad_frames += 1;
                        self.log(&format!("rejected frame {data:02x?}: {e}"));
                    }
                }
            }
            TransportEvent::Disconnected { link } => {
                if Some(link) == self.current_link() {
                    self.on_link_lost(now);
                }
            }
        }
    }

    fn on_status(&mut self, s: Status, now: Instant) {
        if let Phase::Handshake { link, pad, .. } = &self.phase {
            let (link, pad) = (*link, pad.clone());
            self.log("handshake done");
            // Remember the pad, with the freshest ID and MAC.
            let pad = self.target_visible().map(KnownPad::from_ad).unwrap_or(pad);
            self.remembered = Some(pad.clone());
            self.target = Some(pad.clone());
            self.phase = Phase::Ready { link, pad };
            self.backoff = self.cfg.timing.backoff_min;
            self.lost_at = None;
            self.problem = None;
            self.notice = None;
        }
        let l = &mut self.link;
        l.prev_speed = l.status.map(|p| p.speed);
        l.status = Some(s);
        l.status_at = Some(now);
        l.missed = 0;
        l.poll_outstanding = false;

        if s.belt.is_countdown() {
            l.countdown_since.get_or_insert(now);
        } else {
            l.countdown_since = None;
        }
        if !s.is_moving() {
            l.target_speed = None;
            l.slowing = false;
        } else if l.target_speed.is_some_and(|(t, _)| t == s.speed) {
            l.target_speed = None;
        }

        // Stop is confirmed by a status after the first send that shows the
        // belt not moving (nor counting down), or slowing down.
        if let Some(stop) = &mut l.stop
            && stop.first_sent.is_some_and(|first| now > first)
        {
            let falling = s.is_moving() && stop.ref_speed.is_some_and(|r| s.speed < r);
            if !s.is_active() || falling {
                let was_warned = stop.warned;
                l.stop = None;
                l.slowing = s.is_moving();
                self.notice = if was_warned {
                    notice(Level::Info, "Stop confirmed.")
                } else {
                    None
                };
                self.log("stop confirmed");
            } else if stop.ref_speed.is_none() || stop.ref_speed == Some(0) {
                stop.ref_speed = Some(s.speed);
            }
        }

        if let Some(flow) = &mut self.link.start {
            match flow.phase {
                StartPhase::Waking { .. }
                    if s.mode == ModeState::Known(Mode::Manual) && s.belt != BeltState::Sleep =>
                {
                    flow.phase = StartPhase::Ready;
                }
                StartPhase::Sent { .. } if s.is_active() => {
                    self.link.start = None;
                    self.log("start confirmed");
                }
                _ => {}
            }
        }
    }

    /// The belt is coming to a stop: after a confirmed stop, or the pad
    /// reports paused/stopped/standby while the belt still turns.
    fn stopping_belt(&self, s: &Status) -> bool {
        s.is_moving()
            && (self.link.slowing
                || matches!(
                    s.belt,
                    BeltState::Paused | BeltState::Stopped | BeltState::Sleep
                )
                || s.mode == ModeState::Known(Mode::Standby))
    }

    /// Whether the guarded toggle may be sent as Stop: the belt moves at a
    /// steady speed (or speeds up, or ramps down to a speed the app asked
    /// for) and no stop is under way. Sent to a stopped or slowing belt, the
    /// toggle would start it.
    fn toggle_stop_ok(&self) -> Result<(), &'static str> {
        let Some(s) = self.link.status else {
            return Err("No status from the pad yet.");
        };
        if !s.is_moving() {
            return Err("The belt isn't moving.");
        }
        if self.stopping_belt(&s) {
            return Err("Slowing down…");
        }
        let falling = self.link.prev_speed.is_some_and(|p| s.speed < p);
        let ours = self.link.target_speed.is_some_and(|(t, _)| t < s.speed);
        if falling && !ours {
            return Err("Slowing down…");
        }
        if !matches!(
            s.belt,
            BeltState::Running | BeltState::Idle | BeltState::Countdown(_)
        ) {
            return Err("The belt state is unclear; not sending the toggle.");
        }
        Ok(())
    }

    fn on_intent(&mut self, intent: Intent, now: Instant) {
        self.log(&format!("intent {intent:?}"));
        match intent {
            Intent::Stop => self.on_stop(now),
            Intent::Start => self.on_start(now),
            Intent::SetMode { mode } => self.on_set_mode(mode),
            Intent::AdjustSpeed { steps } => self.on_speed(SpeedRequest::Adjust(steps), now),
            Intent::SetSpeed { tenths } => self.on_speed(SpeedRequest::Display(tenths), now),
            Intent::ApplyPreset { index } => {
                if let Some(p) = self.cfg.presets.get(index).copied() {
                    let tenths = p.display_tenths(self.cfg.display_units);
                    self.on_speed(SpeedRequest::Display(tenths), now);
                }
            }
            Intent::SetPreset { index, tenths } => {
                if let Some(p) = self.cfg.presets.get_mut(index) {
                    *p = Preset::from_display_tenths(tenths, self.cfg.display_units);
                }
            }
            Intent::SetDisplayUnits { units } => self.cfg.display_units = units,
            Intent::SetVisible { visible } => self.visible = visible,
            Intent::Connect { id } => self.on_connect(&id, now),
            Intent::Disconnect => self.on_disconnect(),
            Intent::Search => {
                if matches!(self.phase, Phase::Idle | Phase::Searching) {
                    self.phase = Phase::Searching;
                    self.next_attempt = now;
                    self.backoff = self.cfg.timing.backoff_min;
                    self.scan_retry_at = Some(now); // a user action may retry a permission prompt
                    self.notice = None;
                }
            }
            Intent::Forget => {
                self.remembered = None;
                if self.current_link().is_none() {
                    self.target = None;
                }
            }
        }
    }

    fn on_stop(&mut self, now: Instant) {
        if self.current_link().is_none() {
            self.notice = notice(
                Level::Warning,
                format!("Not connected to the pad. {SAFETY}"),
            );
            return;
        }
        if self.link.start.take().is_some() {
            self.log("start cancelled by stop");
        }
        // Stop pre-empts the queue: nothing queued may run after it.
        self.link.queued_mode = None;
        self.link.queued_speed = None;
        self.link.target_speed = None;
        self.notice = None;
        let kind = match self.cfg.stop_method {
            StopMethod::Standby => StopKind::Idempotent(Command::SetMode(Mode::Standby)),
            StopMethod::SpeedZero => StopKind::Idempotent(Command::SpeedZero),
            StopMethod::ToggleGuarded => {
                if self.link.stop.is_some() {
                    self.notice = notice(Level::Info, "Stop sent; waiting for the pad.");
                    return;
                }
                let Some(s) = self.link.status else {
                    self.notice = notice(
                        Level::Warning,
                        format!("No status from the pad yet. {SAFETY}"),
                    );
                    return;
                };
                if s.belt.is_countdown() && !s.is_moving() {
                    // What the toggle does mid-countdown is unknown; standby
                    // is idempotent and keeps the belt from starting.
                    StopKind::Idempotent(Command::SetMode(Mode::Standby))
                } else {
                    match self.toggle_stop_ok() {
                        Ok(()) => StopKind::Toggle,
                        Err(why) => {
                            self.notice = notice(Level::Info, why);
                            return;
                        }
                    }
                }
            }
        };
        if self.link.stop.is_none() {
            self.link.stop = Some(StopState {
                kind,
                pressed: now,
                ref_speed: self.link.status.map(|s| s.speed),
                first_sent: None,
                sends: 0,
                warned: false,
            });
        }
    }

    fn on_start(&mut self, now: Instant) {
        let refuse = |s: &mut Self, text: &str| s.notice = notice(Level::Info, text);
        if !self.is_ready() {
            return refuse(self, "Not connected to the pad yet.");
        }
        if self.stale() {
            return refuse(self, "No status from the pad (link stale).");
        }
        if self.link.stop.is_some() {
            return refuse(self, "Stopping…");
        }
        if self.link.start.is_some() {
            return; // already starting: never a second toggle for one start
        }
        if self.link.queued_mode.is_some()
            || self
                .link
                .last_write
                .is_some_and(|(_, w)| matches!(w, Write::Mode(_)))
        {
            return refuse(self, "Wait for the mode change to finish.");
        }
        let Some(s) = self.link.status else {
            return refuse(self, "No status from the pad yet.");
        };
        if s.is_active() {
            return refuse(self, "The belt is already running.");
        }
        if let Some(code) = s.fault() {
            self.notice = notice(
                Level::Error,
                format!("The pad reports fault {code}. Check the pad."),
            );
            return;
        }
        if !can_start_from(&s) && s.belt != BeltState::Sleep {
            return refuse(self, "The pad can't start from its current state.");
        }
        // From standby (state 5) or mode 2/3, switch to manual first and wait
        // for the status to show it, as KS Fit does.
        let wake = s.belt == BeltState::Sleep
            || !matches!(s.mode, ModeState::Known(Mode::Manual | Mode::Auto));
        let phase = if wake {
            StartPhase::Waking { sent: false }
        } else {
            StartPhase::Ready
        };
        self.link.start = Some(StartFlow {
            pressed: now,
            phase,
        });
        self.notice = None;
    }

    fn on_set_mode(&mut self, mode: Mode) {
        let refuse = |s: &mut Self, text: &str| s.notice = notice(Level::Info, text);
        if !self.is_ready() {
            return refuse(self, "Not connected to the pad yet.");
        }
        let Some(s) = self.link.status else {
            return refuse(self, "No status from the pad yet.");
        };
        if mode != Mode::Standby && self.stale() {
            return refuse(self, "No status from the pad (link stale).");
        }
        if mode == Mode::Auto {
            let stopped = !s.is_active() && (can_start_from(&s) || s.belt == BeltState::Sleep);
            if !stopped || self.link.stop.is_some() {
                return refuse(
                    self,
                    "Auto mode can only be selected while the belt is stopped.",
                );
            }
        }
        if self.link.start.take().is_some() {
            self.log("start cancelled by a mode change");
        }
        self.link.queued_mode = Some(mode);
        self.notice = None;
    }

    fn on_speed(&mut self, req: SpeedRequest, now: Instant) {
        let refuse = |s: &mut Self, text: &str| s.notice = notice(Level::Info, text);
        if !self.is_ready() {
            return refuse(self, "Not connected to the pad yet.");
        }
        if self.stale() {
            return refuse(self, "No status from the pad (link stale).");
        }
        if self.link.stop.is_some() {
            return refuse(self, "Stopping…");
        }
        let Some(s) = self.link.status else {
            return refuse(self, "No status from the pad yet.");
        };
        if !s.is_moving() || self.link.start.is_some() {
            return refuse(self, "Start the belt first.");
        }
        if self.stopping_belt(&s) {
            return refuse(self, "Slowing down…");
        }
        if s.mode != ModeState::Known(Mode::Manual) {
            return refuse(self, "Switch to manual to set the speed.");
        }
        let display = self.cfg.display_units;
        let tenths: i64 = match req {
            SpeedRequest::Display(t) => i64::from(t),
            SpeedRequest::Adjust(steps) => {
                let base = self.link.target_speed.map_or(s.speed, |(t, _)| t);
                i64::from(pad_to_display_tenths(base, display)) + i64::from(steps)
            }
        };
        let units = display_tenths_to_pad(tenths.clamp(0, 10_000) as u32, display);
        let (speed, clamp) = Speed::clamped(i64::from(units));
        self.notice = match clamp {
            Clamp::None => None,
            Clamp::CappedAtMax => notice(
                Level::Info,
                format!(
                    "{} is the walking maximum.",
                    self.speed_text(MAX_SPEED_UNITS)
                ),
            ),
            Clamp::RaisedToMin => notice(
                Level::Info,
                format!(
                    "{} is the minimum. Use Stop to stop.",
                    self.speed_text(MIN_SPEED_UNITS)
                ),
            ),
        };
        let current_target = self.link.target_speed.map_or(s.speed, |(t, _)| t);
        if speed.units() == current_target && self.link.queued_speed.is_none() {
            return;
        }
        // Coalesce: a newer request replaces a queued one.
        self.link.queued_speed = Some(speed);
        self.link.target_speed = Some((speed.units(), now));
    }

    fn on_connect(&mut self, id: &str, now: Instant) {
        if !matches!(self.phase, Phase::Searching | Phase::Idle) {
            return;
        }
        let Some(ad) = self.discovered.iter().find(|ad| ad.id == id).cloned() else {
            self.notice = notice(Level::Warning, "That pad isn't visible any more.");
            return;
        };
        self.target = Some(KnownPad::from_ad(&ad));
        self.phase = Phase::Searching;
        self.next_attempt = now;
        self.backoff = self.cfg.timing.backoff_min;
        self.notice = None;
    }

    fn on_disconnect(&mut self) {
        if let Some(link) = self.current_link() {
            self.drop_link(link);
        }
        // A pending connect attempt is abandoned; if it completes, on_done closes it.
        self.phase = Phase::Idle;
        self.link = LinkState::default();
        self.lost_at = None;
        self.notice = None;
    }

    // --- snapshot --------------------------------------------------------

    fn speed_text(&self, units: u8) -> String {
        let display = self.cfg.display_units;
        let v = f64::from(pad_to_display_tenths(units, display)) / 10.0;
        let unit = match display {
            DisplayUnits::Miles => "mph",
            DisplayUnits::Km => "km/h",
        };
        format!("{v:.1} {unit}")
    }

    fn display_speed(&self, units: u8) -> f64 {
        f64::from(pad_to_display_tenths(units, self.cfg.display_units)) / 10.0
    }

    fn snapshot(&self, now: Instant) -> Snapshot {
        let t = &self.cfg.timing;
        let display = self.cfg.display_units;
        let in_window = self.lost_at.is_some_and(|l| now < l + t.reconnect_window);
        let link = match &self.phase {
            Phase::Idle => LinkPhase::Idle,
            Phase::Searching if in_window => LinkPhase::Reconnecting,
            Phase::Searching => LinkPhase::Searching,
            Phase::Connecting { .. } => LinkPhase::Connecting,
            Phase::Handshake { .. } => LinkPhase::Handshake,
            Phase::Ready { .. } => LinkPhase::Ready,
        };
        let pad = match &self.phase {
            Phase::Connecting { pad, .. }
            | Phase::Handshake { pad, .. }
            | Phase::Ready { pad, .. } => Some(pad.clone()),
            _ => None,
        };
        let has_link = self.current_link().is_some();
        let stale = has_link && self.stale();
        let l = &self.link;
        let stats = l.status.map(|s| Stats {
            belt: match s.belt {
                BeltState::Idle => BeltView::Idle,
                BeltState::Running => BeltView::Running,
                BeltState::Paused => BeltView::Paused,
                BeltState::Stopped => BeltView::Stopped,
                BeltState::Fault => BeltView::Fault,
                BeltState::Sleep => BeltView::Standby,
                BeltState::Countdown(_) => BeltView::Countdown,
                BeltState::LockOff(_) => BeltView::LockOff,
                BeltState::Other(_) => BeltView::Other,
            },
            mode: match s.mode {
                ModeState::Known(Mode::Auto) => ModeView::Auto,
                ModeState::Known(Mode::Manual) => ModeView::Manual,
                ModeState::Known(Mode::Standby) => ModeView::Standby,
                ModeState::Newer => ModeView::Newer,
                ModeState::Unknown(_) => ModeView::Unknown,
            },
            speed: self.display_speed(s.speed),
            target_speed: l.target_speed.map(|(u, _)| self.display_speed(u)),
            time_s: s.time_s,
            distance: (distance_to_display(s.distance_10m, display) * 100.0).round() / 100.0,
            steps: s.steps,
            countdown_s: l.countdown_since.map(|since| {
                let left = t
                    .countdown
                    .saturating_sub(now.saturating_duration_since(since));
                left.as_secs() as u32 + u32::from(left.subsec_nanos() > 0)
            }),
            fault: s.fault(),
        });
        let ready = self.is_ready() && !stale;
        let stopped = l
            .status
            .is_some_and(|s| !s.is_active() && (can_start_from(&s) || s.belt == BeltState::Sleep));
        let can_speed = l.status.is_some_and(|s| {
            s.is_moving()
                && !self.stopping_belt(&s)
                && s.mode == ModeState::Known(Mode::Manual)
                && matches!(
                    s.belt,
                    BeltState::Running | BeltState::Idle | BeltState::Countdown(_)
                )
        });
        let fault = l.status.is_some_and(|s| s.fault().is_some());
        let mode_busy = l.queued_mode.is_some()
            || l.last_write
                .is_some_and(|(_, w)| matches!(w, Write::Mode(_)));
        let controls = Controls {
            stop: has_link,
            start: ready
                && l.stop.is_none()
                && l.start.is_none()
                && !mode_busy
                && stopped
                && !fault,
            speed: ready && l.stop.is_none() && l.start.is_none() && can_speed,
            manual: ready && l.status.is_some(),
            auto: ready && l.status.is_some() && stopped && l.stop.is_none(),
            standby: self.is_ready() && l.status.is_some(),
            disconnect: !matches!(self.phase, Phase::Idle),
        };
        Snapshot {
            link,
            has_link,
            stale,
            link_lost: self.lost_at.is_some() && !has_link,
            scanning: self.scanning,
            pad,
            remembered: self.remembered.clone(),
            discovered: self.discovered.clone(),
            stats,
            units: UnitsView {
                display,
                speed_unit: match display {
                    DisplayUnits::Miles => "mph",
                    DisplayUnits::Km => "km/h",
                },
                distance_unit: match display {
                    DisplayUnits::Miles => "mi",
                    DisplayUnits::Km => "km",
                },
                min_speed: self.display_speed(MIN_SPEED_UNITS),
                max_speed: self.display_speed(MAX_SPEED_UNITS),
            },
            controls,
            stopping: l.stop.is_some(),
            stop_unconfirmed: l.stop.as_ref().is_some_and(|s| s.warned)
                || self
                    .notice
                    .as_ref()
                    .is_some_and(|n| n.text.starts_with("Stop not confirmed")),
            slowing: l.status.is_some_and(|s| self.stopping_belt(&s)),
            starting: l.start.is_some(),
            notice: self.notice.clone(),
            problem: self.problem.clone(),
            stop_method: self.cfg.stop_method,
            presets: self
                .cfg
                .presets
                .map(|p| f64::from(p.display_tenths(display)) / 10.0),
            preset_store: self.cfg.presets,
            bad_frames: self.bad_frames,
        }
    }

    fn publish(&self, now: Instant) {
        let snap = self.snapshot(now);
        self.state_tx.send_if_modified(|cur| {
            if *cur == snap {
                false
            } else {
                *cur = snap;
                true
            }
        });
    }
}

/// States the start/pause toggle starts the belt from (with speed 0).
fn can_start_from(s: &Status) -> bool {
    !s.is_active()
        && matches!(
            s.belt,
            BeltState::Idle | BeltState::Paused | BeltState::Stopped
        )
}

enum SpeedRequest {
    Adjust(i32),
    Display(u32),
}

#[cfg(test)]
mod tests;
