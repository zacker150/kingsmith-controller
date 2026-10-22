//! The BLE seam between the session and a platform BLE stack (design §4).
//!
//! A transport knows GATT, not the pad protocol: it scans with the `fe00`
//! service filter, connects, checks and subscribes to `fe01`, and writes
//! frames to `fe02` without response. Everything asynchronous that the
//! session didn't ask for (scan results, notifications, lost links) arrives
//! as a [`TransportEvent`] on the channel the transport was built with.

use std::future::Future;

use tokio::sync::mpsc;

/// The pad's vendor service, used as the scan filter.
pub const SERVICE_UUID: &str = "0000fe00-0000-1000-8000-00805f9b34fb";
/// Notify: status frames from the pad.
pub const NOTIFY_UUID: &str = "0000fe01-0000-1000-8000-00805f9b34fb";
/// Write without response: commands to the pad.
pub const WRITE_UUID: &str = "0000fe02-0000-1000-8000-00805f9b34fb";

/// Identifies one connection. Events from an older connection carry an older
/// id, so the session can't mistake them for the current link.
pub type LinkId = u64;

/// One device seen by a scan.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct Advertisement {
    /// The platform's identifier: the MAC on Android, a CoreBluetooth UUID on
    /// macOS (specific to that Mac).
    pub id: String,
    pub name: Option<String>,
    /// The MAC address if known: the device address on Android, or the
    /// manufacturer data when it looks like a MAC (design §5.2).
    pub mac: Option<String>,
    pub rssi: Option<i16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportEvent {
    /// Every device the current scan sees right now (replaces earlier lists).
    ScanResults(Vec<Advertisement>),
    /// One notification from `fe01`. One notification is one frame.
    Notification { link: LinkId, data: Vec<u8> },
    /// The link is gone, whoever closed it.
    Disconnected { link: LinkId },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    /// Bluetooth permission missing (Android runtime permission, macOS
    /// privacy setting).
    PermissionDenied,
    /// The Bluetooth adapter is off.
    AdapterOff,
    /// The device is unknown or not advertising.
    NotFound,
    /// Connected, but `fe01`/`fe02` aren't there with the right properties.
    Incompatible(String),
    Timeout,
    NotConnected,
    Other(String),
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PermissionDenied => write!(f, "Bluetooth permission denied"),
            Self::AdapterOff => write!(f, "Bluetooth is off"),
            Self::NotFound => write!(f, "pad not found"),
            Self::Incompatible(why) => write!(f, "not a compatible pad: {why}"),
            Self::Timeout => write!(f, "timed out"),
            Self::NotConnected => write!(f, "not connected"),
            Self::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for TransportError {}

pub type EventSender = mpsc::UnboundedSender<TransportEvent>;
pub type EventReceiver = mpsc::UnboundedReceiver<TransportEvent>;

/// The channel a transport reports its events on.
#[must_use]
pub fn event_channel() -> (EventSender, EventReceiver) {
    mpsc::unbounded_channel()
}

/// A BLE stack, as the session needs it. All methods may be called from any
/// task; the session never runs two of them for the same link at once,
/// except `disconnect`.
pub trait Transport: Send + Sync + 'static {
    /// Starts a continuous scan with the `fe00` service filter. Results arrive
    /// as [`TransportEvent::ScanResults`] until [`Transport::stop_scan`].
    fn start_scan(&self) -> impl Future<Output = Result<(), TransportError>> + Send;

    fn stop_scan(&self) -> impl Future<Output = Result<(), TransportError>> + Send;

    /// Connects to a scanned device and checks that `fe01` has notify and
    /// `fe02` has write-without-response. On a mismatch it disconnects and
    /// returns [`TransportError::Incompatible`].
    fn connect(&self, id: &str) -> impl Future<Output = Result<LinkId, TransportError>> + Send;

    /// Subscribes to `fe01`. Notifications arrive as events.
    fn subscribe(&self, link: LinkId) -> impl Future<Output = Result<(), TransportError>> + Send;

    /// Writes one frame to `fe02`, without response.
    fn write(
        &self,
        link: LinkId,
        frame: &[u8],
    ) -> impl Future<Output = Result<(), TransportError>> + Send;

    fn disconnect(&self, link: LinkId) -> impl Future<Output = Result<(), TransportError>> + Send;
}
