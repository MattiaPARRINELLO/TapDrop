//! The host clipboard, through `wl-clipboard` (Wayland) so images and copied files survive the trip.
//!
//! KDE and GNOME expose a copied file as `text/uri-list`; a copied screenshot as `image/png`. Both
//! mean "send this", so they are tried before plain text.

use anyhow::{Context, Result};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(Debug, Clone)]
pub enum Snapshot {
    Files(Vec<PathBuf>),
    Image(Vec<u8>),
    Text(String),
    Empty,
}

/// What is on the clipboard right now, preferring files, then an image, then text.
pub fn read() -> Snapshot {
    let types = match run(&["--list-types"], None) {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).to_string(),
        _ => return Snapshot::Empty,
    };
    let has = |t: &str| types.lines().any(|line| line.trim() == t);

    if has("text/uri-list") {
        if let Ok(out) = run(&["--type", "text/uri-list"], None) {
            let files = paths_from_uri_list(&String::from_utf8_lossy(&out.stdout));
            if !files.is_empty() {
                return Snapshot::Files(files);
            }
        }
    }
    if has("image/png") {
        if let Ok(out) = run(&["--type", "image/png"], None) {
            if !out.stdout.is_empty() {
                return Snapshot::Image(out.stdout);
            }
        }
    }
    let text_type = types
        .lines()
        .map(str::trim)
        .find(|t| *t == "text/plain;charset=utf-8" || *t == "UTF8_STRING" || *t == "text/plain");
    if let Some(t) = text_type {
        if let Ok(out) = run(&["--no-newline", "--type", t], None) {
            let text = String::from_utf8_lossy(&out.stdout).to_string();
            if !text.is_empty() {
                return Snapshot::Text(text);
            }
        }
    }
    if has("image/png") {
        if let Ok(out) = run(&["--type", "image/png"], None) {
            if !out.stdout.is_empty() {
                return Snapshot::Image(out.stdout);
            }
        }
    }
    Snapshot::Empty
}

pub fn write_text(text: &str) -> Result<()> {
    let out =
        run(&[], Some(text.as_bytes())).context("wl-copy is not usable (no Wayland session?)")?;
    if !out.status.success() {
        anyhow::bail!(
            "wl-copy failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

pub fn write_image(png: &[u8]) -> Result<()> {
    let out = run(&["--type", "image/png"], Some(png))?;
    if !out.status.success() {
        anyhow::bail!(
            "wl-copy failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn run(args: &[&str], stdin: Option<&[u8]>) -> Result<std::process::Output> {
    let program = if stdin.is_some() || args.is_empty() {
        "wl-copy"
    } else {
        "wl-paste"
    };
    let mut child = Command::new(program)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running {program}"))?;
    if let Some(bytes) = stdin {
        child
            .stdin
            .as_mut()
            .context("wl-copy has no stdin")?
            .write_all(bytes)?;
    }
    Ok(child.wait_with_output()?)
}

/// Reads the paths of a `text/uri-list`, dropping comments and anything that is not a local file.
fn paths_from_uri_list(list: &str) -> Vec<PathBuf> {
    list.lines()
        .map(str::trim)
        .filter(|line| line.starts_with("file://"))
        .filter_map(|line| line.strip_prefix("file://"))
        .filter_map(percent_decode)
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .collect()
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_lists_become_local_paths() {
        let list = "# copied\nfile:///home/mattia/Photos%20de%20vacances/a.jpg\nfile:///tmp/b.png\nhttps://example.com\n";
        let decoded = list
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("file://"))
            .filter_map(|l| l.strip_prefix("file://"))
            .filter_map(percent_decode)
            .collect::<Vec<_>>();
        assert_eq!(
            decoded,
            ["/home/mattia/Photos de vacances/a.jpg", "/tmp/b.png"]
        );
    }
}
