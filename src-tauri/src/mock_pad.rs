//! A simulated R1 Pro for `--features mock` runs and tests.
//!
//! The app keeps its real code path (session → `BlecTransport` →
//! `tauri-plugin-blec`), but blec's `mock` feature swaps CoreBluetooth for an
//! in-process world, so nothing touches the radio. This adds a device that
//! looks like the real pad to that world (name, `fe00` advertisement,
//! manufacturer data, GATT layout including the firmware-update service) and
//! drives it with `walkingpad::fake::FakePad`: writes to `fe02` go into the
//! model and its replies come back as `fe01` notifications.

use std::time::{Duration, Instant};

use btleplug::api::CharPropFlags;
use tauri_plugin_blec::mock::{DeviceHandle, DeviceSpec, MockWorld, Op, ServiceSpec};
use uuid::{Uuid, uuid};
use walkingpad::fake::{FakePad, PadConfig};

use crate::ble::{NOTIFY, SERVICE, WRITE};

pub const MAC: [u8; 6] = [0x57, 0x4c, 0x4d, 0x0f, 0x62, 0x75];
/// The pad's firmware-update service and its writable characteristic. The
/// app must never write there.
pub const OTA_SERVICE: Uuid = uuid!("00010203-0405-0607-0809-0a0b0c0d1912");
pub const OTA_WRITE: Uuid = uuid!("00010203-0405-0607-0809-0a0b0c0d2b12");

/// A standard heart-rate monitor for exercising the complete UI without radios.
pub fn install_heart_rate() {
    use crate::heart_rate::{MEASUREMENT, SERVICE};
    let monitor = MockWorld::global().add_device(
        DeviceSpec::new([0x57, 0x41, 0x54, 0x43, 0x48, 0x03])
            .name("Pixel Watch 3 (simulated)")
            .service(ServiceSpec::new(SERVICE).characteristic(MEASUREMENT, CharPropFlags::NOTIFY)),
    );
    tauri::async_runtime::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut bpm = 90;
        loop {
            tick.tick().await;
            monitor.notify(SERVICE, MEASUREMENT, vec![6, bpm]);
            bpm = if bpm >= 110 { 90 } else { bpm + 1 };
        }
    });
}

/// Adds the simulated pad to blec's global mock world and starts driving it.
pub fn install() -> DeviceHandle {
    let device = MockWorld::global().add_device(
        DeviceSpec::new(MAC)
            .name("R1 Pro")
            .manufacturer_data(0x0060, MAC.to_vec())
            .advertised_services(vec![SERVICE])
            .mtu(23)
            .service(
                ServiceSpec::new(SERVICE)
                    .characteristic(NOTIFY, CharPropFlags::READ | CharPropFlags::NOTIFY)
                    .characteristic(WRITE, CharPropFlags::WRITE_WITHOUT_RESPONSE),
            )
            .service(ServiceSpec::new(OTA_SERVICE).characteristic(
                OTA_WRITE,
                CharPropFlags::READ | CharPropFlags::WRITE | CharPropFlags::WRITE_WITHOUT_RESPONSE,
            )),
    );
    let handle = device.clone();
    tauri::async_runtime::spawn(async move {
        let mut pad = FakePad::new(PadConfig::default());
        // `KC_MOCK_RUNNING=1`: the belt is already moving, as if started from
        // the remote (handy for looking at the dashboard).
        if std::env::var_os("KC_MOCK_RUNNING").is_some() {
            pad.set_running(20);
        }
        let mut seen = 0usize;
        let mut last = Instant::now();
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        loop {
            tick.tick().await;
            let now = Instant::now();
            pad.advance(now - last);
            last = now;
            let ops = device.ops();
            for op in ops.iter().skip(seen) {
                match op {
                    Op::Subscribe(c) if *c == NOTIFY => pad.on_subscribe(),
                    Op::Disconnect => pad.on_disconnect(),
                    Op::Write(c, data, _) if *c == WRITE => {
                        let latency = pad.cfg.reply_latency;
                        for reply in pad.receive(data) {
                            let device = device.clone();
                            tauri::async_runtime::spawn(async move {
                                tokio::time::sleep(latency).await;
                                device.notify(SERVICE, NOTIFY, reply);
                            });
                        }
                    }
                    _ => {}
                }
            }
            seen = ops.len();
            // The op log only grows; trim it now and then. Writes are 700 ms
            // apart, so the window for losing one here is negligible.
            if seen > 512 && !cfg!(test) {
                device.clear_ops();
                seen = 0;
            }
        }
    });
    handle
}
