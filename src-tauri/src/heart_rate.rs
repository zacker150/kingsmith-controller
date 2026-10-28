//! Standard Bluetooth Heart Rate Service. This link never sends pad commands
//! or writes characteristics; subscribe() only enables notifications.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri_plugin_blec::{Connection, OnDisconnectHandler, models::CharProps};
use tokio::sync::{mpsc, watch};
use uuid::{Uuid, uuid};
use walkingpad::transport::Advertisement;

use crate::discovery::Discovery;

pub const SERVICE: Uuid = uuid!("0000180d-0000-1000-8000-00805f9b34fb");
pub const MEASUREMENT: Uuid = uuid!("00002a37-0000-1000-8000-00805f9b34fb");
const STALE_AFTER: Duration = Duration::from_secs(10);

pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sample {
    pub at_ms: u64,
    pub bpm: u16,
    pub monitor: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub phase: &'static str,
    pub monitor: Option<Advertisement>,
    pub devices: Vec<Advertisement>,
    pub bpm: Option<u16>,
    pub message: String,
    pub bad_frames: u32,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            phase: "idle",
            monitor: None,
            devices: Vec::new(),
            bpm: None,
            message: "Enable Share Heart Rate on your Pixel Watch, then search.".into(),
            bad_frames: 0,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Intent {
    Search,
    Connect { id: String },
    Disconnect,
    SetVisible { visible: bool },
}

#[derive(Clone)]
pub struct Handle {
    tx: mpsc::UnboundedSender<Intent>,
    pub state: watch::Receiver<Snapshot>,
}

impl Handle {
    pub fn send(&self, intent: Intent) -> Result<(), String> {
        self.tx
            .send(intent)
            .map_err(|_| "Heart-rate service ended".into())
    }
}

enum Event {
    Measurement(u64, Vec<u8>, Instant, u64),
    Disconnected(u64),
    Connected(u64, Result<Connection, String>),
}

/// Recovery is bounded so switching off sharing doesn't keep retrying forever.
struct Recovery {
    until: Instant,
    next: Instant,
    delay: u64,
}

impl Recovery {
    fn new(now: Instant) -> Self {
        Self {
            until: now + Duration::from_secs(120),
            next: now + Duration::from_secs(2),
            delay: 2,
        }
    }

    fn failed(&mut self, now: Instant) {
        self.delay = (self.delay * 2).min(30);
        self.next = now + Duration::from_secs(self.delay);
    }
}

fn connect(discovery: Discovery, id: String, epoch: u64, events: mpsc::UnboundedSender<Event>) {
    tauri::async_runtime::spawn(async move {
        crate::applog::line("heart-rate: connecting to monitor");
        let disconnected = events.clone();
        let callback = OnDisconnectHandler::from_sync(move || {
            let _ = disconnected.send(Event::Disconnected(epoch));
        });
        let result = match discovery.connect(&id, callback).await {
            Ok(link) => {
                let notifications = events.clone();
                match subscribe(&link, move |data| {
                    let _ = notifications.send(Event::Measurement(
                        epoch,
                        data,
                        Instant::now(),
                        unix_ms(),
                    ));
                })
                .await
                {
                    Ok(()) if link.is_connected() => Ok(link),
                    result => {
                        let error = result
                            .err()
                            .unwrap_or_else(|| "Monitor disconnected during setup".into());
                        let _ = link.disconnect().await;
                        Err(error)
                    }
                }
            }
            Err(e) => Err(e.to_string()),
        };
        if let Err(error) = &result {
            crate::applog::line(&format!("heart-rate: connection failed: {error}"));
        }
        if let Err(event) = events.send(Event::Connected(epoch, result))
            && let Event::Connected(_, Ok(link)) = event.0
        {
            let _ = link.disconnect().await;
        }
    });
}

pub fn spawn(discovery: Discovery, samples: mpsc::UnboundedSender<Sample>) -> Handle {
    let (tx, mut intents) = mpsc::unbounded_channel();
    let (state_tx, state) = watch::channel(Snapshot::default());
    tauri::async_runtime::spawn(async move {
        let (events, mut rx) = mpsc::unbounded_channel();
        let mut devices = discovery.heart_devices();
        let mut s = Snapshot::default();
        let mut conn: Option<Connection> = None;
        let mut generation = 0;
        let mut connecting = false;
        let mut recovery: Option<Recovery> = None;
        let mut last_reading: Option<Instant> = None;
        let mut last_report: Option<Instant> = None;
        let mut scan_until = None;
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                Some(intent) = intents.recv() => match intent {
                    Intent::Search if conn.is_none() && !connecting && recovery.is_none() => {
                        s.devices.clear();
                        devices.borrow_and_update();
                        match discovery.heart(true).await {
                            Ok(()) => {
                                s.phase = "searching";
                                s.message = "Searching for heart-rate monitors…".into();
                                scan_until = Some(Instant::now() + Duration::from_secs(30));
                            }
                            Err(e) => { s.phase = "idle"; s.message = e.to_string(); }
                        }
                    }
                    Intent::Connect { id } if conn.is_none() && !connecting && recovery.is_none() => {
                        let Some(monitor) = s.devices.iter().find(|d| d.id == id).cloned() else {
                            s.message = "Search and select a heart-rate monitor first.".into();
                            state_tx.send_replace(s.clone());
                            continue;
                        };
                        scan_until = None;
                        let _ = discovery.heart(false).await;
                        s.phase = "connecting";
                        s.monitor = Some(monitor);
                        s.bpm = None;
                        s.message = "Connecting… Confirm sharing on your watch if prompted.".into();
                        generation += 1;
                        connecting = true;
                        connect(discovery.clone(), id, generation, events.clone());
                    }
                    Intent::Disconnect => {
                        generation += 1;
                        recovery = None;
                        scan_until = None;
                        let _ = discovery.heart(false).await;
                        if let Some(link) = conn.take() { let _ = link.disconnect().await; }
                        s.phase = if connecting { "disconnecting" } else { "idle" };
                        s.bpm = None;
                        s.devices.clear();
                        last_reading = None;
                        s.message = "Heart-rate monitor disconnected.".into();
                        crate::applog::line("heart-rate: disconnected by user; recovery cancelled");
                    }
                    Intent::SetVisible { visible: false } if s.phase == "searching" => {
                        let _ = discovery.heart(false).await;
                        scan_until = None;
                        s.phase = "idle";
                        s.message = "Search paused. Search again when you're ready.".into();
                    }
                    _ => {}
                },
                Ok(()) = devices.changed() => {
                    if s.phase == "searching" { s.devices = devices.borrow_and_update().clone(); }
                }
                Some(event) = rx.recv() => match event {
                    Event::Connected(epoch, result) if epoch == generation && connecting => {
                        connecting = false;
                        match result {
                            Ok(link) => {
                                conn = Some(link);
                                recovery = None;
                                s.phase = "connected";
                                s.message = "Connected · waiting for a reading…".into();
                                last_reading = Some(Instant::now());
                                last_report = None;
                                crate::applog::line("heart-rate: notifications subscribed; connected");
                            }
                            Err(e) => {
                                if let Some(retry) = &mut recovery {
                                    retry.failed(Instant::now());
                                    s.phase = "reconnecting";
                                    s.message = "Reconnecting… Keep Share Heart Rate enabled on your watch.".into();
                                } else { s.phase = "idle"; s.message = e; }
                            }
                        }
                    }
                    Event::Connected(_, result) => {
                        // A user cancellation can finish before the native
                        // connection does. Close that late link, never adopt it.
                        if let Ok(link) = result { let _ = link.disconnect().await; }
                        connecting = false;
                        s.phase = "idle";
                    }
                    Event::Disconnected(epoch) if epoch == generation && conn.is_some() => {
                        conn = None;
                        s.phase = "reconnecting";
                        s.bpm = None;
                        last_reading = None;
                        recovery = Some(Recovery::new(Instant::now()));
                        s.message = "Heart-rate connection lost · reconnecting…".into();
                        crate::applog::line("heart-rate: link lost; starting bounded recovery");
                    }
                    Event::Measurement(epoch, data, received, at_ms) if epoch == generation && conn.is_some() => {
                        match parse_measurement(&data) {
                            Ok(bpm) => {
                                last_reading = Some(received);
                                s.bpm = if received.elapsed() < STALE_AFTER { bpm } else { None };
                                s.message = if s.bpm.is_some() { "Live heart rate" } else { "Waiting for a reading · check watch contact." }.into();
                                if let Some(bpm) = s.bpm {
                                    if last_report.is_none_or(|at| at.elapsed() >= Duration::from_secs(30)) {
                                        crate::applog::line("heart-rate: receiving valid measurements");
                                        last_report = Some(Instant::now());
                                    }
                                    let _ = samples.send(Sample { at_ms, bpm, monitor: s.monitor.as_ref().unwrap().name.clone().unwrap_or_else(|| "Heart-rate monitor".into()) });
                                }
                            }
                            Err(()) => s.bad_frames += 1,
                        }
                    }
                    _ => {}
                },
                _ = tick.tick() => {
                    if scan_until.is_some_and(|until| Instant::now() >= until) {
                        let _ = discovery.heart(false).await;
                        scan_until = None;
                        s.phase = "idle";
                        s.message = if s.devices.is_empty() { "No monitors found. Enable Share Heart Rate (or Extended pairing) on your watch and search again." } else { "Search complete. Select a monitor, or search again." }.into();
                    }
                    if conn.is_some() && last_reading.is_some_and(|at| at.elapsed() >= STALE_AFTER) {
                        if s.bpm.is_some() { crate::applog::line("heart-rate: no valid measurement in ten seconds"); }
                        s.bpm = None;
                        s.message = "No recent reading · check heart-rate sharing on your watch.".into();
                    }
                    if conn.as_ref().is_some_and(|link| !link.is_connected()) {
                        let _ = events.send(Event::Disconnected(generation));
                    }
                    if let Some(retry) = &recovery && !connecting {
                        let now = Instant::now();
                        if now >= retry.until {
                            recovery = None;
                            s.phase = "idle";
                            s.message = "Couldn't reconnect. Enable Share Heart Rate on your watch and search again.".into();
                            crate::applog::line("heart-rate: recovery window expired");
                        } else if now >= retry.next {
                            generation += 1;
                            connecting = true;
                            connect(discovery.clone(), s.monitor.as_ref().unwrap().id.clone(), generation, events.clone());
                        }
                    }
                }
            }
            state_tx.send_if_modified(|last| {
                if *last == s {
                    return false;
                }
                *last = s.clone();
                true
            });
        }
    });
    Handle { tx, state }
}

async fn subscribe(
    link: &Connection,
    callback: impl Fn(Vec<u8>) + Send + Sync + 'static,
) -> Result<(), String> {
    let services = link.services().await.map_err(|e| e.to_string())?;
    let compatible = services.iter().any(|service| {
        service.uuid == SERVICE
            && service
                .characteristics
                .iter()
                .any(|c| c.uuid == MEASUREMENT && c.properties.contains(CharProps::Notify))
    });
    if !compatible {
        return Err("Monitor has no Heart Rate Measurement notifications. Enable sharing on the watch and reconnect.".into());
    }
    link.subscribe(MEASUREMENT, Some(SERVICE), callback)
        .await
        .map_err(|e| e.to_string())
}

/// HRS 1.0: flags, UINT8/UINT16 little-endian BPM, optional energy and RR
/// intervals. Missing contact and zero BPM mean unavailable, never 0 BPM.
pub fn parse_measurement(data: &[u8]) -> Result<Option<u16>, ()> {
    let flags = *data.first().ok_or(())?;
    let wide = flags & 1 != 0;
    let mut end = if wide { 3 } else { 2 };
    if data.len() < end {
        return Err(());
    }
    let bpm = if wide {
        u16::from_le_bytes([data[1], data[2]])
    } else {
        data[1] as u16
    };
    if flags & 8 != 0 {
        end += 2;
    }
    if data.len() < end {
        return Err(());
    }
    if flags & 16 != 0 {
        if data.len() < end + 2 || !(data.len() - end).is_multiple_of(2) {
            return Err(());
        }
    } else if data.len() != end {
        return Err(());
    }
    Ok((bpm > 0 && (flags & 4 == 0 || flags & 2 != 0)).then_some(bpm))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_backs_off_and_has_a_fixed_deadline() {
        let now = Instant::now();
        let mut recovery = Recovery::new(now);
        assert_eq!(recovery.next, now + Duration::from_secs(2));
        for delay in [4, 8, 16, 30, 30] {
            recovery.failed(now);
            assert_eq!(recovery.next, now + Duration::from_secs(delay));
            assert_eq!(recovery.until, now + Duration::from_secs(120));
        }
    }

    #[test]
    fn decodes_both_formats_and_optional_fields() {
        assert_eq!(parse_measurement(&[0, 95]), Ok(Some(95)));
        assert_eq!(parse_measurement(&[1, 44, 1]), Ok(Some(300)));
        assert_eq!(
            parse_measurement(&[0x1f, 95, 0, 10, 0, 0, 4, 1, 4]),
            Ok(Some(95))
        );
        assert_eq!(parse_measurement(&[4, 95]), Ok(None));
        assert_eq!(parse_measurement(&[6, 95]), Ok(Some(95)));
        assert_eq!(parse_measurement(&[0, 0]), Ok(None));
    }

    #[test]
    fn rejects_truncated_or_malformed_measurements() {
        for data in [
            vec![],
            vec![0],
            vec![1, 95],
            vec![8, 95, 0],
            vec![16, 95],
            vec![16, 95, 0],
            vec![0, 95, 1],
        ] {
            assert_eq!(parse_measurement(&data), Err(()), "{data:?}");
        }
    }
}
