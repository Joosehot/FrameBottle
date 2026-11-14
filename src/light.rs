//! LIGHT: a frame's brightness kept in fewer levels, and fake film grain.
//!
//! **Dimmed light** (`--light N`). The brightness plane (Y; the colour
//! planes Cb and Cr are left as they are) is kept as the number of a
//! level, not as the brightness itself. With `--light N` there are N
//! levels on a curve: level i stands for 255 (i / (N - 1)) ^ 2.2, so the
//! dark shades lie one brightness apart and the bright ones many. The
//! player brightens a frame back by the same curve before it shows it.
//!
//! **A light grid for each frame** (`--grid N`). The frame's own
//! brightness values are counted, and N levels are put where the values
//! are densest (each level is the mean of the values that are nearest to
//! it, found by going round a few times). A frame that uses N values or
//! fewer keeps them all. The grid is stored with the frame; a frame whose
//! brightness the grid before it fits nearly as well stores none and uses
//! that one.
//!
//! Both are the same thing in the bottle: a grid (the brightness of each
//! level) stored with a frame, or "the one before"; `--light` stores its
//! curve with the first frame only.
//!
//! **Banding.** A level stands for every brightness between it and half
//! way to its neighbours, so a smooth sky comes back in bands. The player
//! takes each sample toward the mean of its neighbours of the same or the
//! next level, never out of the span its own level stands for. That costs
//! no byte, and unlike noise laid on before packing it does not make the
//! frames harder to pack.
//!
//! **Fake film grain** (`--grain`). What a packing that is not exact
//! leaves out is mostly the grain of the picture. The packer measures how
//! strong it was over the film (one number), and the player lays grain of
//! that strength, made from the frame's number, on what it shows.

use crate::pack::Geometry;
use rayon::prelude::*;

/// A frame that carries a grid (or "the one before") starts with this.
pub const KIND: u8 = 10;

pub struct Light {
    levels: usize,
    /// A grid for each frame (else one curve for the film).
    per_frame: bool,
    last: Option<Vec<u8>>,
    /// Frames that stored a grid of their own, and frames.
    pub stored: usize,
    pub frames: usize,
}

/// Which level each brightness is nearest to.
fn nearest(grid: &[u8]) -> [u8; 256] {
    let mut to = [0u8; 256];
    let mut k = 0;
    for (v, slot) in to.iter_mut().enumerate() {
        while k + 1 < grid.len() && (grid[k + 1] as i32 - v as i32).abs() <= (grid[k] as i32 - v as i32).abs() {
            k += 1;
        }
        *slot = k as u8;
    }
    to
}

/// The squares of how far each brightness is from its level, over a frame's count of values.
fn misses(count: &[u64; 256], grid: &[u8]) -> u64 {
    let to = nearest(grid);
    (0..256).map(|v| count[v] * ((v as i64 - grid[to[v] as usize] as i64).pow(2) as u64)).sum()
}

impl Light {
    pub fn curve(levels: usize) -> Light {
        Light { levels: levels.clamp(2, 256), per_frame: false, last: None, stored: 0, frames: 0 }
    }
    pub fn grid(levels: usize) -> Light {
        Light { levels: levels.clamp(2, 256), per_frame: true, last: None, stored: 0, frames: 0 }
    }

    /// The film's curve: level i is 255 (i / (N - 1)) ^ 2.2. Levels that fall on the same
    /// brightness (the darkest ones) are one level.
    fn curve_grid(&self) -> Vec<u8> {
        let mut g: Vec<u8> = (0..self.levels).map(|i| (255.0 * (i as f64 / (self.levels - 1) as f64).powf(2.2)).round() as u8).collect();
        g.dedup();
        g
    }

    /// The levels put where this frame's brightness values are densest.
    fn frame_grid(&self, count: &[u64; 256]) -> Vec<u8> {
        let used: Vec<u8> = (0..256).filter(|v| count[*v] > 0).map(|v| v as u8).collect();
        if used.len() <= self.levels {
            return used;
        }
        // Start with the same number of samples to each level, then go round: each level
        // becomes the mean of the values nearest to it.
        let total: u64 = count.iter().sum();
        let mut grid: Vec<u8> = Vec::new();
        let (mut seen, mut next) = (0u64, 0usize);
        for (v, c) in count.iter().enumerate() {
            seen += c;
            while next < self.levels && seen * self.levels as u64 > (2 * next as u64 + 1) * total / 2 {
                grid.push(v as u8);
                next += 1;
            }
        }
        grid.dedup();
        for _ in 0..24 {
            let to = nearest(&grid);
            let mut sum = vec![(0u64, 0u64); grid.len()];
            for v in 0..256 {
                sum[to[v] as usize].0 += count[v] * v as u64;
                sum[to[v] as usize].1 += count[v];
            }
            let mut moved: Vec<u8> = sum.iter().zip(&grid).map(|(s, g)| if s.1 > 0 { ((s.0 + s.1 / 2) / s.1) as u8 } else { *g }).collect();
            moved.dedup();
            if moved == grid {
                break;
            }
            grid = moved;
        }
        grid
    }

    /// The frame's brightness plane becomes level numbers. Returns the grid when this
    /// frame stores one (None: it uses the one before).
    pub fn dim(&mut self, geo: &Geometry, frame: &mut [u8]) -> Option<Vec<u8>> {
        let light = &mut frame[..geo.planes[0].len()];
        self.frames += 1;
        let mut stores = None;
        if !self.per_frame {
            if self.last.is_none() {
                stores = Some(self.curve_grid());
            }
        } else {
            let mut count = [0u64; 256];
            for v in light.iter() {
                count[*v as usize] += 1;
            }
            let grid = self.frame_grid(&count);
            // The grid before is used again when it fits this frame nearly as well: a
            // new grid changes what every level number means, and costs its own bytes.
            let fits = self.last.as_ref().is_some_and(|last| misses(&count, last) as f64 <= misses(&count, &grid) as f64 * 1.25 + light.len() as f64 * 0.05);
            if !fits {
                stores = Some(grid);
            }
        }
        if let Some(g) = &stores {
            self.last = Some(g.clone());
            self.stored += 1;
        }
        let to = nearest(self.last.as_ref().unwrap());
        for v in light.iter_mut() {
            *v = to[*v as usize];
        }
        stores
    }
}

/// A packed frame with its grid (or "the one before") in front.
pub fn wrap(grid: Option<&[u8]>, tile: Vec<u8>) -> Vec<u8> {
    let g = grid.unwrap_or(&[]);
    let mut out = vec![KIND];
    out.extend((g.len() as u16).to_le_bytes());
    out.extend(g);
    out.extend(tile);
    out
}

/// The grid a frame stores (if it stores one) and the packed frame itself.
pub fn strip(tile: Vec<u8>) -> (Option<Vec<u8>>, Vec<u8>) {
    if tile.first() != Some(&KIND) || tile.len() < 3 {
        return (None, tile);
    }
    let n = u16::from_le_bytes([tile[1], tile[2]]) as usize;
    let grid = (n > 0 && tile.len() >= 3 + n).then(|| tile[3..3 + n].to_vec());
    (grid, tile[(3 + n).min(tile.len())..].to_vec())
}

/// The frame brightened back: each level number becomes its brightness, moved toward
/// the mean of its neighbours of the same or the next level, inside the span the level
/// stands for (so that a smooth shade does not come back in bands).
pub fn brighten(geo: &Geometry, frame: &[u8], grid: &[u8]) -> Vec<u8> {
    let mut out = frame.to_vec();
    let p = &geo.planes[0];
    let (w, h) = (p.w, p.h);
    let level = |i: u8| grid[(i as usize).min(grid.len() - 1)] as i32;
    // The span each level stands for: half way to its neighbours.
    let span: Vec<(i32, i32)> = (0..grid.len())
        .map(|k| {
            let v = grid[k] as i32;
            (if k > 0 { (grid[k - 1] as i32 + v + 2) / 2 } else { v }, if k + 1 < grid.len() { (v + grid[k + 1] as i32 - 1) / 2 } else { v })
        })
        .collect();
    let src = &frame[..p.len()];
    out[..p.len()].par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (x, o) in row.iter_mut().enumerate() {
            let i = src[y * w + x];
            let v = level(i);
            let (mut sum, mut n) = (2 * v, 2);
            for yy in y.saturating_sub(1)..(y + 2).min(h) {
                for xx in x.saturating_sub(1)..(x + 2).min(w) {
                    let j = src[yy * w + xx];
                    // A neighbour of the same level or the next, and not a shade far off (in a
                    // grid of few levels the next level can be another thing altogether).
                    if (yy, xx) != (y, x) && j.abs_diff(i) <= 1 && (level(j) - v).abs() <= 12 {
                        sum += level(j);
                        n += 1;
                    }
                }
            }
            let (lo, hi) = span[(i as usize).min(grid.len() - 1)];
            *o = ((sum + n / 2) / n).clamp(lo.min(v), hi.max(v)) as u8;
        }
    });
    out
}

/// How strong the grain is that a packing left out: the root of the mean square of
/// what the rebuilt brightness differs from the movie's by, in 16ths of a level.
pub fn grain_of(squares: f64, samples: f64) -> u8 {
    ((squares / samples.max(1.0)).sqrt() * 16.0).round().min(255.0) as u8
}

/// Fake film grain laid on the frame's brightness: `strength` 16ths of a level (as
/// `grain_of` measured), `share` of it, made from the frame's number so that every
/// player lays the same grain.
pub fn grain(geo: &Geometry, frame: &mut [u8], node: usize, strength: u8) {
    // Three quarters of what was measured: what a packing leaves out is not all grain.
    let share = 0.75;
    let p = &geo.planes[0];
    let sigma = strength as f64 / 16.0 * share;
    if sigma <= 0.0 {
        return;
    }
    // The sum of four even draws is near enough a bell: its spread is 2 / sqrt 3 of one draw's width.
    let scale = (sigma * 3f64.sqrt() / 2.0 * 256.0) as i64;
    frame[..p.len()].par_chunks_mut(p.w).enumerate().for_each(|(y, row)| {
        let mut s = (node as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (y as u64).wrapping_mul(0xD1B5_4A32_D192_ED03) | 1;
        for v in row.iter_mut() {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            // Four draws of -128..127, summed: -512..508.
            let draw = (s as u8 as i8) as i64 + ((s >> 8) as u8 as i8) as i64 + ((s >> 16) as u8 as i8) as i64 + ((s >> 24) as u8 as i8) as i64;
            *v = (*v as i64 + ((draw * scale + (1 << 14)) >> 15)).clamp(0, 255) as u8;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_of_few_shades_comes_back_as_it_was() {
        let geo = Geometry::of("yuv420p", 64, 48).unwrap();
        let mut frame: Vec<u8> = (0..geo.frame_len()).map(|i| [16u8, 90, 91, 200][(i / 5) % 4]).collect();
        let real = frame.clone();
        let mut light = Light::grid(64);
        let grid = light.dim(&geo, &mut frame).expect("the first frame stores its grid");
        assert_eq!(grid, vec![16, 90, 91, 200]);
        let (stored, tile) = strip(wrap(Some(&grid), vec![1, 2, 3]));
        assert_eq!((stored.as_deref(), tile), (Some(&grid[..]), vec![1, 2, 3]));
        // Four shades fit in 64 levels: nothing is lost, and nothing is smoothed out of its span.
        assert_eq!(brighten(&geo, &frame, &grid), real);
        // The same frame again uses the grid before it.
        let mut again = real.clone();
        assert!(light.dim(&geo, &mut again).is_none());
    }

    #[test]
    fn the_curve_keeps_dark_shades_apart() {
        let g = Light::curve(64).curve_grid();
        assert!(g.windows(2).all(|w| w[0] < w[1]));
        assert!(g[1] - g[0] <= 1 && g[g.len() - 1] - g[g.len() - 2] >= 6, "dark levels one apart, bright ones many: {g:?}");
    }
}
