//! The light key: the round that decides a tap really came from a phone on the sensor.
//!
//! A tap has three parts, and all three must hold:
//! 1. proximity says something is against the camera bump (a phone, but a hand too, so it only opens
//!    the question);
//! 2. the phone says its screen is white, and the light sensor sees the bump get much brighter than
//!    the room;
//! 3. the phone says its screen is black, and the light sensor sees the bump go dark again, darker
//!    than the room, because the phone blocks it.
//!
//! A hand makes step 2 impossible, a lamp makes step 3 impossible. Light is measured once a second
//! by this hardware, so the windows are seconds, not the 450 ms of the original Mac implementation;
//! the phone's own limits (1.3 s of white, 1.6 s of black) still fit.

use crate::sensors::{Lux, Proximity};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// What the host must send, and to which phone.
#[derive(Debug, Clone)]
pub struct Action {
    pub device_id: String,
    pub message: Value,
}

/// A phone that could be tapped with.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub device_id: String,
    /// The light-key version it announced (3 is the light key with the dark echo).
    pub caps: u8,
    pub last_heard: Instant,
}

const DEFAULT_E: f32 = 300.0;
const MIN_E: f32 = 180.0;
const MAX_E: f32 = 1500.0;
const E_SAMPLES: usize = 12;

const KEY_ON_WINDOW: Duration = Duration::from_secs(4);
const SEEN_WINDOW: Duration = Duration::from_millis(1300);
const KEY_OFF_WINDOW: Duration = Duration::from_millis(2500);
const ECHO_WINDOW: Duration = Duration::from_millis(2000);
const ROUND_COOLDOWN: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// The grab is out; the phone has not said it lit up.
    KeyOn,
    /// The phone is white; the light has to be seen.
    Seen,
    /// The light was seen; the phone is turning black.
    KeyOff,
    /// The phone is black; the dark echo has to be seen.
    Echo,
}

struct Round {
    id: u32,
    device_id: String,
    started: Instant,
    stage: Stage,
    deadline: Instant,
    need: f32,
    peak: f32,
}

pub struct LightKey {
    /// The room's light at the bump, a slow follow, frozen while something covers it.
    room: f32,
    room_dark: bool,
    round: Option<Round>,
    last_round_end: Option<Instant>,
    next_id: u32,
    samples: HashMap<String, VecDeque<f32>>,
    prox: Proximity,
}

impl Default for LightKey {
    fn default() -> Self {
        Self::new()
    }
}

impl LightKey {
    pub fn new() -> Self {
        Self {
            room: 300.0,
            room_dark: false,
            round: None,
            last_round_end: None,
            next_id: 1,
            samples: HashMap::new(),
            prox: Proximity::Far,
        }
    }

    /// What this phone's white screen measures on the bump: the median of its last passes, or the
    /// default until three have landed.
    pub fn contact_level(&self, device_id: &str) -> f32 {
        match self.samples.get(device_id) {
            Some(s) if s.len() >= 3 => median(s).clamp(MIN_E, MAX_E),
            _ => DEFAULT_E,
        }
    }

    pub fn round_running(&self) -> bool {
        self.round.is_some()
    }

    /// A light reading: feeds the room estimate, or the round in progress.
    pub fn on_lux(&mut self, reading: Lux) -> Vec<Action> {
        if self.round.is_none() {
            self.learn_room(reading.lux);
            return Vec::new();
        }
        let stage = self.round.as_ref().map(|r| r.stage);
        if let Some(round) = self.round.as_mut() {
            round.peak = round.peak.max(reading.lux);
            eprintln!(
                "tapdrop: round {} {:?} voit {:.0} lx à +{} ms",
                round.id,
                stage.unwrap_or(Stage::KeyOn),
                reading.lux,
                reading.at.duration_since(round.started).as_millis()
            );
        }
        match stage {
            Some(Stage::Seen) => {
                let seen = self
                    .round
                    .as_ref()
                    .map(|r| reading.lux >= r.need)
                    .unwrap_or(false);
                if !seen {
                    return Vec::new();
                }
                let round = self.round.as_mut().expect("a round is running");
                round.stage = Stage::KeyOff;
                round.deadline = reading.at + KEY_OFF_WINDOW;
                vec![Action {
                    device_id: round.device_id.clone(),
                    message: json!({
                        "t": "key-seen", "id": round.id,
                        "peak": round.peak, "need": round.need,
                    }),
                }]
            }
            Some(Stage::Echo) => {
                let Some(fall) = self.echo_verdict(reading.lux) else {
                    return Vec::new();
                };
                let round = self.round.take().expect("a round is running");
                self.remember_sample(&round.device_id, round.peak - fall);
                self.last_round_end = Some(reading.at);
                eprintln!(
                    "tapdrop: tap validé (pic {:.0} lx, noir {:.0} lx, seuil {:.0})",
                    round.peak, fall, round.need
                );
                vec![Action {
                    device_id: round.device_id.clone(),
                    message: json!({
                        "t": "key-ok", "id": round.id, "peak": round.peak,
                        "need": round.need, "fall": fall,
                    }),
                }]
            }
            _ => Vec::new(),
        }
    }

    /// Proximity is the trigger: something just landed on the camera bump.
    pub fn on_prox(
        &mut self,
        prox: Proximity,
        at: Instant,
        candidates: &[Candidate],
    ) -> Vec<Action> {
        self.prox = prox;
        if prox == Proximity::Far {
            if self.round.is_some() {
                return self.fail("gone", at);
            }
            return Vec::new();
        }
        if self.round.is_some() || self.cooling_down(at) {
            return Vec::new();
        }
        let Some(phone) = pick(candidates) else {
            return Vec::new();
        };
        vec![self.start_round(&phone, at)]
    }

    /// The phone's own proximity sensor fired: same round, aimed at that phone.
    pub fn on_knock(&mut self, device_id: &str, caps: u8, at: Instant) -> Vec<Action> {
        if self.round.is_some() || self.cooling_down(at) {
            return Vec::new();
        }
        let phone = Candidate {
            device_id: device_id.to_string(),
            caps,
            last_heard: at,
        };
        vec![self.start_round(&phone, at)]
    }

    /// A message from the phone during a round.
    pub fn on_phone(&mut self, device_id: &str, message: &Value, at: Instant) -> Vec<Action> {
        let Some(round) = self.round.as_ref() else {
            return Vec::new();
        };
        let id = message["id"].as_u64().unwrap_or_default() as u32;
        if id != round.id || device_id != round.device_id {
            return Vec::new();
        }
        match message["t"].as_str().unwrap_or("") {
            "key-on" if round.stage == Stage::KeyOn => {
                eprintln!(
                    "tapdrop: round {} écran allumé à +{} ms",
                    round.id,
                    at.duration_since(round.started).as_millis()
                );
                if message["src"].as_str() != Some("screen") {
                    return self.fail("screen-only", at);
                }
                let round = self.round.as_mut().expect("a round is running");
                round.stage = Stage::Seen;
                round.deadline = at + SEEN_WINDOW;
                Vec::new()
            }
            "key-off" if matches!(round.stage, Stage::Seen | Stage::KeyOff) => {
                eprintln!(
                    "tapdrop: round {} écran éteint à +{} ms",
                    round.id,
                    at.duration_since(round.started).as_millis()
                );
                let round = self.round.as_mut().expect("a round is running");
                round.stage = Stage::Echo;
                round.deadline = at + ECHO_WINDOW;
                Vec::new()
            }
            "grab-ack" if message["ok"].as_bool() == Some(false) => {
                let why = message["why"].as_str().unwrap_or("declined").to_string();
                eprintln!(
                    "tapdrop: round {} refusé par le téléphone ({why})",
                    round.id
                );
                self.fail(&why, at)
            }
            _ => Vec::new(),
        }
    }

    /// Time passing: a round that stops moving fails with the reason that fits.
    pub fn tick(&mut self, at: Instant) -> Vec<Action> {
        let Some(round) = self.round.as_ref() else {
            return Vec::new();
        };
        if at < round.deadline {
            return Vec::new();
        }
        let why = match round.stage {
            Stage::KeyOn => "no-light-sent",
            Stage::Seen => "not-seen",
            Stage::KeyOff => "no-key-off",
            Stage::Echo => "stayed-lit",
        };
        self.fail(why, at)
    }

    fn start_round(&mut self, phone: &Candidate, at: Instant) -> Action {
        let need = (0.45 * self.contact_level(&phone.device_id)).max(40.0);
        let id = self.next_id;
        self.next_id += 1;
        self.round = Some(Round {
            id,
            device_id: phone.device_id.clone(),
            started: at,
            stage: Stage::KeyOn,
            deadline: at + KEY_ON_WINDOW,
            need,
            peak: 0.0,
        });
        eprintln!(
            "tapdrop: contact sur la bosse, round {id} vers {} (seuil {need:.0} lx)",
            phone.device_id
        );
        Action {
            device_id: phone.device_id.clone(),
            message: json!({ "t": "grab", "id": id, "key": 3, "dark": self.room_dark }),
        }
    }

    fn fail(&mut self, why: &str, at: Instant) -> Vec<Action> {
        let Some(round) = self.round.take() else {
            return Vec::new();
        };
        self.last_round_end = Some(at);
        eprintln!("tapdrop: round {} raté ({why})", round.id);
        vec![Action {
            device_id: round.device_id,
            message: json!({
                "t": "key-fail", "id": round.id, "peak": round.peak,
                "need": round.need, "why": why,
            }),
        }]
    }

    fn cooling_down(&self, at: Instant) -> bool {
        self.last_round_end
            .map(|end| at.duration_since(end) < ROUND_COOLDOWN)
            .unwrap_or(false)
    }

    /// The echo: screen black, and the bump darker than the room because the phone blocks it.
    fn echo_verdict(&self, lux: f32) -> Option<f32> {
        let round = self.round.as_ref()?;
        let e = self.contact_level(&round.device_id);
        let fell = round.peak - lux;
        if self.room_dark {
            return (fell >= 0.45 * e).then_some(lux);
        }
        let blocked = lux <= (0.75 * self.room).max(12.0);
        (fell >= 0.30 * e && blocked).then_some(lux)
    }

    fn learn_room(&mut self, lux: f32) {
        if self.prox == Proximity::Near {
            return;
        }
        self.room += 0.15 * (lux - self.room);
        if self.room < 5.0 {
            self.room_dark = true;
        } else if self.room > 8.0 {
            self.room_dark = false;
        }
    }

    fn remember_sample(&mut self, device_id: &str, sample: f32) {
        let list = self.samples.entry(device_id.to_string()).or_default();
        list.push_back(sample.clamp(MIN_E, MAX_E));
        while list.len() > E_SAMPLES {
            list.pop_front();
        }
    }
}

/// The newest phone that can do light keys.
fn pick(candidates: &[Candidate]) -> Option<Candidate> {
    candidates
        .iter()
        .filter(|c| c.caps >= 3)
        .max_by_key(|c| c.last_heard)
        .cloned()
}

fn median(values: &VecDeque<f32>) -> f32 {
    let mut sorted: Vec<f32> = values.iter().copied().collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    } else {
        sorted[mid]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phone() -> Vec<Candidate> {
        vec![Candidate {
            device_id: "phone".into(),
            caps: 3,
            last_heard: Instant::now(),
        }]
    }

    fn lux(at: Instant, value: f32) -> Lux {
        Lux { lux: value, at }
    }

    fn tap(key: &mut LightKey, t0: Instant, white: f32, black: f32) -> Value {
        key.on_lux(lux(t0, 400.0));
        let grab = key.on_prox(Proximity::Near, t0 + Duration::from_millis(100), &phone());
        let id = grab[0].message["id"].as_u64().unwrap();
        key.on_phone(
            "phone",
            &json!({ "t": "key-on", "id": id, "src": "screen" }),
            t0 + Duration::from_millis(200),
        );
        key.on_lux(lux(t0 + Duration::from_millis(600), white));
        key.on_phone(
            "phone",
            &json!({ "t": "key-off", "id": id }),
            t0 + Duration::from_millis(700),
        );
        let verdict = key.on_lux(lux(t0 + Duration::from_millis(1400), black));
        key.on_prox(Proximity::Far, t0 + Duration::from_millis(1500), &phone());
        verdict
            .into_iter()
            .next()
            .map(|a| a.message)
            .unwrap_or(Value::Null)
    }

    #[test]
    fn a_phone_on_the_bump_passes() {
        let mut key = LightKey::new();
        let verdict = tap(&mut key, Instant::now(), 1057.0, 43.0);
        assert_eq!(verdict["t"], "key-ok", "the dark echo passes: {verdict}");
        assert!(!key.round_running());
    }

    #[test]
    fn a_hand_never_makes_light() {
        let mut key = LightKey::new();
        let t0 = Instant::now();
        key.on_lux(lux(t0, 400.0));
        let grab = key.on_prox(Proximity::Near, t0 + Duration::from_millis(100), &phone());
        let id = grab[0].message["id"].as_u64().unwrap();
        key.on_phone(
            "phone",
            &json!({ "t": "key-on", "id": id, "src": "screen" }),
            t0 + Duration::from_millis(200),
        );
        // A hand darkens the bump but never brightens it: the round must time out.
        key.on_lux(lux(t0 + Duration::from_millis(600), 12.0));
        let failed = key.tick(t0 + Duration::from_millis(1600));
        assert_eq!(failed[0].message["why"], "not-seen");
    }

    #[test]
    fn a_lamp_that_stays_lit_fails_the_echo() {
        let mut key = LightKey::new();
        let t0 = Instant::now();
        key.on_lux(lux(t0, 400.0));
        let grab = key.on_prox(Proximity::Near, t0 + Duration::from_millis(100), &phone());
        let id = grab[0].message["id"].as_u64().unwrap();
        key.on_phone(
            "phone",
            &json!({ "t": "key-on", "id": id, "src": "screen" }),
            t0 + Duration::from_millis(200),
        );
        key.on_lux(lux(t0 + Duration::from_millis(700), 900.0));
        key.on_phone(
            "phone",
            &json!({ "t": "key-off", "id": id }),
            t0 + Duration::from_millis(800),
        );
        // Still bright after the phone says it is black: not a phone on the sensor.
        key.on_lux(lux(t0 + Duration::from_millis(1500), 860.0));
        let failed = key.tick(t0 + Duration::from_millis(2900));
        assert_eq!(failed[0].message["why"], "stayed-lit");
    }

    #[test]
    fn leaving_the_bump_cancels_the_round() {
        let mut key = LightKey::new();
        let t0 = Instant::now();
        let grab = key.on_prox(Proximity::Near, t0 + Duration::from_millis(100), &phone());
        assert_eq!(grab[0].message["t"], "grab");
        let failed = key.on_prox(Proximity::Far, t0 + Duration::from_millis(500), &phone());
        assert_eq!(failed[0].message["why"], "gone");
    }

    #[test]
    fn a_phone_without_the_light_key_gets_nothing() {
        let mut key = LightKey::new();
        let t0 = Instant::now();
        let old = vec![Candidate {
            device_id: "old".into(),
            caps: 0,
            last_heard: t0,
        }];
        assert!(key
            .on_prox(Proximity::Near, t0 + Duration::from_millis(100), &old)
            .is_empty());
    }

    #[test]
    fn the_contact_level_learns_from_passes() {
        let mut key = LightKey::new();
        assert_eq!(key.contact_level("phone"), DEFAULT_E);
        let t0 = Instant::now();
        for (i, white) in [1000.0f32, 1020.0, 1040.0].iter().enumerate() {
            let at = t0 + Duration::from_secs(i as u64 * 10);
            let verdict = tap(&mut key, at, *white, 40.0);
            assert_eq!(verdict["t"], "key-ok", "pass {i}");
        }
        let learned = key.contact_level("phone");
        assert!(
            (learned - 980.0).abs() < 5.0,
            "median of white minus black, got {learned}"
        );
    }

    #[test]
    fn a_knock_from_the_phone_starts_a_round_too() {
        let mut key = LightKey::new();
        let t0 = Instant::now();
        let grab = key.on_knock("phone", 3, t0);
        assert_eq!(grab[0].message["t"], "grab");
        assert_eq!(grab[0].device_id, "phone");
    }
}
