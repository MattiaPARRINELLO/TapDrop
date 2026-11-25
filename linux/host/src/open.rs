//! Opening a link, in Brave when it is installed, in the default browser otherwise, and bringing
//! it to the front.

use std::process::{Command, Stdio};
use std::time::Duration;

/// Opens [url] and raises the browser window.
pub fn url(url: &str) {
    let opened = Command::new("brave")
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if opened.is_err() {
        let _ = Command::new("xdg-open")
            .arg(url)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    // KWin keeps the focus where it is when a background program opens a window, so the window is
    // raised explicitly once it exists. kdotool drives KWin over its scripting interface, which is
    // the only way on Wayland; xdotool is the fallback for an X11 session.
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(450));
        if raise_with_kdotool() {
            return;
        }
        let _ = Command::new("xdotool")
            .args(["search", "--class", "brave", "windowactivate", "--sync"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
}

fn raise_with_kdotool() -> bool {
    let Ok(output) = Command::new("kdotool")
        .args(["search", "--class", "brave"])
        .output()
    else {
        return false;
    };
    let Some(window) = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .map(|line| line.trim().to_string())
    else {
        return false;
    };
    if window.is_empty() {
        return false;
    }
    Command::new("kdotool")
        .args(["windowactivate", &window])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}
