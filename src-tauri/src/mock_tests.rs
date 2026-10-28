//! End to end without a radio: session → `BlecTransport` → blec (mock
//! backend) → simulated pad. Run with `cargo test --features mock`.

use std::time::Duration;

use tauri_plugin_blec::mock::Op;
use walkingpad::session::{self, BeltView, Config, Intent, LinkPhase, SessionHandle, Snapshot};

use crate::{ble, mock_pad};

async fn watch_until<T: Clone + std::fmt::Debug>(
    rx: &mut tokio::sync::watch::Receiver<T>,
    pred: impl Fn(&T) -> bool,
) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let value = rx.borrow_and_update().clone();
        if pred(&value) {
            return value;
        }
        tokio::time::timeout_at(deadline, rx.changed())
            .await
            .unwrap_or_else(|_| panic!("timed out: {value:?}"))
            .unwrap();
    }
}

async fn wait(
    h: &SessionHandle,
    what: &str,
    secs: u64,
    pred: impl Fn(&Snapshot) -> bool,
) -> Snapshot {
    let mut rx = h.watch();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let snap = rx.borrow_and_update().clone();
        if pred(&snap) {
            return snap;
        }
        if tokio::time::timeout_at(deadline, rx.changed())
            .await
            .is_err()
        {
            panic!("timed out waiting for {what}: {snap:#?}");
        }
    }
}

#[test]
fn blec_transport_drives_the_mock_pad_and_writes_only_to_fe02() {
    // blec's handler is process-global; the plugin initializes it with a
    // blocking call, so do that outside the runtime.
    let _plugin = tauri_plugin_blec::try_init().expect("blec init");
    tauri::async_runtime::block_on(async {
        let device = mock_pad::install();
        use crate::heart_rate::{self, Intent as HeartIntent};
        use btleplug::api::CharPropFlags;
        use tauri_plugin_blec::mock::{ConnectBehaviour, DeviceSpec, MockWorld, ServiceSpec};
        let monitor = MockWorld::global().add_device(
            DeviceSpec::new([0x57, 0x41, 0x54, 0x43, 0x48, 0x03])
                .name("Pixel Watch 3")
                .service(
                    ServiceSpec::new(heart_rate::SERVICE)
                        .characteristic(heart_rate::MEASUREMENT, CharPropFlags::NOTIFY),
                ),
        );
        let (tx, rx) = walkingpad::transport::event_channel();
        let discovery = crate::discovery::Discovery::new(tx.clone());
        let (s, h) = session::new(
            ble::BlecTransport::new(tx, discovery.clone()),
            rx,
            Config::default(),
        );
        let history_dir =
            std::env::temp_dir().join(format!("kc-integration-walks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&history_dir);
        let (mut walks, samples) = crate::walks::spawn(history_dir.clone(), h.watch());
        let heart = heart_rate::spawn(discovery.clone(), samples);
        let mut heart_state = heart.state.clone();
        tauri::async_runtime::spawn(s.run());

        // First run: the pad shows up in the fe00 scan, MAC from the
        // manufacturer data; nothing connects until asked.
        let snap = wait(&h, "scan results", 10, |s| !s.discovered.is_empty()).await;
        let ad = snap.discovered[0].clone();
        assert_eq!(ad.mac.as_deref(), Some("57:4C:4D:0F:62:75"));
        assert_eq!(ad.name.as_deref(), Some("R1 Pro"));
        assert!(!device.is_connected());
        assert_eq!(
            snap.discovered.len(),
            1,
            "heart monitors never enter the pad picker"
        );
        heart.send(HeartIntent::Search).unwrap();
        let found = watch_until(&mut heart_state, |s| !s.devices.is_empty()).await;
        assert_eq!(
            found.devices.len(),
            1,
            "pads never enter the heart monitor picker"
        );
        monitor.set_connect_behaviour(ConnectBehaviour::OkAfter(Duration::from_secs(2)));
        heart
            .send(HeartIntent::Connect {
                id: found.devices[0].id.clone(),
            })
            .unwrap();
        watch_until(&mut heart_state, |s| s.phase == "connecting").await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while !monitor.ops().contains(&Op::Connect) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("watch connection started");
        tokio::time::timeout(Duration::from_millis(200), discovery.pad(false))
            .await
            .expect("a pending watch connection must not block pad scan cancellation")
            .unwrap();
        discovery.pad(true).await.unwrap();
        watch_until(&mut heart_state, |s| s.phase == "connected").await;
        assert!(monitor.is_connected());
        // Connecting the watch stops blec discovery internally; the shared
        // scanner must resume for the pad, without dropping the watch link.
        tokio::time::timeout(Duration::from_secs(1), async {
            while !MockWorld::global().is_scanning() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("pad discovery resumes after the watch connects");
        watch_until(&mut h.watch(), |s| s.scanning && !s.discovered.is_empty()).await;

        h.send(Intent::Connect { id: ad.id.clone() });
        let snap = wait(&h, "ready", 10, |s| {
            s.link == LinkPhase::Ready && s.stats.is_some()
        })
        .await;
        assert_eq!(
            snap.remembered.unwrap().mac.as_deref(),
            Some("57:4C:4D:0F:62:75")
        );
        assert!(
            monitor.is_connected(),
            "connecting the pad keeps the monitor connected"
        );

        h.send(Intent::Start);
        wait(&h, "running", 10, |s| {
            s.stats
                .as_ref()
                .is_some_and(|st| st.belt == BeltView::Running && st.speed > 0.0)
        })
        .await;
        watch_until(&mut walks, |s| s.current.is_some()).await;
        assert!(monitor.notify(
            heart_rate::SERVICE,
            heart_rate::MEASUREMENT,
            vec![1, 120, 0]
        ));
        watch_until(&mut heart_state, |s| s.bpm == Some(120)).await;
        watch_until(&mut walks, |s| {
            s.current.as_ref().is_some_and(|w| w.samples == 1)
        })
        .await;
        monitor.notify(heart_rate::SERVICE, heart_rate::MEASUREMENT, vec![4, 95]);
        watch_until(&mut heart_state, |s| s.bpm.is_none()).await;
        monitor.notify(heart_rate::SERVICE, heart_rate::MEASUREMENT, vec![0]);
        watch_until(&mut heart_state, |s| s.bad_frames == 1).await;
        monitor.notify(heart_rate::SERVICE, heart_rate::MEASUREMENT, vec![0, 140]);
        watch_until(&mut heart_state, |s| s.bpm == Some(140)).await;
        watch_until(&mut walks, |s| {
            s.current.as_ref().is_some_and(|w| w.samples == 2)
        })
        .await;
        watch_until(&mut heart_state, |s| {
            s.bpm.is_none() && s.message.contains("No recent")
        })
        .await;
        assert!(
            device.is_connected(),
            "stale heart rate doesn't disconnect the pad"
        );
        h.send(Intent::AdjustSpeed { steps: 2 });
        tokio::time::sleep(Duration::from_millis(1600)).await;
        h.send(Intent::Stop);
        wait(&h, "stopping", 2, |s| s.stopping).await;
        wait(&h, "stop confirmed", 10, |s| !s.stopping).await;
        let history =
            watch_until(&mut walks, |s| s.current.is_none() && !s.recent.is_empty()).await;
        assert_eq!(history.recent[0].average_bpm, Some(130));
        assert_eq!(history.recent[0].max_bpm, Some(140));
        assert_eq!(history.recent[0].samples, 2);
        let saved: crate::walks::Walk = serde_json::from_slice(
            &std::fs::read(history_dir.join(format!("walk-{}.json", history.recent[0].id)))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(saved.heart_rate.len(), 2);
        assert!(
            saved
                .heart_rate
                .iter()
                .all(|s| s.at_ms >= saved.started_ms && s.at_ms <= saved.ended_ms.unwrap())
        );
        monitor.drop_link();
        watch_until(&mut heart_state, |s| {
            s.phase == "reconnecting" && s.message.contains("lost")
        })
        .await;
        heart
            .send(HeartIntent::SetVisible { visible: false })
            .unwrap();
        watch_until(&mut heart_state, |s| s.phase == "connected").await;
        assert!(
            monitor.is_connected(),
            "watch reconnects even while the UI is hidden"
        );
        assert!(
            heart_state.borrow().bpm.is_none(),
            "old readings are never reused after reconnect"
        );
        monitor.notify(heart_rate::SERVICE, heart_rate::MEASUREMENT, vec![0, 130]);
        watch_until(&mut heart_state, |s| s.bpm == Some(130)).await;
        assert!(
            device.is_connected(),
            "watch link loss doesn't disconnect the pad"
        );
        monitor.drop_link();
        watch_until(&mut heart_state, |s| s.phase == "reconnecting").await;
        heart.send(HeartIntent::Disconnect).unwrap();
        watch_until(&mut heart_state, |s| s.phase == "idle").await;
        let attempts = monitor
            .ops()
            .iter()
            .filter(|op| **op == Op::Connect)
            .count();
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(!monitor.is_connected());
        assert_eq!(
            monitor
                .ops()
                .iter()
                .filter(|op| **op == Op::Connect)
                .count(),
            attempts,
            "manual disconnect cancels scheduled recovery"
        );
        // Cancel a native connection before its delayed result arrives. The
        // late success must close its link rather than silently reconnect.
        heart
            .send(HeartIntent::SetVisible { visible: true })
            .unwrap();
        heart.send(HeartIntent::Search).unwrap();
        let found = watch_until(&mut heart_state, |s| !s.devices.is_empty()).await;
        heart
            .send(HeartIntent::Connect {
                id: found.devices[0].id.clone(),
            })
            .unwrap();
        watch_until(&mut heart_state, |s| s.phase == "connecting").await;
        heart.send(HeartIntent::Disconnect).unwrap();
        watch_until(&mut heart_state, |s| s.phase == "disconnecting").await;
        watch_until(&mut heart_state, |s| s.phase == "idle").await;
        assert!(
            !monitor.is_connected(),
            "late connection success is cleaned up after cancellation"
        );
        h.send(Intent::Disconnect);
        wait(&h, "idle", 10, |s| s.link == LinkPhase::Idle).await;

        let writes: Vec<_> = device
            .ops()
            .into_iter()
            .filter_map(|op| match op {
                Op::Write(c, data, _) => Some((c, data)),
                _ => None,
            })
            .collect();
        assert!(writes.len() >= 6, "{writes:?}");
        for (c, data) in &writes {
            assert_eq!(*c, ble::WRITE, "write to {c}: {data:02x?}");
            assert_ne!(*c, mock_pad::OTA_WRITE);
        }
        let frames: Vec<&Vec<u8>> = writes.iter().map(|(_, d)| d).collect();
        let has = |f: &[u8]| frames.iter().any(|w| w.as_slice() == f);
        assert!(has(&[0xf7, 0xa2, 0x00, 0x00, 0xa2, 0xfd]), "status request");
        assert!(has(&[0xf7, 0xa2, 0x04, 0x01, 0xa7, 0xfd]), "start toggle");
        assert!(has(&[0xf7, 0xa2, 0x02, 0x02, 0xa6, 0xfd]), "standby stop");
        let toggles = frames
            .iter()
            .filter(|w| w.as_slice() == [0xf7, 0xa2, 0x04, 0x01, 0xa7, 0xfd])
            .count();
        assert_eq!(toggles, 1, "the toggle is only ever the one Start");
        assert!(
            monitor.ops().iter().all(|op| !matches!(op, Op::Write(..))),
            "the heart monitor is read-only"
        );
        std::fs::remove_dir_all(history_dir).unwrap();
    });
}
