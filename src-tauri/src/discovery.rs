//! blec owns one scanner. Multiplex it so searching for a heart-rate monitor
//! cannot cancel WalkingPad discovery (or vice versa).

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use tauri_plugin_blec::{
    Connection, OnDisconnectHandler,
    models::{AdapterState, BleDevice, ScanFilter},
};
use tokio::sync::{Mutex, mpsc, watch};
use walkingpad::transport::{Advertisement, EventSender, TransportError, TransportEvent};

use crate::{
    ble::{self, handler, map_err},
    heart_rate,
};

#[derive(Default)]
struct ScanState {
    pad: bool,
    heart: bool,
    running: bool,
    generation: u64,
}

#[derive(Clone)]
pub struct Discovery {
    state: Arc<Mutex<ScanState>>,
    connecting: Arc<AtomicUsize>,
    pad_events: EventSender,
    heart_devices: watch::Sender<Vec<Advertisement>>,
}

impl Discovery {
    pub fn new(pad_events: EventSender) -> Self {
        Self {
            state: Arc::new(Mutex::new(ScanState::default())),
            connecting: Arc::new(AtomicUsize::new(0)),
            pad_events,
            heart_devices: watch::channel(Vec::new()).0,
        }
    }

    pub fn heart_devices(&self) -> watch::Receiver<Vec<Advertisement>> {
        self.heart_devices.subscribe()
    }

    pub async fn pad(&self, enabled: bool) -> Result<(), TransportError> {
        self.set(true, enabled).await
    }

    pub async fn heart(&self, enabled: bool) -> Result<(), TransportError> {
        self.set(false, enabled).await
    }

    async fn set(&self, pad: bool, enabled: bool) -> Result<(), TransportError> {
        let h = handler().await?;
        if enabled {
            if !tauri_plugin_blec::check_permissions(true)
                .await
                .map_err(map_err)?
            {
                return Err(TransportError::PermissionDenied);
            }
            if matches!(h.get_adapter_state().await, AdapterState::Off) {
                return Err(TransportError::AdapterOff);
            }
        }
        let mut state = self.state.lock().await;
        if pad {
            state.pad = enabled;
        } else {
            state.heart = enabled;
            if enabled {
                self.heart_devices.send_replace(Vec::new());
            }
        }
        if !state.pad && !state.heart {
            state.running = false;
            state.generation += 1;
            return h.stop_scan().await.map_err(map_err);
        }
        self.ensure_running(&mut state).await
    }

    async fn ensure_running(&self, state: &mut ScanState) -> Result<(), TransportError> {
        if state.running
            || (!state.pad && !state.heart)
            || self.connecting.load(Ordering::Acquire) > 0
        {
            return Ok(());
        }
        let mut rx = Self::scan().await?;
        state.running = true;
        state.generation += 1;
        let generation = state.generation;
        let this = self.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                // Always drain: blec's scan producer expects a live receiver,
                // even after its last consumer has cancelled.
                while let Some(devices) = rx.recv().await {
                    let state = this.state.lock().await;
                    if state.generation != generation {
                        continue;
                    }
                    if state.pad {
                        let _ = this.pad_events.send(TransportEvent::ScanResults(matching(
                            &devices,
                            ble::SERVICE,
                        )));
                    }
                    if state.heart {
                        this.heart_devices.send_if_modified(|last| {
                            let next = matching(&devices, heart_rate::SERVICE);
                            if *last == next {
                                return false;
                            }
                            *last = next;
                            true
                        });
                    }
                }
                let mut state = this.state.lock().await;
                if state.generation != generation || (!state.pad && !state.heart) {
                    return;
                }
                // Connecting stops blec's scanner. Wait without holding the
                // mutex: pad Stop/visibility/disconnect must remain responsive.
                if this.connecting.load(Ordering::Acquire) > 0 {
                    drop(state);
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    continue;
                }
                match Self::scan().await {
                    Ok(next) => rx = next,
                    Err(e) => {
                        state.running = false;
                        crate::applog::line(&format!("scan restart failed: {e}"));
                        return;
                    }
                }
            }
        });
        Ok(())
    }

    async fn scan() -> Result<mpsc::Receiver<Vec<BleDevice>>, TransportError> {
        let (tx, rx) = mpsc::channel(16);
        handler()
            .await?
            .discover(
                Some(tx),
                60_000,
                ScanFilter::AnyService(vec![ble::SERVICE, heart_rate::SERVICE]),
                false,
            )
            .await
            .map_err(map_err)?;
        Ok(rx)
    }

    pub async fn connect(
        &self,
        id: &str,
        on_disconnect: OnDisconnectHandler,
    ) -> Result<Connection, TransportError> {
        let _guard = {
            // Serialize only the transition into connecting with scan startup,
            // never the connection attempt itself.
            let _state = self.state.lock().await;
            self.connecting.fetch_add(1, Ordering::AcqRel);
            Connecting(self.clone())
        };
        handler()
            .await?
            .connect(id, on_disconnect, false)
            .await
            .map_err(map_err)
    }
}

fn matching(devices: &[BleDevice], service: uuid::Uuid) -> Vec<Advertisement> {
    devices
        .iter()
        .filter(|d| d.services.contains(&service))
        .map(ble::to_ad)
        .collect()
}

// Also resumes discovery if a caller cancels a connection future. This guard
// tracks in-flight connections without taking the pad session's scan mutex.
struct Connecting(Discovery);
impl Drop for Connecting {
    fn drop(&mut self) {
        self.0.connecting.fetch_sub(1, Ordering::AcqRel);
        let discovery = self.0.clone();
        tauri::async_runtime::spawn(async move {
            let mut state = discovery.state.lock().await;
            if let Err(e) = discovery.ensure_running(&mut state).await {
                crate::applog::line(&format!("scan resume failed: {e}"));
            }
        });
    }
}
