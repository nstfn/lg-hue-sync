use std::time::Instant;

use super::{CaptureError, CapturedFrame, ScreenCapture};

/// Synthetic capture for macOS hosts, tests and CI, and the placeholder that holds no native
/// capture owner on the TV.
pub struct MockCapture {
    width: u32,
    height: u32,
    buffer: Vec<u8>,
    start_time: Instant,
    is_tv_hardware: bool,
}

impl MockCapture {
    pub fn new(width: u32, height: u32) -> Self {
        let size = (width * height * 4) as usize;
        let is_tv_hardware = std::path::Path::new(super::vtcapture::LIBVTCAPTURE_PATH).exists();
        Self {
            width,
            height,
            buffer: vec![0; size],
            start_time: Instant::now(),
            is_tv_hardware,
        }
    }
}

impl ScreenCapture for MockCapture {
    fn acquire_frame(&mut self) -> std::result::Result<CapturedFrame<'_>, CaptureError> {
        if self.is_tv_hardware {
            // On real TV hardware, keep buffer black during fallback/settling
            // rather than distracting rainbow test pattern
            return Ok(CapturedFrame {
                data: &self.buffer,
                width: self.width,
                height: self.height,
                is_bgra: false,
            });
        }

        let elapsed = self.start_time.elapsed().as_secs_f32();
        let base_hue = (elapsed * 45.0) % 360.0;

        for y in 0..self.height {
            let row_offset = (y * self.width * 4) as usize;
            for x in 0..self.width {
                let pixel_offset = row_offset + (x * 4) as usize;
                let hue = (base_hue + (x as f32 / self.width as f32) * 180.0) % 360.0;
                let (r, g, b) = hsv_to_rgb(hue, 0.9, 0.9);
                self.buffer[pixel_offset] = r;
                self.buffer[pixel_offset + 1] = g;
                self.buffer[pixel_offset + 2] = b;
                self.buffer[pixel_offset + 3] = 255;
            }
        }

        Ok(CapturedFrame {
            data: &self.buffer,
            width: self.width,
            height: self.height,
            is_bgra: false,
        })
    }

    fn resolution(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    (
        ((r + m) * 255.0).round() as u8,
        ((g + m) * 255.0).round() as u8,
        ((b + m) * 255.0).round() as u8,
    )
}
