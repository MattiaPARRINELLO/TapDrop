//! The webcam as a 30 fps light sensor.
//!
//! The ambient light sensor in the bump answers once a second, which is too slow to catch the
//! phone's white flash. The camera next to it sees the same light thirty times a second, so a tap
//! is decided in one frame. Exposure is fixed for the round: auto-exposure would chase the white
//! screen back to mid-grey and hide it.
//!
//! Opening the camera lights its LED, so this only runs while a round is in progress.

use anyhow::{Context, Result};
use v4l::buffer::Type;
use v4l::control::{Control, Value};
use v4l::io::traits::CaptureStream;
use v4l::prelude::*;
use v4l::video::Capture;
use v4l::FourCC;

/// V4L2 control ids: auto exposure and its absolute time, from `linux/v4l2-controls.h`.
const AUTO_EXPOSURE: u32 = 0x009a_0901;
const EXPOSURE_ABSOLUTE: u32 = 0x009a_0902;
const MANUAL: i64 = 1;
const APERTURE_PRIORITY: i64 = 3;
/// Short enough that a phone screen still saturates the frame, long enough for a dim room.
const EXPOSURE: i64 = 80;

const WIDTH: u32 = 320;
const HEIGHT: u32 = 180;

/// Which camera to use. The first one on this machine is the built-in one; `TAPDROP_CAMERA` overrides.
fn camera_index() -> usize {
    std::env::var("TAPDROP_CAMERA")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// Whether a camera this program could use exists at all.
pub fn available() -> bool {
    Device::new(camera_index()).is_ok()
}

/// Streams the mean brightness of each frame until the receiver goes away.
///
/// Values are 0 for a black frame and about 250 for a phone screen at full brightness; the caller
/// scales them into the light key's units.
pub fn stream_means(tx: tokio::sync::mpsc::Sender<f32>) -> Result<()> {
    let mut device = Device::new(camera_index()).context("opening the camera")?;
    set_control(&mut device, AUTO_EXPOSURE, MANUAL);
    set_control(&mut device, EXPOSURE_ABSOLUTE, EXPOSURE);

    let mut format = device.format().context("reading the camera format")?;
    format.fourcc = FourCC::new(b"YUYV");
    format.width = WIDTH;
    format.height = HEIGHT;
    device
        .set_format(&format)
        .context("setting the camera format")?;

    let mut stream = MmapStream::with_buffers(&device, Type::VideoCapture, 4)
        .context("starting the camera stream")?;
    loop {
        let (buffer, _meta) = stream.next().context("reading a frame")?;
        if tx.blocking_send(frame_mean(buffer)).is_err() {
            break;
        }
    }
    // Never leave the camera in manual exposure: that would break every video call after a tap.
    set_control(&mut device, AUTO_EXPOSURE, APERTURE_PRIORITY);
    Ok(())
}

fn set_control(device: &mut Device, id: u32, value: i64) {
    let control = Control {
        id,
        value: Value::Integer(value),
    };
    if let Err(e) = device.set_control(control) {
        eprintln!("tapdrop: camera control {id:#x} refused: {e}");
    }
}

/// YUYV stores brightness first, then chroma: the mean of the even bytes is the picture's light.
fn frame_mean(buffer: &[u8]) -> f32 {
    let luma = buffer.iter().step_by(2);
    let mut sum: u64 = 0;
    let mut count: u64 = 0;
    for value in luma {
        sum += u64::from(*value);
        count += 1;
    }
    if count == 0 {
        0.0
    } else {
        sum as f32 / count as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mean_reads_brightness_not_colour() {
        // Two pixels: (Y=200, U=0, Y=100, V=255)
        assert_eq!(frame_mean(&[200, 0, 100, 255]), 150.0);
        assert_eq!(frame_mean(&[0, 255, 0, 255]), 0.0);
        assert_eq!(frame_mean(&[]), 0.0);
    }
}
