//! Ties the sensors, the camera and the live links to the light key.

use crate::camera;
use crate::island::Push;
use crate::iio::{Lux, Proximity};
use crate::lightkey::{Action, LightKey};
use crate::watch::Hub;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What the person should change, in the same words the phone uses.
fn hint_for(why: &str) -> Option<&'static str> {
    match why {
        "facing" | "screen-only" => Some("Retourne le téléphone, écran vers la bosse"),
        "not-seen" | "no-light-sent" => Some("Colle l'écran à plat sur la bosse"),
        "stayed-lit" | "no-key-off" => Some("Garde le téléphone sur la bosse une seconde de plus"),
        "busy" | "cooldown" | "late" | "other-phone" => Some("Un instant, réessaie"),
        _ => None,
    }
}

/// How long a round may keep the camera open, LED included.
const CAMERA_LIMIT: Duration = Duration::from_secs(6);

/// The camera's frame mean is 0…255; scaling it by eight puts it in the same range as the light
/// sensor's lux on this machine (room ≈ 380, phone screen ≈ 1380, black screen ≈ 20), so the light
/// key's thresholds and its learning work unchanged.
const CAMERA_TO_LUX: f32 = 8.0;

pub struct TapService {
    key: Mutex<LightKey>,
    hub: Arc<Hub>,
    island: Push,
    /// Use the camera as the light source; otherwise the ambient light sensor feeds the key.
    camera: bool,
    camera_busy: AtomicBool,
}

impl TapService {
    pub fn new(hub: Arc<Hub>, camera: bool, island: Push) -> Arc<Self> {
        Arc::new(Self {
            key: Mutex::new(LightKey::new()),
            hub,
            island,
            camera,
            camera_busy: AtomicBool::new(false),
        })
    }

    pub fn uses_camera(&self) -> bool {
        self.camera
    }

    pub async fn lux(&self, reading: Lux) {
        let actions = self.key.lock().unwrap().on_lux(reading);
        self.send(actions).await;
    }

    pub async fn prox(self: &Arc<Self>, prox: Proximity, at: Instant) {
        let candidates = self.hub.candidates().await;
        let actions = self.key.lock().unwrap().on_prox(prox, at, &candidates);
        self.start_camera_if_grabbing(&actions);
        self.send(actions).await;
    }

    /// A live-link message from a phone: knocks and the light-key answers.
    pub async fn phone(self: &Arc<Self>, device_id: &str, caps: u8, message: &Value) {
        let now = Instant::now();
        let actions = {
            let mut key = self.key.lock().unwrap();
            if message["t"] == "knock" {
                key.on_knock(device_id, caps, now)
            } else {
                key.on_phone(device_id, message, now)
            }
        };
        self.start_camera_if_grabbing(&actions);
        self.send(actions).await;
    }

    pub async fn tick(&self) {
        let actions = self.key.lock().unwrap().tick(Instant::now());
        self.send(actions).await;
    }

    /// The camera only opens for a round: its LED is a visible cost, and other apps may want it.
    fn start_camera_if_grabbing(self: &Arc<Self>, actions: &[Action]) {
        if !self.camera || !actions.iter().any(|a| a.message["t"] == "grab") {
            return;
        }
        // The person needs a sign of life now, not two seconds later: a pulse on the island.
        self.island.detected();
        if self.camera_busy.swap(true, Ordering::SeqCst) {
            return;
        }
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        tokio::task::spawn_blocking(move || {
            if let Err(e) = camera::stream_means(tx) {
                eprintln!("tapdrop: caméra indisponible : {e:#}");
            }
        });
        let this = self.clone();
        tokio::spawn(async move {
            let deadline = Instant::now() + CAMERA_LIMIT;
            while let Some(mean) = rx.recv().await {
                this.lux(Lux { lux: mean * CAMERA_TO_LUX, at: Instant::now() }).await;
                let running = this.key.lock().unwrap().round_running();
                if !running || Instant::now() > deadline {
                    break;
                }
            }
            this.camera_busy.store(false, Ordering::SeqCst);
        });
    }

    async fn send(&self, actions: Vec<Action>) {
        for action in actions {
            if action.message["t"] == "key-fail" {
                if let Some(hint) = hint_for(action.message["why"].as_str().unwrap_or("")) {
                    self.island.hint(hint);
                }
            }
            self.hub.push_device(&action.device_id, action.message).await;
        }
    }
}
