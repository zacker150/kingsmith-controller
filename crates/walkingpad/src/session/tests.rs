//! Session behaviour against the scripted fake pad (design §4.2), on paused
//! tokio time so every timing is exact.

use std::time::Duration;

use tokio::time::Instant;

use super::*;
use crate::fake::{Call, FakeTransport, PadConfig, WriteRecord};
use crate::protocol::{Status, WireCommand};
use crate::transport::event_channel;

const ID: &str = "F076ECBD-4FC5-B48F-A341-D7101DBCFBB8";
const MAC: &str = "57:4C:4D:0F:62:75";

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn remembered() -> KnownPad {
    KnownPad {
        id: ID.into(),
        mac: Some(MAC.into()),
        name: Some("R1 Pro".into()),
    }
}

fn config() -> Config {
    Config {
        remembered: Some(remembered()),
        ..Config::default()
    }
}

fn config_with(stop_method: StopMethod) -> Config {
    Config {
        stop_method,
        ..config()
    }
}

struct H {
    fake: FakeTransport,
    s: SessionHandle,
}

impl H {
    fn new(cfg: Config, pad: PadConfig) -> Self {
        let (tx, rx) = event_channel();
        let fake = FakeTransport::new(tx);
        fake.add_pad(ID, Some(MAC), pad);
        let s = spawn(fake.clone(), rx, cfg);
        Self { fake, s }
    }

    /// A session connected to a ready pad (idle, manual).
    async fn ready(cfg: Config, pad: PadConfig) -> Self {
        let h = Self::new(cfg, pad);
        h.wait("ready", 10_000, |s| {
            s.link == LinkPhase::Ready && s.stats.is_some()
        })
        .await;
        h
    }

    fn send(&self, i: Intent) {
        assert!(self.s.send(i));
    }

    async fn wait(
        &self,
        what: &str,
        timeout_ms: u64,
        pred: impl Fn(&Snapshot) -> bool,
    ) -> Snapshot {
        let mut rx = self.s.watch();
        let deadline = Instant::now() + ms(timeout_ms);
        loop {
            let snap = rx.borrow_and_update().clone();
            if pred(&snap) {
                return snap;
            }
            match tokio::time::timeout_at(deadline, rx.changed()).await {
                Ok(Ok(())) => {}
                _ => panic!(
                    "timed out waiting for {what}; last snapshot: {:#?}",
                    self.s.snapshot()
                ),
            }
        }
    }

    /// Waits until a status poll was just written, so the next slot is a
    /// full 700 ms away.
    async fn just_after_poll(&self) {
        let n = self.fake.writes().len();
        loop {
            tokio::time::sleep(ms(5)).await;
            let w = self.fake.writes();
            if w.len() > n && w.last().unwrap().command == Some(WireCommand::AskStatus) {
                return;
            }
        }
    }

    fn writes_since(&self, t: Instant) -> Vec<WriteRecord> {
        self.fake
            .writes()
            .into_iter()
            .filter(|w| w.at >= t)
            .collect()
    }

    fn cmds_since(&self, t: Instant) -> Vec<WireCommand> {
        self.fake.commands_since(t)
    }

    fn count_since(&self, t: Instant, c: &WireCommand) -> usize {
        self.cmds_since(t).iter().filter(|x| *x == c).count()
    }

    fn status(&self) -> Status {
        self.fake.with_pad(|p| p.status())
    }
}

async fn sleep(n: u64) {
    tokio::time::sleep(ms(n)).await;
}

/// Commands that can start the belt or make it faster.
fn is_motion(c: &WireCommand) -> bool {
    matches!(c, WireCommand::StartPause | WireCommand::SetMode(0))
        || matches!(c, WireCommand::SetSpeed(n) if *n > 0)
}

fn assert_spacing(writes: &[WriteRecord], min: Duration) {
    for pair in writes.windows(2) {
        let gap = pair[1].at - pair[0].at;
        assert!(
            gap >= min,
            "writes {:?} and {:?} only {gap:?} apart",
            pair[0].command,
            pair[1].command
        );
    }
}

// --- handshake ---------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn handshake_retries_every_300ms_until_the_pad_answers() {
    // The README says the pad ignores requests for ~1 s after subscribing.
    let pad = PadConfig {
        ready_after: ms(1000),
        ..PadConfig::default()
    };
    let h = H::ready(config(), pad).await;
    let ready_at = Instant::now();
    let asks: Vec<_> = h
        .fake
        .writes()
        .into_iter()
        .filter(|w| w.at < ready_at)
        .collect();
    assert!(
        asks.iter()
            .all(|w| w.command == Some(WireCommand::AskStatus)),
        "{asks:?}"
    );
    // 0, 300, 600, 900 ms unanswered; 1200 ms answered.
    assert_eq!(asks.len(), 5, "{asks:?}");
    for pair in asks.windows(2) {
        assert_eq!(pair[1].at - pair[0].at, ms(300));
    }
}

#[tokio::test(start_paused = true)]
async fn handshake_survives_the_first_request_being_dropped() {
    // What the spike saw on the real pad: the request sent right after
    // subscribing gets no reply; the next one, 300 ms later, does.
    let pad = PadConfig {
        ready_after: ms(0),
        drops_first_ask: true,
        ..PadConfig::default()
    };
    let h = H::ready(config(), pad).await;
    let ready_at = Instant::now();
    let asks: Vec<_> = h
        .fake
        .writes()
        .into_iter()
        .filter(|w| w.at < ready_at)
        .collect();
    assert_eq!(asks.len(), 2, "{asks:?}");
    assert_eq!(asks[1].at - asks[0].at, ms(300));
    let subscribed = h
        .fake
        .calls()
        .into_iter()
        .find_map(|(at, c)| matches!(c, Call::Subscribe(_)).then_some(at))
        .unwrap();
    assert!(
        asks[0].at - subscribed < ms(10),
        "first request goes right after subscribing"
    );
    assert!(ready_at - asks[1].at <= ms(150));
}

#[tokio::test(start_paused = true)]
async fn handshake_gives_up_after_3s_of_silence() {
    let pad = PadConfig {
        answers: false,
        ..PadConfig::default()
    };
    let h = H::new(config(), pad);
    h.wait("handshake", 5_000, |s| s.link == LinkPhase::Handshake)
        .await;
    let started = Instant::now();
    let snap = h
        .wait("handshake failure", 5_000, |s| {
            s.notice
                .as_ref()
                .is_some_and(|n| n.text.contains("didn't answer"))
        })
        .await;
    assert!(!snap.has_link);
    assert!(Instant::now() - started >= ms(3000));
    assert!(Instant::now() - started < ms(3100));
    assert!(
        h.fake
            .calls()
            .iter()
            .any(|(_, c)| matches!(c, Call::Disconnect(_)))
    );
    // Only status requests, ~every 300 ms.
    let asks = h.writes_since(started);
    assert!(
        asks.iter()
            .all(|w| w.command == Some(WireCommand::AskStatus))
    );
    assert!((9..=11).contains(&asks.len()), "{}", asks.len());
}

#[tokio::test(start_paused = true)]
async fn polls_every_750ms_when_idle() {
    let h = H::ready(config(), PadConfig::default()).await;
    let t = Instant::now();
    sleep(7_500).await;
    let w = h.writes_since(t);
    assert!(w.iter().all(|w| w.command == Some(WireCommand::AskStatus)));
    assert!((9..=11).contains(&w.len()), "{}", w.len());
    for pair in w.windows(2) {
        assert_eq!(pair[1].at - pair[0].at, ms(750));
    }
}

// --- launch, remembering --------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn launch_connects_to_the_remembered_pad_and_never_moves_the_belt() {
    let h = H::new(config(), PadConfig::default());
    // The belt is already running (started from the remote).
    h.fake.with_pad(|p| p.set_running(25));
    h.wait("ready", 5_000, |s| {
        s.link == LinkPhase::Ready && s.stats.is_some()
    })
    .await;
    sleep(30_000).await;
    let cmds = h.cmds_since(Instant::now() - ms(60_000));
    assert!(
        cmds.iter().all(|c| *c == WireCommand::AskStatus),
        "{cmds:?}"
    );
    assert_eq!(h.status().speed, 25);
}

#[tokio::test(start_paused = true)]
async fn first_run_lists_pads_and_connects_only_when_asked() {
    let cfg = Config {
        remembered: None,
        ..Config::default()
    };
    let h = H::new(cfg, PadConfig::default());
    let snap = h
        .wait("scan results", 2_000, |s| !s.discovered.is_empty())
        .await;
    assert_eq!(snap.link, LinkPhase::Searching);
    assert_eq!(snap.discovered[0].mac.as_deref(), Some(MAC));
    sleep(5_000).await;
    assert!(
        !h.fake
            .calls()
            .iter()
            .any(|(_, c)| matches!(c, Call::Connect(_)))
    );
    h.send(Intent::Connect { id: ID.into() });
    let snap = h.wait("ready", 5_000, |s| s.link == LinkPhase::Ready).await;
    assert_eq!(snap.remembered, Some(remembered()));
}

#[tokio::test(start_paused = true)]
async fn remembered_pad_matches_by_mac_across_platform_ids() {
    // Remembered on Android (ID = MAC); seen on the Mac under a CoreBluetooth UUID.
    let android = KnownPad {
        id: MAC.into(),
        mac: Some(MAC.into()),
        name: None,
    };
    let cfg = Config {
        remembered: Some(android),
        ..Config::default()
    };
    let h = H::new(cfg, PadConfig::default());
    let snap = h.wait("ready", 5_000, |s| s.link == LinkPhase::Ready).await;
    assert_eq!(snap.remembered.unwrap().id, ID);
}

#[test]
fn the_pad_is_remembered_by_mac_first() {
    let pad = remembered();
    let ad = |id: &str, mac: Option<&str>| Advertisement {
        id: id.into(),
        name: Some("R1 Pro".into()),
        mac: mac.map(str::to_string),
        rssi: None,
    };
    assert!(pad.matches(&ad("other-id", Some(MAC))));
    assert!(pad.matches(&ad(MAC, Some("57:4c:4d:0f:62:75"))));
    // Same platform ID but a different MAC: a different pad.
    assert!(!pad.matches(&ad(ID, Some("57:4C:4E:11:79:0C"))));
    // No MAC in the advertisement: fall back to the platform ID.
    assert!(pad.matches(&ad(ID, None)));
    assert!(!pad.matches(&ad("other-id", None)));
}

#[tokio::test(start_paused = true)]
async fn scanning_stops_in_the_background() {
    let cfg = Config {
        remembered: None,
        ..Config::default()
    };
    let h = H::new(cfg, PadConfig::default());
    h.wait("scanning", 1_000, |s| s.scanning).await;
    h.send(Intent::SetVisible { visible: false });
    h.wait("not scanning", 1_000, |s| !s.scanning).await;
    assert!(!h.fake.scanning());
    h.send(Intent::SetVisible { visible: true });
    h.wait("scanning again", 1_000, |s| s.scanning).await;
}

#[tokio::test(start_paused = true)]
async fn permission_denied_waits_for_the_user() {
    let h = H::new(config(), PadConfig::default());
    h.fake
        .set_scan_error(Some(TransportError::PermissionDenied));
    h.send(Intent::Disconnect);
    sleep(100).await;
    h.send(Intent::Search);
    sleep(500).await;
    let snap = h.s.snapshot();
    assert_eq!(snap.problem.unwrap().kind, ProblemKind::PermissionDenied);
    assert!(!snap.scanning);
    let calls = h.fake.calls().len();
    sleep(30_000).await;
    assert_eq!(h.fake.calls().len(), calls, "no automatic re-prompt");
    h.fake.set_scan_error(None);
    h.send(Intent::Search);
    h.wait("ready", 5_000, |s| s.link == LinkPhase::Ready).await;
}

// --- write spacing and coalescing ----------------------------------------------

#[tokio::test(start_paused = true)]
async fn writes_are_at_least_700ms_apart_and_speed_requests_coalesce() {
    let h = H::ready(config(), PadConfig::default()).await;
    h.fake.with_pad(|p| p.set_running(20));
    sleep(1_000).await;
    let t = Instant::now();
    // Hold ↑ for 3 s: a key repeat every 50 ms.
    for _ in 0..60 {
        h.send(Intent::AdjustSpeed { steps: 1 });
        sleep(50).await;
    }
    sleep(5_000).await;
    let w = h.writes_since(t);
    assert_spacing(&w, ms(700));
    let speeds: Vec<u8> = w
        .iter()
        .filter_map(|w| match w.command {
            Some(WireCommand::SetSpeed(n)) => Some(n),
            _ => None,
        })
        .collect();
    // 60 presses, a handful of writes, every one followed by a poll.
    assert!(!speeds.is_empty() && speeds.len() <= 4, "{speeds:?}");
    assert!(speeds.windows(2).all(|p| p[1] > p[0]), "{speeds:?}");
    // 1.2 mph + 6.0 mph worth of steps, capped at the 6.0 km/h walking max.
    assert_eq!(*speeds.last().unwrap(), 60);
    for pair in w.windows(2) {
        if pair[0].command != Some(WireCommand::AskStatus) {
            assert_eq!(pair[1].command, Some(WireCommand::AskStatus));
        }
    }
}

#[tokio::test(start_paused = true)]
async fn speed_steps_are_0_1_mph_from_the_target() {
    let h = H::ready(config(), PadConfig::default()).await;
    h.fake.with_pad(|p| p.set_running(32)); // 2.0 mph
    sleep(1_000).await;
    h.just_after_poll().await;
    let t = Instant::now();
    // Five quick steps collapse into one write of 2.5 mph = 40.2 → 40 units.
    for _ in 0..5 {
        h.send(Intent::AdjustSpeed { steps: 1 });
    }
    sleep(2_000).await;
    assert_eq!(h.count_since(t, &WireCommand::SetSpeed(40)), 1);
    assert_eq!(
        h.cmds_since(t)
            .iter()
            .filter(|c| matches!(c, WireCommand::SetSpeed(_)))
            .count(),
        1
    );
    let snap = h.s.snapshot();
    assert_eq!(snap.units.speed_unit, "mph");
    // Shift+↓ from the target (2.5 mph, still ramping): 2.0 mph = 32 units.
    let t = Instant::now();
    h.send(Intent::AdjustSpeed { steps: -5 });
    sleep(2_000).await;
    let speeds: Vec<_> = h
        .cmds_since(t)
        .into_iter()
        .filter(|c| matches!(c, WireCommand::SetSpeed(_)))
        .collect();
    assert_eq!(speeds, vec![WireCommand::SetSpeed(32)]);
}

// --- clamping -----------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn presets_and_steps_never_exceed_6_kmh() {
    let h = H::ready(config(), PadConfig::default()).await;
    h.fake.with_pad(|p| p.set_running(30));
    sleep(1_000).await;
    let t = Instant::now();
    h.send(Intent::SetPreset {
        index: 2,
        tenths: 50,
    }); // 5.0 mph preset
    h.send(Intent::ApplyPreset { index: 2 });
    let snap = h.wait("cap notice", 1_000, |s| s.notice.is_some()).await;
    assert!(snap.notice.unwrap().text.contains("walking maximum"));
    assert!((snap.units.max_speed - 3.7).abs() < 1e-9);
    assert!((snap.units.min_speed - 0.3).abs() < 1e-9);
    sleep(2_000).await;
    h.send(Intent::AdjustSpeed { steps: 30 });
    sleep(3_000).await;
    let max = h
        .cmds_since(t)
        .iter()
        .filter_map(|c| match c {
            WireCommand::SetSpeed(n) => Some(*n),
            _ => None,
        })
        .max();
    assert_eq!(max, Some(60));
}

#[tokio::test(start_paused = true)]
async fn km_units_clamp_to_the_walking_range_and_never_send_zero() {
    let h = H::ready(config(), PadConfig::default()).await;
    h.fake.with_pad(|p| p.set_running(20));
    sleep(1_000).await;
    let t = Instant::now();
    h.send(Intent::SetSpeed { tenths: 50 }); // 5.0 mph
    sleep(2_000).await;
    h.send(Intent::AdjustSpeed { steps: -100 });
    sleep(2_000).await;
    let speeds: Vec<u8> = h
        .cmds_since(t)
        .iter()
        .filter_map(|c| match c {
            WireCommand::SetSpeed(n) => Some(*n),
            _ => None,
        })
        .collect();
    assert_eq!(speeds, vec![60, 5]);
}

#[tokio::test(start_paused = true)]
async fn speed_needs_a_running_belt_in_manual() {
    let h = H::ready(config(), PadConfig::default()).await;
    let t = Instant::now();
    h.send(Intent::SetSpeed { tenths: 20 });
    sleep(2_000).await;
    h.fake.with_pad(|p| {
        p.set_running(20);
        p.set_mode_byte(0);
    });
    sleep(1_000).await;
    h.send(Intent::AdjustSpeed { steps: 1 });
    sleep(2_000).await;
    assert!(h.cmds_since(t).iter().all(|c| *c == WireCommand::AskStatus));
    assert!(!h.s.snapshot().controls.speed);
}

// --- stop -----------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn stop_sends_speed_zero_within_one_slot_and_is_confirmed() {
    let h = H::ready(config_with(StopMethod::SpeedZero), PadConfig::default()).await;
    h.fake.with_pad(|p| p.set_running(30));
    sleep(2_000).await;
    h.just_after_poll().await;
    let pressed = Instant::now();
    h.send(Intent::Stop);
    h.wait("stopping", 100, |s| s.stopping).await;
    h.wait("stop confirmed", 3_000, |s| !s.stopping).await;
    let w = h.writes_since(pressed);
    let stop = w
        .iter()
        .find(|w| w.command == Some(WireCommand::SetSpeed(0)))
        .unwrap();
    assert!(stop.at - pressed <= ms(700));
    sleep(20_000).await;
    assert_eq!(h.count_since(pressed, &WireCommand::SetSpeed(0)), 1);
    assert_eq!(h.count_since(pressed, &WireCommand::StartPause), 0);
    let st = h.status();
    assert_eq!((st.belt, st.speed), (BeltState::Idle, 0));
}

#[tokio::test(start_paused = true)]
async fn stop_never_toggles_when_the_belt_is_stopped() {
    let h = H::ready(config_with(StopMethod::SpeedZero), PadConfig::default()).await;
    let t = Instant::now();
    for _ in 0..3 {
        h.send(Intent::Stop);
        sleep(2_000).await;
    }
    assert_eq!(h.count_since(t, &WireCommand::StartPause), 0);
    assert!(h.count_since(t, &WireCommand::SetSpeed(0)) >= 1);
    assert_eq!(h.status().belt, BeltState::Idle);
    assert!(!h.s.snapshot().stopping);
}

#[tokio::test(start_paused = true)]
async fn standby_stop_never_toggles_whatever_the_belt_does() {
    let h = H::ready(config(), PadConfig::default()).await;
    let t = Instant::now();
    h.send(Intent::Stop); // idle
    sleep(2_000).await;
    h.send(Intent::SetMode { mode: Mode::Manual });
    sleep(2_000).await;
    h.fake.with_pad(|p| {
        p.set_running(40);
        p.receive(&Command::SpeedZero.encode()); // slowing
    });
    for _ in 0..6 {
        h.send(Intent::Stop);
        sleep(500).await;
    }
    sleep(5_000).await;
    assert_eq!(h.count_since(t, &WireCommand::StartPause), 0);
    assert!(h.count_since(t, &WireCommand::SetMode(2)) >= 2);
    assert_eq!(h.status().speed, 0);
}

#[tokio::test(start_paused = true)]
async fn stop_never_toggles_when_the_belt_is_slowing() {
    let h = H::ready(config(), PadConfig::default()).await;
    h.fake.with_pad(|p| {
        p.set_running(40);
        // already slowing down (stopped from the remote)
        p.receive(&Command::SpeedZero.encode());
    });
    sleep(300).await;
    let t = Instant::now();
    // Mash Stop while it slows down.
    for _ in 0..10 {
        h.send(Intent::Stop);
        sleep(400).await;
    }
    sleep(5_000).await;
    assert_eq!(h.count_since(t, &WireCommand::StartPause), 0);
    let st = h.status();
    assert_eq!(
        (st.belt, st.speed),
        (BeltState::Sleep, 0),
        "stopped in standby, not restarted"
    );
}

// --- stop with the guarded toggle -----------------------------------------------------

#[tokio::test(start_paused = true)]
async fn toggle_stop_is_sent_once_and_further_presses_send_nothing_while_slowing() {
    let h = H::ready(config_with(StopMethod::ToggleGuarded), PadConfig::default()).await;
    h.fake.with_pad(|p| p.set_running(40));
    sleep(2_000).await;
    let t = Instant::now();
    h.send(Intent::Stop);
    h.wait("slowing", 3_000, |s| s.slowing).await;
    // Mash Stop while it slows down: nothing more is sent.
    for _ in 0..8 {
        h.send(Intent::Stop);
        sleep(300).await;
    }
    let snap = h
        .wait("stopped", 10_000, |s| {
            s.stats.as_ref().is_some_and(|st| st.speed == 0.0) && !s.slowing
        })
        .await;
    assert!(!snap.stopping);
    sleep(5_000).await;
    assert_eq!(h.count_since(t, &WireCommand::StartPause), 1);
    let st = h.status();
    assert_eq!((st.belt, st.speed), (BeltState::Paused, 0));
}

#[tokio::test(start_paused = true)]
async fn toggle_stop_sends_nothing_to_a_stopped_or_slowing_belt() {
    let h = H::ready(config_with(StopMethod::ToggleGuarded), PadConfig::default()).await;
    let t = Instant::now();
    h.send(Intent::Stop); // idle belt
    sleep(2_000).await;
    h.fake.with_pad(|p| {
        p.set_running(40);
        p.receive(&Command::SpeedZero.encode()); // stopped from the remote, slowing
    });
    sleep(1_600).await;
    for _ in 0..10 {
        h.send(Intent::Stop);
        sleep(400).await;
    }
    sleep(5_000).await;
    assert_eq!(h.count_since(t, &WireCommand::StartPause), 0);
    let st = h.status();
    assert_eq!(
        (st.belt, st.speed),
        (BeltState::Idle, 0),
        "the belt did not restart"
    );
}

#[tokio::test(start_paused = true)]
async fn toggle_stop_is_never_retried_automatically() {
    // The pad ignores the toggle (lost frame).
    let pad = PadConfig {
        ignores_start: true,
        ..PadConfig::default()
    };
    let h = H::ready(config_with(StopMethod::ToggleGuarded), pad).await;
    h.fake.with_pad(|p| p.set_running(30));
    sleep(2_000).await;
    let t = Instant::now();
    h.send(Intent::Stop);
    let snap = h.wait("warning", 6_000, |s| s.stop_unconfirmed).await;
    assert!(!snap.stopping);
    sleep(20_000).await;
    assert_eq!(h.count_since(t, &WireCommand::StartPause), 1);
    // A new press may send one more.
    h.send(Intent::Stop);
    sleep(3_000).await;
    assert_eq!(h.count_since(t, &WireCommand::StartPause), 2);
}

#[tokio::test(start_paused = true)]
async fn toggle_stop_uses_standby_during_the_countdown() {
    let h = H::ready(config_with(StopMethod::ToggleGuarded), PadConfig::default()).await;
    h.send(Intent::Start);
    h.wait("countdown", 3_000, |s| {
        s.stats
            .as_ref()
            .is_some_and(|st| st.belt == BeltView::Countdown)
    })
    .await;
    let t = Instant::now();
    h.send(Intent::Stop);
    sleep(5_000).await;
    assert_eq!(h.count_since(t, &WireCommand::StartPause), 0);
    assert!(h.cmds_since(t).contains(&WireCommand::SetMode(2)));
    assert_eq!(h.status().speed, 0);
}

#[tokio::test(start_paused = true)]
async fn stop_defaults_to_standby() {
    let h = H::ready(config(), PadConfig::default()).await;
    assert_eq!(h.s.snapshot().stop_method, StopMethod::Standby);
    h.fake.with_pad(|p| p.set_running(30));
    sleep(1_000).await;
    let t = Instant::now();
    h.send(Intent::Stop);
    h.wait("stopping", 100, |s| s.stopping).await;
    h.wait("confirmed", 3_000, |s| !s.stopping).await;
    let cmds = h.cmds_since(t);
    assert!(cmds.contains(&WireCommand::SetMode(2)));
    assert!(!cmds.contains(&WireCommand::SetSpeed(0)));
    assert!(!cmds.contains(&WireCommand::StartPause));
}

#[tokio::test(start_paused = true)]
async fn unconfirmed_stop_is_resent_and_warns_after_5s_but_never_toggles() {
    // A pad on which speed 0 does nothing (what supervised test 1 might find).
    let pad = PadConfig {
        speed_zero_stops: false,
        ..PadConfig::default()
    };
    let h = H::ready(config_with(StopMethod::SpeedZero), pad).await;
    h.fake.with_pad(|p| p.set_running(30));
    sleep(1_000).await;
    let t = Instant::now();
    h.send(Intent::Stop);
    sleep(4_900).await;
    assert!(!h.s.snapshot().stop_unconfirmed);
    let snap = h.wait("warning", 200, |s| s.stop_unconfirmed).await;
    let n = snap.notice.unwrap();
    assert_eq!(n.level, Level::Error);
    assert!(n.text.starts_with("Stop not confirmed"));
    assert!(snap.controls.stop);
    sleep(5_000).await;
    // Re-sent every other slot (Stop, status, Stop, …), ~1.45 s apart.
    let stops = h.count_since(t, &WireCommand::SetSpeed(0));
    assert!((6..=8).contains(&stops), "{stops}");
    assert_spacing(&h.writes_since(t), ms(700));
    assert_eq!(h.count_since(t, &WireCommand::StartPause), 0);
}

#[tokio::test(start_paused = true)]
async fn stop_preempts_and_drops_the_queue() {
    let h = H::ready(config_with(StopMethod::SpeedZero), PadConfig::default()).await;
    h.fake.with_pad(|p| p.set_running(20));
    sleep(1_000).await;
    h.just_after_poll().await;
    let t = Instant::now();
    h.send(Intent::AdjustSpeed { steps: 5 });
    h.send(Intent::SetMode {
        mode: Mode::Standby,
    });
    h.send(Intent::Stop);
    sleep(10_000).await;
    let cmds = h.cmds_since(t);
    assert_eq!(cmds[0], WireCommand::SetSpeed(0), "{cmds:?}");
    assert!(!cmds.iter().any(is_motion), "{cmds:?}");
    assert!(
        !cmds.contains(&WireCommand::SetMode(2)),
        "queued mode dropped: {cmds:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn stop_cancels_a_pending_start() {
    let h = H::ready(config(), PadConfig::default()).await;
    h.just_after_poll().await;
    let t = Instant::now();
    h.send(Intent::Start);
    h.send(Intent::Stop);
    sleep(10_000).await;
    assert_eq!(h.count_since(t, &WireCommand::StartPause), 0);
    assert!(!h.status().is_active(), "{:?}", h.status());
}

#[tokio::test(start_paused = true)]
async fn stop_works_during_the_handshake() {
    let pad = PadConfig {
        ready_after: ms(2_000),
        ..PadConfig::default()
    };
    let h = H::new(config_with(StopMethod::SpeedZero), pad);
    h.fake.with_pad(|p| p.set_running(30));
    h.wait("handshake", 5_000, |s| s.link == LinkPhase::Handshake)
        .await;
    let snap = h.s.snapshot();
    assert!(snap.controls.stop, "Stop is enabled whenever a link exists");
    let t = Instant::now();
    h.send(Intent::Stop);
    h.wait("confirmed", 5_000, |s| {
        s.link == LinkPhase::Ready && !s.stopping
    })
    .await;
    assert!(h.count_since(t, &WireCommand::SetSpeed(0)) >= 1);
    assert_eq!(h.count_since(t, &WireCommand::StartPause), 0);
}

// --- start ----------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn start_is_sent_once_and_shows_the_countdown() {
    let h = H::ready(config(), PadConfig::default()).await;
    let t = Instant::now();
    h.send(Intent::Start);
    h.send(Intent::Start);
    h.send(Intent::Start);
    let snap = h
        .wait("countdown", 3_000, |s| {
            s.stats
                .as_ref()
                .is_some_and(|st| st.belt == BeltView::Countdown)
        })
        .await;
    assert!(
        snap.stats
            .unwrap()
            .countdown_s
            .is_some_and(|c| (1..=3).contains(&c))
    );
    h.wait("running", 5_000, |s| {
        s.stats
            .as_ref()
            .is_some_and(|st| st.belt == BeltView::Running)
    })
    .await;
    sleep(20_000).await;
    assert_eq!(h.count_since(t, &WireCommand::StartPause), 1);
    assert!(!h.s.snapshot().starting);
}

#[tokio::test(start_paused = true)]
async fn unconfirmed_start_is_never_retried() {
    let pad = PadConfig {
        ignores_start: true,
        ..PadConfig::default()
    };
    let h = H::ready(config(), pad).await;
    let t = Instant::now();
    h.send(Intent::Start);
    let snap = h
        .wait("start not confirmed", 7_000, |s| {
            s.notice
                .as_ref()
                .is_some_and(|n| n.text.starts_with("Start not confirmed"))
        })
        .await;
    assert!(!snap.starting);
    assert!(snap.controls.start, "a new press is allowed");
    sleep(30_000).await;
    assert_eq!(h.count_since(t, &WireCommand::StartPause), 1);
}

#[tokio::test(start_paused = true)]
async fn start_from_standby_wakes_the_pad_first() {
    let h = H::ready(config(), PadConfig::default()).await;
    h.fake.with_pad(|p| p.enter_standby());
    h.wait("standby", 2_000, |s| {
        s.stats
            .as_ref()
            .is_some_and(|st| st.belt == BeltView::Standby)
    })
    .await;
    let t = Instant::now();
    h.send(Intent::Start);
    h.wait("running", 10_000, |s| {
        s.stats
            .as_ref()
            .is_some_and(|st| st.belt == BeltView::Running)
    })
    .await;
    let cmds: Vec<_> = h
        .cmds_since(t)
        .into_iter()
        .filter(|c| *c != WireCommand::AskStatus)
        .collect();
    assert_eq!(cmds, vec![WireCommand::SetMode(1), WireCommand::StartPause]);
    assert_spacing(&h.writes_since(t), ms(700));
}

#[tokio::test(start_paused = true)]
async fn start_from_mode_3_switches_to_manual_first() {
    let h = H::ready(config(), PadConfig::default()).await;
    h.fake.with_pad(|p| p.set_mode_byte(3));
    h.wait("mode 3", 2_000, |s| {
        s.stats
            .as_ref()
            .is_some_and(|st| st.mode == ModeView::Newer)
    })
    .await;
    let t = Instant::now();
    h.send(Intent::Start);
    h.wait("running", 10_000, |s| {
        s.stats
            .as_ref()
            .is_some_and(|st| st.belt == BeltView::Running)
    })
    .await;
    let cmds: Vec<_> = h
        .cmds_since(t)
        .into_iter()
        .filter(|c| *c != WireCommand::AskStatus)
        .collect();
    assert_eq!(cmds, vec![WireCommand::SetMode(1), WireCommand::StartPause]);
}

#[tokio::test(start_paused = true)]
async fn a_non_zero_speed_counts_as_moving_even_if_the_state_lags() {
    let h = H::ready(config(), PadConfig::default()).await;
    // The state byte still says 0 but the belt turns.
    h.fake.with_pad(|p| {
        p.set_running(15);
    });
    h.fake.with_device(|d| d.pad.set_mode_byte(1));
    sleep(1_000).await;
    let t = Instant::now();
    h.send(Intent::Start);
    sleep(3_000).await;
    assert_eq!(h.count_since(t, &WireCommand::StartPause), 0);
    assert!(!h.s.snapshot().controls.start);
}

#[tokio::test(start_paused = true)]
async fn faults_are_shown_and_block_start() {
    let h = H::ready(config(), PadConfig::default()).await;
    h.fake.with_pad(|p| p.set_fault(3, false));
    let snap = h
        .wait("fault", 2_000, |s| {
            s.stats
                .as_ref()
                .is_some_and(|st| st.fault.as_deref() == Some("E03"))
        })
        .await;
    assert!(!snap.controls.start);
    let t = Instant::now();
    h.send(Intent::Start);
    sleep(3_000).await;
    assert_eq!(h.count_since(t, &WireCommand::StartPause), 0);
    h.fake.with_pad(|p| p.set_fault(0, true));
    let snap = h
        .wait("state 4", 2_000, |s| {
            s.stats
                .as_ref()
                .is_some_and(|st| st.belt == BeltView::Fault)
        })
        .await;
    assert_eq!(snap.stats.unwrap().fault.as_deref(), Some("E00"));
}

#[tokio::test(start_paused = true)]
async fn start_is_refused_while_the_belt_runs() {
    let h = H::ready(config(), PadConfig::default()).await;
    h.fake.with_pad(|p| p.set_running(20));
    sleep(1_000).await;
    let t = Instant::now();
    h.send(Intent::Start);
    sleep(5_000).await;
    assert_eq!(h.count_since(t, &WireCommand::StartPause), 0);
    assert!(!h.s.snapshot().controls.start);
}

#[tokio::test(start_paused = true)]
async fn start_is_cancelled_if_a_fresher_status_shows_the_belt_moving() {
    let h = H::ready(config(), PadConfig::default()).await;
    h.just_after_poll().await;
    // Someone starts the belt with the remote just before the next poll…
    sleep(740).await;
    h.fake.with_pad(|p| p.set_running(20));
    sleep(15).await;
    // …and Start is pressed after that poll went out but before its reply,
    // while the app still shows the belt idle.
    let snap = h.s.snapshot();
    assert!(snap.controls.start);
    let t = Instant::now();
    h.send(Intent::Start);
    sleep(10_000).await;
    assert_eq!(
        h.count_since(t, &WireCommand::StartPause),
        0,
        "the toggle would have paused it"
    );
    assert_eq!(h.status().belt, BeltState::Running);
    assert!(!h.s.snapshot().starting);
}

// --- mode ---------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn auto_mode_only_while_stopped_standby_any_time() {
    let h = H::ready(config(), PadConfig::default()).await;
    h.fake.with_pad(|p| p.set_running(20));
    sleep(1_000).await;
    let t = Instant::now();
    h.send(Intent::SetMode { mode: Mode::Auto });
    sleep(2_000).await;
    assert!(!h.cmds_since(t).contains(&WireCommand::SetMode(0)));
    assert!(!h.s.snapshot().controls.auto);
    h.send(Intent::SetMode {
        mode: Mode::Standby,
    });
    sleep(2_000).await;
    assert!(h.cmds_since(t).contains(&WireCommand::SetMode(2)));
    sleep(5_000).await;
    let t = Instant::now();
    h.send(Intent::SetMode { mode: Mode::Auto });
    sleep(2_000).await;
    assert!(h.cmds_since(t).contains(&WireCommand::SetMode(0)));
}

// --- stale link -----------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn three_missed_replies_mark_the_link_stale_and_stop_stays_enabled() {
    let h = H::ready(config_with(StopMethod::SpeedZero), PadConfig::default()).await;
    h.fake.with_pad(|p| p.cfg.answers = false);
    let t = Instant::now();
    let snap = h.wait("stale", 5_000, |s| s.stale).await;
    let elapsed = Instant::now() - t;
    assert!(elapsed >= ms(1_500) && elapsed <= ms(3_100), "{elapsed:?}");
    assert!(snap.controls.stop);
    assert!(!snap.controls.start && !snap.controls.speed && !snap.controls.auto);
    assert!(snap.has_link);
    h.send(Intent::Stop);
    sleep(2_000).await;
    assert!(h.cmds_since(t).contains(&WireCommand::SetSpeed(0)));
    h.fake.with_pad(|p| p.cfg.answers = true);
    h.wait("fresh", 3_000, |s| !s.stale).await;
}

#[tokio::test(start_paused = true)]
async fn bad_frames_are_counted_and_not_applied() {
    let h = H::ready(config(), PadConfig::default()).await;
    let mut bad = h.status();
    bad.belt = BeltState::Running;
    bad.speed = 60;
    let mut frame = bad.encode();
    frame[18] ^= 0xff; // break the CRC
    h.fake.inject(frame);
    h.fake.inject(vec![]);
    h.fake.inject(vec![0xf8, 0xa2, 0x00, 0xfd]);
    let snap = h.wait("bad frames", 1_000, |s| s.bad_frames == 3).await;
    assert_eq!(snap.stats.unwrap().belt, BeltView::Idle);
}

// --- disconnects ------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn queued_commands_are_dropped_on_disconnect_and_nothing_moves_after_reconnect() {
    let h = H::ready(config(), PadConfig::default()).await;
    h.fake.with_pad(|p| p.set_running(20));
    sleep(1_000).await;
    h.just_after_poll().await;
    let t = Instant::now();
    h.send(Intent::AdjustSpeed { steps: 5 });
    h.send(Intent::SetMode {
        mode: Mode::Standby,
    });
    h.fake.drop_link(true);
    h.wait("link down", 1_000, |s| !s.has_link).await;
    let snap = h
        .wait("ready again", 10_000, |s| s.link == LinkPhase::Ready)
        .await;
    assert!(!snap.link_lost);
    sleep(30_000).await;
    let cmds = h.cmds_since(t);
    assert!(
        cmds.iter().all(|c| *c == WireCommand::AskStatus),
        "{cmds:?}"
    );
    // The belt kept going the whole time; nobody told it otherwise.
    assert_eq!(h.status().speed, 20);
    assert_eq!(h.status().belt, BeltState::Running);
}

#[tokio::test(start_paused = true)]
async fn a_pending_stop_or_start_is_not_replayed_after_reconnect() {
    let h = H::ready(config(), PadConfig::default()).await;
    h.just_after_poll().await;
    let t = Instant::now();
    h.send(Intent::Start);
    h.fake.drop_link(true);
    h.wait("link down", 1_000, |s| !s.has_link).await;
    h.wait("ready again", 10_000, |s| s.link == LinkPhase::Ready)
        .await;
    sleep(20_000).await;
    let cmds = h.cmds_since(t);
    assert!(
        cmds.iter().all(|c| *c == WireCommand::AskStatus),
        "{cmds:?}"
    );
    assert!(!h.s.snapshot().starting);
}

#[tokio::test(start_paused = true)]
async fn reconnect_window_then_connection_lost_screen() {
    let h = H::ready(config(), PadConfig::default()).await;
    h.fake.drop_link(false); // the pad stops advertising
    let snap = h
        .wait("reconnecting", 1_000, |s| s.link == LinkPhase::Reconnecting)
        .await;
    assert!(snap.link_lost && !snap.has_link && !snap.controls.stop);
    sleep(59_000).await;
    assert_eq!(h.s.snapshot().link, LinkPhase::Reconnecting);
    let snap = h
        .wait("connect screen", 2_000, |s| s.link == LinkPhase::Searching)
        .await;
    assert!(snap.link_lost && snap.scanning);
    // Touch M: the pad advertises again and the app connects by itself.
    sleep(120_000).await;
    h.fake.set_advertising(true);
    let snap = h.wait("ready", 5_000, |s| s.link == LinkPhase::Ready).await;
    assert!(!snap.link_lost);
}

#[tokio::test(start_paused = true)]
async fn reconnect_attempts_back_off() {
    let h = H::ready(config(), PadConfig::default()).await;
    h.fake.fail_next_connects(4);
    let t = Instant::now();
    h.fake.drop_link(true);
    h.wait("link down", 1_000, |s| !s.has_link).await;
    h.wait("ready", 30_000, |s| s.link == LinkPhase::Ready)
        .await;
    let connects: Vec<Instant> = h
        .fake
        .calls()
        .into_iter()
        .filter(|(at, c)| *at >= t && matches!(c, Call::Connect(_)))
        .map(|(at, _)| at)
        .collect();
    assert_eq!(connects.len(), 5, "{connects:?}");
    let gaps: Vec<Duration> = connects.windows(2).map(|p| p[1] - p[0]).collect();
    // each failed attempt takes 300 ms, then waits 1, 2, 4, 8 s
    for (gap, backoff) in gaps.iter().zip([1, 2, 4, 8]) {
        assert!(*gap >= Duration::from_secs(backoff), "{gaps:?}");
        assert!(*gap < Duration::from_secs(backoff) + ms(600), "{gaps:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn disconnect_hands_the_pad_over_and_stops_searching() {
    let h = H::ready(config(), PadConfig::default()).await;
    let t = Instant::now();
    h.send(Intent::Disconnect);
    let snap = h.wait("idle", 2_000, |s| s.link == LinkPhase::Idle).await;
    assert!(!snap.link_lost && !snap.has_link);
    sleep(30_000).await;
    assert!(!h.fake.is_connected());
    assert!(!h.fake.scanning());
    let reconnects = h
        .fake
        .calls()
        .into_iter()
        .filter(|(at, c)| *at >= t && matches!(c, Call::Connect(_)))
        .count();
    assert_eq!(reconnects, 0);
    h.send(Intent::Search);
    h.wait("ready", 5_000, |s| s.link == LinkPhase::Ready).await;
}

#[tokio::test(start_paused = true)]
async fn incompatible_device_is_reported() {
    let h = H::new(config(), PadConfig::default());
    h.fake.set_incompatible(true);
    h.wait("notice", 5_000, |s| {
        s.notice
            .as_ref()
            .is_some_and(|n| n.text.contains("compatible"))
    })
    .await;
    assert!(!h.fake.is_connected());
}

// --- units and presets -------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn stats_are_shown_in_miles_or_km() {
    let h = H::ready(config(), PadConfig::default()).await;
    h.fake.with_pad(|p| p.set_running(32));
    sleep(2_000).await;
    let snap = h.s.snapshot();
    let st = snap.stats.unwrap();
    assert_eq!(st.speed, 2.0);
    assert_eq!(snap.units.distance_unit, "mi");
    assert_eq!(snap.presets, [1.0, 2.0, 3.0]);
    h.send(Intent::SetDisplayUnits {
        units: DisplayUnits::Km,
    });
    let snap = h.wait("km", 1_000, |s| s.units.speed_unit == "km/h").await;
    assert_eq!(snap.stats.unwrap().speed, 3.2);
    assert_eq!(snap.presets, [1.6, 3.2, 4.8]);
    h.send(Intent::SetPreset {
        index: 0,
        tenths: 25,
    });
    let snap = h.wait("preset", 1_000, |s| s.presets[0] == 2.5).await;
    assert_eq!(snap.presets[0], 2.5);
    h.send(Intent::SetDisplayUnits {
        units: DisplayUnits::Miles,
    });
    let snap = h.wait("mph", 1_000, |s| s.units.speed_unit == "mph").await;
    assert_eq!(snap.presets[0], 1.6);
}

#[test]
fn presets_round_trip_in_both_units() {
    for t in 0..=100 {
        for d in [DisplayUnits::Miles, DisplayUnits::Km] {
            assert_eq!(Preset::from_display_tenths(t, d).display_tenths(d), t);
        }
    }
}
