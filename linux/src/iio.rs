//! The two sensors in the camera bump: ambient light (IIO `als`) and proximity (IIO `prox`).
//!
//! Light is measured once a second by this hardware; proximity twenty times a second. The tap uses
//! proximity as the trigger and light as the proof, because proximity alone cannot tell a phone from
//! a hand, and light alone is too slow to notice the phone arriving.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// What a light sensor gives: lux readings at whatever rate the hardware manages.
#[derive(Debug, Clone, Copy)]
pub struct Lux {
    pub lux: f32,
    pub at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proximity {
    /// Nothing within a few centimetres: a person sitting at the desk reads this.
    Far,
    /// Something is against the sensor: a phone, a hand, a closed lid. No return signal at all.
    Near,
}

#[derive(Debug, Clone, Copy)]
pub enum Event {
    Light(Lux),
    Prox(Proximity, Instant),
}

/// Where the two devices live, found by name rather than by a hard-coded index.
pub struct Paths {
    als_raw: PathBuf,
    als_scale: PathBuf,
    prox_raw: PathBuf,
}

impl Paths {
    pub fn describe(&self) -> String {
        format!("{} · {}", self.als_raw.display(), self.prox_raw.display())
    }

    /// Scans the IIO devices for one named `als` and one named `prox`.
    pub fn discover() -> Result<Self> {
        let root = Path::new("/sys/bus/iio/devices");
        let mut als = None;
        let mut prox = Vec::new();
        let entries = std::fs::read_dir(root).with_context(|| format!("reading {}", root.display()))?;
        for entry in entries.flatten() {
            let dir = entry.path();
            let name = std::fs::read_to_string(dir.join("name")).unwrap_or_default().trim().to_string();
            match name.as_str() {
                "als" if als.is_none() => {
                    if dir.join("in_illuminance_raw").exists() {
                        als = Some(dir);
                    }
                }
                "prox" if dir.join("in_proximity0_raw").exists() => prox.push(dir),
                _ => {}
            }
        }
        let als = als.context("no ambient light sensor (IIO device named \"als\")")?;
        let prox = pick_prox(prox).context("no proximity sensor (IIO device named \"prox\")")?;
        Ok(Self {
            als_raw: als.join("in_illuminance_raw"),
            als_scale: als.join("in_illuminance_scale"),
            prox_raw: prox.join("in_proximity0_raw"),
        })
    }

    pub fn read_lux(&self) -> Option<f32> {
        let raw = read_f32(&self.als_raw)?;
        let scale = read_f32(&self.als_scale).unwrap_or(1.0);
        Some(raw * scale)
    }

    pub fn read_prox(&self) -> Option<Proximity> {
        let raw = read_f32(&self.prox_raw)?;
        // 0 is "something is right there"; anything else is a sane distance.
        Some(if raw == 0.0 { Proximity::Near } else { Proximity::Far })
    }
}

/// Several devices answer to "prox". The useful one also reports a distance in metres (a second
/// channel with a millimetre scale); the other only says "near or far" and did not react to a phone.
fn pick_prox(candidates: Vec<PathBuf>) -> Option<PathBuf> {
    let mut fallback = None;
    for dir in candidates {
        let scale = read_f32(&dir.join("in_proximity1_scale"));
        let raw = read_f32(&dir.join("in_proximity1_raw"));
        if let (Some(scale), Some(raw)) = (scale, raw) {
            if scale <= 0.01 && raw > 1.0 {
                return Some(dir);
            }
        }
        fallback.get_or_insert(dir);
    }
    fallback
}

fn read_f32(path: &Path) -> Option<f32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Polls both sensors and reports what changed. Runs until the channel is closed.
///
/// Light is only reported when its value actually moves (the device updates it once a second
/// anyway); proximity is reported on every edge, and the caller gets it within ~50 ms.
pub async fn run(paths: Paths, tx: tokio::sync::mpsc::Sender<Event>) {
    let mut last_lux: Option<f32> = None;
    let mut last_prox: Option<Proximity> = None;
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(20));
    loop {
        ticker.tick().await;
        if let Some(lux) = paths.read_lux() {
            let moved = last_lux.map(|l| (l - lux).abs() >= 0.5).unwrap_or(true);
            if moved {
                last_lux = Some(lux);
                if tx.send(Event::Light(Lux { lux, at: Instant::now() })).await.is_err() {
                    return;
                }
            }
        }
        if let Some(prox) = paths.read_prox() {
            if last_prox != Some(prox) {
                last_prox = Some(prox);
                if tx.send(Event::Prox(prox, Instant::now())).await.is_err() {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proximity_reads_zero_as_near() {
        let dir = std::env::temp_dir().join(format!("tapdrop-iio-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let raw = dir.join("in_proximity0_raw");
        std::fs::write(&raw, "1\n").unwrap();
        let paths = Paths {
            als_raw: dir.join("als"),
            als_scale: dir.join("scale"),
            prox_raw: raw.clone(),
        };
        assert_eq!(paths.read_prox(), Some(Proximity::Far));
        std::fs::write(&raw, "0\n").unwrap();
        assert_eq!(paths.read_prox(), Some(Proximity::Near));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn light_is_raw_times_scale() {
        let dir = std::env::temp_dir().join(format!("tapdrop-iio-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("in_illuminance_raw"), "4484\n").unwrap();
        std::fs::write(dir.join("in_illuminance_scale"), "0.100000\n").unwrap();
        let paths = Paths {
            als_raw: dir.join("in_illuminance_raw"),
            als_scale: dir.join("in_illuminance_scale"),
            prox_raw: dir.join("prox"),
        };
        let lux = paths.read_lux().unwrap();
        assert!((lux - 448.4).abs() < 0.01, "got {lux}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
