//! End to end without a radio: session → `BlecTransport` → blec (mock
//! backend) → simulated pad. Run with `cargo test --features mock`.

use std::time::Duration;

use tauri_plugin_blec::mock::Op;
use walkingpad::session::{self, BeltView, Config, Intent, LinkPhase, SessionHandle, Snapshot};

use crate::{ble, mock_pad};

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
        let (tx, rx) = walkingpad::transport::event_channel();
        let (s, h) = session::new(ble::BlecTransport::new(tx), rx, Config::default());
        tauri::async_runtime::spawn(s.run());

        // First run: the pad shows up in the fe00 scan, MAC from the
        // manufacturer data; nothing connects until asked.
        let snap = wait(&h, "scan results", 10, |s| !s.discovered.is_empty()).await;
        let ad = snap.discovered[0].clone();
        assert_eq!(ad.mac.as_deref(), Some("57:4C:4D:0F:62:75"));
        assert_eq!(ad.name.as_deref(), Some("R1 Pro"));
        assert!(!device.is_connected());

        h.send(Intent::Connect { id: ad.id.clone() });
        let snap = wait(&h, "ready", 10, |s| {
            s.link == LinkPhase::Ready && s.stats.is_some()
        })
        .await;
        assert_eq!(
            snap.remembered.unwrap().mac.as_deref(),
            Some("57:4C:4D:0F:62:75")
        );

        h.send(Intent::Start);
        wait(&h, "running", 10, |s| {
            s.stats
                .as_ref()
                .is_some_and(|st| st.belt == BeltView::Running && st.speed > 0.0)
        })
        .await;
        h.send(Intent::AdjustSpeed { steps: 2 });
        tokio::time::sleep(Duration::from_millis(1600)).await;
        h.send(Intent::Stop);
        wait(&h, "stopping", 2, |s| s.stopping).await;
        wait(&h, "stop confirmed", 10, |s| !s.stopping).await;
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
    });
}
