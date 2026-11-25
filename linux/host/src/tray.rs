//! The status bar item: an icon in the KDE panel with the few actions that matter.
//!
//! It is a StatusNotifierItem, which every modern KDE and GNOME panel shows. The icon is drawn here
//! so there is no theme dependency: a rounded slab with the notch cut out.

use crate::server::Host;
use crate::store::Store;
use ksni::menu::{MenuItem, StandardItem};
use ksni::{Icon, ToolTip, Tray, TrayMethods};
use std::process::Command as Process;
use std::sync::Arc;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::sync::Notify;

enum Command {
    OpenUi,
    Clipboard,
    Approve(String),
    Deny(String),
    Quit,
}

pub struct TapTray {
    tx: UnboundedSender<Command>,
    devices: Vec<String>,
    pending: Vec<(String, String)>,
    notch: bool,
}

impl TapTray {
    fn status_line(&self) -> String {
        if let Some((code, device)) = self.pending.first() {
            return format!("Pairing {device}: code {code}");
        }
        let phones = match self.devices.len() {
            0 => "no phone".to_string(),
            1 => format!("{} paired", self.devices[0]),
            n => format!("{n} phones paired"),
        };
        let tap = if self.notch {
            "tap ready"
        } else {
            "tap unavailable"
        };
        format!("{phones} · {tap}")
    }
}

impl Tray for TapTray {
    fn id(&self) -> String {
        "tapdrop-linux".to_string()
    }

    fn title(&self) -> String {
        "TapDrop".to_string()
    }

    fn icon_name(&self) -> String {
        // A theme name when the desktop has one, the drawn icon otherwise.
        "phone-symbolic".to_string()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        vec![notch_icon(22), notch_icon(44)]
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: "TapDrop".to_string(),
            description: self.status_line(),
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let tx = self.tx.clone();
        let mut items: Vec<MenuItem<Self>> = vec![
            StandardItem {
                label: "Open TapDrop".to_string(),
                activate: Box::new(move |_| drop(tx.send(Command::OpenUi))),
                ..Default::default()
            }
            .into(),
            {
                let tx = self.tx.clone();
                StandardItem {
                    label: "Send the clipboard".to_string(),
                    activate: Box::new(move |_| drop(tx.send(Command::Clipboard))),
                    ..Default::default()
                }
                .into()
            },
        ];

        for (code, device) in &self.pending {
            items.push(
                StandardItem {
                    label: format!("Pairing {device}: {code}"),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
            items.push({
                let (tx, code) = (self.tx.clone(), code.clone());
                StandardItem {
                    label: format!("Accept {code}"),
                    activate: Box::new(move |_| drop(tx.send(Command::Approve(code.clone())))),
                    ..Default::default()
                }
                .into()
            });
            items.push({
                let (tx, code) = (self.tx.clone(), code.clone());
                StandardItem {
                    label: format!("Refuse {code}"),
                    activate: Box::new(move |_| drop(tx.send(Command::Deny(code.clone())))),
                    ..Default::default()
                }
                .into()
            });
        }

        items.push(MenuItem::Separator);
        items.push({
            let tx = self.tx.clone();
            StandardItem {
                label: "Quit TapDrop".to_string(),
                activate: Box::new(move |_| drop(tx.send(Command::Quit))),
                ..Default::default()
            }
            .into()
        });
        items
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.send(Command::OpenUi);
    }
}

/// Starts the icon and the loop that carries out what it asks for.
pub async fn start(host: Arc<Host>, store: Arc<Store>, shutdown: Arc<Notify>) {
    let (tx, mut rx) = unbounded_channel();
    let state = TapTray {
        tx: tx.clone(),
        devices: store
            .snapshot()
            .devices
            .iter()
            .map(|d| d.name.clone())
            .collect(),
        pending: Vec::new(),
        notch: host.hub.notch,
    };
    let handle = match state.spawn().await {
        Ok(handle) => handle,
        Err(e) => {
            eprintln!("tapdrop: no status bar icon ({e})");
            return;
        }
    };

    // Keep the icon in step with pairing and the paired phones.
    {
        let handle = handle.clone();
        let host = host.clone();
        let store = store.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                ticker.tick().await;
                let devices: Vec<String> = store
                    .snapshot()
                    .devices
                    .iter()
                    .map(|d| d.name.clone())
                    .collect();
                let pending: Vec<(String, String)> =
                    host.pending.list().await.into_iter().collect();
                let notch = host.hub.notch;
                handle
                    .update(move |tray| {
                        let changed = tray.devices != devices
                            || tray.pending != pending
                            || tray.notch != notch;
                        tray.devices = devices;
                        tray.pending = pending;
                        tray.notch = notch;
                        changed
                    })
                    .await;
            }
        });
    }

    while let Some(command) = rx.recv().await {
        match command {
            Command::OpenUi => open_ui(),
            Command::Clipboard => {
                let snapshot = tokio::task::spawn_blocking(crate::clipboard::read)
                    .await
                    .ok();
                if crate::control::queue_clipboard(&store, snapshot).await {
                    host.announce_inbox().await;
                    crate::notify::send("TapDrop", "Clipboard queued for the phone");
                }
            }
            Command::Approve(code) => {
                if host.pending.approve(&code).await {
                    crate::notify::send("Paired", "The phone can send now");
                }
            }
            Command::Deny(code) => {
                let _ = host.pending.deny(&code).await;
            }
            Command::Quit => {
                shutdown.notify_waiters();
                return;
            }
        }
    }
}

/// The window is its own process: GTK wants the main thread, the service owns it for tokio.
fn open_ui() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    if let Err(e) = Process::new(exe).arg("ui").spawn() {
        eprintln!("tapdrop: impossible d'ouvrir l'interface : {e}");
    }
}

/// A rounded slab with the notch bitten out of its top edge, as ARGB32 network-order pixels.
fn notch_icon(size: i32) -> Icon {
    let mut data = vec![0u8; (size * size * 4) as usize];
    let radius = size as f32 * 0.22;
    let left = size as f32 * 0.10;
    let right = size as f32 * 0.90;
    let top = size as f32 * 0.22;
    let bottom = size as f32 * 0.78;
    let notch = (size as f32 * 0.26, top, size as f32 * 0.13);
    for y in 0..size {
        for x in 0..size {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let inside = rounded_rect(fx, fy, left, top, right, bottom, radius);
            let bitten = (fx - notch.0).powi(2) + (fy - notch.1).powi(2) < notch.2.powi(2);
            let index = ((y * size + x) * 4) as usize;
            if inside && !bitten {
                data[index] = 255; // alpha
                data[index + 1] = 255;
                data[index + 2] = 255;
                data[index + 3] = 255;
            }
        }
    }
    Icon {
        width: size,
        height: size,
        data,
    }
}

fn rounded_rect(x: f32, y: f32, left: f32, top: f32, right: f32, bottom: f32, radius: f32) -> bool {
    if x < left || x > right || y < top || y > bottom {
        return false;
    }
    let corners = [
        (
            left + radius,
            top + radius,
            x < left + radius && y < top + radius,
        ),
        (
            right - radius,
            top + radius,
            x > right - radius && y < top + radius,
        ),
        (
            left + radius,
            bottom - radius,
            x < left + radius && y > bottom - radius,
        ),
        (
            right - radius,
            bottom - radius,
            x > right - radius && y > bottom - radius,
        ),
    ];
    for (cx, cy, in_corner) in corners {
        if in_corner && (x - cx).powi(2) + (y - cy).powi(2) > radius.powi(2) {
            return false;
        }
    }
    true
}
