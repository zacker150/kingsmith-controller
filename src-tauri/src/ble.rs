//! [`Transport`] over `tauri-plugin-blec` 0.18 (`blec`): btleplug on desktop,
//! the plugin's Kotlin backend on Android.
//!
//! Knows GATT only: pad discovery via the shared scanner, the `fe01`/`fe02` check,
//! notifications and write-without-response. The protocol lives in
//! `walkingpad`.
//!
//! **Writes go to `fe02` and nowhere else.** The pad also has a firmware
//! update service (`00010203-…-0a0b0c0d1912`) with a writable characteristic
//! (`…2b12`). The connection is wrapped in [`PadLink`], whose only write
//! method targets the hard-coded `fe02`, and this file contains exactly one
//! GATT write call (checked by a test).

use std::sync::Mutex;
use std::time::Duration;

use tauri_plugin_blec::models::{BleDevice, CharProps, WriteType};
use tauri_plugin_blec::{Connection, Handler, OnDisconnectHandler, Timeouts};
use uuid::{Uuid, uuid};
use walkingpad::protocol::{mac_from_manufacturer_data, normalize_mac};
use walkingpad::transport::{
    Advertisement, EventSender, LinkId, Transport, TransportError, TransportEvent,
};

use crate::applog;

pub const SERVICE: Uuid = uuid!("0000fe00-0000-1000-8000-00805f9b34fb");
pub const NOTIFY: Uuid = uuid!("0000fe01-0000-1000-8000-00805f9b34fb");
/// The only characteristic this app ever writes to.
pub const WRITE: Uuid = uuid!("0000fe02-0000-1000-8000-00805f9b34fb");

/// A connection to the pad. The `Connection` is private, so nothing outside
/// these methods can write to it, and the one write targets `fe02`.
#[derive(Clone)]
struct PadLink(Connection);

impl PadLink {
    /// Writes one command frame to `fe02`, without response.
    async fn write_command(&self, frame: &[u8]) -> Result<(), tauri_plugin_blec::Error> {
        self.0
            .send_data(WRITE, Some(SERVICE), frame, WriteType::WithoutResponse)
            .await
    }

    async fn subscribe_status(
        &self,
        on_frame: impl Fn(Vec<u8>) + Send + Sync + 'static,
    ) -> Result<(), tauri_plugin_blec::Error> {
        self.0.subscribe(NOTIFY, Some(SERVICE), on_frame).await
    }

    async fn disconnect(&self) -> Result<(), tauri_plugin_blec::Error> {
        self.0.disconnect().await
    }
}

pub struct BlecTransport {
    events: EventSender,
    discovery: crate::discovery::Discovery,
    link: Mutex<Option<(LinkId, PadLink)>>,
    next_link: Mutex<LinkId>,
}

impl BlecTransport {
    #[must_use]
    pub fn new(events: EventSender, discovery: crate::discovery::Discovery) -> Self {
        Self {
            events,
            discovery,
            link: Mutex::new(None),
            next_link: Mutex::new(1),
        }
    }

    fn conn(&self, link: LinkId) -> Result<PadLink, TransportError> {
        match &*self
            .link
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            Some((l, c)) if *l == link => Ok(c.clone()),
            _ => Err(TransportError::NotConnected),
        }
    }
}

/// The handler. On Android the plugin initializes it only after the app is
/// ready, so wait for it.
pub(crate) async fn handler() -> Result<&'static Handler, TransportError> {
    let started = tokio::time::Instant::now();
    loop {
        match tauri_plugin_blec::get_handler() {
            Ok(h) => {
                static TIMEOUTS: std::sync::Once = std::sync::Once::new();
                TIMEOUTS.call_once(|| {
                    // blec retries a connect 3 times; a pad that isn't advertising
                    // never answers, so keep each try short (it connects in 0.3 s).
                    h.set_timeouts(Timeouts {
                        connect: Duration::from_secs(5),
                        discover_services: Duration::from_secs(10),
                        read: Duration::from_secs(5),
                        write: Duration::from_secs(2),
                        subscribe: Duration::from_secs(5),
                        disconnect: Duration::from_secs(3),
                    });
                });
                return Ok(h);
            }
            Err(e) if started.elapsed() > Duration::from_secs(15) => {
                return Err(TransportError::Other(format!(
                    "Bluetooth not initialized: {e}"
                )));
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
}

pub(crate) fn map_err(e: tauri_plugin_blec::Error) -> TransportError {
    use tauri_plugin_blec::Error as E;
    match e {
        E::Timeout(_) => TransportError::Timeout,
        E::NoDeviceConnected | E::DeviceNotConnected(_) => TransportError::NotConnected,
        E::UnknownPeripheral(_) => TransportError::NotFound,
        other => {
            let text = other.to_string();
            let lower = text.to_lowercase();
            if lower.contains("permission")
                || lower.contains("unauthorized")
                || lower.contains("not authorized")
            {
                TransportError::PermissionDenied
            } else {
                TransportError::Other(text)
            }
        }
    }
}

pub(crate) fn to_ad(d: &BleDevice) -> Advertisement {
    // Android hands out the MAC as the address; elsewhere (macOS) the
    // manufacturer data is the only place it shows (design §5.2).
    let from_address = if cfg!(target_os = "android") {
        normalize_mac(&d.address)
    } else {
        None
    };
    let mut keys: Vec<_> = d.manufacturer_data.keys().copied().collect();
    keys.sort_unstable();
    let from_mfr = keys
        .iter()
        .find_map(|k| mac_from_manufacturer_data(&d.manufacturer_data[k]));
    Advertisement {
        id: d.address.clone(),
        name: (!d.name.is_empty() && d.name != d.address).then(|| d.name.clone()),
        mac: from_address.or(from_mfr),
        rssi: d.rssi,
    }
}

impl Transport for BlecTransport {
    async fn start_scan(&self) -> Result<(), TransportError> {
        self.discovery.pad(true).await
    }

    async fn stop_scan(&self) -> Result<(), TransportError> {
        self.discovery.pad(false).await
    }

    async fn connect(&self, id: &str) -> Result<LinkId, TransportError> {
        let link = {
            let mut n = self
                .next_link
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *n += 1;
            *n
        };
        let events = self.events.clone();
        let on_disconnect = OnDisconnectHandler::from_sync(move || {
            let _ = events.send(TransportEvent::Disconnected { link });
        });
        let conn = self.discovery.connect(id, on_disconnect).await?;
        if let Err(why) = check_gatt(&conn).await {
            applog::line(&format!("{id}: {why}"));
            let _ = conn.disconnect().await;
            return Err(TransportError::Incompatible(why));
        }
        *self
            .link
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((link, PadLink(conn)));
        Ok(link)
    }

    async fn subscribe(&self, link: LinkId) -> Result<(), TransportError> {
        let conn = self.conn(link)?;
        let events = self.events.clone();
        conn.subscribe_status(move |data: Vec<u8>| {
            let _ = events.send(TransportEvent::Notification { link, data });
        })
        .await
        .map_err(map_err)
    }

    async fn write(&self, link: LinkId, frame: &[u8]) -> Result<(), TransportError> {
        self.conn(link)?.write_command(frame).await.map_err(map_err)
    }

    async fn disconnect(&self, link: LinkId) -> Result<(), TransportError> {
        let conn = self.conn(link)?;
        *self
            .link
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        conn.disconnect().await.map_err(map_err)
    }
}

/// `fe01` must notify and `fe02` must take writes without response.
async fn check_gatt(conn: &Connection) -> Result<(), String> {
    let services = conn
        .services()
        .await
        .map_err(|e| format!("service discovery failed: {e}"))?;
    let service = services
        .iter()
        .find(|s| s.uuid == SERVICE)
        .ok_or("service fe00 missing")?;
    let has = |uuid: Uuid, prop: CharProps| {
        service
            .characteristics
            .iter()
            .any(|c| c.uuid == uuid && c.properties.contains(prop))
    };
    if !has(NOTIFY, CharProps::Notify) {
        return Err("fe01 has no notify".into());
    }
    if !has(WRITE, CharProps::WriteWithoutResponse) {
        return Err("fe02 has no write-without-response".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// No code path in this file can write to a characteristic other than
    /// `fe02`: there is exactly one GATT write call, and it names `WRITE`.
    #[test]
    fn the_only_gatt_write_targets_fe02() {
        let source = include_str!("ble.rs");
        let code: String = source
            .split("#[cfg(test)]")
            .next()
            .unwrap()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        for api in [
            "send_data_to",
            "send_string",
            "recv_data",
            "write_descriptor",
        ] {
            assert!(!code.contains(api), "unexpected GATT call {api}");
        }
        let writes: Vec<&str> = code
            .match_indices(".send_data(")
            .map(|(i, _)| &code[i..])
            .collect();
        assert_eq!(writes.len(), 1, "exactly one write call");
        assert!(
            writes[0].starts_with(".send_data(WRITE,Some(SERVICE),"),
            "{}",
            &writes[0][..40]
        );
        assert_eq!(
            super::WRITE.to_string(),
            "0000fe02-0000-1000-8000-00805f9b34fb"
        );
        assert_eq!(
            super::SERVICE.to_string(),
            "0000fe00-0000-1000-8000-00805f9b34fb"
        );
    }
}
