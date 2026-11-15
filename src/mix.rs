//! MIX: the exact packer that guesses each sample with several guessers mixed, and
//! writes what the guess missed with several learners mixed (`--mix`).
//!
//! Every sample comes back exactly. For 8-bit YUV 4:2:0.
//!
//! **The guess.** Ten guessers offer a value for each sample:
//!
//! - from this frame alone: the median of left, above and their slope (LOCO-I's MED);
//!   the slope-following guess of CALIC (GAP); left; above; their mean;
//! - two learning guessers (one quick, one slow): a weighed sum of what stands around
//!   the sample, whose weights move after every sample by their part of the miss. With
//!   a frame before they see how the neighbours changed since it and how it slopes
//!   around the sample, to the right and below too;
//! - from the frame before (not with `--alone`), at the place a vector found for the
//!   sample's block of 16 x 16 points to: the sample there; that sample changed as the
//!   three neighbours changed (their median); and changed as left and above changed
//!   (their mean).
//!
//! Each speaks as loud as it was right on the samples just rebuilt (left, above, above
//! left, above right): the guess is their weighed mix, in whole numbers. Then the guess
//! is put right by what guesses in the same surroundings missed by on average.
//!
//! **The miss** is written yes/no by yes/no: is there one, its sign, how large (in
//! buckets), the digits inside the bucket. For each yes/no five learners say how likely
//! it is, each from its own view of the surroundings, each at two speeds:
//!
//! - how large the misses nearby were, and how far apart the guessers stand (for the
//!   sign: the signs of the misses nearby);
//! - the three slopes around the sample, LOCO-I's contexts (for the sign: on which side
//!   of the guess the frame before, the median and CALIC's guess stand);
//! - for light: how far the frame before stands from the guess, and how many of four
//!   neighbours stayed what they were (alone: the misses two away); for colour: how
//!   large the miss of the light was at the same place, and for the second colour the
//!   miss of the first;
//! - the misses of the four neighbours themselves, each within two;
//! - how far and to which side the frame before, the median and the learning guesser
//!   stand from the guess, and how many neighbours stayed.
//!
//! Two mixers weigh the ten likelihoods by how right each has been for this kind of
//! yes/no (logistic mixing, in whole numbers; one mixer's weights go by how large the
//! misses nearby were, the other's by how many neighbours stayed), a last learner puts
//! the mixed likelihood right by how such likelihoods came out, and the range coder
//! writes with it.
//!
//! A frame is cut into bands of rows that are packed side by side on every core; a band
//! holds its light and its colours, reads the frame before freely, and of this frame
//! only itself.

use crate::guess::{Decoder, Encoder};
use crate::pack::Geometry;
use rayon::prelude::*;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// The packer as it is now, for the lightning's brand.
pub const VERSION: &str = "mix 2";

/// Thousandths of a bit spent on: light, the first colour, the second colour, vectors;
/// and again by what was said: is there a miss, its sign, its size.
pub static SPENT: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

const NP: usize = 10;
const SLOTS: usize = 16;
/// A miss's size by bucket: where each starts, and how many digits lie inside it.
const LO: [u32; 15] = [1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129];
const EXTRA: [usize; 14] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5];
const BLOCK: usize = 16;
const REACH: i32 = 7;

/// How fast the mixer learns (a shift), how fast the fast learners do (a shift), how
/// often at most the slow ones count, and how fast the learning guesser learns (of 65536).
const MIXER: i32 = 13;
const FAST: i32 = 4;
const SLOW: u16 = 1000;
const LEARN: i32 = 50000;

struct Tables {
    squash: Vec<u16>,
    stretch: Vec<i16>,
    weight: Vec<u64>,
    cost: Vec<f32>,
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        // 4096 / (1 + e^(-d / 256)), from a table of 33 points (as lpaq does it).
        let curve = |d: i32| -> i32 {
            const P: [i32; 33] = [1, 2, 3, 6, 10, 16, 27, 45, 73, 120, 194, 310, 488, 747, 1101, 1546, 2047, 2549, 2994, 3348, 3607, 3785, 3901, 3975, 4022, 4050, 4068, 4079, 4085, 4089, 4092, 4093, 4094];
            if d > 2047 {
                return 4095;
            }
            if d < -2047 {
                return 0;
            }
            let (w, k) = (d & 127, ((d >> 7) + 16) as usize);
            (P[k] * (128 - w) + P[k + 1] * w + 64) >> 7
        };
        let squash: Vec<u16> = (-2048..2048).map(|d| curve(d) as u16).collect();
        let mut stretch = vec![2047i16; 4096];
        let mut at = 0usize;
        for d in -2047..=2047 {
            let v = curve(d) as usize;
            for s in stretch.iter_mut().take(v + 1).skip(at) {
                *s = d as i16;
            }
            at = at.max(v + 1);
        }
        // A guesser wrong by s on the neighbours speaks 1 / (s + 2)^2 as loud.
        let weight = (0..2048u64).map(|s| ((1u64 << 44) / ((s + 2) * (s + 2))).max(1)).collect();
        let cost = (0..=4096).map(|i| -((i.max(1) as f32) / 4096.0).log2()).collect();
        Tables { squash, stretch, weight, cost }
    })
}

/// One way through the syntax for the packer and the player.
enum Coder<'a> {
    Write(Encoder, [f64; 7]),
    Read(Decoder<'a>),
}

impl Coder<'_> {
    /// One yes/no that is `p` likely (of 4096) to be yes. `what`: the plane (3: a
    /// vector) and what is said (4 is there a miss, 5 its sign, 6 its size).
    #[inline(always)]
    fn bit(&mut self, p: i32, v: bool, what: (usize, usize)) -> bool {
        let mut no = (4096 - p) as u16;
        match self {
            Coder::Write(e, spent) => {
                let bits = tables().cost[if v { p as usize } else { 4096 - p as usize }] as f64;
                spent[what.0] += bits;
                if what.1 != what.0 {
                    spent[what.1] += bits;
                }
                e.bit(&mut no, v);
                v
            }
            Coder::Read(d) => d.bit(&mut no),
        }
    }
    /// One yes/no the coder learns on its own.
    #[inline(always)]
    fn plain(&mut self, p: &mut u16, v: bool, what: (usize, usize)) -> bool {
        match self {
            Coder::Write(e, spent) => {
                let bits = tables().cost[if v { 4096 - *p as usize } else { *p as usize }] as f64;
                spent[what.0] += bits;
                if what.1 != what.0 {
                    spent[what.1] += bits;
                }
                e.bit(p, v);
                v
            }
            Coder::Read(d) => d.bit(p),
        }
    }
    /// A whole number with a sign (a vector's part).
    fn number(&mut self, p: &mut ([u16; 32], [u16; 2]), v: i32) -> i32 {
        let what = (3, 3);
        if !self.plain(&mut p.1[0], v != 0, what) {
            return 0;
        }
        let negative = self.plain(&mut p.1[1], v < 0, what);
        let n = v.unsigned_abs().clamp(1, 65535);
        let want = 32 - n.leading_zeros() as usize;
        let mut digits = 1;
        while digits < 16 && self.plain(&mut p.0[digits - 1], digits < want, what) {
            digits += 1;
        }
        let mut value = 1u32;
        for j in (0..digits - 1).rev() {
            value = (value << 1) | self.plain(&mut p.0[16 + j], (n >> j) & 1 == 1, what) as u32;
        }
        if negative { -(value as i32) } else { value as i32 }
    }
}

const A_SIZE: usize = 9 * 6 * 3;
const A_ALL: usize = A_SIZE + 81;
const B_ALL: usize = 125 * 3;
const C_ALL: usize = 128 * 3;
const D_ALL: usize = 625 * 3;
const E_ALL: usize = 625 * 3;
const SETS: usize = SLOTS * 3 * 3;

/// What a band has learned so far.
pub struct Model {
    /// The learning guessers' weights (65536 is one), for each plane.
    lw: [[i32; 14]; 3],
    lw_slow: [[i32; 14]; 3],
    /// The three learners: for each surrounding and each kind of yes/no, how likely
    /// "yes" is (of 65536), learned fast and learned slowly.
    a: Vec<[u16; 3]>,
    b: Vec<[u16; 3]>,
    c: Vec<[u16; 3]>,
    d: Vec<[u16; 3]>,
    e: Vec<[u16; 3]>,
    /// The last word: the mixed likelihood put right by how such likelihoods came out.
    apm: Vec<u16>,
    /// The mixer's weights (65536 is one), by kind of yes/no, plane and how large the misses nearby were.
    w: Vec<[i32; 11]>,
    /// A second set of weights, by kind of yes/no, plane and how many neighbours stayed what they were.
    w2: Vec<[i32; 11]>,
    /// The digits inside a bucket.
    extra: [[u16; 8]; 14],
    /// What guesses in the same surroundings missed by: the sum and how many.
    bias: Vec<(i32, i32)>,
    vector: [([u16; 32], [u16; 2]); 2],
}

impl Model {
    fn new() -> Model {
        Model {
            lw: [[0; 14]; 3],
            lw_slow: [[0; 14]; 3],
            a: vec![[32768, 32768, 0]; A_ALL * SLOTS],
            b: vec![[32768, 32768, 0]; B_ALL * SLOTS],
            c: vec![[32768, 32768, 0]; C_ALL * SLOTS],
            d: vec![[32768, 32768, 0]; D_ALL * SLOTS],
            e: vec![[32768, 32768, 0]; E_ALL * SLOTS],
            apm: (0..SETS * 33).map(|i| tables().squash[(((i % 33) as i32 - 16) * 128).clamp(-2047, 2047) as usize + 2048] * 16).collect(),
            w: vec![[8000, 8000, 8000, 8000, 8000, 8000, 8000, 8000, 8000, 8000, 0]; SETS],
            w2: vec![[8000, 8000, 8000, 8000, 8000, 8000, 8000, 8000, 8000, 8000, 0]; SLOTS * 3 * 5],
            extra: [[2048; 8]; 14],
            bias: vec![(0, 0); A_SIZE],
            vector: [([2048; 32], [2048; 2]); 2],
        }
    }

    /// One yes/no of a miss: the three learners' six likelihoods mixed, the yes/no
    /// written or read, and all of them taught.
    #[inline(always)]
    fn mixed(&mut self, c: &mut Coder, t: &Tables, ix: [usize; 5], (set, set2): (usize, usize), v: bool, what: (usize, usize)) -> bool {
        let pr = [self.a[ix[0]], self.b[ix[1]], self.c[ix[2]], self.d[ix[3]], self.e[ix[4]]];
        let mut st = [256i32; 11];
        for k in 0..5 {
            st[2 * k] = t.stretch[(pr[k][0] >> 4) as usize] as i32;
            st[2 * k + 1] = t.stretch[(pr[k][1] >> 4) as usize] as i32;
        }
        let (w, w2) = (&mut self.w[set], &mut self.w2[set2]);
        let dot: i64 = (0..11).map(|k| w[k] as i64 * st[k] as i64).sum();
        let dot2: i64 = (0..11).map(|k| w2[k] as i64 * st[k] as i64).sum();
        let (d1, d2) = ((dot >> 16).clamp(-2047, 2047) as i32, (dot2 >> 16).clamp(-2047, 2047) as i32);
        let (p1, p2) = (t.squash[(d1 + 2048) as usize] as i32, t.squash[(d2 + 2048) as usize] as i32);
        // The two sets of weights speak together: the mean of what they say.
        let d = (d1 + d2) >> 1;
        let p = (t.squash[(d + 2048) as usize] as i32).clamp(1, 4095);
        // The last word: between the two nearest of 33 likelihoods learned for this kind of yes/no.
        let (k, part) = (set.min(SETS - 1) * 33 + ((d + 2048) >> 7) as usize, (d + 2048) & 127);
        let seen = (self.apm[k] as i32 * (128 - part) + self.apm[k + 1] as i32 * part) >> 11;
        let said = ((p + 3 * seen + 2) >> 2).clamp(1, 4095);
        let bit = c.bit(said, v, what);
        let (err, err2) = (((bit as i32) << 12) - p1, ((bit as i32) << 12) - p2);
        for k in 0..11 {
            w[k] += (st[k] * err) >> MIXER;
            w2[k] += (st[k] * err2) >> MIXER;
        }
        let target = if bit { 65535i32 } else { 0 };
        self.apm[k] = (self.apm[k] as i32 + ((target - self.apm[k] as i32) >> 6)) as u16;
        self.apm[k + 1] = (self.apm[k + 1] as i32 + ((target - self.apm[k + 1] as i32) >> 6)) as u16;
        // The fast one moves a 16th of the way; the slow one 1 / (how often it was asked + 2),
        // so that a surrounding met for the first time is believed at once.
        let learn = |p: &mut [u16; 3]| {
            p[0] = (p[0] as i32 + ((target - p[0] as i32) >> FAST)) as u16;
            p[1] = (p[1] as i32 + (target - p[1] as i32) / (p[2] as i32 + 2)) as u16;
            p[2] = (p[2] + 1).min(SLOW);
        };
        learn(&mut self.a[ix[0]]);
        learn(&mut self.b[ix[1]]);
        learn(&mut self.c[ix[2]]);
        learn(&mut self.d[ix[3]]);
        learn(&mut self.e[ix[4]]);
        bit
    }

    /// A miss: written, or read. `at` is where the sample stands for the three
    /// learners (and for the sign, the first learner's other view).
    #[inline(always)]
    fn miss(&mut self, c: &mut Coder, t: &Tables, pk: usize, at: [usize; 5], signs: (usize, usize), (set, set2): (usize, usize), v: i32) -> i32 {
        let ix = |slot: usize| [at[0] * SLOTS + slot, at[1] * SLOTS + slot, at[2] * SLOTS + slot, at[3] * SLOTS + slot, at[4] * SLOTS + slot];
        let sets = |slot: usize| ((slot * 3 + pk) * 3 + set, (slot * 3 + pk) * 5 + set2);
        if !self.mixed(c, t, ix(0), sets(0), v != 0, (pk, 4)) {
            return 0;
        }
        let mut sx = ix(1);
        sx[0] = signs.0 * SLOTS + 1;
        sx[1] = signs.1 * SLOTS + 1;
        let negative = self.mixed(c, t, sx, sets(1), v < 0, (pk, 5));
        let size = v.unsigned_abs().clamp(1, 128);
        let want = LO.iter().rposition(|lo| *lo <= size).unwrap_or(0).min(13);
        let mut b = 0;
        while b < 13 && self.mixed(c, t, ix(2 + b), sets(2 + b), want > b, (pk, 6)) {
            b += 1;
        }
        let mut inside = 0u32;
        for j in (0..EXTRA[b]).rev() {
            inside = (inside << 1) | c.plain(&mut self.extra[b][j], (size.wrapping_sub(LO[b]) >> j) & 1 == 1, (pk, 6)) as u32;
        }
        let size = (LO[b] + inside) as i32;
        if negative { -size } else { size }
    }
}

#[inline(always)]
fn median(a: i32, b: i32, c: i32) -> i32 {
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    if c >= hi { hi } else if c <= lo { lo } else { c }
}

/// How large, in 9 steps.
#[inline(always)]
fn step(near: u32) -> usize {
    match near {
        0..=3 => near as usize,
        4..=5 => 4,
        6..=8 => 5,
        9..=13 => 6,
        14..=22 => 7,
        _ => 8,
    }
}

/// How far apart, in 6 steps.
#[inline(always)]
fn apart(d: i32) -> usize {
    match d {
        0 => 0,
        1..=2 => 1,
        3..=5 => 2,
        6..=11 => 3,
        12..=24 => 4,
        _ => 5,
    }
}

/// A slope, in 5 steps.
#[inline(always)]
fn slope(d: i32) -> usize {
    match d {
        i32::MIN..=-6 => 0,
        -5..=-2 => 1,
        -1..=1 => 2,
        2..=5 => 3,
        _ => 4,
    }
}

/// A miss's size, in 5 steps.
#[inline(always)]
fn five(d: u32) -> usize {
    match d {
        0 => 0,
        1 => 1,
        2..=3 => 2,
        4..=7 => 3,
        _ => 4,
    }
}

/// One plane of a band, sample by sample: guessed, its miss written or read, learned.
/// `cur` holds the band's rows of the plane (the real ones for the packer; the player
/// fills them), `res` takes each sample's miss. `light` is the misses of the band's
/// light with its width (for a colour), `first` those of the first colour (for the second).
#[allow(clippy::too_many_arguments)]
fn plane(c: &mut Coder, m: &mut Model, pk: usize, (w, h): (usize, usize), cur: &mut [u8], y0: usize, prev: Option<&[u8]>, vectors: &[(i32, i32)], res: &mut [i8], light: Option<(&[i8], usize)>, first: Option<&[i8]>) {
    let t = tables();
    let write = matches!(c, Coder::Write(..));
    let rows = cur.len() / w;
    let bs = if pk == 0 { BLOCK } else { BLOCK / 2 };
    let across = (if pk == 0 { w } else { w * 2 }).div_ceil(BLOCK);
    let n = if prev.is_some() { NP } else { 7 };
    let (mut lw, mut lw_slow) = (m.lw[pk], m.lw_slow[pk]);
    let (mut ec, mut eu) = (vec![0u8; NP * w], vec![0u8; NP * w]);
    for yy in 0..rows {
        let up = yy > 0;
        for x in 0..w {
            let i = yy * w + x;
            let left = x > 0;
            let right = x + 1 < w;
            // The frame before, where the block's vector points.
            let (vx, vy) = if prev.is_some() {
                let v = vectors[(yy / bs) * across + (x / bs).min(across - 1)];
                if pk == 0 { v } else { (v.0 >> 1, v.1 >> 1) }
            } else {
                (0, 0)
            };
            let pm = |dx: i32, dy: i32| -> i32 {
                let p = prev.unwrap();
                let (px, py) = ((x as i32 + vx + dx).clamp(0, w as i32 - 1) as usize, ((y0 + yy) as i32 + vy + dy).clamp(0, h as i32 - 1) as usize);
                p[py * w + px] as i32
            };
            let t0 = if prev.is_some() { pm(0, 0) } else { 128 };
            let west = if left { cur[i - 1] as i32 } else if up { cur[i - w] as i32 } else { t0 };
            let north = if up { cur[i - w] as i32 } else { west };
            let nw = if up && left { cur[i - w - 1] as i32 } else if up { north } else { west };
            let ne = if up && right { cur[i - w + 1] as i32 } else { north };
            let ww = if x > 1 { cur[i - 2] as i32 } else { west };
            let nn = if yy > 1 { cur[i - 2 * w] as i32 } else { north };
            let nne = if yy > 1 && right { cur[i - 2 * w + 1] as i32 } else { ne };
            let med = median(west, north, west + north - nw);
            // CALIC's guess: along the edge when one is there, else level with a lean.
            let (dh, dv) = ((west - ww).abs() + (north - nw).abs() + (north - ne).abs(), (west - nw).abs() + (north - nn).abs() + (ne - nne).abs());
            let gap = if dv - dh > 80 {
                west
            } else if dh - dv > 80 {
                north
            } else {
                let p = (west + north) / 2 + (ne - nw) / 4;
                (if dv - dh > 32 {
                    (p + west) / 2
                } else if dv - dh > 8 {
                    (3 * p + west) / 4
                } else if dh - dv > 32 {
                    (p + north) / 2
                } else if dh - dv > 8 {
                    (3 * p + north) / 4
                } else {
                    p
                })
                .clamp(0, 255)
            };
            let mut offers = [med, gap, west, north, (west + north + 1) / 2, 0, 0, 0, 0, 0];
            // The learning guesser: a base, and what stands around it told from the base.
            let (mut base, mut feats) = (west, [north - west, nw - west, ne - west, ww - west, nn - west, nne - west, med - west, gap - west, 0, 0, 0, 0, 0, 0]);
            let mut stayed = 0usize;
            if prev.is_some() {
                let (cw, cn) = (west - pm(-1, 0), north - pm(0, -1));
                let change = match (left, up) {
                    (true, true) => median(cw, cn, nw - pm(-1, -1)),
                    (true, false) => cw,
                    (false, true) => cn,
                    (false, false) => 0,
                };
                stayed = (left && cw == 0) as usize + (up && cn == 0) as usize + (up && left && nw == pm(-1, -1)) as usize + (up && right && ne == pm(1, -1)) as usize;
                offers[7] = t0;
                offers[8] = (t0 + change).clamp(0, 255);
                offers[9] = if left && up { (t0 + (cw + cn) / 2).clamp(0, 255) } else { offers[8] };
                // From the frame before: how the neighbours changed, and how the frame before
                // itself slopes around the sample (to the right and below too: it is all there).
                base = t0;
                feats = [if left { cw } else { 0 }, if up { cn } else { 0 }, if up && left { nw - pm(-1, -1) } else { 0 }, if up && right { ne - pm(1, -1) } else { 0 }, pm(1, 0) - t0, pm(0, 1) - t0, pm(-1, 0) - t0, pm(0, -1) - t0, med - t0, gap - t0, pm(1, 1) - t0, pm(-1, 1) - t0, if x > 1 { ww - pm(-2, 0) } else { 0 }, if yy > 1 { nn - pm(0, -2) } else { 0 }];
            }
            let lean: i64 = (0..14).map(|k| lw[k] as i64 * feats[k] as i64).sum();
            let learned = (base + ((lean + 32768) >> 16) as i32).clamp(0, 255);
            offers[5] = learned;
            let lean: i64 = (0..14).map(|k| lw_slow[k] as i64 * feats[k] as i64).sum();
            let learned_slow = (base + ((lean + 32768) >> 16) as i32).clamp(0, 255);
            offers[6] = learned_slow;
            // The mix: each speaks as loud as it was right on the neighbours.
            let (mut sum, mut total) = (0u64, 0u64);
            let (mut low, mut high) = (255, 0);
            for g in 0..n {
                let s = (if left { 2 * ec[g * w + x - 1] as usize } else { 0 }) + (if up { 2 * eu[g * w + x] as usize } else { 0 }) + (if up && left { eu[g * w + x - 1] as usize } else { 0 }) + (if up && right { eu[g * w + x + 1] as usize } else { 0 });
                let wt = t.weight[s.min(2047)];
                sum += wt;
                total += wt * offers[g] as u64;
                (low, high) = (low.min(offers[g]), high.max(offers[g]));
            }
            let mixed = ((total + sum / 2) / sum) as i32;
            // Where the sample stands.
            let r = |k: usize| res[k] as i32;
            let (rw, rn) = (if left { r(i - 1) } else { 0 }, if up { r(i - w) } else { 0 });
            let (rnw, rne) = (if up && left { r(i - w - 1) } else { 0 }, if up && right { r(i - w + 1) } else { 0 });
            let near = step((2 * (rw.unsigned_abs() + rn.unsigned_abs()) + rnw.unsigned_abs() + rne.unsigned_abs() + 1) / 3);
            let at_a = (near * 6 + apart(high - low)) * 3 + pk;
            let at_b = ((slope(ne - north) * 5 + slope(north - nw)) * 5 + slope(nw - west)) * 3 + pk;
            let mut sign_first = 1;
            let at_c = pk * 128
                + if let Some((lr, lw)) = light {
                    let lrows = lr.len() / lw;
                    let (lx, ly) = ((2 * x).min(lw - 1), (2 * yy).min(lrows - 1));
                    let (lx1, ly1) = ((lx + 1).min(lw - 1), (ly + 1).min(lrows - 1));
                    let l = lr[ly * lw + lx].unsigned_abs() as u32 + lr[ly * lw + lx1].unsigned_abs() as u32 + lr[ly1 * lw + lx].unsigned_abs() as u32 + lr[ly1 * lw + lx1].unsigned_abs() as u32;
                    let f = first.map_or(0, |f| {
                        sign_first = (f[i] as i32).signum() + 1;
                        five(f[i].unsigned_abs() as u32)
                    });
                    (five((l + 1) / 2) * 5 + f) * 3 + near.min(6) / 3
                } else if prev.is_some() {
                    (apart((t0 - mixed).abs()) * 5 + stayed) * 3 + near.min(6) / 3
                } else {
                    let (rww, rnn) = (if x > 1 { r(i - 2) } else { 0 }, if yy > 1 { r(i - 2 * w) } else { 0 });
                    five(rww.unsigned_abs()) * 5 + five(rnn.unsigned_abs())
                };
            let signs = A_SIZE + (((rw.signum() + 1) * 3 + rn.signum() + 1) as usize * 3 + sign_first as usize) * 3 + pk;
            // Put right by what guesses that stood here missed by.
            let b = &mut m.bias[at_a];
            let lean = if b.1 > 0 { (2 * b.0 + b.1 * b.0.signum()) / (2 * b.1) } else { 0 };
            let guess = (mixed + lean).clamp(0, 255);
            let real = cur[i];
            let want = if write { real.wrapping_sub(guess as u8) as i8 as i32 } else { 0 };
            let five_of = |r: i32| (r.clamp(-2, 2) + 2) as usize;
            let at_d = (((five_of(rw) * 5 + five_of(rn)) * 5 + five_of(rnw)) * 5 + five_of(rne)) * 3 + pk;
            // For the sign: on which side of the guess the frame before, the median and CALIC's guess stand.
            let side = |v: i32| ((v - guess).signum() + 1) as usize;
            let signs = (signs, ((side(if prev.is_some() { t0 } else { west }) * 3 + side(med)) * 3 + side(gap)) * 3 + pk);
            // How far, and to which side, the frame before, the median and CALIC's guess stand from the guess.
            let off = |v: i32| ((v - guess).clamp(-2, 2) + 2) as usize;
            let at_e = (((off(if prev.is_some() { t0 } else { west }) * 5 + off(med)) * 5 + off(learned)) * 5 + stayed) * 3 + pk;
            let got = m.miss(c, t, pk, [at_a, at_b, at_c, at_d, at_e], signs, (if near < 2 { 0 } else if near < 5 { 1 } else { 2 }, stayed), want);
            let real = if write { real } else { (guess as u8).wrapping_add(got as u8) };
            cur[i] = real;
            res[i] = got as i8;
            let b = &mut m.bias[at_a];
            b.0 += got as i8 as i32;
            b.1 += 1;
            if b.1 == 32 {
                *b = (b.0 / 2, 16);
            }
            for g in 0..n {
                ec[g * w + x] = (real as i32 - offers[g]).unsigned_abs().min(255) as u8;
            }
            // The learning guesser is taught: each weight moves by its part of the miss.
            let norm: i32 = feats.iter().map(|f| f * f).sum::<i32>() + 32;
            let pull = ((real as i32 - learned) * LEARN) as i64 * ((1i64 << 24) / norm as i64);
            let pull_slow = ((real as i32 - learned_slow) * (LEARN / 16)) as i64 * ((1i64 << 24) / norm as i64);
            for k in 0..14 {
                lw[k] += ((pull * feats[k] as i64) >> 24) as i32;
                lw_slow[k] += ((pull_slow * feats[k] as i64) >> 24) as i32;
            }
        }
        std::mem::swap(&mut ec, &mut eu);
    }
    (m.lw[pk], m.lw_slow[pk]) = (lw, lw_slow);
}

/// The vector of each block of 16 x 16 of light in rows `y0..y1`: where in the frame
/// before the block is most like itself. A block that is nearly what it was stays.
fn find(w: usize, h: usize, real: &[u8], prev: &[u8], y0: usize, y1: usize) -> Vec<(i32, i32)> {
    let mut out = Vec::new();
    for by in (y0..y1).step_by(BLOCK) {
        for bx in (0..w).step_by(BLOCK) {
            let (bw, bh) = (BLOCK.min(w - bx), BLOCK.min(h - by));
            let unlike = |dx: i32, dy: i32, limit: u32| -> u32 {
                let mut s = 0u32;
                for j in 0..bh {
                    let py = ((by + j) as i32 + dy).clamp(0, h as i32 - 1) as usize;
                    for k in 0..bw {
                        let px = ((bx + k) as i32 + dx).clamp(0, w as i32 - 1) as usize;
                        s += real[(by + j) * w + bx + k].abs_diff(prev[py * w + px]) as u32;
                    }
                    if s >= limit {
                        return s;
                    }
                }
                s
            };
            let still = unlike(0, 0, u32::MAX);
            let mut best = (still, 0, 0);
            if still > (bw * bh) as u32 {
                for dy in -REACH..=REACH {
                    for dx in -REACH..=REACH {
                        let s = unlike(dx, dy, best.0) + 4 * (dx.abs() + dy.abs()) as u32;
                        if s < best.0 {
                            best = (s, dx, dy);
                        }
                    }
                }
                // A place only a little more alike is the grain's doing.
                if best.0 * 10 > still * 9 {
                    best = (still, 0, 0);
                }
            }
            out.push((best.1, best.2));
        }
    }
    out
}

/// The chroma rows that go with the light's rows `y0..y1`.
fn colour_rows(geo: &Geometry, y0: usize, y1: usize) -> (usize, usize) {
    (y0 / 2, if y1 >= geo.height { geo.planes[1].h } else { y1 / 2 })
}

/// One band: its vectors, its light, its two colours; written, or read into `cur`.
fn band(geo: &Geometry, c: &mut Coder, m: &mut Model, cur: &mut [Vec<u8>; 3], prev: Option<[&[u8]; 3]>, mut vectors: Vec<(i32, i32)>, y0: usize, y1: usize) {
    let (w, h) = (geo.width, geo.height);
    if prev.is_some() {
        // The vectors, each told from the one to its left.
        let across = w.div_ceil(BLOCK);
        let blocks = across * (y1 - y0).div_ceil(BLOCK);
        vectors.resize(blocks, (0, 0));
        for k in 0..blocks {
            let from = if k % across > 0 { vectors[k - 1] } else { (0, 0) };
            vectors[k] = (from.0 + c.number(&mut m.vector[0], vectors[k].0 - from.0), from.1 + c.number(&mut m.vector[1], vectors[k].1 - from.1));
        }
    }
    let mut res: [Vec<i8>; 3] = std::array::from_fn(|p| vec![0i8; cur[p].len()]);
    let (c0, _) = colour_rows(geo, y0, y1);
    let [light, first, second] = cur;
    let [res_light, res_first, res_second] = &mut res;
    plane(c, m, 0, (w, h), light, y0, prev.map(|p| p[0]), &vectors, res_light, None, None);
    let (cw, ch) = (geo.planes[1].w, geo.planes[1].h);
    plane(c, m, 1, (cw, ch), first, c0, prev.map(|p| p[1]), &vectors, res_first, Some((&res_light[..], w)), None);
    plane(c, m, 2, (cw, ch), second, c0, prev.map(|p| p[2]), &vectors, res_second, Some((&res_light[..], w)), Some(&res_first[..]));
}

const FROM_BEFORE: u8 = 2;
const STILL: u8 = 4;
const HEAD: usize = 3;

/// Does rebuilding this frame need the frame before it?
pub fn from_before(payload: &[u8]) -> bool {
    payload.first().is_some_and(|f| f & FROM_BEFORE != 0)
}

fn band_rows(geo: &Geometry, bands: usize) -> usize {
    geo.height.div_ceil(bands.max(1)).max(1).div_ceil(BLOCK) * BLOCK
}

fn planes<'a>(geo: &Geometry, f: &'a [u8]) -> [&'a [u8]; 3] {
    let at = geo.offsets();
    std::array::from_fn(|i| &f[at[i]..at[i] + geo.planes[i].len()])
}

/// The rows of each plane that belong to the band of light rows `y0..y1`.
fn cut<'a>(geo: &Geometry, f: [&'a [u8]; 3], y0: usize, y1: usize) -> [&'a [u8]; 3] {
    let (c0, c1) = colour_rows(geo, y0, y1);
    let cw = geo.planes[1].w;
    [&f[0][y0 * geo.width..y1 * geo.width], &f[1][c0 * cw..c1 * cw], &f[2][c0 * cw..c1 * cw]]
}

/// What every band has learned from the frames so far. A frame that stands alone starts
/// it anew; a frame packed from the frame before goes on from what that frame left, so
/// the player must rebuild the frames in order (as it does anyway).
pub struct Memory(Vec<Model>);

impl Memory {
    pub fn new() -> Memory {
        Memory(Vec::new())
    }
    fn ready(&mut self, bands: usize, anew: bool) {
        if anew || self.0.len() != bands {
            self.0 = (0..bands).map(|_| Model::new()).collect();
        }
    }
}

/// Packs a frame exactly. `before`: the frame before it (None: the frame stands alone).
/// The bytes: flags, the number of bands, each band's length, the bands.
pub fn pack(geo: &Geometry, frame: &[u8], before: Option<&[u8]>, bands: usize, memory: &mut Memory) -> Vec<u8> {
    let mut out = vec![if before.is_some() { FROM_BEFORE } else { 0 }];
    out.extend((bands as u16).to_le_bytes());
    if before == Some(frame) {
        out[0] |= STILL;
        return out;
    }
    let (real, prev) = (planes(geo, frame), before.map(|b| planes(geo, b)));
    let rows = band_rows(geo, bands);
    let starts: Vec<usize> = (0..geo.height).step_by(rows).collect();
    memory.ready(starts.len(), before.is_none());
    let done: Vec<(Vec<u8>, [f64; 7])> = starts
        .par_iter()
        .zip(memory.0.par_iter_mut())
        .map(|(y0, m)| {
            let y0 = *y0;
            let y1 = (y0 + rows).min(geo.height);
            let vectors = prev.map_or(Vec::new(), |p| find(geo.width, geo.height, real[0], p[0], y0, y1));
            let mut cur = cut(geo, real, y0, y1).map(|p| p.to_vec());
            let mut c = Coder::Write(Encoder::new(), [0.0; 7]);
            band(geo, &mut c, m, &mut cur, prev, vectors, y0, y1);
            match c {
                Coder::Write(e, spent) => (e.finish(), spent),
                Coder::Read(_) => unreachable!(),
            }
        })
        .collect();
    for (_, spent) in &done {
        for (total, s) in SPENT.iter().zip(spent) {
            total.fetch_add((s * 1000.0) as u64, Ordering::Relaxed);
        }
    }
    for (bytes, _) in &done {
        out.extend((bytes.len() as u32).to_le_bytes());
    }
    for (bytes, _) in &done {
        out.extend(bytes);
    }
    out
}

/// The player: the frame, exactly.
pub fn unpack(geo: &Geometry, payload: &[u8], before: Option<&[u8]>, memory: &mut Memory) -> Result<Vec<u8>, String> {
    if payload.len() < HEAD || !crate::block::fits(geo) {
        return Err("not a mixed frame".into());
    }
    let before = if from_before(payload) { Some(before.filter(|b| b.len() == geo.frame_len()).ok_or("a mixed frame was rebuilt without the frame before it")?) } else { None };
    if payload[0] & STILL != 0 {
        return Ok(before.ok_or("a still frame was rebuilt without the frame before it")?.to_vec());
    }
    let rows = band_rows(geo, u16::from_le_bytes([payload[1], payload[2]]) as usize);
    let starts: Vec<usize> = (0..geo.height).step_by(rows).collect();
    let mut data = HEAD + 4 * starts.len();
    let mut coded = Vec::new();
    for i in 0..starts.len() {
        let len = payload.get(HEAD + 4 * i..HEAD + 4 * i + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize).ok_or("a mixed frame is cut")?;
        coded.push(payload.get(data..data + len).ok_or("a mixed frame is cut")?);
        data += len;
    }
    let prev = before.map(|b| planes(geo, b));
    let cw = geo.planes[1].w;
    memory.ready(starts.len(), before.is_none());
    let made: Vec<[Vec<u8>; 3]> = starts
        .par_iter()
        .zip(coded)
        .zip(memory.0.par_iter_mut())
        .map(|((y0, bytes), m)| {
            let y1 = (y0 + rows).min(geo.height);
            let (c0, c1) = colour_rows(geo, *y0, y1);
            let mut cur = [vec![0u8; (y1 - y0) * geo.width], vec![0u8; (c1 - c0) * cw], vec![0u8; (c1 - c0) * cw]];
            let mut c = Coder::Read(Decoder::new(bytes));
            band(geo, &mut c, m, &mut cur, prev, Vec::new(), *y0, y1);
            cur
        })
        .collect();
    let mut out = Vec::with_capacity(geo.frame_len());
    for p in 0..3 {
        for b in &made {
            out.extend(&b[p]);
        }
    }
    if out.len() != geo.frame_len() {
        return Err("a mixed frame's bands do not make a frame".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_comes_back_exactly() {
        for (w, h) in [(70, 50), (64, 48), (33, 17)] {
            let geo = Geometry::of("yuv420p", w, h).unwrap();
            let mut s = 7u32;
            let mut noise = |flat: bool| -> Vec<u8> {
                (0..geo.frame_len())
                    .map(|i| {
                        s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                        if flat && (i / 97) % 3 != 0 { (100 + (i % w) / 2 + (s >> 30) as usize) as u8 } else { (s >> 24) as u8 }
                    })
                    .collect()
            };
            let a = noise(true);
            let mut b = a.clone();
            for i in (0..b.len()).step_by(7) {
                b[i] = b[i].wrapping_add(3);
            }
            let wild = noise(false);
            for bands in [1, 3, 16] {
                // The player goes through the frames in the packer's order, with its own memory.
                let (mut packer, mut player) = (Memory::new(), Memory::new());
                let first = pack(&geo, &a, None, bands, &mut packer);
                assert_eq!(unpack(&geo, &first, None, &mut player).unwrap(), a, "{w}x{h} alone, {bands} bands");
                let second = pack(&geo, &b, Some(&a), bands, &mut packer);
                assert_eq!(unpack(&geo, &second, Some(&a), &mut player).unwrap(), b, "{w}x{h} from before, {bands} bands");
                let same = pack(&geo, &b, Some(&b), bands, &mut packer);
                assert_eq!(same.len(), HEAD);
                assert_eq!(unpack(&geo, &same, Some(&b), &mut player).unwrap(), b);
                let third = pack(&geo, &wild, Some(&b), bands, &mut packer);
                assert_eq!(unpack(&geo, &third, Some(&b), &mut player).unwrap(), wild, "{w}x{h} noise, {bands} bands");
            }
        }
    }
}
