//! Local walk history, separate from the pad's connection/safety state machine.
//! Fresh pad status starts/ends a walk; heart-rate loss never controls the belt.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use walkingpad::session::{BeltView, LinkPhase, Snapshot as PadSnapshot, Stats};

use crate::heart_rate::{Sample, unix_ms};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Walk {
    pub id: String,
    pub started_ms: u64,
    pub ended_ms: Option<u64>,
    pub interrupted: bool,
    pub time_s: u32,
    pub distance_m: u32,
    pub steps: u32,
    pub heart_rate: Vec<Sample>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub id: String,
    pub started_ms: u64,
    pub ended_ms: Option<u64>,
    pub interrupted: bool,
    pub time_s: u32,
    pub distance_m: u32,
    pub steps: u32,
    pub samples: usize,
    pub average_bpm: Option<u16>,
    pub max_bpm: Option<u16>,
}

impl Walk {
    fn summary(&self) -> Summary {
        Summary {
            id: self.id.clone(),
            started_ms: self.started_ms,
            ended_ms: self.ended_ms,
            interrupted: self.interrupted,
            time_s: self.time_s,
            distance_m: self.distance_m,
            steps: self.steps,
            samples: self.heart_rate.len(),
            average_bpm: (!self.heart_rate.is_empty()).then(|| {
                (self
                    .heart_rate
                    .iter()
                    .map(|s| u64::from(s.bpm))
                    .sum::<u64>()
                    / self.heart_rate.len() as u64) as u16
            }),
            max_bpm: self.heart_rate.iter().map(|s| s.bpm).max(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub current: Option<Summary>,
    pub recent: Vec<Summary>,
    pub error: Option<String>,
}

#[derive(Default)]
struct Recorder {
    current: Option<Walk>,
    last_stats: Option<Stats>,
    last_seen_ms: u64,
    pending: Vec<Walk>,
    sequence: u64,
}

impl Recorder {
    fn observe(&mut self, pad: &PadSnapshot, now: u64) {
        if pad.link != LinkPhase::Ready || pad.stale {
            self.finish(true);
            self.last_stats = None;
            return;
        }
        let Some(stats) = pad.stats.as_ref() else {
            return;
        };
        let moving = stats.belt == BeltView::Running || stats.speed > 0.0;
        if moving
            && self.current.is_some()
            && self
                .last_stats
                .as_ref()
                .is_some_and(|s| stats.time_s < s.time_s)
        {
            // A counter reset while still moving is a new pad session.
            self.finish(true);
            self.last_stats = None;
        }
        if moving && self.current.is_none() {
            self.sequence += 1;
            self.current = Some(Walk {
                id: format!("{now}-{}", self.sequence),
                started_ms: now,
                ended_ms: None,
                interrupted: false,
                time_s: 0,
                distance_m: 0,
                steps: 0,
                heart_rate: Vec::new(),
            });
            // Joining an already-moving pad records only from this point.
            self.last_stats = Some(stats.clone());
        }
        if let (Some(walk), Some(last)) = (&mut self.current, &self.last_stats) {
            walk.time_s += stats.time_s.saturating_sub(last.time_s);
            walk.distance_m += stats.distance_m.saturating_sub(last.distance_m);
            walk.steps += stats.steps.saturating_sub(last.steps);
        }
        self.last_stats = Some(stats.clone());
        self.last_seen_ms = now;
        if !moving && stats.belt != BeltView::Countdown {
            self.finish(false);
        }
    }

    fn sample(&mut self, sample: Sample) {
        if let Some(walk) = &mut self.current
            && sample.at_ms >= walk.started_ms
            && sample.at_ms <= self.last_seen_ms + 10_000
        {
            walk.heart_rate.push(sample);
        }
    }

    fn finish(&mut self, interrupted: bool) {
        if let Some(mut walk) = self.current.take() {
            walk.ended_ms = Some(self.last_seen_ms.max(walk.started_ms));
            walk.interrupted = interrupted;
            self.pending.push(walk);
        }
    }
}

fn save(path: &Path, walk: &Walk) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(walk)?)?;
    std::fs::rename(tmp, path)
}

fn load(dir: &Path) -> std::io::Result<Vec<Summary>> {
    std::fs::create_dir_all(dir)?;
    let active = dir.join("active.json");
    if active.exists() {
        match serde_json::from_slice::<Walk>(&std::fs::read(&active)?) {
            Ok(mut walk) if walk.id.chars().all(|c| c.is_ascii_digit() || c == '-') => {
                let finished = dir.join(format!("walk-{}.json", walk.id));
                if !finished.exists() {
                    walk.interrupted = true;
                    walk.ended_ms = Some(walk.ended_ms.unwrap_or(walk.started_ms));
                    save(&finished, &walk)?;
                }
                std::fs::remove_file(&active)?;
            }
            _ => {
                std::fs::rename(&active, dir.join("active.json.bad"))?;
            }
        }
    }
    let mut recent = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|name| name.starts_with("walk-") && name.ends_with(".json"))
        {
            // Keep a damaged file on disk; it must not hide all other walks.
            if let Ok(bytes) = std::fs::read(&path)
                && let Ok(walk) = serde_json::from_slice::<Walk>(&bytes)
            {
                recent.push(walk.summary());
            }
        }
    }
    recent.sort_by_key(|w| std::cmp::Reverse(w.started_ms));
    recent.truncate(10);
    Ok(recent)
}

/// One file per walk; a checkpoint every five seconds survives app shutdown.
/// Mock history is isolated from real walks by the caller's directory.
pub fn spawn(
    dir: PathBuf,
    mut pad: watch::Receiver<PadSnapshot>,
) -> (watch::Receiver<Snapshot>, mpsc::UnboundedSender<Sample>) {
    let (tx, mut samples) = mpsc::unbounded_channel();
    let (state_tx, state) = watch::channel(Snapshot::default());
    tauri::async_runtime::spawn(async move {
        let mut s = Snapshot::default();
        match load(&dir) {
            Ok(recent) => s.recent = recent,
            Err(e) => s.error = Some(format!("Couldn't load walk history: {e}")),
        }
        let mut recorder = Recorder::default();
        let mut checkpoint_id = None;
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            let checkpoint = tokio::select! {
                Ok(()) = pad.changed() => {
                    recorder.observe(&pad.borrow_and_update(), unix_ms());
                    false
                }
                Some(sample) = samples.recv() => {
                    recorder.observe(&pad.borrow(), unix_ms());
                    recorder.sample(sample);
                    false
                }
                _ = tick.tick() => true,
                else => break,
            };
            let result = (|| -> std::io::Result<()> {
                std::fs::create_dir_all(&dir)?;
                while let Some(walk) = recorder.pending.first() {
                    save(&dir.join(format!("walk-{}.json", walk.id)), walk)?;
                    let summary = walk.summary();
                    s.recent.retain(|w| w.id != summary.id);
                    s.recent.insert(0, summary);
                    s.recent.truncate(10);
                    recorder.pending.remove(0);
                }
                let active = dir.join("active.json");
                if let Some(walk) = &recorder.current {
                    if checkpoint || checkpoint_id.as_ref() != Some(&walk.id) {
                        let mut saved = walk.clone();
                        saved.ended_ms = Some(recorder.last_seen_ms);
                        save(&active, &saved)?;
                        checkpoint_id = Some(walk.id.clone());
                    }
                } else if active.exists() {
                    std::fs::remove_file(active)?;
                    checkpoint_id = None;
                }
                Ok(())
            })();
            if let Err(e) = result {
                s.error = Some(format!("Couldn't save walk history; retrying: {e}"));
            } else if checkpoint {
                s.error = None;
            }
            s.current = recorder.current.as_ref().map(Walk::summary);
            state_tx.send_if_modified(|last| {
                if *last == s {
                    return false;
                }
                *last = s.clone();
                true
            });
        }
    });
    (state, tx)
}

#[cfg(all(test, feature = "mock"))]
mod tests {
    use super::*;
    use walkingpad::{
        fake::FakeTransport,
        session::{self, Config, ModeView},
        transport::event_channel,
    };

    fn pad() -> PadSnapshot {
        let (tx, rx) = event_channel();
        let (_, handle) = session::new(FakeTransport::new(tx), rx, Config::default());
        let mut pad = handle.snapshot();
        pad.link = LinkPhase::Ready;
        pad.stats = Some(Stats {
            belt: BeltView::Running,
            mode: ModeView::Manual,
            speed: 2.0,
            target_speed: None,
            time_s: 100,
            distance: 0.25,
            distance_m: 400,
            steps: 500,
            countdown_s: None,
            fault: None,
        });
        pad
    }

    fn reading(at_ms: u64, bpm: u16) -> Sample {
        Sample {
            at_ms,
            bpm,
            monitor: "Pixel Watch 3".into(),
        }
    }

    #[test]
    fn records_only_observed_motion_and_keeps_heart_rate_with_the_walk() {
        let mut recorder = Recorder::default();
        let mut pad = pad();
        recorder.sample(reading(999, 200));
        recorder.observe(&pad, 1000);
        recorder.sample(reading(999, 200));
        recorder.sample(reading(1000, 110));
        let stats = pad.stats.as_mut().unwrap();
        stats.time_s = 110;
        stats.distance_m = 440;
        stats.steps = 520;
        recorder.observe(&pad, 11_000);
        recorder.sample(reading(11_000, 120));
        // Speed takes precedence when the belt-state byte lags while slowing.
        pad.stats.as_mut().unwrap().belt = BeltView::Stopped;
        recorder.observe(&pad, 11_100);
        assert!(recorder.current.is_some());
        pad.stats.as_mut().unwrap().speed = 0.0;
        recorder.observe(&pad, 12_000);
        recorder.sample(reading(13_000, 200));
        assert!(recorder.current.is_none());
        let walk = &recorder.pending[0];
        assert_eq!((walk.time_s, walk.distance_m, walk.steps), (10, 40, 20));
        assert_eq!(walk.heart_rate.len(), 2);
        assert_eq!(walk.summary().average_bpm, Some(115));
        assert_eq!(walk.summary().max_bpm, Some(120));
        assert!(!walk.interrupted);
        assert_eq!(walk.ended_ms, Some(12_000));
    }

    #[test]
    fn stale_status_and_counter_resets_end_partial_walks_without_inventing_data() {
        let mut recorder = Recorder::default();
        let mut pad = pad();
        recorder.observe(&pad, 1000);
        pad.stale = true;
        recorder.observe(&pad, 5000);
        assert!(recorder.pending[0].interrupted);
        assert_eq!(recorder.pending[0].ended_ms, Some(1000));
        recorder.sample(reading(5000, 100));
        pad.stale = false;
        recorder.observe(&pad, 6000);
        pad.stats.as_mut().unwrap().time_s = 1;
        pad.stats.as_mut().unwrap().distance_m = 0;
        recorder.observe(&pad, 7000);
        assert_eq!(recorder.pending.len(), 2);
        assert!(recorder.pending[1].interrupted);
        assert_eq!(recorder.current.as_ref().unwrap().distance_m, 0);
        assert!(recorder.current.as_ref().unwrap().heart_rate.is_empty());
        pad.link = LinkPhase::Reconnecting;
        recorder.observe(&pad, 8000);
        assert_eq!(recorder.pending.len(), 3);
        assert!(recorder.current.is_none());
    }

    #[test]
    fn checkpoints_recover_without_overwriting_completed_walks() {
        let dir =
            std::env::temp_dir().join(format!("kc-walks-{}-{}", std::process::id(), unix_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut recorder = Recorder::default();
        recorder.observe(&pad(), 1000);
        recorder.sample(reading(1000, 95));
        let mut walk = recorder.current.take().unwrap();
        walk.ended_ms = Some(5000);
        save(&dir.join("active.json"), &walk).unwrap();
        let recent = load(&dir).unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].average_bpm, Some(95));
        assert!(recent[0].interrupted);
        assert_eq!(recent[0].ended_ms, Some(5000));
        // A crash after saving a completed walk but before deleting active.json.
        walk.interrupted = false;
        save(&dir.join(format!("walk-{}.json", walk.id)), &walk).unwrap();
        save(&dir.join("active.json"), &walk).unwrap();
        assert!(!load(&dir).unwrap()[0].interrupted);
        std::fs::write(dir.join("active.json"), "broken").unwrap();
        assert_eq!(load(&dir).unwrap().len(), 1);
        assert!(dir.join("active.json.bad").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
