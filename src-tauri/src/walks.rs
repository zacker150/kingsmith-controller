//! Local walk history, separate from the pad's connection/safety state machine.
//! Fresh pad status starts/ends a walk; heart-rate loss never controls the belt.

use std::{
    cmp::Reverse,
    collections::BinaryHeap,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch};
use walkingpad::session::{BeltView, LinkPhase, Snapshot as PadSnapshot, Stats};

use crate::heart_rate::{Sample, unix_ms};

const RECENT_LIMIT: usize = 10;

pub struct Handle {
    pub state: watch::Receiver<Snapshot>,
    pub dir: PathBuf,
    commands: mpsc::UnboundedSender<Delete>,
}

struct Delete {
    id: String,
    reply: oneshot::Sender<Result<(), String>>,
}

impl Handle {
    pub async fn delete(&self, id: String) -> Result<(), String> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Delete { id, reply })
            .map_err(|_| "Walk history is unavailable")?;
        result.await.map_err(|_| "Walk history is unavailable")?
    }
}

/// Only recorder-generated IDs can address a completed walk file.
pub fn saved_path(dir: &Path, id: &str) -> Result<PathBuf, String> {
    let valid = id.split_once('-').is_some_and(|(start, sequence)| {
        [start, sequence].iter().all(|part| {
            !part.is_empty()
                && part.bytes().all(|c| c.is_ascii_digit())
                && part.parse::<u64>().is_ok()
        })
    });
    if !valid {
        return Err("Invalid saved walk ID".into());
    }
    Ok(dir.join(format!("walk-{id}.json")))
}

pub fn read_saved(dir: &Path, id: &str) -> Result<Walk, String> {
    let bytes = std::fs::read(saved_path(dir, id)?)
        .map_err(|e| format!("Couldn't read saved walk: {e}"))?;
    let walk: Walk =
        serde_json::from_slice(&bytes).map_err(|e| format!("Couldn't read saved walk: {e}"))?;
    if walk.id != id || walk.ended_ms.is_none() {
        return Err("This walk has not finished saving".into());
    }
    Ok(walk)
}

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
    load_recent(dir, |path| std::fs::read(path))
}

/// IDs are `<start milliseconds>-<sequence>`. Rank filenames numerically before
/// decoding samples, so historical walks don't add unbounded file I/O at launch.
fn load_recent(
    dir: &Path,
    mut read: impl FnMut(&Path) -> std::io::Result<Vec<u8>>,
) -> std::io::Result<Vec<Summary>> {
    let mut files = BinaryHeap::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if let Some(id) = path
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(|name| name.strip_prefix("walk-"))
            .and_then(|name| name.strip_suffix(".json"))
            && let Some((start, sequence)) = id.split_once('-')
            && let (Ok(start), Ok(sequence)) = (start.parse::<u64>(), sequence.parse::<u64>())
        {
            files.push(Reverse((start, sequence, path)));
            if files.len() > RECENT_LIMIT {
                files.pop();
            }
        }
    }
    let mut recent = Vec::new();
    for Reverse((_, _, path)) in files.into_sorted_vec() {
        // Keep a damaged file on disk; it must not hide the other recent walks.
        if let Ok(bytes) = read(&path)
            && let Ok(walk) = serde_json::from_slice::<Walk>(&bytes)
        {
            recent.push(walk.summary());
        }
    }
    recent.sort_by_key(|w| std::cmp::Reverse(w.started_ms));
    Ok(recent)
}

/// One file per walk; a checkpoint every five seconds survives app shutdown.
/// Mock history is isolated from real walks by the caller's directory.
pub fn spawn(
    dir: PathBuf,
    mut pad: watch::Receiver<PadSnapshot>,
) -> (Handle, mpsc::UnboundedSender<Sample>) {
    let (tx, mut samples) = mpsc::unbounded_channel();
    let (commands, mut requests) = mpsc::unbounded_channel::<Delete>();
    let (state_tx, state) = watch::channel(Snapshot::default());
    let history_dir = dir.clone();
    tauri::async_runtime::spawn(async move {
        let mut s = Snapshot::default();
        let mut history_loaded = false;
        let mut recorder = Recorder::default();
        let mut checkpoint_id = None;
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            let checkpoint = tokio::select! {
                Some(request) = requests.recv() => {
                    let result = (|| -> Result<(), String> {
                        let path = saved_path(&dir, &request.id)?;
                        if !history_loaded || recorder.current.as_ref().is_some_and(|w| w.id == request.id)
                            || recorder.pending.iter().any(|w| w.id == request.id) {
                            return Err("Walk history is still saving; try again shortly".into());
                        }
                        std::fs::remove_file(path).map_err(|e| format!("Couldn't delete walk: {e}"))?;
                        s.recent.retain(|w| w.id != request.id);
                        // Refill the list from disk after deleting one of the ten visible walks.
                        match load_recent(&dir, |p| std::fs::read(p)) {
                            Ok(recent) => s.recent = recent,
                            Err(e) => s.error = Some(format!("Walk deleted, but couldn't refresh history: {e}")),
                        }
                        Ok(())
                    })();
                    state_tx.send_replace(s.clone());
                    let _ = request.reply.send(result);
                    continue;
                }
                changed = pad.changed() => {
                    if changed.is_err() { break; }
                    recorder.observe(&pad.borrow_and_update(), unix_ms());
                    false
                }
                sample = samples.recv() => {
                    let Some(sample) = sample else { break; };
                    recorder.observe(&pad.borrow(), unix_ms());
                    recorder.sample(sample);
                    false
                }
                _ = tick.tick() => true,
                else => break,
            };
            let result = (|| -> std::io::Result<()> {
                // Recovery owns active.json until it succeeds. In the meantime
                // new walks keep recording in memory, and each event/tick retries
                // recovery before any checkpoint replacement or cleanup.
                if !history_loaded {
                    s.recent = load(&dir)?;
                    history_loaded = true;
                }
                std::fs::create_dir_all(&dir)?;
                while let Some(walk) = recorder.pending.first() {
                    save(&dir.join(format!("walk-{}.json", walk.id)), walk)?;
                    let summary = walk.summary();
                    s.recent.retain(|w| w.id != summary.id);
                    s.recent.insert(0, summary);
                    s.recent.truncate(RECENT_LIMIT);
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
                let action = if history_loaded { "save" } else { "recover" };
                s.error = Some(format!("Couldn't {action} walk history; retrying: {e}"));
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
    (
        Handle {
            state,
            dir: history_dir,
            commands,
        },
        tx,
    )
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

    fn stored_walk(id: &str, started_ms: u64) -> Walk {
        Walk {
            id: id.into(),
            started_ms,
            ended_ms: Some(started_ms + 1000),
            interrupted: false,
            time_s: 1,
            distance_m: 10,
            steps: 2,
            heart_rate: vec![reading(started_ms, 95)],
        }
    }

    #[test]
    fn saved_walk_paths_reject_traversal_and_unfinished_or_mismatched_files() {
        let dir =
            std::env::temp_dir().join(format!("kc-walk-read-{}-{}", std::process::id(), unix_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        for id in [
            "",
            "../active",
            "active",
            "1/2-3",
            "1-2/3",
            "1-2-3",
            "1-",
            "-1",
            "18446744073709551616-1",
        ] {
            assert!(saved_path(&dir, id).is_err(), "{id}");
        }
        let path = saved_path(&dir, "1000-1").unwrap();
        assert_eq!(path.parent(), Some(dir.as_path()));
        save(&path, &stored_walk("1000-2", 1000)).unwrap();
        assert!(read_saved(&dir, "1000-1").is_err());
        let mut unfinished = stored_walk("1000-1", 1000);
        unfinished.ended_ms = None;
        save(&path, &unfinished).unwrap();
        assert!(read_saved(&dir, "1000-1").is_err());
        save(&path, &stored_walk("1000-1", 1000)).unwrap();
        assert_eq!(read_saved(&dir, "1000-1").unwrap().steps, 2);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn delete_refills_history_persists_and_protects_the_recording() {
        tauri::async_runtime::block_on(async {
            let dir = std::env::temp_dir().join(format!(
                "kc-walk-delete-{}-{}",
                std::process::id(),
                unix_ms()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            for start in 1000..1011 {
                let id = format!("{start}-1");
                save(&saved_path(&dir, &id).unwrap(), &stored_walk(&id, start)).unwrap();
            }
            let (pad_tx, pad_rx) = watch::channel(pad());
            let (history, _samples) = spawn(dir.clone(), pad_rx);
            let mut state = history.state.clone();
            wait_history(&mut state, |s| s.recent.len() == 10).await;
            history.delete("1010-1".into()).await.unwrap();
            let after = state.borrow().clone();
            assert_eq!(after.recent.len(), 10);
            assert_eq!(after.recent.last().unwrap().id, "1000-1");
            assert!(!saved_path(&dir, "1010-1").unwrap().exists());
            assert!(!load(&dir).unwrap().iter().any(|w| w.id == "1010-1"));
            assert!(history.delete("../active".into()).await.is_err());
            assert!(history.delete("999-1".into()).await.is_err());

            let mut moving = pad();
            pad_tx.send_replace(moving.clone());
            let recording = wait_history(&mut state, |s| s.current.is_some()).await;
            let id = recording.current.unwrap().id;
            let active = std::fs::read(dir.join("active.json")).unwrap();
            assert!(history.delete(id.clone()).await.is_err());
            assert_eq!(std::fs::read(dir.join("active.json")).unwrap(), active);
            moving.stats.as_mut().unwrap().belt = BeltView::Stopped;
            moving.stats.as_mut().unwrap().speed = 0.0;
            pad_tx.send_replace(moving);
            wait_history(&mut state, |s| {
                s.current.is_none() && s.recent.iter().any(|w| w.id == id)
            })
            .await;
            history.delete(id.clone()).await.unwrap();
            assert!(!saved_path(&dir, &id).unwrap().exists());
            drop(pad_tx);
            std::fs::remove_dir_all(dir).unwrap();
        });
    }

    async fn wait_history(
        state: &mut watch::Receiver<Snapshot>,
        predicate: impl Fn(&Snapshot) -> bool,
    ) -> Snapshot {
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let snapshot = state.borrow_and_update().clone();
                if predicate(&snapshot) {
                    return snapshot;
                }
                state.changed().await.expect("history task still running");
            }
        })
        .await
        .expect("history state updated")
    }

    #[test]
    fn failed_recovery_preserves_checkpoint_while_new_walks_are_buffered() {
        tauri::async_runtime::block_on(async {
            let dir = std::env::temp_dir().join(format!(
                "kc-walk-recovery-{}-{}",
                std::process::id(),
                unix_ms()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let active = dir.join("active.json");
            save(&active, &stored_walk("1000-1", 1000)).unwrap();
            let original = std::fs::read(&active).unwrap();
            // Deterministically fail the recovery write (as full/read-only
            // storage would), while leaving active.json readable/deletable.
            let blocker = dir.join("walk-1000-1.json.tmp");
            std::fs::create_dir(&blocker).unwrap();
            let (pad_tx, pad_rx) = watch::channel(pad());
            let (history, samples) = spawn(dir.clone(), pad_rx);
            let mut state = history.state.clone();
            wait_history(&mut state, |s| s.error.is_some()).await;
            assert_eq!(std::fs::read(&active).unwrap(), original);

            let mut moving = pad();
            moving.stats.as_mut().unwrap().time_s = 101;
            pad_tx.send_replace(moving.clone());
            let recording = wait_history(&mut state, |s| s.current.is_some()).await;
            let new_id = recording.current.unwrap().id;
            samples.send(reading(unix_ms(), 120)).unwrap();
            wait_history(&mut state, |s| {
                s.current.as_ref().is_some_and(|w| w.samples == 1)
            })
            .await;
            assert_eq!(
                std::fs::read(&active).unwrap(),
                original,
                "a new walk must not overwrite unrecovered data"
            );

            let stats = moving.stats.as_mut().unwrap();
            stats.belt = BeltView::Stopped;
            stats.speed = 0.0;
            stats.time_s = 105;
            pad_tx.send_replace(moving);
            wait_history(&mut state, |s| s.current.is_none()).await;
            assert_eq!(
                std::fs::read(&active).unwrap(),
                original,
                "cleanup must not delete unrecovered data"
            );
            assert!(!dir.join(format!("walk-{new_id}.json")).exists());

            std::fs::remove_dir(blocker).unwrap();
            let recovered =
                wait_history(&mut state, |s| s.recent.len() == 2 && s.error.is_none()).await;
            assert_eq!(recovered.recent[0].id, new_id);
            assert_eq!(recovered.recent[0].time_s, 4);
            assert_eq!(recovered.recent[0].average_bpm, Some(120));
            assert_eq!(recovered.recent[1].id, "1000-1");
            assert!(recovered.recent[1].interrupted);
            assert_eq!(recovered.recent[1].average_bpm, Some(95));
            assert!(!active.exists());

            drop(pad_tx);
            drop(samples);
            tokio::time::timeout(Duration::from_secs(1), async {
                while state.changed().await.is_ok() {}
            })
            .await
            .expect("history task stopped when inputs closed");
            std::fs::remove_dir_all(dir).unwrap();
        });
    }

    #[test]
    fn startup_reads_only_the_ten_newest_files_in_numeric_order() {
        let dir = std::env::temp_dir().join(format!(
            "kc-walk-recent-{}-{}",
            std::process::id(),
            unix_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for start in 1..=20 {
            save(
                &dir.join(format!("walk-{start}-1.json")),
                &stored_walk(&format!("{start}-1"), start),
            )
            .unwrap();
        }
        for sequence in [9, 10] {
            save(
                &dir.join(format!("walk-20-{sequence}.json")),
                &stored_walk(&format!("20-{sequence}"), 20),
            )
            .unwrap();
        }
        std::fs::write(dir.join("walk-999-1.json.tmp"), "incomplete write").unwrap();
        let expected = [
            "20-10", "20-9", "20-1", "19-1", "18-1", "17-1", "16-1", "15-1", "14-1", "13-1",
        ];
        let mut reads = Vec::new();
        let recent = load_recent(&dir, |path| {
            let name = path.file_name().unwrap().to_str().unwrap();
            assert!(
                expected.iter().any(|id| name == format!("walk-{id}.json")),
                "historical samples must not be read: {name}"
            );
            reads.push(name.to_owned());
            std::fs::read(path)
        })
        .unwrap();
        assert_eq!(reads, expected.map(|id| format!("walk-{id}.json")));
        assert_eq!(
            recent.iter().map(|w| w.id.as_str()).collect::<Vec<_>>(),
            expected
        );
        assert_eq!(recent[0].average_bpm, Some(95));

        // A damaged selected file must not trigger reads of older sample files.
        std::fs::write(dir.join("walk-20-10.json"), "broken").unwrap();
        let mut reads = 0;
        let recent = load_recent(&dir, |path| {
            reads += 1;
            std::fs::read(path)
        })
        .unwrap();
        assert_eq!(reads, RECENT_LIMIT);
        assert_eq!(recent.len(), RECENT_LIMIT - 1);
        std::fs::remove_dir_all(dir).unwrap();
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
