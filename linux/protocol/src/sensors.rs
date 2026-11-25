//! What the sensors report, in the shape the light key reasons about.

use std::time::Instant;

/// A light reading, with the moment it was taken.
#[derive(Debug, Clone, Copy)]
pub struct Lux {
    pub lux: f32,
    pub at: Instant,
}

/// Whether something is against the camera bump.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proximity {
    /// Nothing within a few centimetres: a person sitting at the desk reads this.
    Far,
    /// Something is against the sensor: a phone, a hand, a closed lid.
    Near,
}
