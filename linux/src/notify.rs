//! Desktop notices.
//!
//! `notify-send` on this machine is broken against the installed libnotify, so the notification goes
//! through D-Bus directly when `gdbus` is there, which every GNOME and KDE session has.

use std::process::{Command, Stdio};

pub fn send(title: &str, body: &str) {
    let via_gdbus = Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.freedesktop.Notifications",
            "--object-path",
            "/org/freedesktop/Notifications",
            "--method",
            "org.freedesktop.Notifications.Notify",
            "TapDrop",
            "0",
            "",
            title,
            body,
            "[]",
            "{}",
            "6000",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if via_gdbus.is_ok() {
        return;
    }
    let _ = Command::new("notify-send")
        .args([
            "--app-name",
            "TapDrop",
            "--expire-time",
            "6000",
            title,
            body,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// A phone wants to pair: the person has to compare the code and approve.
pub fn pairing(code: &str, device: &str) {
    send(
        &format!("Appairage de {device}"),
        &format!("Code {code} sur les deux écrans ? Puis : tapdrop approve {code}"),
    );
}
