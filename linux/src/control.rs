//! The control socket: one JSON line in, one JSON line out.
//!
//! It is how a terminal (or a tray, later) talks to the running service: queue a file, approve a
//! pairing, read the state. It lives in the runtime directory, so only this user can reach it.

use crate::server::{self, Host};
use crate::store::Store;
use crate::watch::Hub;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

pub fn socket_path() -> PathBuf {
    crate::store::runtime_dir().join("control.sock")
}

/// Sends one request to the running service and returns its answer.
pub async fn request(value: &Value) -> Result<Value> {
    let mut stream = UnixStream::connect(socket_path())
        .await
        .context("le service TapDrop ne tourne pas (lance « tapdrop run »)")?;
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    stream.write_all(&line).await?;
    let mut reader = BufReader::new(stream);
    let mut answer = String::new();
    reader.read_line(&mut answer).await?;
    Ok(serde_json::from_str(&answer)?)
}

/// The same request, for the window: GTK owns its thread and cannot await.
pub fn request_blocking(value: &Value) -> Result<Value> {
    use std::io::{BufRead, BufReader, Write};
    let mut stream = std::os::unix::net::UnixStream::connect(socket_path())
        .context("le service TapDrop ne tourne pas")?;
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    let mut answer = String::new();
    BufReader::new(stream).read_line(&mut answer)?;
    Ok(serde_json::from_str(&answer)?)
}

pub struct Control {
    host: Arc<Host>,
    hub: Arc<Hub>,
    port: u16,
    shutdown: Arc<tokio::sync::Notify>,
}

impl Control {
    pub fn new(host: Arc<Host>, hub: Arc<Hub>, port: u16, shutdown: Arc<tokio::sync::Notify>) -> Self {
        Self { host, hub, port, shutdown }
    }

    pub async fn run(self) -> Result<()> {
        let dir = crate::store::runtime_dir();
        tokio::fs::create_dir_all(&dir).await?;
        let path = socket_path();
        if path.exists() {
            // A leftover socket from a crash: it cannot answer, so remove it.
            if UnixStream::connect(&path).await.is_err() {
                let _ = tokio::fs::remove_file(&path).await;
            }
        }
        let listener = UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
        loop {
            let (stream, _) = listener.accept().await?;
            let this = Reply {
                host: self.host.clone(),
                hub: self.hub.clone(),
                port: self.port,
                shutdown: self.shutdown.clone(),
            };
            tokio::spawn(async move {
                if let Err(e) = this.serve(stream).await {
                    eprintln!("tapdrop: control request failed: {e:#}");
                }
            });
        }
    }
}

struct Reply {
    host: Arc<Host>,
    hub: Arc<Hub>,
    port: u16,
    shutdown: Arc<tokio::sync::Notify>,
}

impl Reply {
    async fn serve(&self, stream: UnixStream) -> Result<()> {
        let (read, mut write) = stream.into_split();
        let mut line = String::new();
        BufReader::new(read).read_line(&mut line).await?;
        let request: Value = serde_json::from_str(line.trim())?;
        let answer = self.handle(request).await;
        let mut body = serde_json::to_vec(&answer)?;
        body.push(b'\n');
        write.write_all(&body).await?;
        Ok(())
    }

    async fn handle(&self, request: Value) -> Value {
        let store = self.host.store.clone();
        match request["cmd"].as_str().unwrap_or("") {
            "status" => {
                let state = store.snapshot();
                json!({
                    "ok": true, "name": state.name, "port": self.port, "host_id": state.id,
                    "save_folder": state.save_folder,
                    "open_links": state.open_links,
                    "devices": state.devices.iter().map(|d| json!({
                        "id": d.id, "name": d.name, "last_seen": d.last_seen,
                        "live": false,
                    })).collect::<Vec<_>>(),
                    "outbox": state.outbox.len(),
                    "pending": self.host.pending.list().await
                        .iter().map(|(code, device)| json!({"code": code, "device": device}))
                        .collect::<Vec<_>>(),
                    "addresses": crate::net::reachable().iter().map(|a| json!({
                        "iface": a.iface, "ip": a.ip, "kind": a.kind,
                        "endpoint": a.endpoint(self.port),
                    })).collect::<Vec<_>>(),
                    "notch": self.hub.notch,
                })
            }
            "send" => {
                let paths: Vec<PathBuf> = request["paths"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|v| v.as_str()).map(PathBuf::from).collect())
                    .unwrap_or_default();
                let mut queued = 0;
                for path in paths {
                    let canonical = tokio::fs::canonicalize(&path).await.unwrap_or(path);
                    if tokio::fs::metadata(&canonical).await.is_ok() {
                        store.queue(server::new_queue_item(Some(canonical), None));
                        queued += 1;
                    }
                }
                if request["clipboard"].as_bool().unwrap_or(false) {
                    let snapshot = tokio::task::spawn_blocking(crate::clipboard::read).await.ok();
                    let path = queue_clipboard(&store, snapshot).await;
                    if path {
                        queued += 1;
                    }
                }
                self.host.announce_inbox().await;
                json!({ "ok": true, "queued": queued })
            }
            "clipboard" => {
                let snapshot = tokio::task::spawn_blocking(crate::clipboard::read).await.ok();
                let queued = queue_clipboard(&store, snapshot).await;
                self.host.announce_inbox().await;
                json!({ "ok": true, "queued": queued })
            }
            "approve" => {
                let code = request["code"].as_str().unwrap_or_default();
                let done = self.host.pending.approve(code).await;
                json!({ "ok": done })
            }
            "deny" => {
                let code = request["code"].as_str().unwrap_or_default();
                let done = self.host.pending.deny(code).await;
                json!({ "ok": done })
            }
            "pending" => json!({
                "ok": true,
                "pending": self.host.pending.list().await
                    .iter().map(|(code, device)| json!({"code": code, "device": device}))
                    .collect::<Vec<_>>(),
            }),
            "forget" => {
                let id = request["device"].as_str().unwrap_or_default();
                store.remove_device(id);
                json!({ "ok": true })
            }
            "links" => {
                let enabled = request["enabled"].as_bool().unwrap_or(true);
                store.set_open_links(enabled);
                json!({ "ok": true })
            }
            "rename" => {
                let name = request["name"].as_str().unwrap_or_default();
                if name.trim().is_empty() {
                    json!({ "ok": false, "error": "nom vide" })
                } else {
                    store.rename(name.trim());
                    json!({ "ok": true })
                }
            }
            "folder" => {
                let path = request["path"].as_str().unwrap_or_default();
                let expanded = shellexpand_home(path);
                if tokio::fs::create_dir_all(&expanded).await.is_ok() {
                    store.set_folder(expanded.clone());
                    json!({ "ok": true, "path": expanded })
                } else {
                    json!({ "ok": false, "error": "dossier impossible à créer" })
                }
            }
            "quit" => {
                self.shutdown.notify_waiters();
                json!({ "ok": true })
            }
            other => json!({ "ok": false, "error": format!("commande inconnue : {other}") }),
        }
    }
}

pub async fn queue_clipboard(store: &Store, snapshot: Option<crate::clipboard::Snapshot>) -> bool {
    use crate::clipboard::Snapshot;
    match snapshot {
        Some(Snapshot::Text(text)) => {
            store.queue(server::new_queue_item(None, Some(text)));
            true
        }
        Some(Snapshot::Image(png)) => {
            let path = std::env::temp_dir().join(format!("tapdrop-clip-{}.png", uuid::Uuid::new_v4()));
            if tokio::fs::write(&path, png).await.is_ok() {
                store.queue(server::new_queue_item(Some(path), None));
                return true;
            }
            false
        }
        Some(Snapshot::Files(paths)) => {
            let mut any = false;
            for path in paths {
                store.queue(server::new_queue_item(Some(path), None));
                any = true;
            }
            any
        }
        _ => false,
    }
}

fn shellexpand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
        None => PathBuf::from(path),
    }
}

