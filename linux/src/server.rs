//! The TCP server: pairing, sessions, receiving, and what leaves for the phone.
//!
//! The wire format is the one in PROTOCOL.md, so an unmodified TapDrop phone works here. One
//! connection carries one request, except the live link which stays open until the phone goes quiet.

use crate::clipboard::{self, Snapshot};
use crate::crypto::{session_key, HostKeyPair, PairingSecret};
use crate::files::{self, IncomingFile};
use crate::island::Push;
use crate::notify;
use crate::store::{now, QueuedItem, Store};
use crate::tap::TapService;
use crate::watch::{self, Hub, LiveLink};
use crate::wire::{FrameIo, Msg, SecureChannel, DIR_HOST_TO_PHONE, DIR_PHONE_TO_HOST};
use rand::RngCore;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, Mutex};

/// How long a phone waits for the person to approve a pairing before it gives up.
const PAIR_TIMEOUT: Duration = Duration::from_secs(125);
const OPENER_TIMEOUT: Duration = Duration::from_secs(15);

/// A pairing request waiting for the person to compare the code.
pub struct PendingPairs {
    inner: Mutex<Vec<Pending>>,
    next_id: AtomicU64,
}

struct Pending {
    id: u64,
    code: String,
    device: String,
    tx: Option<oneshot::Sender<bool>>,
}

impl PendingPairs {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { inner: Mutex::new(Vec::new()), next_id: AtomicU64::new(1) })
    }

    async fn ask(&self, code: &str, device: &str) -> (u64, oneshot::Receiver<bool>) {
        let (tx, rx) = oneshot::channel();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.inner.lock().await.push(Pending {
            id,
            code: code.to_string(),
            device: device.to_string(),
            tx: Some(tx),
        });
        (id, rx)
    }

    pub async fn list(&self) -> Vec<(String, String)> {
        self.inner.lock().await.iter().map(|p| (p.code.clone(), p.device.clone())).collect()
    }

    /// Approves the request with that code and tells the waiting connection so.
    pub async fn approve(&self, code: &str) -> bool {
        self.decide(code, true).await
    }

    pub async fn deny(&self, code: &str) -> bool {
        self.decide(code, false).await
    }

    async fn decide(&self, code: &str, ok: bool) -> bool {
        let mut pending = self.inner.lock().await;
        if let Some(pos) = pending.iter().position(|p| p.code == code) {
            let mut p = pending.remove(pos);
            if let Some(tx) = p.tx.take() {
                let _ = tx.send(ok);
            }
            return true;
        }
        false
    }

    async fn cancel(&self, id: u64) {
        self.inner.lock().await.retain(|p| p.id != id);
    }
}

pub struct Host {
    pub store: Arc<Store>,
    pub hub: Arc<Hub>,
    pub pending: Arc<PendingPairs>,
    pub island: Push,
    pub tap: Arc<TapService>,
    /// Tests and first runs: take any pairing request without asking.
    pub auto_pair: bool,
}

impl Host {
    pub async fn handle(self: Arc<Self>, stream: TcpStream) {
        let peer = stream.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default();
        let mut io = FrameIo::new(stream);
        let first = match tokio::time::timeout(OPENER_TIMEOUT, io.read_json()).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                eprintln!("tapdrop: {peer} sent no usable opener: {e:#}");
                return;
            }
            Err(_) => {
                eprintln!("tapdrop: {peer} never sent an opener");
                return;
            }
        };
        let result = match first["t"].as_str().unwrap_or("") {
            "pair" => self.pair(io, &first).await,
            "hello" => self.session(io, &first).await,
            other => Err(anyhow::anyhow!("unknown opener {other:?}")),
        };
        if let Err(e) = result {
            eprintln!("tapdrop: connection from {peer} ended: {e:#}");
        }
    }

    async fn pair(&self, mut io: FrameIo<TcpStream>, m: &Value) -> anyhow::Result<()> {
        let device_id = m["device"].as_str().unwrap_or_default().to_string();
        let device_name = m["name"].as_str().unwrap_or("Android").to_string();
        let phone_pub = base64_decode(m["pub"].as_str().unwrap_or_default())?;

        let mine = HostKeyPair::generate();
        let host_pub = mine.public_der();
        let shared = mine.shared_secret(&phone_pub)?;
        let secret = PairingSecret::derive(&shared, &phone_pub, &host_pub);
        let state = self.store.snapshot();

        io.write_json(&json!({
            "t": "pair-wait", "v": 1, "mac": state.id, "name": state.name,
            "pub": base64_encode(&host_pub),
        }))
        .await?;

        let approved = if self.auto_pair {
            true
        } else {
            self.island.code(&device_name, &secret.code);
            notify::pairing(&secret.code, &device_name);
            let (id, rx) = self.pending.ask(&secret.code, &device_name).await;
            let verdict = tokio::time::timeout(PAIR_TIMEOUT, rx).await.ok().and_then(Result::ok);
            self.pending.cancel(id).await;
            verdict.unwrap_or(false)
        };

        if !approved {
            self.island.hide();
            io.write_json(&json!({ "t": "pair-no" })).await?;
            return Ok(());
        }
        self.store.add_device(&device_id, &device_name, secret.key);
        self.island.done("Appairé", &format!("{device_name} peut envoyer"));
        io.write_json(&json!({ "t": "pair-ok" })).await?;
        notify::send("Appairé", &format!("{device_name} peut maintenant envoyer vers {}", state.name));
        Ok(())
    }

    async fn session(&self, io: FrameIo<TcpStream>, m: &Value) -> anyhow::Result<()> {
        let device_id = m["device"].as_str().unwrap_or_default().to_string();
        let phone_nonce = base64_decode(m["nonce"].as_str().unwrap_or_default())?;
        if phone_nonce.len() != 16 {
            anyhow::bail!("bad hello nonce");
        }
        let state = self.store.snapshot();

        let Some(device) = state.device(&device_id) else {
            if let Some(gone) = self.store.forgotten(&device_id) {
                return self.tell_forgotten(io, &gone.key, &phone_nonce, &state).await;
            }
            // "mac" lets the phone tell this apart from another host now answering at that address.
            let mut io = io;
            io.write_json(&json!({ "t": "error", "code": "unpaired", "mac": state.id })).await?;
            return Ok(());
        };
        let pair_key = device.pair_key()?;
        let host_nonce = random_16();

        let mut io = io;
        io.write_json(&json!({
            "t": "hello", "v": 1, "mac": state.id, "name": state.name,
            "nonce": base64_encode(&host_nonce),
        }))
        .await?;

        let key = session_key(&pair_key, &phone_nonce, &host_nonce);
        let (reader, writer) = io.into_inner().into_split();
        let mut reader = SecureChannel::new(reader, &key, DIR_HOST_TO_PHONE, DIR_PHONE_TO_HOST);
        let mut writer = SecureChannel::new(writer, &key, DIR_HOST_TO_PHONE, DIR_PHONE_TO_HOST);

        let request = reader.recv_json().await?;
        self.store.touch(&device_id);
        match request["t"].as_str().unwrap_or("") {
            "ping" => {
                writer
                    .send_json(&json!({
                        "t": "pong", "name": state.name, "pending": self.store.outbox_len(),
                    }))
                    .await?;
            }
            "send" => {
                if let Err(e) = self
                    .receive_batch(&mut reader, &mut writer, &request, &device.name)
                    .await
                {
                    self.island.failed("Transfert interrompu");
                    return Err(e);
                }
            }
            "getclip" => self.send_clipboard(&mut reader, &mut writer, &state.name).await?,
            "inbox" => self.send_inbox(&mut reader, &mut writer, &device.name).await?,
            "watch" => self.watch(&state, &device_id, &device.name, reader, writer).await?,
            "names" => {
                let answer = self
                    .store
                    .take_names(&device_id, &request)
                    .or_else(|| self.store.snapshot().names_message(&device_id))
                    .unwrap_or_else(|| json!({ "t": "names" }));
                writer.send_json(&answer).await?;
            }
            "unpair" => {
                self.store.remove_device(&device_id);
                writer.send_json(&json!({ "t": "ok" })).await?;
                notify::send("Oublié", &format!("{} a oublié cet ordinateur", device.name));
            }
            other => anyhow::bail!("unknown request {other:?}"),
        }
        Ok(())
    }

    /// A phone this host forgot comes back with its old key: an encrypted `forgotten` tells it so for
    /// real, which an unencrypted `unpaired` could not (anyone on the network could send that).
    async fn tell_forgotten(
        &self,
        io: FrameIo<TcpStream>,
        old_key_b64: &str,
        phone_nonce: &[u8],
        state: &crate::store::State,
    ) -> anyhow::Result<()> {
        let old_key = base64_decode(old_key_b64)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("stored key is not 32 bytes"))?;
        let host_nonce = random_16();
        let mut io = io;
        io.write_json(&json!({
            "t": "hello", "v": 1, "mac": state.id, "name": state.name,
            "nonce": base64_encode(&host_nonce),
        }))
        .await?;
        let key = session_key(&old_key, phone_nonce, &host_nonce);
        let (reader, writer) = io.into_inner().into_split();
        let mut reader = SecureChannel::new(reader, &key, DIR_HOST_TO_PHONE, DIR_PHONE_TO_HOST);
        let mut writer = SecureChannel::new(writer, &key, DIR_HOST_TO_PHONE, DIR_PHONE_TO_HOST);
        let request = reader.recv_json().await?; // proves the phone still holds the old key
        let reply = if request["t"] == "unpair" { json!({ "t": "ok" }) } else { json!({ "t": "forgotten" }) };
        writer.send_json(&reply).await?;
        Ok(())
    }

    // MARK: phone → host

    async fn receive_batch(
        &self,
        reader: &mut SecureChannel<OwnedReadHalf>,
        writer: &mut SecureChannel<OwnedWriteHalf>,
        _header: &Value,
        from: &str,
    ) -> anyhow::Result<()> {
        let folder = self.store.snapshot().save_folder;
        let total_bytes = _header["total"].as_i64().unwrap_or(-1);
        let mut total_received: u64 = 0;
        let mut files: Vec<PathBuf> = Vec::new();
        let mut images_for_clipboard: Vec<PathBuf> = Vec::new();
        let mut texts: Vec<(String, bool, Option<String>)> = Vec::new();
        let mut ocr: Vec<PathBuf> = Vec::new();
        let mut announced = false;
        let mut last_progress = std::time::Instant::now();

        loop {
            match reader.recv().await? {
                Msg::Json(v) => match v["t"].as_str().unwrap_or("") {
                    "file" => {
                        let name = v["name"].as_str().unwrap_or("file");
                        let is_ocr = v["ocr"].as_bool().unwrap_or(false);
                        let clip = v["clip"].as_bool().unwrap_or(false);
                        let mut file = IncomingFile::create(&folder, name)?;
                        if !announced {
                            announced = true;
                            self.island.receiving(from, name);
                        }
                        loop {
                            match reader.recv().await? {
                                Msg::Data(d) => {
                                    file.write(&d)?;
                                    total_received += d.len() as u64;
                                    if last_progress.elapsed().as_millis() >= 120 {
                                        last_progress = std::time::Instant::now();
                                        self.island.progress(total_received, total_bytes.max(0) as u64, name);
                                    }
                                }
                                Msg::Json(eof) => {
                                    if eof["t"] != "eof" {
                                        anyhow::bail!("expected eof, got {}", eof["t"]);
                                    }
                                    break;
                                }
                            }
                        }
                        let path = file.commit()?;
                        if is_ocr {
                            ocr.push(path);
                        } else {
                            if clip {
                                images_for_clipboard.push(path.clone());
                            }
                            files.push(path);
                        }
                    }
                    "text" => texts.push((
                        v["text"].as_str().unwrap_or_default().to_string(),
                        v["clip"].as_bool().unwrap_or(false),
                        v["otp"].as_str().map(str::to_string),
                    )),
                    "done" => break,
                    other => anyhow::bail!("unexpected {other} in a batch"),
                },
                Msg::Data(_) => anyhow::bail!("file bytes outside a file"),
            }
        }

        eprintln!(
            "tapdrop: lot de {from} : {} fichier(s), {} texte(s), {} image(s)",
            files.len(),
            texts.len(),
            ocr.len()
        );
        for (text, _, otp) in &texts {
            eprintln!(
                "tapdrop: texte reçu ({}) : {}",
                otp.as_deref().unwrap_or("normal"),
                text.chars().take(90).collect::<String>()
            );
        }
        let saved = files.len() + texts.len();
        writer.send_json(&json!({ "t": "ok", "saved": saved + ocr.len() })).await?;
        if let Some((code, _, Some(source))) = texts.iter().find(|t| t.2.is_some()) {
            self.island.code(source, code);
        }

        let open_links = self.store.snapshot().open_links;
        let state_name = self.store.snapshot().name;
        let has_otp = texts.iter().any(|t| t.2.is_some());
        let first = files.first().cloned();
        let folder_name = folder
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let summary = match files.len() {
            0 if texts.is_empty() => "Rien reçu".to_string(),
            0 => format!("{} texte(s)", texts.len()),
            1 => files[0]
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
            n => format!("{n} fichiers"),
        };
        deliver(from, &state_name, files, images_for_clipboard, texts, ocr, open_links).await;
        if !has_otp {
            self.island.saved("Enregistré", &format!("{summary} · {folder_name}"), first.as_deref());
        }
        Ok(())
    }

    // MARK: host → phone

    async fn send_clipboard(
        &self,
        reader: &mut SecureChannel<OwnedReadHalf>,
        writer: &mut SecureChannel<OwnedWriteHalf>,
        host_name: &str,
    ) -> anyhow::Result<()> {
        let snapshot = tokio::task::spawn_blocking(clipboard::read).await?;
        let mut items = Vec::new();
        let mut temp: Option<PathBuf> = None;
        match snapshot {
            Snapshot::Files(paths) => {
                for p in paths {
                    if let Ok(item) = OutgoingItem::from_path(&p, false).await {
                        items.push(item);
                    }
                }
            }
            Snapshot::Image(png) => {
                let path = std::env::temp_dir().join(format!("tapdrop-clip-{}.png", uuid::Uuid::new_v4()));
                tokio::fs::write(&path, png).await?;
                items.push(OutgoingItem::File {
                    path: path.clone(),
                    name: "Presse-papiers.png".to_string(),
                    clip: true,
                });
                temp = Some(path);
            }
            Snapshot::Text(text) => items.push(OutgoingItem::Text { text, clip: true }),
            Snapshot::Empty => {}
        }
        let result = self.send_batch(reader, writer, items, host_name).await;
        if let Some(p) = temp {
            let _ = tokio::fs::remove_file(p).await;
        }
        result
    }

    async fn send_inbox(
        &self,
        reader: &mut SecureChannel<OwnedReadHalf>,
        writer: &mut SecureChannel<OwnedWriteHalf>,
        to: &str,
    ) -> anyhow::Result<()> {
        let queued = self.store.snapshot().outbox;
        let mut items = Vec::new();
        for q in &queued {
            if let Some(path) = &q.path {
                if let Ok(item) = OutgoingItem::from_path(path, false).await {
                    items.push(item);
                }
            } else if let Some(text) = &q.text {
                items.push(OutgoingItem::Text { text: text.clone(), clip: true });
            }
        }
        let count = items.len();
        if count > 0 {
            self.island.sending(to, &format!("{count} élément(s)"));
        }
        self.send_batch(reader, writer, items, &self.store.snapshot().name).await?;
        let ids: Vec<String> = queued.iter().map(|q| q.id.clone()).collect();
        self.store.dequeue(&ids);
        if count > 0 {
            self.island.done("Envoyé", &format!("{count} élément(s) vers {to}"));
            notify::send("Envoyé", &format!("{count} élément(s) vers {to}"));
        }
        Ok(())
    }

    async fn send_batch(
        &self,
        reader: &mut SecureChannel<OwnedReadHalf>,
        writer: &mut SecureChannel<OwnedWriteHalf>,
        items: Vec<OutgoingItem>,
        from: &str,
    ) -> anyhow::Result<()> {
        let total: u64 = items
            .iter()
            .map(|i| match i {
                OutgoingItem::File { path, .. } => files::size_of(path),
                OutgoingItem::Text { .. } => 0,
            })
            .sum();
        writer
            .send_json(&json!({ "t": "send", "count": items.len(), "total": total, "from": from }))
            .await?;
        for item in items {
            match item {
                OutgoingItem::File { path, name, clip } => {
                    writer
                        .send_json(&json!({
                            "t": "file", "name": name, "size": files::size_of(&path),
                            "mime": files::mime_for(&path), "clip": clip,
                        }))
                        .await?;
                    let mut file = tokio::fs::File::open(&path).await?;
                    loop {
                        let mut buf = vec![0u8; crate::wire::CHUNK];
                        let n = tokio::io::AsyncReadExt::read(&mut file, &mut buf).await?;
                        if n == 0 {
                            break;
                        }
                        writer.send_data(&buf[..n]).await?;
                    }
                    writer.send_json(&json!({ "t": "eof" })).await?;
                }
                OutgoingItem::Text { text, clip } => {
                    writer.send_json(&json!({ "t": "text", "text": text, "clip": clip })).await?;
                }
            }
        }
        writer.send_json(&json!({ "t": "done" })).await?;
        let ack = reader.recv_json().await?;
        if ack["t"] != "ok" {
            anyhow::bail!("phone did not acknowledge the batch");
        }
        Ok(())
    }

    // MARK: live link

    async fn watch(
        &self,
        state: &crate::store::State,
        device_id: &str,
        _device_name: &str,
        reader: SecureChannel<OwnedReadHalf>,
        mut writer: SecureChannel<OwnedWriteHalf>,
    ) -> anyhow::Result<()> {
        writer
            .send_json(&json!({
                "t": "watching", "pending": self.store.outbox_len(), "notch": self.hub.notch,
            }))
            .await?;
        let link = Arc::new(LiveLink::from_writer(device_id, writer));
        let link_id = self.hub.add(link.clone()).await;
        if let Some(names) = state.names_message(device_id) {
            let _ = link.send(names).await;
        }
        watch::run(
            self.store.clone(),
            self.hub.clone(),
            self.tap.clone(),
            link_id,
            link,
            reader,
        )
        .await;
        Ok(())
    }

    /// Wakes a phone's live link because something is now queued for it.
    pub async fn announce_inbox(&self) {
        let pending = self.store.outbox_len();
        self.hub.push_all(json!({ "t": "inbox", "pending": pending })).await;
    }
}

enum OutgoingItem {
    File { path: PathBuf, name: String, clip: bool },
    Text { text: String, clip: bool },
}

impl OutgoingItem {
    async fn from_path(path: &std::path::Path, clip: bool) -> anyhow::Result<Self> {
        let meta = tokio::fs::metadata(path).await?;
        if meta.is_dir() {
            anyhow::bail!("{} is a folder", path.display());
        }
        Ok(Self::File {
            path: path.to_path_buf(),
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "file".to_string()),
            clip,
        })
    }
}

/// What happens once a batch from the phone has landed.
async fn deliver(
    from: &str,
    host_name: &str,
    files: Vec<PathBuf>,
    clip_images: Vec<PathBuf>,
    texts: Vec<(String, bool, Option<String>)>,
    ocr: Vec<PathBuf>,
    open_links: bool,
) {
    let mut notes: Vec<String> = Vec::new();

    // A one-time code goes straight to the clipboard, it never touches the disk.
    if files.is_empty() {
        if let Some((code, _, Some(source))) = texts.first() {
            let text = code.clone();
            let source = source.clone();
            let _ = tokio::task::spawn_blocking(move || clipboard::write_text(&text)).await;
            notify::send(&format!("Code {source}"), "Copié, prêt à coller");
            return;
        }
    }

    if let Some(image) = clip_images.first() {
        if let Ok(bytes) = tokio::fs::read(image).await {
            let _ = tokio::task::spawn_blocking(move || clipboard::write_image(&bytes)).await;
            notes.push("Image copiée".to_string());
        }
    }

    for (text, _clip, _) in &texts {
        let value = text.clone();
        let _ = tokio::task::spawn_blocking(move || clipboard::write_text(&value)).await;
        if open_links {
            if let Some(url) = clean_url(text) {
                let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
                notes.push(format!("Lien ouvert : {url}"));
                continue;
            }
        }
        notes.push(format!("Texte copié : {}", snippet(text.trim(), 60)));
    }

    for image in &ocr {
        match read_text_in_image(image).await {
            Some(text) => {
                let value = text.clone();
                let _ = tokio::task::spawn_blocking(move || clipboard::write_text(&value)).await;
                notes.push(format!("Texte lu : {}", snippet(&text, 60)));
            }
            None => {
                if let Ok(bytes) = tokio::fs::read(image).await {
                    let _ = tokio::task::spawn_blocking(move || clipboard::write_image(&bytes)).await;
                    notes.push("Image copiée".to_string());
                }
            }
        }
    }

    if !files.is_empty() {
        let summary = if files.len() == 1 {
            files[0]
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
        } else {
            format!("{} fichiers", files.len())
        };
        notes.insert(0, summary);
    }

    if notes.is_empty() {
        notify::send("Rien reçu", &format!("Rien d'exploitable de {from}"));
    } else {
        notify::send(&format!("Reçu de {from}"), &notes.join(" · "));
    }
    let _ = host_name;
}

/// A URL a browser can open, even when the phone's address bar adds invisible marks or drops a colon.
fn clean_url(text: &str) -> Option<String> {
    let cleaned: String = text
        .chars()
        .filter(|c| !matches!(*c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}'))
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() || trimmed.contains(char::is_whitespace) {
        return None;
    }
    let url = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else if let Some(rest) = trimmed.strip_prefix("https//") {
        format!("https://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("http//") {
        format!("http://{rest}")
    } else if trimmed.contains('.') && !trimmed.contains("//") {
        format!("https://{trimmed}")
    } else {
        return None;
    };
    Some(url)
}

fn snippet(text: &str, max: usize) -> String {
    let mut s: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        s.push('…');
    }
    s
}

/// Reads the words off a screenshot, when `tesseract` is around. `None` means "show the picture".
async fn read_text_in_image(path: &std::path::Path) -> Option<String> {
    let found = tokio::process::Command::new("tesseract")
        .arg(path)
        .arg("stdout")
        .arg("-l")
        .arg("fra+eng")
        .output()
        .await
        .ok()?;
    if !found.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&found.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn base64_decode(text: &str) -> anyhow::Result<Vec<u8>> {
    use base64::Engine;
    Ok(base64::engine::general_purpose::STANDARD.decode(text)?)
}

fn random_16() -> [u8; 16] {
    let mut nonce = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    nonce
}

/// Runs until the process is stopped. Binds the preferred port, and any free one when it is taken.
pub async fn serve(host: Arc<Host>, preferred_port: u16) -> anyhow::Result<u16> {
    let listener = match TcpListener::bind(("0.0.0.0", preferred_port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("tapdrop: port {preferred_port} busy ({e}), taking a free one");
            TcpListener::bind(("0.0.0.0", 0)).await?
        }
    };
    let port = listener.local_addr()?.port();
    let host_for_accept = host.clone();
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let host = host_for_accept.clone();
                    tokio::spawn(async move { host.handle(stream).await });
                }
                Err(e) => {
                    eprintln!("tapdrop: accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
    });
    Ok(port)
}

/// One item waiting for the phone, stamped so the queue can be told apart.
pub fn new_queue_item(path: Option<PathBuf>, text: Option<String>) -> QueuedItem {
    QueuedItem { id: format!("{}-{}", now(), uuid::Uuid::new_v4()), path, text }
}
