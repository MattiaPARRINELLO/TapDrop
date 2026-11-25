//! Discovery: the phone finds this host over DNS-SD, as `_tapdrop._tcp`, with its id in the TXT record.
//!
//! Avahi is the registrar when it is installed: it advertises the host on each interface separately, so
//! a phone on the Wi-Fi only ever learns the Wi-Fi address, never a docker or VirtualBox bridge. Without
//! avahi, a small built-in responder publishes the one address the machine would use to reach the LAN.

use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceInfo};
use std::process::{Child, Command, Stdio};

pub const SERVICE_TYPE: &str = "_tapdrop._tcp.local.";
const AVAHI_TYPE: &str = "_tapdrop._tcp";

pub enum Advertisement {
    Avahi(Child),
    Builtin(ServiceDaemon),
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        match self {
            Advertisement::Avahi(child) => {
                let _ = child.kill();
                let _ = child.wait();
            }
            Advertisement::Builtin(daemon) => {
                let _ = daemon.shutdown();
            }
        }
    }
}

pub fn advertise(instance: &str, port: u16, id: &str) -> Result<Advertisement> {
    match publish_with_avahi(instance, port, id) {
        Ok(child) => Ok(Advertisement::Avahi(child)),
        Err(e) => {
            eprintln!("tapdrop: avahi unavailable ({e}), publishing myself");
            Ok(Advertisement::Builtin(publish_builtin(instance, port, id)?))
        }
    }
}

fn publish_with_avahi(instance: &str, port: u16, id: &str) -> Result<Child> {
    let child = Command::new("avahi-publish-service")
        .args([
            "--no-fail",
            instance,
            AVAHI_TYPE,
            &port.to_string(),
            &format!("id={id}"),
            "v=1",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("avahi-publish-service")?;
    Ok(child)
}

fn publish_builtin(instance: &str, port: u16, id: &str) -> Result<ServiceDaemon> {
    let daemon = ServiceDaemon::new().context("starting the mDNS responder")?;
    let host_name = format!("{}.local.", host_label(instance));
    let lan = local_ipv4().unwrap_or_default();
    let info = ServiceInfo::new(
        SERVICE_TYPE,
        instance,
        &host_name,
        lan.as_str(),
        port,
        &[("id", id), ("v", "1")][..],
    )
    .with_context(|| format!("advertising {instance} on {SERVICE_TYPE}"))?;
    let info = if lan.is_empty() {
        info.enable_addr_auto()
    } else {
        info
    };
    daemon
        .register(info)
        .context("registering the DNS-SD service")?;
    Ok(daemon)
}

/// The machine's address on the route it would use to reach the internet, without sending anything.
fn local_ipv4() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("1.1.1.1:80").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    if ip.is_loopback() {
        None
    } else {
        Some(ip.to_string())
    }
}

/// A DNS label derived from the host name: letters, digits and dashes only.
fn host_label(name: &str) -> String {
    let label: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let collapsed = label
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if collapsed.is_empty() {
        "tapdrop".to_string()
    } else {
        collapsed
    }
}

#[cfg(test)]
mod tests {
    use super::host_label;

    #[test]
    fn host_labels_are_plain_dns() {
        assert_eq!(host_label("Yoga 7 (Mattia)"), "yoga-7-mattia");
        assert_eq!(host_label("  "), "tapdrop");
        assert_eq!(host_label("PC_2"), "pc-2");
    }
}
