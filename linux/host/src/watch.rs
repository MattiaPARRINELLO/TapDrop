//! The live link: the one connection a phone keeps open while its screen is on.
//!
//! The phone pushes short messages (a heartbeat, what it can do, its name); the host pushes what it
//! has for the phone (its queue, and later a light-key round). One phone has one live link, so a new
//! one replaces the old and grabs never go down a stale connection.

use crate::store::Store;
use crate::tap::TapService;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use tapdrop_protocol::lightkey::Candidate;
use tapdrop_protocol::wire::{Msg, SecureChannel};
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::Mutex;

/// A phone that has said nothing for this long is gone: it sends `ka` every 15 s.
pub const SILENCE_LIMIT: std::time::Duration = std::time::Duration::from_secs(45);

pub struct LiveLink {
    pub device_id: String,
    /// Which light-key version the phone announced (`caps`), 0 when it never said.
    pub key_version: AtomicU8,
    /// When this link opened, which is how a trigger picks the newest phone.
    pub since: std::time::Instant,
    writer: Mutex<SecureChannel<OwnedWriteHalf>>,
}

impl LiveLink {
    pub fn from_writer(device_id: &str, writer: SecureChannel<OwnedWriteHalf>) -> Self {
        Self {
            device_id: device_id.to_string(),
            key_version: AtomicU8::new(0),
            since: std::time::Instant::now(),
            writer: Mutex::new(writer),
        }
    }

    pub async fn send(&self, v: Value) -> anyhow::Result<()> {
        self.writer.lock().await.send_json(&v).await
    }
}

pub struct Hub {
    links: Mutex<HashMap<usize, Arc<LiveLink>>>,
    next_id: AtomicUsize,
    /// The host has a light sensor it can tap with. Always false until the light key exists.
    pub notch: bool,
}

impl Hub {
    pub fn new(notch: bool) -> Arc<Self> {
        Arc::new(Self {
            links: Mutex::new(HashMap::new()),
            next_id: AtomicUsize::new(1),
            notch,
        })
    }

    pub async fn add(&self, link: Arc<LiveLink>) -> usize {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut links = self.links.lock().await;
        // One live link per phone: the newest wins, the old one is dropped (its reader then ends).
        links.retain(|_, l| l.device_id != link.device_id);
        links.insert(id, link);
        id
    }

    pub async fn remove(&self, id: usize) {
        self.links.lock().await.remove(&id);
    }

    /// A message for one phone (a grab, an answer, a queued file).
    pub async fn push_device(&self, device_id: &str, v: Value) {
        let link = {
            let links = self.links.lock().await;
            links.values().find(|l| l.device_id == device_id).cloned()
        };
        if let Some(link) = link {
            if let Err(e) = link.send(v).await {
                eprintln!("tapdrop: live link to {} failed: {e:#}", device_id);
            }
        }
    }

    pub async fn push_all(&self, v: Value) {
        let links: Vec<Arc<LiveLink>> = self.links.lock().await.values().cloned().collect();
        for link in links {
            let _ = link.send(v.clone()).await;
        }
    }

    #[allow(dead_code)]
    pub async fn is_live(&self, device_id: &str) -> bool {
        self.links
            .lock()
            .await
            .values()
            .any(|l| l.device_id == device_id)
    }

    /// The phones a tap could be aimed at, newest first.
    pub async fn candidates(&self) -> Vec<Candidate> {
        let links = self.links.lock().await;
        links
            .values()
            .map(|l| Candidate {
                device_id: l.device_id.clone(),
                caps: l.key_version.load(Ordering::Relaxed),
                last_heard: l.since,
            })
            .collect()
    }
}

/// Reads a live link until the phone goes quiet, closes it, or the host stops the session.
pub async fn run(
    store: Arc<Store>,
    hub: Arc<Hub>,
    tap: Arc<TapService>,
    link_id: usize,
    link: Arc<LiveLink>,
    mut reader: SecureChannel<OwnedReadHalf>,
) {
    loop {
        let msg = match tokio::time::timeout(SILENCE_LIMIT, reader.recv()).await {
            Err(_) => {
                eprintln!(
                    "tapdrop: nothing from {} for 45 s, dropping its live link",
                    link.device_id
                );
                break;
            }
            Ok(Err(e)) => {
                eprintln!("tapdrop: live link ended: {e:#}");
                break;
            }
            Ok(Ok(msg)) => msg,
        };
        let value = match msg {
            Msg::Json(v) => v,
            Msg::Data(_) => continue,
        };
        if !handle(&store, &tap, &link, &value).await {
            continue;
        }
        store.touch(&link.device_id);
    }
    hub.remove(link_id).await;
}

/// Returns true when the message was a heartbeat (the only one that counts as "still here").
async fn handle(store: &Store, tap: &Arc<TapService>, link: &LiveLink, m: &Value) -> bool {
    match m["t"].as_str().unwrap_or("") {
        "ka" => {
            let _ = link.send(json!({ "t": "ka" })).await;
            true
        }
        "caps" => {
            let version = m["key"].as_u64().unwrap_or(0).min(255) as u8;
            link.key_version.store(version, Ordering::Relaxed);
            false
        }
        "names" => {
            if let Some(answer) = store.take_names(&link.device_id, m) {
                let _ = link.send(answer).await;
            }
            false
        }
        "knock" | "key-on" | "key-off" | "grab-ack" | "grab-cancel" => {
            let caps = link.key_version.load(Ordering::Relaxed);
            tap.phone(&link.device_id, caps, m).await;
            false
        }
        "diag" | "index" | "tap-note" => false,
        _ => false,
    }
}
