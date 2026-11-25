//! The addresses a phone could reach: the Wi-Fi one, and any tunnel a person set up.
//!
//! Discovery over DNS-SD only works on the same link, so a phone on another network needs the
//! address written into it. This lists the ones worth typing, best first.

use std::process::Command;

#[derive(Debug, Clone)]
pub struct Address {
    pub iface: String,
    pub ip: String,
    /// Where it comes from, in the words a person would use.
    pub kind: &'static str,
}

impl Address {
    pub fn endpoint(&self, port: u16) -> String {
        format!("{}:{port}", self.ip)
    }
}

/// Cached for a few seconds: the window asks for the status every two seconds, and forking `ip`
/// that often is pure waste.
pub fn reachable() -> Vec<Address> {
    use std::sync::{Mutex, OnceLock};
    type Cache = Mutex<Option<(std::time::Instant, Vec<Address>)>>;
    static CACHE: OnceLock<Cache> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock() {
        if let Some((at, list)) = guard.as_ref() {
            if at.elapsed().as_secs() < 5 {
                return list.clone();
            }
        }
    }
    let list = scan();
    if let Ok(mut guard) = cache.lock() {
        *guard = Some((std::time::Instant::now(), list.clone()));
    }
    list
}

fn scan() -> Vec<Address> {
    let Ok(output) = Command::new("ip")
        .args(["-o", "-4", "addr", "show"])
        .output()
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut found: Vec<Address> = text.lines().filter_map(parse_line).collect();
    // Tailscale and Bluetooth first: those work away from this network. Then the LAN.
    found.sort_by_key(|a| match a.kind {
        "Tailscale" => 0,
        "WireGuard" => 1,
        "Bluetooth" => 2,
        "Wi-Fi" => 3,
        _ => 4,
    });
    found
}

fn parse_line(line: &str) -> Option<Address> {
    let mut parts = line.split_whitespace();
    let first = parts.next()?;
    // `ip -o` prefixes each line with its index: "2: wlan0 inet …".
    let iface = if first
        .trim_end_matches(':')
        .chars()
        .all(|c| c.is_ascii_digit())
    {
        parts.next()?.to_string()
    } else {
        first.trim_end_matches(':').to_string()
    };
    let mut inet = None;
    while let Some(token) = parts.next() {
        if token == "inet" {
            inet = parts.next();
            break;
        }
    }
    let ip = inet?.split('/').next()?.to_string();
    if iface == "lo" || iface.starts_with("docker") || iface.starts_with("br-") {
        return None;
    }
    if iface.starts_with("veth") || iface.starts_with("virbr") || iface.starts_with("vboxnet") {
        return None;
    }
    if iface.starts_with("vmnet") {
        return None;
    }
    Some(Address {
        kind: kind_of(&iface, &ip),
        iface,
        ip,
    })
}

fn kind_of(iface: &str, ip: &str) -> &'static str {
    if iface.starts_with("tailscale") || in_cgnat(ip) {
        return "Tailscale";
    }
    if iface.starts_with("wg") {
        return "WireGuard";
    }
    if iface.starts_with("bnep") || iface.starts_with("bt") {
        return "Bluetooth";
    }
    if iface.starts_with("wl") {
        return "Wi-Fi";
    }
    if iface.starts_with("en") || iface.starts_with("eth") {
        return "Ethernet";
    }
    "Réseau"
}

/// Tailscale hands out addresses from 100.64.0.0/10 (the carrier-grade NAT block).
fn in_cgnat(ip: &str) -> bool {
    let mut octets = ip.split('.').filter_map(|o| o.parse::<u16>().ok());
    match (octets.next(), octets.next()) {
        (Some(a), Some(b)) => a == 100 && (64..128).contains(&b),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_addresses_a_phone_could_use() {
        let lan = parse_line("2: wlan0    inet 10.105.4.176/21 brd 10.105.7.255 scope global dynamic noprefixroute wlan0").unwrap();
        assert_eq!((lan.ip.as_str(), lan.kind), ("10.105.4.176", "Wi-Fi"));
        let ts =
            parse_line("9: tailscale0    inet 100.101.102.103/32 scope global tailscale0").unwrap();
        assert_eq!(ts.kind, "Tailscale");
        assert!(parse_line("1: lo    inet 127.0.0.1/8 scope host lo").is_none());
        assert!(parse_line("5: docker0    inet 172.17.0.1/16 scope global docker0").is_none());
        assert!(parse_line("11: bnep0    inet 192.168.44.7/24 scope global bnep0").is_some());
    }

    #[test]
    fn cgnat_is_tailscale() {
        assert!(in_cgnat("100.64.0.1"));
        assert!(in_cgnat("100.127.255.254"));
        assert!(!in_cgnat("100.128.0.1"));
        assert!(!in_cgnat("192.168.1.2"));
    }
}
