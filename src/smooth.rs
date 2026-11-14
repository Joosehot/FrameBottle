//! SMOOTHER: the frames between node A and node B, guessed from vectors.
//!
//! The frame between is cut into blocks. For each block one vector d is
//! found such that A at (p - d) and B at (p + d) look most alike: what is
//! at p in the middle came from p - d in A and goes to p + d in B. The
//! search is done first on the frames at half size, then put right by one
//! pixel at full size. A guessed frame at k/n of the way takes A at
//! p - 2d k/n and B at p + 2d (n-k)/n and mixes them by how near each is.
//!
//! Guessed frames are never stored in a bottle and never count as frames
//! of the movie.

use crate::motor::Rules;
use crate::pack::Geometry;
use rayon::prelude::*;

/// One grey value per pixel, for the search.
fn luma(geo: &Geometry, frame: &[u8]) -> Vec<u8> {
    let p = &geo.planes[0];
    let data = &frame[..p.len()];
    match (p.ch, p.bps) {
        (1, 1) => data.to_vec(),
        // 10 bits in two bytes, little end first.
        (1, _) => data.chunks_exact(2).map(|s| (u16::from_le_bytes([s[0], s[1]]) >> 2).min(255) as u8).collect(),
        // RGB: green carries most of what the eye calls light.
        _ => data.chunks_exact(p.px()).map(|s| s[1]).collect(),
    }
}

fn half(l: &[u8], w: usize, h: usize) -> (Vec<u8>, usize, usize) {
    let (w2, h2) = (w.div_ceil(2), h.div_ceil(2));
    let mut out = vec![0u8; w2 * h2];
    for y in 0..h2 {
        let (y0, y1) = (2 * y, (2 * y + 1).min(h - 1));
        for x in 0..w2 {
            let (x0, x1) = (2 * x, (2 * x + 1).min(w - 1));
            let s = l[y0 * w + x0] as u32 + l[y0 * w + x1] as u32 + l[y1 * w + x0] as u32 + l[y1 * w + x1] as u32;
            out[y * w2 + x] = ((s + 2) / 4) as u8;
        }
    }
    (out, w2, h2)
}

/// How unlike A at (block - d) and B at (block + d) are: the sum of differences.
#[allow(clippy::too_many_arguments)]
fn sad(a: &[u8], b: &[u8], w: usize, h: usize, x0: usize, y0: usize, bs: usize, dx: i32, dy: i32) -> u32 {
    let (bw, bh) = (bs.min(w - x0), bs.min(h - y0));
    let (xa, ya, xb, yb) = (x0 as i32 - dx, y0 as i32 - dy, x0 as i32 + dx, y0 as i32 + dy);
    let inside = |x: i32, y: i32| x >= 0 && y >= 0 && x as usize + bw <= w && y as usize + bh <= h;
    let mut s = 0u32;
    if inside(xa, ya) && inside(xb, yb) {
        for y in 0..bh {
            let ra = &a[(ya as usize + y) * w + xa as usize..][..bw];
            let rb = &b[(yb as usize + y) * w + xb as usize..][..bw];
            s += ra.iter().zip(rb).map(|(p, q)| p.abs_diff(*q) as u32).sum::<u32>();
        }
    } else {
        let cl = |v: i32, max: usize| v.clamp(0, max as i32 - 1) as usize;
        for y in 0..bh as i32 {
            let (ra, rb) = (cl(ya + y, h) * w, cl(yb + y, h) * w);
            for x in 0..bw as i32 {
                s += a[ra + cl(xa + x, w)].abs_diff(b[rb + cl(xb + x, w)]) as u32;
            }
        }
    }
    s
}

pub struct Vectors {
    pub block: usize,
    pub bw: usize,
    pub bh: usize,
    /// Per block: A is read at p - d, B at p + d (so the block moved 2d from A to B).
    pub d: Vec<(i32, i32)>,
}

impl Vectors {
    /// How many blocks have a vector that is not zero.
    pub fn moving(&self) -> usize {
        self.d.iter().filter(|d| **d != (0, 0)).count()
    }
}

pub fn vectors(geo: &Geometry, a: &[u8], b: &[u8], rules: &Rules) -> Vectors {
    let (w, h, bs) = (geo.width, geo.height, rules.block);
    let (la, lb) = (luma(geo, a), luma(geo, b));
    let ((ha, w2, h2), (hb, _, _)) = (half(&la, w, h), half(&lb, w, h));
    let (bw, bh) = (w.div_ceil(bs), h.div_ceil(bs));
    // A block may move `range` pixels from A to B: d is half of that, and half again at half size.
    let r2 = (rules.range / 4) as i32;
    let d: Vec<(i32, i32)> = (0..bw * bh)
        .into_par_iter()
        .map(|i| {
            let (bx, by) = (i % bw, i / bw);
            let (x2, y2, bs2) = (bx * bs / 2, by * bs / 2, bs / 2);
            let (x0, y0) = (bx * bs, by * bs);
            let pixels = (bs.min(w - x0) * bs.min(h - y0)) as f64;
            // A block that stands still this well can be beaten clearly by no vector: not searched.
            let still = sad(&la, &lb, w, h, x0, y0, bs, 0, 0) as f64;
            if still <= rules.zero_bias * pixels {
                return (0, 0);
            }
            let mut best = (0i32, 0i32);
            if x2 < w2 && y2 < h2 {
                let mut best_cost = sad(&ha, &hb, w2, h2, x2, y2, bs2, 0, 0) as f64;
                for dy in -r2..=r2 {
                    for dx in -r2..=r2 {
                        if (dx, dy) == (0, 0) {
                            continue;
                        }
                        let cost = sad(&ha, &hb, w2, h2, x2, y2, bs2, dx, dy) as f64 + rules.zero_bias * (bs2 * bs2) as f64;
                        if cost < best_cost {
                            best_cost = cost;
                            best = (dx, dy);
                        }
                    }
                }
            }
            if best == (0, 0) {
                return best;
            }
            // Put right at full size, by one pixel each way.
            let mut fine = (best.0 * 2, best.1 * 2);
            let mut fine_sad = u32::MAX;
            for dy in -1..=1 {
                for dx in -1..=1 {
                    let c = (best.0 * 2 + dx, best.1 * 2 + dy);
                    let s = sad(&la, &lb, w, h, x0, y0, bs, c.0, c.1);
                    if s < fine_sad {
                        fine_sad = s;
                        fine = c;
                    }
                }
            }
            // No vector explains this block (something new came in), or the vector is no
            // clear gain over standing still (it followed the grain): it is mixed in place.
            if fine_sad as f64 / pixels > rules.poor_match || fine_sad as f64 + rules.zero_bias * pixels >= still {
                return (0, 0);
            }
            fine
        })
        .collect();
    Vectors { block: bs, bw, bh, d }
}

/// The guessed frame `k` of `n` steps from A to B (0 < k < n).
pub fn between(geo: &Geometry, a: &[u8], b: &[u8], v: &Vectors, k: usize, n: usize) -> Vec<u8> {
    let mut out = vec![0u8; geo.frame_len()];
    let (wb, wa) = ((k * 256 / n) as u32, 256 - (k * 256 / n) as u32);
    let (ka, kb) = (k as i32, (n - k) as i32);
    let mut rest = &mut out[..];
    for (p, at) in geo.planes.iter().zip(geo.offsets()) {
        let (plane, r) = rest.split_at_mut(p.len());
        rest = r;
        let (pa, pb) = (&a[at..at + p.len()], &b[at..at + p.len()]);
        let (stride, px, bps) = (p.stride(), p.px(), p.bps);
        let (bw_px, bh_px) = (v.block >> p.sx, v.block >> p.sy);
        let cl = |x: i32, max: usize| x.clamp(0, max as i32 - 1) as usize;
        plane.par_chunks_mut(stride * bh_px).enumerate().for_each(|(by, rows)| {
            for (ry, row) in rows.chunks_mut(stride).enumerate() {
                let y = (by * bh_px + ry) as i32;
                for bx in 0..v.bw {
                    let (dx, dy) = v.d[by.min(v.bh - 1) * v.bw + bx];
                    // The whole way is 2d; A is k/n of it behind, B is (n-k)/n ahead.
                    let (ax, ay) = ((-2 * dx * ka / n as i32) >> p.sx, (-2 * dy * ka / n as i32) >> p.sy);
                    let (bx_, by_) = ((2 * dx * kb / n as i32) >> p.sx, (2 * dy * kb / n as i32) >> p.sy);
                    let (ra, rb) = (cl(y + ay, p.h) * stride, cl(y + by_, p.h) * stride);
                    let x_end = ((bx + 1) * bw_px).min(p.w);
                    for x in bx * bw_px..x_end {
                        let (sa, sb) = (ra + cl(x as i32 + ax, p.w) * px, rb + cl(x as i32 + bx_, p.w) * px);
                        let o = x * px;
                        if bps == 1 {
                            for c in 0..px {
                                row[o + c] = ((pa[sa + c] as u32 * wa + pb[sb + c] as u32 * wb + 128) >> 8) as u8;
                            }
                        } else {
                            for c in (0..px).step_by(2) {
                                let (va, vb) = (u16::from_le_bytes([pa[sa + c], pa[sa + c + 1]]) as u32, u16::from_le_bytes([pb[sb + c], pb[sb + c + 1]]) as u32);
                                let m = ((va * wa + vb * wb + 128) >> 8) as u16;
                                row[o + c..o + c + 2].copy_from_slice(&m.to_le_bytes());
                            }
                        }
                    }
                }
            }
        });
    }
    out
}
