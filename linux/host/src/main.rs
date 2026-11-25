//! tapdrop-linux: the host half of TapDrop, for a Linux desktop.
//!
//! The phone keeps its own TapDrop app; this listens on the local network, pairs with it, and puts
//! what arrives where a Linux user expects it (Downloads, the Wayland clipboard, the browser).

mod camera;
mod clipboard;
mod control;
mod iio;
mod island;
mod logging;
mod mdns;
mod net;
mod notify;
mod server;
mod store;
mod tap;
mod tray;
mod ui;
mod watch;

use anyhow::Result;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::Notify;

fn main() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let auto_pair = args.iter().any(|a| a == "--auto-pair");
    args.retain(|a| a != "--auto-pair");

    // GTK owns the main thread of a window; everything else runs on tokio instead.
    if args.first().map(String::as_str) == Some("ui") {
        return ui::run();
    }
    if args.first().map(String::as_str) == Some("island") {
        return island::run();
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(dispatch(args, auto_pair))
}

async fn dispatch(args: Vec<String>, auto_pair: bool) -> Result<()> {
    match args.first().map(String::as_str) {
        None | Some("run") => run(auto_pair).await?,
        Some("status") => show_status().await,
        Some("send") => {
            send(&args[1..]).await;
        }
        Some("clipboard") => {
            call(json!({ "cmd": "clipboard" })).await;
        }
        Some("pending") => list_pending().await,
        Some("approve") => decide("approve", &args[1..]).await,
        Some("deny") => decide("deny", &args[1..]).await,
        Some("devices") => list_devices().await,
        Some("sensors") => sensors().await,
        Some("rename") => rename(&args[1..]).await,
        Some("folder") => folder(&args[1..]).await,
        Some("forget") => forget(&args[1..]).await,
        Some("quit") => {
            call(json!({ "cmd": "quit" })).await;
        }
        Some("help" | "--help" | "-h") => help(),
        Some(other) => {
            eprintln!("unknown command: {other}");
            help();
        }
    }
    Ok(())
}

async fn run(auto_pair: bool) -> Result<()> {
    let store = Arc::new(store::Store::open()?);
    let island = island::Push::new();
    start_island();
    // No light sensor, no light key: phones then only share, they cannot tap.
    let sensors = iio::Paths::discover().ok();
    // The camera reads light thirty times a second; the sensor next to it only once.
    let camera = camera::available();
    let hub = watch::Hub::new(sensors.is_some());
    let tap = tap::TapService::new(hub.clone(), camera, island.clone());
    let tapping = tap.clone();
    if let Some(paths) = sensors {
        start_tap(paths, tapping);
    }
    let pending = server::PendingPairs::new();
    let host = Arc::new(server::Host {
        store: store.clone(),
        hub: hub.clone(),
        pending,
        island: island.clone(),
        tap: tap.clone(),
        auto_pair,
    });

    let preferred = store.snapshot().port;
    let port = server::serve(host.clone(), preferred).await?;
    let state = store.snapshot();
    let _advertisement = mdns::advertise(&state.name, port, &state.id)?;

    let shutdown = Arc::new(Notify::new());
    let control = control::Control::new(host.clone(), hub.clone(), port, shutdown.clone());
    tokio::spawn(async move {
        if let Err(e) = control.run().await {
            eprintln!("tapdrop: control socket failed: {e:#}");
        }
    });

    tokio::spawn(tray::start(host.clone(), store.clone(), shutdown.clone()));

    println!(
        "TapDrop listening on port {port}, visible as “{}”.",
        state.name
    );
    if hub.notch {
        if tap.uses_camera() {
            println!("Tap ready: the webcam is the light sensor (its LED lights up during a tap).");
        } else {
            println!("Tap ready through the light sensor (one reading a second, less reliable).");
        }
    } else {
        println!("No light sensor: sharing only, tapping is unavailable.");
    }
    if state.devices.is_empty() {
        println!("No phone paired: open TapDrop on Android and choose this computer.");
    } else {
        println!(
            "Paired: {}.",
            state
                .devices
                .iter()
                .map(|d| d.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = shutdown.notified() => {},
    }
    island.quit();
    println!("TapDrop stopping.");
    Ok(())
}

/// The island draws in its own process: GTK wants the main thread, the service needs it for tokio.
fn start_island() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let _ = std::process::Command::new(exe)
        .arg("island")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Feeds the light key from the camera bump, and lets its rounds time out.
fn start_tap(paths: iio::Paths, tap: Arc<tap::TapService>) {
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    tokio::spawn(iio::run(paths, tx));
    let ticking = tap.clone();
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                // With a camera, the sensor's once-a-second readings would only confuse the key.
                iio::Event::Light(reading) if !tap.uses_camera() => tap.lux(reading).await,
                iio::Event::Light(_) => {}
                iio::Event::Prox(prox, at) => tap.prox(prox, at).await,
            }
        }
    });
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(100));
        loop {
            ticker.tick().await;
            ticking.tick().await;
        }
    });
}

async fn call(request: Value) -> Option<Value> {
    match control::request(&request).await {
        Ok(answer) => Some(answer),
        Err(e) => {
            eprintln!("tapdrop: {e}");
            None
        }
    }
}

async fn show_status() {
    let Some(status) = call(json!({ "cmd": "status" })).await else {
        return;
    };
    println!("Name    : {}", status["name"].as_str().unwrap_or("?"));
    println!("Port    : {}", status["port"]);
    println!(
        "Folder  : {}",
        status["save_folder"].as_str().unwrap_or("?")
    );
    println!("Queue   : {} item(s)", status["outbox"]);
    println!(
        "Paired  : {}",
        status["devices"].as_array().map(|a| a.len()).unwrap_or(0)
    );
    for device in status["devices"].as_array().cloned().unwrap_or_default() {
        println!(
            "  - {} ({}){}",
            device["name"].as_str().unwrap_or("?"),
            device["id"].as_str().unwrap_or("?"),
            match device["live"].as_bool() {
                Some(true) => ", online",
                _ => "",
            }
        );
    }
    for address in status["addresses"].as_array().cloned().unwrap_or_default() {
        println!(
            "Address : {}  ({}, {})",
            address["endpoint"].as_str().unwrap_or("?"),
            address["kind"].as_str().unwrap_or("?"),
            address["iface"].as_str().unwrap_or("?")
        );
    }
    for pending in status["pending"].as_array().cloned().unwrap_or_default() {
        println!(
            "Pairing waiting: {} is asking for code {}",
            pending["device"].as_str().unwrap_or("?"),
            pending["code"].as_str().unwrap_or("?")
        );
    }
}

async fn send(paths: &[String]) -> Option<Value> {
    let paths: Vec<String> = paths
        .iter()
        .filter(|p| !p.starts_with("--"))
        .map(|p| {
            let expanded = match p.strip_prefix("~/") {
                Some(rest) => dirs::home_dir()
                    .unwrap_or_default()
                    .join(rest)
                    .display()
                    .to_string(),
                None => p.clone(),
            };
            expanded
        })
        .collect();
    let wants_clipboard = std::env::args().any(|a| a == "--clipboard");
    if paths.is_empty() && !wants_clipboard {
        eprintln!("usage: tapdrop send <file…> [--clipboard]");
        return None;
    }
    let answer =
        call(json!({ "cmd": "send", "paths": paths, "clipboard": wants_clipboard })).await?;
    println!("{} item(s) queued for the phone.", answer["queued"]);
    Some(answer)
}

async fn list_pending() {
    let Some(answer) = call(json!({ "cmd": "pending" })).await else {
        return;
    };
    let pending = answer["pending"].as_array().cloned().unwrap_or_default();
    if pending.is_empty() {
        println!("No pairing waiting.");
    }
    for p in pending {
        println!(
            "{} is asking for code {}",
            p["device"].as_str().unwrap_or("?"),
            p["code"].as_str().unwrap_or("?")
        );
    }
}

async fn decide(command: &str, args: &[String]) {
    let Some(code) = args.first() else {
        eprintln!("usage: tapdrop {command} <code>");
        return;
    };
    let Some(answer) = call(json!({ "cmd": command, "code": code })).await else {
        return;
    };
    if answer["ok"].as_bool().unwrap_or(false) {
        println!("Done.");
    } else {
        println!("No pairing with that code.");
    }
}

async fn list_devices() {
    let Some(answer) = call(json!({ "cmd": "status" })).await else {
        return;
    };
    let devices = answer["devices"].as_array().cloned().unwrap_or_default();
    if devices.is_empty() {
        println!("No phone paired.");
    }
    for d in devices {
        println!(
            "{} — {}",
            d["name"].as_str().unwrap_or("?"),
            d["id"].as_str().unwrap_or("?")
        );
    }
}

async fn rename(args: &[String]) {
    let Some(name) = args.first() else {
        eprintln!("usage: tapdrop rename <name>");
        return;
    };
    let Some(answer) = call(json!({ "cmd": "rename", "name": name })).await else {
        return;
    };
    println!(
        "{}",
        if answer["ok"].as_bool().unwrap_or(false) {
            "Renamed."
        } else {
            "Name refused."
        }
    );
}

async fn folder(args: &[String]) {
    let Some(path) = args.first() else {
        eprintln!("usage: tapdrop folder <path>");
        return;
    };
    let Some(answer) = call(json!({ "cmd": "folder", "path": path })).await else {
        return;
    };
    if answer["ok"].as_bool().unwrap_or(false) {
        println!(
            "Files will land in {}.",
            answer["path"].as_str().unwrap_or(path)
        );
    } else {
        eprintln!(
            "Folder refused: {}",
            answer["error"].as_str().unwrap_or("?")
        );
    }
}

/// Prints what the camera bump's sensors see, which is the first thing to check when a tap does not count.
async fn sensors() {
    let paths = match iio::Paths::discover() {
        Ok(paths) => paths,
        Err(e) => {
            eprintln!("Sensors not found: {e}");
            return;
        }
    };
    println!("{}", paths.describe());
    for _ in 0..10 {
        let lux = paths.read_lux();
        let prox = paths.read_prox();
        println!(
            "light {:>7} lx   proximity {}",
            lux.map(|l| format!("{l:.1}")).unwrap_or_else(|| "?".into()),
            match prox {
                Some(tapdrop_protocol::sensors::Proximity::Near) => "against",
                Some(tapdrop_protocol::sensors::Proximity::Far) => "clear",
                None => "?",
            }
        );
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
    }
}

async fn forget(args: &[String]) {
    let Some(device) = args.first() else {
        eprintln!("usage: tapdrop forget <phone-id>");
        return;
    };
    let _ = call(json!({ "cmd": "forget", "device": device })).await;
    println!("Forgotten.");
}

fn help() {
    println!(
        "\
tapdrop-linux

  tapdrop run [--auto-pair]   start the service (pairs without asking with --auto-pair)
  tapdrop status              service state and paired phones
  tapdrop pending             pairings waiting for approval
  tapdrop approve <code>      accept the pairing showing this code
  tapdrop deny <code>         refuse that pairing
  tapdrop send <file…>        queue files for the phone
  tapdrop send --clipboard    send this computer's clipboard
  tapdrop clipboard           the same, shorter
  tapdrop devices             paired phones
  tapdrop sensors             what the camera bump's sensors see
  tapdrop forget <id>         forget a phone
  tapdrop rename <name>       rename this computer for the phone
  tapdrop folder <path>       choose where received files land
  tapdrop quit                stop the service"
    );
}
