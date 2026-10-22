//! A scripted fake pad (design §4.2) and an in-process [`Transport`] over it.
//!
//! [`FakePad`] is a pure model of the pad: it answers status requests,
//! applies speed with a ramp, models the start/pause **toggle** (including
//! restarting a belt that is still slowing down, the qdomyos-zwift hazard),
//! the start countdown and standby. Time only moves when the driver calls
//! [`FakePad::advance`].
//!
//! [`FakeTransport`] drives one or more fake pads on the tokio clock, so
//! tests run on paused time. It can drop the link, stop advertising, fail
//! connects and record every frame written to it.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::protocol::{BeltState, Mode, ModeState, Status, WireCommand, decode_command};
use crate::transport::{
    Advertisement, EventSender, LinkId, Transport, TransportError, TransportEvent,
};

/// How the fake pad behaves. The defaults follow what was measured on the
/// real pad (design §5.1) where known, and ph4/qdomyos-zwift otherwise.
#[derive(Debug, Clone)]
pub struct PadConfig {
    /// Status requests sooner than this after subscribing get no reply.
    pub ready_after: Duration,
    /// The first status request after subscribing gets no reply, however
    /// late (the real pad ignored a request 1–5 ms after subscribing in all
    /// four spike rounds and answered the next one 300 ms later).
    pub drops_first_ask: bool,
    /// Delay between a request and its notification.
    pub reply_latency: Duration,
    /// False makes the pad ignore status requests (a silent link).
    pub answers: bool,
    /// Whether every command, not only *ask status*, gets a status reply.
    pub reply_to_commands: bool,
    /// Whether *speed 0* stops a running belt (supervised test 1 decides).
    pub speed_zero_stops: bool,
    /// Ignore the start/pause toggle (to test an unconfirmed Start).
    pub ignores_start: bool,
    /// Speed after the countdown, pad units.
    pub start_speed: u8,
    pub countdown: Duration,
    /// Ramp rate: time per pad unit of speed change.
    pub ramp_per_unit: Duration,
    /// The pad's own maximum, pad units.
    pub max_speed: u8,
    /// Idle time before the pad drops into standby (state 5).
    pub standby_after_idle: Option<Duration>,
}

impl Default for PadConfig {
    fn default() -> Self {
        Self {
            ready_after: Duration::from_millis(120),
            drops_first_ask: true,
            reply_latency: Duration::from_millis(40),
            answers: true,
            reply_to_commands: false,
            speed_zero_stops: true,
            ignores_start: false,
            start_speed: 20,
            countdown: Duration::from_secs(3),
            ramp_per_unit: Duration::from_millis(100),
            max_speed: 60,
            standby_after_idle: Some(Duration::from_secs(15 * 60)),
        }
    }
}

/// The pad model.
#[derive(Debug, Clone)]
pub struct FakePad {
    pub cfg: PadConfig,
    belt: BeltState,
    mode: u8,
    speed: u8,
    target: u8,
    /// Speed to go back to if a toggle restarts a slowing belt.
    resume_speed: u8,
    /// The state the belt settles in once it has slowed to 0.
    stop_state: BeltState,
    fault_code: u8,
    ramp_acc: Duration,
    countdown_left: Duration,
    idle_for: Duration,
    time_ms: u64,
    /// Speed (pad units) × milliseconds; distance and steps derive from it.
    motion_acc: u64,
    since_subscribe: Option<Duration>,
    /// Status requests seen since subscribing.
    asks_since_subscribe: u32,
    button: u8,
    /// Every well-formed command received, in order.
    pub received: Vec<WireCommand>,
}

impl FakePad {
    /// An awake pad: idle, manual mode, speed 0 (this pad's README frame).
    #[must_use]
    pub fn new(cfg: PadConfig) -> Self {
        Self {
            cfg,
            belt: BeltState::Idle,
            mode: Mode::Manual.byte(),
            speed: 0,
            target: 0,
            resume_speed: 0,
            stop_state: BeltState::Idle,
            fault_code: 0,
            ramp_acc: Duration::ZERO,
            countdown_left: Duration::ZERO,
            idle_for: Duration::ZERO,
            time_ms: 0,
            motion_acc: 0,
            since_subscribe: None,
            asks_since_subscribe: 0,
            button: 0,
            received: Vec::new(),
        }
    }

    #[must_use]
    pub fn status(&self) -> Status {
        Status {
            belt: self.belt,
            speed: self.speed,
            mode: ModeState::from_byte(self.mode),
            time_s: (self.time_ms / 1000) as u32,
            // speed is 0.1 km/h = 100 m/h; × ms → m = acc / 36_000; 10 m units:
            distance_10m: (self.motion_acc / 360_000) as u32,
            // ~1.5 steps/s at 2.0 km/h
            steps: (self.motion_acc / 13_333) as u32,
            byte14: 0,
            fault_code: self.fault_code,
            remote_key: self.button,
            byte17: 0,
        }
    }

    #[must_use]
    pub fn belt(&self) -> BeltState {
        self.belt
    }

    #[must_use]
    pub fn speed(&self) -> u8 {
        self.speed
    }

    #[must_use]
    pub fn target(&self) -> u8 {
        self.target
    }

    #[must_use]
    pub fn mode(&self) -> u8 {
        self.mode
    }

    /// Puts the belt in motion at `speed` (as if started from the remote).
    pub fn set_running(&mut self, speed: u8) {
        self.belt = BeltState::Running;
        self.speed = speed;
        self.target = speed;
        self.resume_speed = speed;
    }

    /// Reports a fault code in byte 15 (and state 4 if `state4`).
    pub fn set_fault(&mut self, code: u8, state4: bool) {
        self.fault_code = code;
        if state4 {
            self.belt = BeltState::Fault;
            self.speed = 0;
            self.target = 0;
        }
    }

    /// Drops the pad into standby (state 5, "sleep"), as after ~15 min idle.
    pub fn enter_standby(&mut self) {
        self.belt = BeltState::Sleep;
        self.mode = Mode::Standby.byte();
        self.speed = 0;
        self.target = 0;
    }

    pub fn set_mode_byte(&mut self, mode: u8) {
        self.mode = mode;
    }

    pub fn on_subscribe(&mut self) {
        self.since_subscribe = Some(Duration::ZERO);
        self.asks_since_subscribe = 0;
    }

    pub fn on_disconnect(&mut self) {
        self.since_subscribe = None;
    }

    fn ready(&self) -> bool {
        self.since_subscribe
            .is_some_and(|t| t >= self.cfg.ready_after)
    }

    /// Handles one frame written to `fe02`; returns the notifications to send.
    pub fn receive(&mut self, frame: &[u8]) -> Vec<Vec<u8>> {
        let Ok(cmd) = decode_command(frame) else {
            return Vec::new(); // garbage is ignored, like the real pad presumably does
        };
        self.received.push(cmd.clone());
        match cmd {
            WireCommand::AskStatus => {}
            WireCommand::SetSpeed(0) => {
                if self.cfg.speed_zero_stops {
                    match self.belt {
                        BeltState::Running => {
                            self.resume_speed = self.target.max(self.speed);
                            self.target = 0;
                            self.stop_state = BeltState::Idle;
                        }
                        BeltState::Countdown(_) => self.belt = BeltState::Idle,
                        _ => {}
                    }
                }
            }
            WireCommand::SetSpeed(n) => {
                if self.belt == BeltState::Running && self.mode == Mode::Manual.byte() {
                    self.target = n.min(self.cfg.max_speed);
                }
            }
            WireCommand::SetMode(m) => match Mode::from_byte(m) {
                Some(Mode::Auto | Mode::Manual) => {
                    self.mode = m;
                    if self.belt == BeltState::Sleep {
                        self.belt = BeltState::Idle;
                    }
                    self.idle_for = Duration::ZERO;
                }
                Some(Mode::Standby) => {
                    self.mode = m;
                    match self.belt {
                        BeltState::Running => {
                            self.resume_speed = self.target.max(self.speed);
                            self.target = 0;
                            self.stop_state = BeltState::Sleep;
                        }
                        BeltState::Idle
                        | BeltState::Paused
                        | BeltState::Stopped
                        | BeltState::Countdown(_) => self.belt = BeltState::Sleep,
                        _ => {}
                    }
                }
                None => {}
            },
            WireCommand::StartPause if self.cfg.ignores_start => {}
            WireCommand::StartPause => match self.belt {
                BeltState::Idle | BeltState::Paused | BeltState::Stopped
                    if self.mode == Mode::Manual.byte() || self.mode == Mode::Auto.byte() =>
                {
                    self.belt = BeltState::Countdown(9);
                    self.countdown_left = self.cfg.countdown;
                    self.time_ms = 0;
                    self.motion_acc = 0;
                }
                BeltState::Running if self.target > 0 => {
                    // pause a running belt
                    self.resume_speed = self.target;
                    self.target = 0;
                    self.stop_state = BeltState::Paused;
                }
                BeltState::Running => {
                    // the hazard: a toggle restarts a belt that is slowing down
                    self.target = self.resume_speed.max(self.cfg.start_speed);
                }
                BeltState::Countdown(_) => self.belt = BeltState::Idle,
                _ => {}
            },
            WireCommand::AskRecord(_)
            | WireCommand::SetPreference { .. }
            | WireCommand::Other(_) => {}
        }
        let is_ask = matches!(cmd, WireCommand::AskStatus);
        if is_ask {
            self.asks_since_subscribe += 1;
            if self.cfg.drops_first_ask && self.asks_since_subscribe == 1 {
                return Vec::new();
            }
        }
        if self.cfg.answers && self.ready() && (is_ask || self.cfg.reply_to_commands) {
            vec![self.status().encode()]
        } else {
            Vec::new()
        }
    }

    /// Advances the model by `dt`.
    pub fn advance(&mut self, dt: Duration) {
        let step = Duration::from_millis(10);
        let mut left = dt;
        while !left.is_zero() {
            let d = left.min(step);
            left -= d;
            self.step(d);
        }
    }

    fn step(&mut self, d: Duration) {
        if let Some(t) = &mut self.since_subscribe {
            *t += d;
        }
        match self.belt {
            BeltState::Countdown(_) => {
                self.countdown_left = self.countdown_left.saturating_sub(d);
                if self.countdown_left.is_zero() {
                    self.belt = BeltState::Running;
                    self.speed = 0;
                    self.target = self.cfg.start_speed;
                    self.resume_speed = self.cfg.start_speed;
                }
            }
            BeltState::Running => {
                self.ramp_acc += d;
                while self.ramp_acc >= self.cfg.ramp_per_unit && self.speed != self.target {
                    self.ramp_acc -= self.cfg.ramp_per_unit;
                    if self.speed < self.target {
                        self.speed += 1;
                    } else {
                        self.speed -= 1;
                    }
                }
                if self.speed == self.target {
                    self.ramp_acc = Duration::ZERO;
                }
                let ms = d.as_millis() as u64;
                self.time_ms += ms;
                self.motion_acc += u64::from(self.speed) * ms;
                if self.speed == 0 && self.target == 0 {
                    self.belt = if self.mode == Mode::Standby.byte() {
                        BeltState::Sleep
                    } else {
                        self.stop_state
                    };
                    self.stop_state = BeltState::Idle;
                    self.idle_for = Duration::ZERO;
                }
            }
            BeltState::Idle | BeltState::Paused | BeltState::Stopped => {
                self.idle_for += d;
                if self
                    .cfg
                    .standby_after_idle
                    .is_some_and(|t| self.idle_for >= t)
                {
                    self.enter_standby();
                }
            }
            BeltState::Sleep | BeltState::Fault | BeltState::LockOff(_) | BeltState::Other(_) => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// One fake device in range.
#[derive(Debug, Clone)]
pub struct FakeDevice {
    pub ad: Advertisement,
    pub advertising: bool,
    /// Whether it advertises again after a disconnect.
    pub readvertise: bool,
    pub pad: FakePad,
    pub link: Option<LinkId>,
}

/// A frame the transport was asked to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteRecord {
    pub at: Instant,
    pub link: LinkId,
    pub frame: Vec<u8>,
    /// The decoded frame, `None` if it didn't decode.
    pub command: Option<WireCommand>,
}

/// Calls the session made, for assertions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    StartScan,
    StopScan,
    Connect(String),
    Subscribe(LinkId),
    Disconnect(LinkId),
}

#[derive(Debug)]
struct State {
    devices: Vec<FakeDevice>,
    next_link: LinkId,
    scan_task: Option<JoinHandle<()>>,
    writes: Vec<WriteRecord>,
    calls: Vec<(Instant, Call)>,
    fail_connects: u32,
    connect_delay: Duration,
    /// How long a connect to a device that isn't advertising pends.
    connect_timeout: Duration,
    incompatible: bool,
    scan_error: Option<TransportError>,
    last_sync: Instant,
}

impl State {
    fn sync(&mut self) {
        let now = Instant::now();
        let dt = now.saturating_duration_since(self.last_sync);
        self.last_sync = now;
        if !dt.is_zero() {
            for d in &mut self.devices {
                d.pad.advance(dt);
            }
        }
    }

    fn linked(&mut self, link: LinkId) -> Option<&mut FakeDevice> {
        self.devices.iter_mut().find(|d| d.link == Some(link))
    }
}

/// An in-process transport over fake pads. Cloning shares the same world.
#[derive(Debug, Clone)]
pub struct FakeTransport {
    state: Arc<Mutex<State>>,
    events: EventSender,
}

impl FakeTransport {
    /// An empty world. Must be created inside a tokio runtime.
    #[must_use]
    pub fn new(events: EventSender) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                devices: Vec::new(),
                next_link: 1,
                scan_task: None,
                writes: Vec::new(),
                calls: Vec::new(),
                fail_connects: 0,
                connect_delay: Duration::from_millis(300),
                connect_timeout: Duration::from_secs(5),
                incompatible: false,
                scan_error: None,
                last_sync: Instant::now(),
            })),
            events,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Adds an advertising pad, named `R1 Pro` with this pad's MAC unless
    /// `id` says otherwise.
    pub fn add_pad(&self, id: &str, mac: Option<&str>, cfg: PadConfig) {
        let mut s = self.lock();
        s.sync();
        s.devices.push(FakeDevice {
            ad: Advertisement {
                id: id.to_string(),
                name: Some("R1 Pro".to_string()),
                mac: mac.map(str::to_string),
                rssi: Some(-55),
            },
            advertising: true,
            readvertise: true,
            pad: FakePad::new(cfg),
            link: None,
        });
    }

    /// Runs `f` on the first device, after bringing the model up to date.
    pub fn with_device<R>(&self, f: impl FnOnce(&mut FakeDevice) -> R) -> R {
        let mut s = self.lock();
        s.sync();
        f(&mut s.devices[0])
    }

    /// Runs `f` on the first pad's model, after bringing it up to date.
    pub fn with_pad<R>(&self, f: impl FnOnce(&mut FakePad) -> R) -> R {
        self.with_device(|d| f(&mut d.pad))
    }

    pub fn set_advertising(&self, advertising: bool) {
        self.with_device(|d| d.advertising = advertising);
    }

    /// The link breaks (pad out of range, Bluetooth off). The pad keeps
    /// whatever it was doing: the belt keeps running.
    pub fn drop_link(&self, readvertise: bool) {
        let link = {
            let mut s = self.lock();
            s.sync();
            let d = &mut s.devices[0];
            let link = d.link.take();
            d.pad.on_disconnect();
            d.advertising = readvertise;
            link
        };
        if let Some(link) = link {
            let _ = self.events.send(TransportEvent::Disconnected { link });
        }
    }

    /// Delivers `data` as a notification on the current link, as if the pad
    /// had sent it.
    pub fn inject(&self, data: Vec<u8>) {
        let link = self.lock().devices.iter().find_map(|d| d.link);
        if let Some(link) = link {
            let _ = self
                .events
                .send(TransportEvent::Notification { link, data });
        }
    }

    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.lock().devices.iter().any(|d| d.link.is_some())
    }

    pub fn fail_next_connects(&self, n: u32) {
        self.lock().fail_connects = n;
    }

    pub fn set_incompatible(&self, incompatible: bool) {
        self.lock().incompatible = incompatible;
    }

    pub fn set_scan_error(&self, error: Option<TransportError>) {
        self.lock().scan_error = error;
    }

    pub fn set_connect_delay(&self, delay: Duration) {
        self.lock().connect_delay = delay;
    }

    #[must_use]
    pub fn writes(&self) -> Vec<WriteRecord> {
        self.lock().writes.clone()
    }

    /// The decoded commands written since `since`.
    #[must_use]
    pub fn commands_since(&self, since: Instant) -> Vec<WireCommand> {
        self.lock()
            .writes
            .iter()
            .filter(|w| w.at >= since)
            .filter_map(|w| w.command.clone())
            .collect()
    }

    #[must_use]
    pub fn calls(&self) -> Vec<(Instant, Call)> {
        self.lock().calls.clone()
    }

    #[must_use]
    pub fn scanning(&self) -> bool {
        self.lock()
            .scan_task
            .as_ref()
            .is_some_and(|t| !t.is_finished())
    }

    fn call(&self, c: Call) {
        self.lock().calls.push((Instant::now(), c));
    }
}

impl Transport for FakeTransport {
    async fn start_scan(&self) -> Result<(), TransportError> {
        self.call(Call::StartScan);
        let mut s = self.lock();
        if let Some(e) = s.scan_error.clone() {
            return Err(e);
        }
        if let Some(t) = s.scan_task.take() {
            t.abort();
        }
        let state = Arc::clone(&self.state);
        let events = self.events.clone();
        s.scan_task = Some(tokio::spawn(async move {
            loop {
                let ads: Vec<Advertisement> = {
                    let s = state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    s.devices
                        .iter()
                        .filter(|d| d.advertising)
                        .map(|d| d.ad.clone())
                        .collect()
                };
                if events.send(TransportEvent::ScanResults(ads)).is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }));
        Ok(())
    }

    async fn stop_scan(&self) -> Result<(), TransportError> {
        self.call(Call::StopScan);
        if let Some(t) = self.lock().scan_task.take() {
            t.abort();
        }
        Ok(())
    }

    async fn connect(&self, id: &str) -> Result<LinkId, TransportError> {
        self.call(Call::Connect(id.to_string()));
        let (delay, timeout, fail, advertising) = {
            let mut s = self.lock();
            s.sync();
            let fail = s.fail_connects > 0;
            if fail {
                s.fail_connects -= 1;
            }
            let advertising = s
                .devices
                .iter()
                .find(|d| d.ad.id == id)
                .map(|d| d.advertising && d.link.is_none());
            (s.connect_delay, s.connect_timeout, fail, advertising)
        };
        match advertising {
            None => return Err(TransportError::NotFound),
            // BLE can only connect to an advertising device: the attempt pends.
            Some(false) => {
                tokio::time::sleep(timeout).await;
                return Err(TransportError::Timeout);
            }
            Some(true) => {}
        }
        tokio::time::sleep(delay).await;
        if fail {
            return Err(TransportError::Other("fake connect failure".into()));
        }
        let mut s = self.lock();
        s.sync();
        let link = s.next_link;
        s.next_link += 1;
        let incompatible = s.incompatible;
        let d = s
            .devices
            .iter_mut()
            .find(|d| d.ad.id == id)
            .ok_or(TransportError::NotFound)?;
        if !d.advertising || d.link.is_some() {
            return Err(TransportError::Timeout);
        }
        if incompatible {
            return Err(TransportError::Incompatible(
                "fe02 has no write-without-response".into(),
            ));
        }
        d.link = Some(link);
        d.advertising = false;
        Ok(link)
    }

    async fn subscribe(&self, link: LinkId) -> Result<(), TransportError> {
        self.call(Call::Subscribe(link));
        let mut s = self.lock();
        s.sync();
        let d = s.linked(link).ok_or(TransportError::NotConnected)?;
        d.pad.on_subscribe();
        Ok(())
    }

    async fn write(&self, link: LinkId, frame: &[u8]) -> Result<(), TransportError> {
        let (replies, latency) = {
            let mut s = self.lock();
            s.sync();
            s.writes.push(WriteRecord {
                at: Instant::now(),
                link,
                frame: frame.to_vec(),
                command: decode_command(frame).ok(),
            });
            let d = s.linked(link).ok_or(TransportError::NotConnected)?;
            (d.pad.receive(frame), d.pad.cfg.reply_latency)
        };
        for reply in replies {
            let state = Arc::clone(&self.state);
            let events = self.events.clone();
            tokio::spawn(async move {
                tokio::time::sleep(latency).await;
                let still_linked = {
                    let s = state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    s.devices.iter().any(|d| d.link == Some(link))
                };
                if still_linked {
                    let _ = events.send(TransportEvent::Notification { link, data: reply });
                }
            });
        }
        Ok(())
    }

    async fn disconnect(&self, link: LinkId) -> Result<(), TransportError> {
        self.call(Call::Disconnect(link));
        {
            let mut s = self.lock();
            s.sync();
            let Some(d) = s.linked(link) else {
                return Err(TransportError::NotConnected);
            };
            d.link = None;
            d.pad.on_disconnect();
            d.advertising = d.readvertise;
        }
        let _ = self.events.send(TransportEvent::Disconnected { link });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Command, Reply, Speed, decode_reply};

    fn status(pad: &mut FakePad) -> Status {
        let replies = pad.receive(&Command::AskStatus.encode());
        match decode_reply(&replies[0]).unwrap() {
            Reply::Status(s) => s,
            Reply::Record(_) => unreachable!(),
        }
    }

    fn ready_pad() -> FakePad {
        let mut pad = FakePad::new(PadConfig::default());
        pad.on_subscribe();
        pad.advance(Duration::from_secs(1));
        // the first request after subscribing goes unanswered
        assert!(pad.receive(&Command::AskStatus.encode()).is_empty());
        pad
    }

    #[test]
    fn first_request_after_subscribe_is_dropped() {
        let mut pad = FakePad::new(PadConfig::default());
        pad.on_subscribe();
        assert!(pad.receive(&Command::AskStatus.encode()).is_empty());
        pad.advance(Duration::from_millis(300));
        assert_eq!(pad.receive(&Command::AskStatus.encode()).len(), 1);
        // even a late first request is dropped
        pad.on_subscribe();
        pad.advance(Duration::from_secs(2));
        assert!(pad.receive(&Command::AskStatus.encode()).is_empty());
        assert_eq!(pad.receive(&Command::AskStatus.encode()).len(), 1);
    }

    #[test]
    fn not_ready_right_after_subscribe() {
        let cfg = PadConfig {
            drops_first_ask: false,
            ..PadConfig::default()
        };
        let mut pad = FakePad::new(cfg);
        pad.on_subscribe();
        assert!(pad.receive(&Command::AskStatus.encode()).is_empty());
        pad.advance(Duration::from_millis(150));
        assert_eq!(pad.receive(&Command::AskStatus.encode()).len(), 1);
    }

    #[test]
    fn start_counts_down_then_runs_and_ramps() {
        let mut pad = ready_pad();
        pad.receive(&Command::StartPause.encode());
        assert_eq!(status(&mut pad).belt, BeltState::Countdown(9));
        pad.advance(Duration::from_secs(3));
        assert_eq!(status(&mut pad).belt, BeltState::Running);
        pad.advance(Duration::from_secs(3));
        assert_eq!(status(&mut pad).speed, 20);
        let s = Speed::clamped(30).0;
        pad.receive(&Command::SetSpeed(s).encode());
        pad.advance(Duration::from_millis(500));
        assert_eq!(status(&mut pad).speed, 25);
    }

    #[test]
    fn toggle_pauses_a_running_belt() {
        let mut pad = ready_pad();
        pad.set_running(20);
        pad.receive(&Command::StartPause.encode());
        pad.advance(Duration::from_secs(3));
        let s = status(&mut pad);
        assert_eq!((s.belt, s.speed), (BeltState::Paused, 0));
        // and resumes it
        pad.receive(&Command::StartPause.encode());
        assert_eq!(status(&mut pad).belt, BeltState::Countdown(9));
    }

    #[test]
    fn speed_zero_stops_and_toggle_restarts_a_slowing_belt() {
        let mut pad = ready_pad();
        pad.set_running(30);
        pad.receive(&Command::SpeedZero.encode());
        pad.advance(Duration::from_millis(500));
        let s = status(&mut pad);
        assert_eq!((s.belt, s.speed), (BeltState::Running, 25));
        // The hazard the session must never trigger:
        pad.receive(&Command::StartPause.encode());
        pad.advance(Duration::from_secs(1));
        assert!(status(&mut pad).speed > 25, "toggle restarted the belt");
    }

    #[test]
    fn standby_while_running_decelerates_into_state_5() {
        let mut pad = ready_pad();
        pad.set_running(20);
        pad.receive(&Command::SetMode(Mode::Standby).encode());
        pad.advance(Duration::from_secs(3));
        let s = status(&mut pad);
        assert_eq!((s.belt, s.speed), (BeltState::Sleep, 0));
        // manual wakes it
        pad.receive(&Command::SetMode(Mode::Manual).encode());
        assert_eq!(status(&mut pad).belt, BeltState::Idle);
    }

    #[test]
    fn idle_pad_drops_into_standby_and_ignores_start() {
        let mut pad = ready_pad();
        pad.advance(Duration::from_secs(15 * 60));
        assert_eq!(status(&mut pad).belt, BeltState::Sleep);
        pad.receive(&Command::StartPause.encode());
        pad.advance(Duration::from_secs(4));
        assert_eq!(status(&mut pad).belt, BeltState::Sleep);
    }
}
