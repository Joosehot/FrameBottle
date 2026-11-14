//! The player's own window: each frame is shown when it is due.

use crate::pack::Geometry;
use rayon::prelude::*;
use std::time::{Duration, Instant};

pub struct Screen {
    window: minifb::Window,
    pixels: Vec<u32>,
    w: usize,
    h: usize,
    step: Duration,
    due: Instant,
}

impl Screen {
    /// A window for frames of this layout, `fps` of them a second. A frame
    /// larger than 1600 x 900 is shown smaller (the frame itself is not changed).
    pub fn open(geo: &Geometry, fps: (u64, u64)) -> Result<Screen, String> {
        let shrink = (geo.width as f64 / 1600.0).max(geo.height as f64 / 900.0).max(1.0);
        let (w, h) = (((geo.width as f64 / shrink) as usize).max(1), ((geo.height as f64 / shrink) as usize).max(1));
        let window = minifb::Window::new("Frame Bottle", w, h, minifb::WindowOptions::default()).map_err(|e| format!("the window: {e}"))?;
        Ok(Screen { window, pixels: vec![0; w * h], w, h, step: Duration::from_secs_f64(fps.1 as f64 / fps.0 as f64), due: Instant::now() })
    }

    /// Shows the frame at its time. False when the window was closed (or Esc pressed).
    pub fn show(&mut self, geo: &Geometry, frame: &[u8]) -> bool {
        paint(geo, frame, &mut self.pixels, self.w, self.h);
        let now = Instant::now();
        if self.due > now {
            std::thread::sleep(self.due - now);
        } else if now - self.due > self.step * 4 {
            // The frames came late: the clock starts again from here.
            self.due = now;
        }
        self.due += self.step;
        self.window.update_with_buffer(&self.pixels, self.w, self.h).is_ok() && self.window.is_open() && !self.window.is_key_down(minifb::Key::Escape)
    }
}

/// The frame as 0RGB pixels of a `w` x `h` window (the nearest pixel is taken).
fn paint(geo: &Geometry, frame: &[u8], out: &mut [u32], w: usize, h: usize) {
    let p0 = &geo.planes[0];
    let at = geo.offsets();
    let clamp = |v: i32| v.clamp(0, 255) as u32;
    out.par_chunks_mut(w).enumerate().for_each(|(oy, row)| {
        let sy = oy * geo.height / h;
        for (ox, px) in row.iter_mut().enumerate() {
            let sx = ox * geo.width / w;
            *px = if geo.planes.len() == 3 {
                // YUV (video range; BT.709 from 720 rows up, BT.601 below).
                let sample = |i: usize, x: usize, y: usize| {
                    let p = &geo.planes[i];
                    let o = at[i] + (y * p.w + x) * p.bps;
                    if p.bps == 1 { frame[o] as i32 } else { (u16::from_le_bytes([frame[o], frame[o + 1]]) >> 2) as i32 }
                };
                let c = &geo.planes[1];
                let (y, u, v) = (sample(0, sx, sy) - 16, sample(1, sx >> c.sx, sy >> c.sy) - 128, sample(2, sx >> c.sx, sy >> c.sy) - 128);
                let (r, g, b) = if geo.height >= 720 {
                    (298 * y + 459 * v, 298 * y - 55 * u - 136 * v, 298 * y + 541 * u)
                } else {
                    (298 * y + 409 * v, 298 * y - 100 * u - 208 * v, 298 * y + 516 * u)
                };
                clamp((r + 128) >> 8) << 16 | clamp((g + 128) >> 8) << 8 | clamp((b + 128) >> 8)
            } else {
                let o = (sy * p0.w + sx) * p0.px();
                match p0.ch {
                    1 => (frame[o] as u32) * 0x010101,
                    // RGBA is shown over black.
                    4 => {
                        let a = frame[o + 3] as u32;
                        let over = |v: u8| (v as u32 * a + 127) / 255;
                        over(frame[o]) << 16 | over(frame[o + 1]) << 8 | over(frame[o + 2])
                    }
                    _ => (frame[o] as u32) << 16 | (frame[o + 1] as u32) << 8 | frame[o + 2] as u32,
                }
            };
        }
    });
}
