//! Persistent state: who this host is, which phones are paired, and what waits to be sent.
//!
//! One TOML file in the user's config directory, rewritten atomically on every change. It holds no
//! secret beyond the per-phone long-term keys, which never leave the machine.

use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

/// How long a forgotten phone is remembered, only to tell it that it was forgotten.
const FORGOTTEN_RETENTION_DAYS: i64 = 120;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    /// The long-term pairing key, base64.
    pub key: String,
    /// Epoch seconds.
    pub paired_at: i64,
    #[serde(default)]
    pub last_seen: Option<i64>,
    /// When a person last chose this name, epoch ms; 0 for the system name sent at pairing.
    #[serde(default)]
    pub name_at: i64,
}

impl Device {
    pub fn pair_key(&self) -> Result<[u8; 32]> {
        let raw = B64
            .decode(&self.key)
            .context("paired device key is not base64")?;
        raw.try_into()
            .map_err(|_| anyhow::anyhow!("paired device key is not 32 bytes"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgottenDevice {
    pub id: String,
    pub key: String,
    pub forgotten_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedItem {
    pub id: String,
    #[serde(default)]
    pub path: Option<PathBuf>,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct State {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub name_at: i64,
    pub port: u16,
    pub save_folder: PathBuf,
    #[serde(default = "default_true")]
    pub open_links: bool,
    #[serde(default)]
    pub devices: Vec<Device>,
    #[serde(default)]
    pub forgotten: Vec<ForgottenDevice>,
    #[serde(default)]
    pub outbox: Vec<QueuedItem>,
}

fn default_true() -> bool {
    true
}

impl State {
    fn fresh() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: system_hostname(),
            name_at: 0,
            port: 47913,
            save_folder: downloads_folder(),
            open_links: true,
            devices: Vec::new(),
            forgotten: Vec::new(),
            outbox: Vec::new(),
        }
    }

    pub fn device(&self, id: &str) -> Option<&Device> {
        self.devices.iter().find(|d| d.id == id)
    }

    pub fn names_message(&self, device_id: &str) -> Option<serde_json::Value> {
        let device = self.device(device_id)?;
        Some(serde_json::json!({
            "t": "names",
            "mac": { "name": self.name, "at": self.name_at },
            "phone": { "name": device.name, "at": device.name_at },
        }))
    }
}

pub struct Store {
    path: PathBuf,
    state: Mutex<State>,
}

impl Store {
    pub fn open() -> Result<Self> {
        let dir = config_dir();
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join("state.toml");
        let state = match fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::fresh(),
            Err(e) => return Err(e).context("reading the state file"),
        };
        let store = Self {
            path,
            state: Mutex::new(state),
        };
        store.save()?;
        Ok(store)
    }

    pub fn snapshot(&self) -> State {
        let guard = self.state.lock().unwrap();
        State {
            ..clone_state(&guard)
        }
    }

    pub fn add_device(&self, id: &str, name: &str, key: [u8; 32]) {
        let mut s = self.state.lock().unwrap();
        s.devices.retain(|d| d.id != id);
        s.devices.push(Device {
            id: id.to_string(),
            name: name.to_string(),
            key: B64.encode(key),
            paired_at: now(),
            last_seen: None,
            name_at: 0,
        });
        drop(s);
        self.save_quietly();
    }

    pub fn remove_device(&self, id: &str) {
        let mut s = self.state.lock().unwrap();
        if let Some(dev) = s.devices.iter().find(|d| d.id == id).cloned() {
            s.forgotten.retain(|f| f.id != id);
            s.forgotten.push(ForgottenDevice {
                id: dev.id,
                key: dev.key,
                forgotten_at: now(),
            });
        }
        s.devices.retain(|d| d.id != id);
        drop(s);
        self.save_quietly();
    }

    pub fn forgotten(&self, id: &str) -> Option<ForgottenDevice> {
        let s = self.state.lock().unwrap();
        let horizon = now() - FORGOTTEN_RETENTION_DAYS * 86_400;
        s.forgotten
            .iter()
            .find(|f| f.id == id && f.forgotten_at >= horizon)
            .cloned()
    }

    pub fn touch(&self, id: &str) {
        let mut s = self.state.lock().unwrap();
        if let Some(dev) = s.devices.iter_mut().find(|d| d.id == id) {
            dev.last_seen = Some(now());
        }
        drop(s);
        self.save_quietly();
    }

    pub fn queue(&self, item: QueuedItem) {
        let mut s = self.state.lock().unwrap();
        s.outbox.push(item);
        drop(s);
        self.save_quietly();
    }

    pub fn dequeue(&self, ids: &[String]) {
        let mut s = self.state.lock().unwrap();
        s.outbox.retain(|q| !ids.contains(&q.id));
        drop(s);
        self.save_quietly();
    }

    pub fn outbox_len(&self) -> usize {
        self.state.lock().unwrap().outbox.len()
    }

    /// Applies a `names` message: the newest change wins (PROTOCOL.md → "Names"). Returns our answer
    /// when the two views still differ, which makes both sides settle on the same names.
    pub fn take_names(
        &self,
        device_id: &str,
        message: &serde_json::Value,
    ) -> Option<serde_json::Value> {
        let (peer_host, peer_phone) = (entry(&message["mac"]), entry(&message["phone"]));
        let mut differs = false;
        {
            let mut s = self.state.lock().unwrap();
            if let Some((name, at)) = peer_host {
                // What the phone thinks this host is called: a rename made over there, taken when newer.
                if at > s.name_at {
                    s.name = name;
                    s.name_at = at;
                }
                differs |= s.name != message["mac"]["name"].as_str().unwrap_or("");
            }
            if let Some((name, at)) = peer_phone {
                if let Some(dev) = s.devices.iter_mut().find(|d| d.id == device_id) {
                    // The phone speaking about itself: the owner, taken unless ours is newer.
                    if at >= dev.name_at {
                        dev.name = name;
                        dev.name_at = at;
                    }
                    differs |= dev.name != message["phone"]["name"].as_str().unwrap_or("");
                }
            }
        }
        if differs {
            self.save_quietly();
            self.snapshot().names_message(device_id)
        } else {
            None
        }
    }

    pub fn set_open_links(&self, enabled: bool) {
        let mut s = self.state.lock().unwrap();
        s.open_links = enabled;
        drop(s);
        self.save_quietly();
    }

    pub fn set_folder(&self, folder: PathBuf) {
        let mut s = self.state.lock().unwrap();
        s.save_folder = folder;
        drop(s);
        self.save_quietly();
    }

    pub fn rename(&self, name: &str) {
        let mut s = self.state.lock().unwrap();
        s.name = name.to_string();
        s.name_at = now() * 1000;
        drop(s);
        self.save_quietly();
    }

    fn save(&self) -> Result<()> {
        let text = {
            let s = self.state.lock().unwrap();
            toml::to_string_pretty(&*s)?
        };
        let tmp = self.path.with_extension("toml.tmp");
        fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("replacing {}", self.path.display()))?;
        Ok(())
    }

    fn save_quietly(&self) {
        if let Err(e) = self.save() {
            eprintln!("tapdrop: could not save state: {e:#}");
        }
    }
}

fn clone_state(s: &State) -> State {
    State {
        id: s.id.clone(),
        name: s.name.clone(),
        name_at: s.name_at,
        port: s.port,
        save_folder: s.save_folder.clone(),
        open_links: s.open_links,
        devices: s.devices.clone(),
        forgotten: s.forgotten.clone(),
        outbox: s.outbox.clone(),
    }
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("tapdrop-linux")
}

pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("tapdrop-linux")
}

fn downloads_folder() -> PathBuf {
    dirs::download_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join("Downloads")))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn system_hostname() -> String {
    fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Linux".to_string())
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A usable `names` entry: no control characters, spaces collapsed, at most 40 characters.
fn entry(v: &serde_json::Value) -> Option<(String, i64)> {
    let raw = v["name"].as_str()?;
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if cleaned.is_empty() {
        return None;
    }
    let name: String = cleaned.chars().take(40).collect();
    Some((name, v["at"].as_i64().unwrap_or(0).max(0)))
}
