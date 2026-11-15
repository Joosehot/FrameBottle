//! MIX: the exact packer that guesses each sample with several guessers mixed, and
//! writes what the guess missed with several learners mixed (`--mix`).
//!
//! Every sample comes back exactly. For 8-bit YUV 4:2:0.
//!
//! **The guess.** Four guessers offer a value for each sample:
//!
//! - from this frame alone: the median of left, above and their slope (LOCO-I's MED),
//!   and the slope-following guess of CALIC (GAP);
//! - two learning guessers (one quick, one slow): a weighed sum of what stands around
//!   the sample, whose weights move after every sample by their part of the miss. With
//!   a frame before they start from the sample there (at the place a vector found for
//!   the sample's block of 16 x 16 points to), and see how the neighbours changed since
//!   it and how it slopes around the sample, to the right and below too.
//!
//! Each speaks as loud as it was right on the samples just rebuilt (left, above, above
//! left, above right): the guess is their weighed mix, in whole numbers. Then the guess
//! is put right by what guesses in the same surroundings missed by on average.
//!
//! **The miss** is written yes/no by yes/no: is there one, its sign, how large (in
//! buckets), the digits inside the bucket. For each yes/no three learners say how likely
//! it is, each from its own view of the surroundings:
//!
//! - how large the misses nearby were, and how far apart the guessers stand (for the
//!   sign: the signs of the misses nearby);
//! - the misses of the four neighbours themselves, each within two;
//! - how far and to which side the frame before, the median and the learning guesser
//!   stand from the guess, and how many neighbours stayed what they were.
//!
//! Two mixers weigh the three likelihoods by how right each has been for this kind of
//! yes/no (logistic mixing, in whole numbers; one mixer's weights go by how large the
//! misses nearby were, the other's by how many neighbours stayed), and the range coder
//! writes with the mean of what they say.
//!
//! What was left out, and what each cost in bytes, is in the README (the mix made
//! light): the guessers and learners here are the ones that carried the packing.
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
pub const VERSION: &str = "mix 3";

/// Thousandths of a bit spent on: light, the first colour, the second colour, vectors;
/// and again by what was said: is there a miss, its sign, its size.
pub static SPENT: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

/// With FB_PROFILE set: the processor's ticks spent in each stage of a sample, and the
/// samples.
pub static TICKS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];

fn profiled() -> bool {
    static P: OnceLock<bool> = OnceLock::new();
    *P.get_or_init(|| std::env::var("FB_PROFILE").is_ok())
}

#[inline(always)]
fn now() -> u64 {
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// The guessers, and the things a learning guesser weighs (two are never used).
const NP: usize = 4;
const FEATS: usize = 16;
const SLOTS: usize = 16;
/// A miss's size by bucket: where each starts, and how many digits lie inside it.
const LO: [u32; 15] = [1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129];
const EXTRA: [usize; 14] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5];
const BLOCK: usize = 16;
const REACH: i32 = 7;
/// Two samples of room on each side of a row, and two rows above.
const PAD: usize = 2;

/// How fast the mixer learns (a shift), how often at most a learner counts, and how
/// fast the learning guesser learns (of 65536).
const MIXER: i32 = 13;
const SLOW: u16 = 1000;
const LEARN: i32 = 50000;

/// How large the misses nearby were, in 9 steps; how far apart, in 6.
const STEP: [u8; 24] = [0, 1, 2, 3, 4, 4, 5, 5, 5, 6, 6, 6, 6, 6, 7, 7, 7, 7, 7, 7, 7, 7, 7, 8];
const APART: [u8; 26] = [0, 1, 1, 2, 2, 2, 3, 3, 3, 3, 3, 3, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 5];

struct Tables {
    squash: Vec<u16>,
    stretch: Vec<i16>,
    weight: Vec<u32>,
    cost: Vec<f32>,
    /// 65536 / (n + 2): a learner asked n times moves this much of the way.
    part: Vec<i32>,
    /// 2^24 / 2n, rounded up: dividing by 2n is a multiplication.
    half: [u32; 33],
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
        let weight = (0..1536u32).map(|s| ((1u32 << 22) / ((s + 2) * (s + 2))).max(1)).collect();
        let cost = (0..=4096).map(|i| -((i.max(1) as f32) / 4096.0).log2()).collect();
        let part = (0..=SLOW as i32).map(|n| 65536 / (n + 2)).collect();
        let half = std::array::from_fn(|n| if n == 0 { 0 } else { (1u32 << 24).div_ceil(2 * n as u32) });
        Tables { squash, stretch, weight, cost, part, half }
    })
}

/// One way through the syntax for the packer and the player.
trait Bits {
    const WRITE: bool;
    /// One yes/no that is `p` likely (of 4096) to be yes. `what`: the plane (3: a
    /// vector) and what is said (4 is there a miss, 5 its sign, 6 its size).
    fn bit(&mut self, p: i32, v: bool, what: (usize, usize)) -> bool;
    /// One yes/no the coder learns on its own.
    fn plain(&mut self, p: &mut u16, v: bool, what: (usize, usize)) -> bool;
}

struct Writer(Encoder, [f64; 7]);
struct Reader<'a>(Decoder<'a>);

impl Bits for Writer {
    const WRITE: bool = true;
    #[inline(always)]
    fn bit(&mut self, p: i32, v: bool, what: (usize, usize)) -> bool {
        let bits = tables().cost[if v { p as usize } else { 4096 - p as usize }] as f64;
        self.1[what.0] += bits;
        if what.1 != what.0 {
            self.1[what.1] += bits;
        }
        self.0.fixed(4096 - p as u32, v);
        v
    }
    #[inline(always)]
    fn plain(&mut self, p: &mut u16, v: bool, what: (usize, usize)) -> bool {
        let bits = tables().cost[if v { 4096 - *p as usize } else { *p as usize }] as f64;
        self.1[what.0] += bits;
        if what.1 != what.0 {
            self.1[what.1] += bits;
        }
        self.0.bit(p, v);
        v
    }
}

impl Bits for Reader<'_> {
    const WRITE: bool = false;
    #[inline(always)]
    fn bit(&mut self, p: i32, _: bool, _: (usize, usize)) -> bool {
        self.0.fixed(4096 - p as u32)
    }
    #[inline(always)]
    fn plain(&mut self, p: &mut u16, _: bool, _: (usize, usize)) -> bool {
        self.0.bit(p)
    }
}

/// A whole number with a sign (a vector's part).
fn number<C: Bits>(c: &mut C, p: &mut ([u16; 32], [u16; 2]), v: i32) -> i32 {
    let what = (3, 3);
    if !c.plain(&mut p.1[0], v != 0, what) {
        return 0;
    }
    let negative = c.plain(&mut p.1[1], v < 0, what);
    let n = v.unsigned_abs().clamp(1, 65535);
    let want = 32 - n.leading_zeros() as usize;
    let mut digits = 1;
    while digits < 16 && c.plain(&mut p.0[digits - 1], digits < want, what) {
        digits += 1;
    }
    let mut value = 1u32;
    for j in (0..digits - 1).rev() {
        value = (value << 1) | c.plain(&mut p.0[16 + j], (n >> j) & 1 == 1, what) as u32;
    }
    if negative { -(value as i32) } else { value as i32 }
}

const A_SIZE: usize = 9 * 6 * 3;
const A_ALL: usize = A_SIZE + 81;
const D_ALL: usize = 625 * 3;
const E_ALL: usize = 625 * 3;
const SETS: usize = SLOTS * 3 * 3;

/// What a band has learned so far.
pub struct Model {
    /// The learning guessers' weights (65536 is one), for each plane.
    lw: [[i32; FEATS]; 3],
    lw_slow: [[i32; FEATS]; 3],
    /// The three learners: for each surrounding and each kind of yes/no, how likely
    /// "yes" is (of 65536), and how often it was asked.
    a: Vec<[u16; 2]>,
    d: Vec<[u16; 2]>,
    e: Vec<[u16; 2]>,
    /// The mixer's weights (65536 is one; the last weighs nothing but itself), by kind
    /// of yes/no, plane and how large the misses nearby were.
    w: Vec<[i32; 4]>,
    /// A second set of weights, by kind of yes/no, plane and how many neighbours stayed what they were.
    w2: Vec<[i32; 4]>,
    /// The digits inside a bucket.
    extra: [[u16; 8]; 14],
    /// What guesses in the same surroundings missed by: the sum and how many.
    bias: Vec<(i32, i32)>,
    vector: [([u16; 32], [u16; 2]); 2],
    /// For the profile: the ticks the range coder took.
    coder_ticks: u64,
    /// The band's working room, made once and used for every frame.
    room: Room,
    /// The light map: is a tile put right (by its left neighbour and the frame before), its
    /// offset and gain, and the frame before's map for each plane.
    lit_on: [[u16; 4]; 3],
    lit_num: [[([u16; 32], [u16; 2]); 2]; 3],
    lit: [Vec<(i32, i32)>; 3],
}

/// A band's working room: the rows as they are rebuilt, the frame before where the
/// vectors point, each plane's misses, and the guessers' misses on two rows.
#[derive(Default)]
struct Room {
    px: Vec<u8>,
    moved: Vec<u8>,
    res: [Vec<i8>; 3],
    ec: Vec<[u8; NP]>,
    eu: Vec<[u8; NP]>,
}

/// The frame before, for a band: the rows of each plane it may read, and which row
/// of the plane the first of them is.
#[derive(Clone, Copy)]
struct Before<'a> {
    planes: [&'a [u8]; 3],
    first: [usize; 3],
}

impl Model {
    fn new() -> Model {
        Model {
            lw: [[0; FEATS]; 3],
            lw_slow: [[0; FEATS]; 3],
            a: vec![[32768, 0]; A_ALL * SLOTS],
            d: vec![[32768, 0]; D_ALL * SLOTS],
            e: vec![[32768, 0]; E_ALL * SLOTS],
            w: vec![[26000, 26000, 26000, 0]; SETS],
            w2: vec![[26000, 26000, 26000, 0]; SLOTS * 3 * 5],
            extra: [[2048; 8]; 14],
            bias: vec![(0, 0); A_SIZE],
            vector: [([2048; 32], [2048; 2]); 2],
            coder_ticks: 0,
            room: Room::default(),
            lit_on: [[2048; 4]; 3],
            lit_num: [[([2048; 32], [2048; 2]); 2]; 3],
            lit: Default::default(),
        }
    }

    /// One yes/no of a miss: the three learners' likelihoods mixed, the yes/no written
    /// or read, and all of them taught.
    #[inline(always)]
    fn mixed<C: Bits>(&mut self, c: &mut C, t: &Tables, ix: [usize; 3], (set, set2): (usize, usize), v: bool, what: (usize, usize)) -> bool {
        let (pa, pd, pe) = (self.a[ix[0]], self.d[ix[1]], self.e[ix[2]]);
        let st = [t.stretch[(pa[0] >> 4) as usize] as i32, t.stretch[(pd[0] >> 4) as usize] as i32, t.stretch[(pe[0] >> 4) as usize] as i32, 256];
        let (w, w2) = (&mut self.w[set], &mut self.w2[set2]);
        let dot: i64 = (0..4).map(|k| w[k] as i64 * st[k] as i64).sum();
        let dot2: i64 = (0..4).map(|k| w2[k] as i64 * st[k] as i64).sum();
        let (d1, d2) = ((dot >> 16).clamp(-2047, 2047) as i32, (dot2 >> 16).clamp(-2047, 2047) as i32);
        let (p1, p2) = (t.squash[(d1 + 2048) as usize] as i32, t.squash[(d2 + 2048) as usize] as i32);
        // The two sets of weights speak together: the mean of what they say.
        let p = (t.squash[(((d1 + d2) >> 1) + 2048) as usize] as i32).clamp(1, 4095);
        let bit = if profiled() {
            let from = now();
            let bit = c.bit(p, v, what);
            self.coder_ticks += now() - from;
            bit
        } else {
            c.bit(p, v, what)
        };
        let (err, err2) = (((bit as i32) << 12) - p1, ((bit as i32) << 12) - p2);
        for k in 0..4 {
            w[k] += (st[k] * err) >> MIXER;
            w2[k] += (st[k] * err2) >> MIXER;
        }
        // A learner moves 1 / (how often it was asked + 2) of the way, so that a
        // surrounding met for the first time is believed at once.
        let target = if bit { 65535i32 } else { 0 };
        let learn = |p: [u16; 2]| -> [u16; 2] { [(p[0] as i32 + (((target - p[0] as i32) * t.part[p[1] as usize]) >> 16)) as u16, (p[1] + 1).min(SLOW)] };
        self.a[ix[0]] = learn(pa);
        self.d[ix[1]] = learn(pd);
        self.e[ix[2]] = learn(pe);
        bit
    }

    /// A miss: written, or read. `at` is where the sample stands for the three
    /// learners (and for the sign, the first learner's other view).
    #[inline(always)]
    fn miss<C: Bits>(&mut self, c: &mut C, t: &Tables, pk: usize, at: [usize; 3], signs: usize, (set, set2): (usize, usize), v: i32) -> i32 {
        let ix = |slot: usize| [at[0] * SLOTS + slot, at[1] * SLOTS + slot, at[2] * SLOTS + slot];
        let sets = |slot: usize| ((slot * 3 + pk) * 3 + set, (slot * 3 + pk) * 5 + set2);
        if !self.mixed(c, t, ix(0), sets(0), v != 0, (pk, 4)) {
            return 0;
        }
        let mut sx = ix(1);
        sx[0] = signs * SLOTS + 1;
        let negative = self.mixed(c, t, sx, sets(1), v < 0, (pk, 5));
        let size = v.unsigned_abs().clamp(1, 128);
        let want = if C::WRITE { LO.iter().rposition(|lo| *lo <= size).unwrap_or(0).min(13) } else { 0 };
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
    c.clamp(lo, hi)
}

/// One plane of a band, sample by sample: guessed, its miss written or read, learned.
/// `cur` holds the band's rows of the plane (the real ones for the packer; the player
/// fills them). `before` (with T) is the frame before where the vectors point, with
/// room around it; `res` takes each sample's miss, with the same room; `first` is the
/// misses of the first colour (for the second).
#[allow(clippy::too_many_arguments)]
fn plane<C: Bits, const T: bool>(c: &mut C, m: &mut Model, pk: usize, w: usize, cur: &mut [u8], before: &[u8], (px, ec, eu): (&mut Vec<u8>, &mut Vec<[u8; NP]>, &mut Vec<[u8; NP]>), res: &mut [i8], first: Option<&[i8]>, two: bool) {
    let t = tables();
    let rows = cur.len() / w;
    let sw = w + 2 * PAD;
    // The band's rows as they are rebuilt, two rows above them (the frame before, or grey).
    px.resize((rows + PAD) * sw, 0);
    if T {
        px[..PAD * sw].copy_from_slice(&before[..PAD * sw]);
    } else {
        px[..PAD * sw].fill(128);
    }
    let (mut lw, mut lw_slow) = (m.lw[pk], m.lw_slow[pk]);
    let prof = profiled();
    let (mut ticks, mut tick) = ([0u64; 8], if prof { now() } else { 0 });
    let coder_before = m.coder_ticks;
    macro_rules! lap {
        ($k:expr) => {
            if prof {
                let t = now();
                ticks[$k] += t - tick;
                tick = t;
            }
        };
    }
    // The guessers' misses on this row and the row above, a nothing at each end.
    ec.clear();
    ec.resize(w + 2, [0u8; NP]);
    eu.clear();
    eu.resize(w + 2, [0u8; NP]);
    for yy in 0..rows {
        let o = (yy + PAD) * sw + PAD;
        // Left of the row stands what is above its first sample; right of it, its last.
        (px[o - 1], px[o - 2]) = (px[o - sw], px[o - sw]);
        for x in 0..w {
            let i = o + x;
            let p = |k: usize| px[k] as i32;
            let (west, north, nw, ne, ww, nn, nne) = (p(i - 1), p(i - sw), p(i - sw - 1), p(i - sw + 1), p(i - 2), p(i - 2 * sw), p(i - 2 * sw + 1));
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
            // The learning guesser: a base, and what stands around it told from the base.
            let (base, stayed, feats): (i32, usize, [i32; FEATS]) = if T {
                // From the frame before: how the neighbours changed, and how the frame before
                // itself slopes around the sample (to the right and below too: it is all there).
                let q = |k: usize| before[k] as i32;
                let t0 = q(i);
                let (cw, cn, cnw, cne) = (west - q(i - 1), north - q(i - sw), nw - q(i - sw - 1), ne - q(i - sw + 1));
                let stayed = (cw == 0) as usize + (cn == 0) as usize + (cnw == 0) as usize + (cne == 0) as usize;
                (t0, stayed, [cw, cn, cnw, cne, q(i + 1) - t0, q(i + sw) - t0, q(i - 1) - t0, q(i - sw) - t0, med - t0, gap - t0, q(i + sw + 1) - t0, q(i + sw - 1) - t0, ww - q(i - 2), nn - q(i - 2 * sw), 0, 0])
            } else {
                (west, 0, [north - west, nw - west, ne - west, ww - west, nn - west, nne - west, med - west, gap - west, 0, 0, 0, 0, 0, 0, 0, 0])
            };
            lap!(0);
            let (mut lean, mut lean_slow, mut norm) = (0i32, 0i32, 32i32);
            for k in 0..FEATS {
                lean = lean.wrapping_add(lw[k].wrapping_mul(feats[k]));
                lean_slow = lean_slow.wrapping_add(lw_slow[k].wrapping_mul(feats[k]));
                norm += feats[k] * feats[k];
            }
            let learned = (base + (lean.wrapping_add(32768) >> 16)).clamp(0, 255);
            let learned_slow = (base + (lean_slow.wrapping_add(32768) >> 16)).clamp(0, 255);
            let offers = [med, gap, learned, learned_slow];
            lap!(1);
            // The mix: each speaks as loud as it was right on the neighbours.
            let (e_w, e_n, e_nw, e_ne) = (ec[x], eu[x + 1], eu[x], eu[x + 2]);
            let (mut sum, mut total) = (0u32, 0u32);
            for g in if two { 2 } else { 0 }..NP {
                let wt = t.weight[2 * e_w[g] as usize + 2 * e_n[g] as usize + e_nw[g] as usize + e_ne[g] as usize];
                sum += wt;
                total += wt * offers[g] as u32;
            }
            let mixed = ((total + sum / 2) / sum) as i32;
            let low = med.min(gap).min(learned).min(learned_slow);
            let high = med.max(gap).max(learned).max(learned_slow);
            lap!(2);
            // Where the sample stands.
            let r = |k: usize| res[k] as i32;
            let (rw, rn, rnw, rne) = (r(i - 1), r(i - sw), r(i - sw - 1), r(i - sw + 1));
            let near = STEP[(((2 * (rw.unsigned_abs() + rn.unsigned_abs()) + rnw.unsigned_abs() + rne.unsigned_abs() + 1) / 3) as usize).min(23)] as usize;
            let at_a = (near * 6 + APART[((high - low) as usize).min(25)] as usize) * 3 + pk;
            let sign_first = first.map_or(1, |f| (f[i] as i32).signum() + 1);
            let signs = A_SIZE + (((rw.signum() + 1) * 3 + rn.signum() + 1) * 3 + sign_first) as usize * 3 + pk;
            // Put right by what guesses that stood here missed by.
            let b = m.bias[at_a];
            let lean = ((2 * b.0 + b.1 * b.0.signum()).unsigned_abs() * t.half[b.1 as usize]) >> 24;
            let guess = (mixed + if b.0 < 0 { -(lean as i32) } else { lean as i32 }).clamp(0, 255);
            let five = |r: i32| (r.clamp(-2, 2) + 2) as usize;
            let at_d = (((five(rw) * 5 + five(rn)) * 5 + five(rnw)) * 5 + five(rne)) * 3 + pk;
            // How far, and to which side, the frame before, the median and the learning guesser stand from the guess.
            let at_e = (((five(base - guess) * 5 + five(med - guess)) * 5 + five(learned - guess)) * 5 + stayed) * 3 + pk;
            let real = if C::WRITE { cur[yy * w + x] } else { 0 };
            let want = if C::WRITE { real.wrapping_sub(guess as u8) as i8 as i32 } else { 0 };
            lap!(3);
            let got = m.miss(c, t, pk, [at_a, at_d, at_e], signs, (if near < 2 { 0 } else if near < 5 { 1 } else { 2 }, stayed), want);
            lap!(4);
            let real = if C::WRITE { real } else { (guess as u8).wrapping_add(got as u8) };
            px[i] = real;
            res[i] = got as i8;
            let b = &mut m.bias[at_a];
            b.0 += got as i8 as i32;
            b.1 += 1;
            if b.1 == 32 {
                *b = (b.0 / 2, 16);
            }
            ec[x + 1] = offers.map(|o| (real as i32 - o).unsigned_abs() as u8);
            // The learning guessers are taught: each weight moves by its part of the miss.
            let over = ((1u32 << 24) / norm as u32) as i64;
            let pull = ((((real as i32 - learned) * LEARN) as i64 * over) >> 21) as i32;
            let pull_slow = ((((real as i32 - learned_slow) * (LEARN / 16)) as i64 * over) >> 17) as i32;
            for k in 0..FEATS {
                lw[k] = lw[k].wrapping_add(pull.wrapping_mul(feats[k]) >> 3);
                lw_slow[k] = lw_slow[k].wrapping_add(pull_slow.wrapping_mul(feats[k]) >> 7);
            }
            lap!(6);
        }
        (px[o + w], px[o + w + 1]) = (px[o + w - 1], px[o + w - 1]);
        if !C::WRITE {
            cur[yy * w..(yy + 1) * w].copy_from_slice(&px[o..o + w]);
        }
        std::mem::swap(ec, eu);
    }
    (m.lw[pk], m.lw_slow[pk]) = (lw, lw_slow);
    if prof {
        ticks[5] = m.coder_ticks - coder_before;
        ticks[4] -= ticks[5];
        ticks[7] = (rows * w) as u64;
        for (total, t) in TICKS.iter().zip(ticks) {
            total.fetch_add(t, Ordering::Relaxed);
        }
    }
}

/// The profile, in words (after a run with FB_PROFILE set).
pub fn profile() -> Option<String> {
    let t: Vec<f64> = TICKS.iter().map(|a| a.load(Ordering::Relaxed) as f64).collect();
    let (all, samples) = (t[..7].iter().sum::<f64>(), t[7]);
    if !profiled() || samples == 0.0 {
        return None;
    }
    let names = ["the frame before and the plain guessers", "the learning guessers", "the weighing of the guessers", "where the sample stands (contexts)", "the learners and mixers", "the range coder", "teaching the guessers"];
    let mut out = format!("PROFILE of the mix: {:.0} ticks a sample (with the clock's own)\n", all / samples);
    for (name, t) in names.iter().zip(&t) {
        out += &format!("  {name:42} {:5.1} %  {:4.0} ticks a sample\n", 100.0 * t / all, t / samples);
    }
    Some(out)
}

/// The vector of each block of 16 x 16 of light in rows `y0..y1`: where in the frame
/// before the block is most like itself. A block that is nearly what it was stays.
fn find(w: usize, h: usize, real: &[u8], prev: &[u8], y0: usize, y1: usize, quarter: bool) -> Vec<(i32, i32)> {
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
                // Vectors are counted in quarter pixels from here on.
                best = (best.0, 4 * best.1, 4 * best.2);
                if quarter {
                    // Between the pixels: the eight places half a pixel around the best, then the eight a quarter around that.
                    let g = |x: i32, y: i32| prev[y.clamp(0, h as i32 - 1) as usize * w + x.clamp(0, w as i32 - 1) as usize] as i32;
                    let unlike = |qx: i32, qy: i32, limit: u32| -> u32 {
                        let mut s = 0u32;
                        for j in 0..bh {
                            for k in 0..bw {
                                s += (real[(by + j) * w + bx + k] as i32 - crate::block::quarter_of(&g, 4 * (bx + k) as i32 + qx, 4 * (by + j) as i32 + qy)).unsigned_abs();
                            }
                            if s >= limit {
                                return s;
                            }
                        }
                        s
                    };
                    for step in [2, 1] {
                        let (cx, cy) = (best.1, best.2);
                        for (ax, ay) in [(-1, -1), (0, -1), (1, -1), (-1, 0), (1, 0), (-1, 1), (0, 1), (1, 1)] {
                            let (qx, qy) = (cx + ax * step, cy + ay * step);
                            let s = unlike(qx, qy, best.0) + (qx.abs() + qy.abs()) as u32 + 2;
                            if s < best.0 {
                                best = (s, qx, qy);
                            }
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

/// The frame before as the band's blocks see it: each block's samples from where its
/// vector points, with two rows above the band, one below and two samples at each side
/// (from the nearest block's vector). `prev` starts at the plane's row `first`;
/// `shift`: a block's side as a power of two. Vectors are in quarter pixels of light
/// (eighths of a colour pixel).
#[allow(clippy::too_many_arguments)]
fn moved(out: &mut Vec<u8>, prev: &[u8], first: usize, (w, h): (usize, usize), y0: usize, rows: usize, vectors: &[(i32, i32)], across: usize, shift: usize, halve: bool) {
    let sw = w + 2 * PAD;
    out.resize((rows + PAD + 1) * sw, 0);
    let (block_rows, block_cols) = ((vectors.len() / across).max(1), w.div_ceil(1 << shift).min(across));
    for r in -(PAD as i32)..=rows as i32 {
        let br = ((r.clamp(0, rows as i32 - 1) as usize) >> shift).min(block_rows - 1);
        let o = (r + PAD as i32) as usize * sw + PAD;
        let mut bc = 0;
        while bc < block_cols {
            // Blocks side by side with one vector are taken at once.
            let v = vectors[br * across + bc];
            let mut end = bc + 1;
            while end < block_cols && vectors[br * across + end] == v {
                end += 1;
            }
            let (x0, x1) = (if bc == 0 { -(PAD as i32) } else { (bc << shift) as i32 }, if end == block_cols { (w + PAD) as i32 } else { (end << shift) as i32 });
            let (unit, part) = if halve { (3, 7) } else { (2, 3) };
            let (vx, vy) = (v.0 >> unit, v.1 >> unit);
            let to = &mut out[(o as i32 + x0) as usize..(o as i32 + x1) as usize];
            if v.0 & part != 0 || v.1 & part != 0 {
                // Between the pixels.
                let g = |x: i32, y: i32| prev[(y.clamp(0, h as i32 - 1) as usize - first) * w + x.clamp(0, w as i32 - 1) as usize] as i32;
                let y = y0 as i32 + r;
                for (k, s) in to.iter_mut().enumerate() {
                    let x = x0 + k as i32;
                    *s = if halve {
                        let (x8, y8) = (8 * x + v.0, 8 * y + v.1);
                        let (px, py, fx, fy) = (x8 >> 3, y8 >> 3, x8 & 7, y8 & 7);
                        (((8 - fx) * (8 - fy) * g(px, py) + fx * (8 - fy) * g(px + 1, py) + (8 - fx) * fy * g(px, py + 1) + fx * fy * g(px + 1, py + 1) + 32) >> 6) as u8
                    } else {
                        crate::block::quarter_of(&g, 4 * x + v.0, 4 * y + v.1) as u8
                    };
                }
                bc = end;
                continue;
            }
            let from = ((y0 as i32 + r + vy).clamp(0, h as i32 - 1) as usize - first) * w;
            // What lies inside the frame, and the frame's edge for what does not.
            let (a, b) = ((-vx).clamp(x0, x1), (w as i32 - vx).clamp(x0, x1));
            if a < b {
                to[(a - x0) as usize..(b - x0) as usize].copy_from_slice(&prev[from + (a + vx) as usize..from + (b + vx) as usize]);
                to[..(a - x0) as usize].fill(prev[from]);
                to[(b - x0) as usize..].fill(prev[from + w - 1]);
            } else {
                to.fill(prev[from + if x0 + vx < 0 { 0 } else { w - 1 }]);
            }
            bc = end;
        }
    }
}

/// How a frame is packed beyond the mix itself (kept in the frame's flags).
#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct How {
    /// The light map: 0 none, 1 an offset for each tile, 2 an offset and a gain.
    pub light: u8,
    /// Only the two learning guessers are weighed for the guess.
    pub two: bool,
    /// Vectors to a quarter of a pixel (the frame before between its pixels by H.264's
    /// six-pixel filter, as the blocks do it; colour by the four pixels around).
    pub quarter: bool,
}

impl How {
    fn flags(self) -> u8 {
        (if self.light > 0 { LIGHT } else { 0 }) | (if self.two { TWO } else { 0 }) | (if self.quarter { QUARTER } else { 0 })
    }
    fn of(flags: u8) -> How {
        How { light: if flags & LIGHT != 0 { 2 } else { 0 }, two: flags & TWO != 0, quarter: flags & QUARTER != 0 }
    }
}

/// Tiles whose light was put right, and tiles, as the packer counted them.
pub static LIT: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// A tile of the light map: 32 x 32 of light, 16 x 16 of colour.
const TILE: usize = 32;
/// A tile left as it is: no offset, a gain of one (64).
const PLAIN: (i32, i32) = (0, 64);

/// The packer's choice for one tile: the offset (and gain, of 64) that bring the frame
/// before nearest to the real samples, or PLAIN when that is not worth its bits.
fn light_of(cur: &[u8], w: usize, mv: &[u8], (x0, x1, y0, y1): (usize, usize, usize, usize), gain: bool) -> (i32, i32) {
    let sw = w + 2 * PAD;
    let n = ((x1 - x0) * (y1 - y0)) as i64;
    let (mut sc, mut sm, mut smm, mut scm) = (0i64, 0i64, 0i64, 0i64);
    for y in y0..y1 {
        for x in x0..x1 {
            let (a, b) = (cur[y * w + x] as i64, mv[(y + PAD) * sw + x + PAD] as i64);
            sc += a;
            sm += b;
            smm += b * b;
            scm += a * b;
        }
    }
    let far = |o: i32, g: i32| -> i64 {
        let mut s = 0i64;
        for y in y0..y1 {
            for x in x0..x1 {
                s += (cur[y * w + x] as i32 - (((g * mv[(y + PAD) * sw + x + PAD] as i32) >> 6) + o).clamp(0, 255)).abs() as i64;
            }
        }
        s
    };
    // A tile must come nearer by an eighth of a step a sample (and the map's own bits) to be put right.
    let mut best = (PLAIN, far(0, 64) - n / 8 - 16);
    let o = (2 * (sc - sm) + n).div_euclid(2 * n) as i32;
    if o != 0 {
        let s = far(o, 64);
        if s < best.1 {
            best = ((o, 64), s);
        }
    }
    let spread = n * smm - sm * sm;
    if gain && spread > 0 {
        let g = (64 * (n * scm - sc * sm) + spread / 2).div_euclid(spread).clamp(48, 80) as i32;
        if g != 64 {
            let o = ((2 * (64 * sc - g as i64 * sm) + 64 * n).div_euclid(128 * n) as i32).clamp(-255, 255);
            let s = far(o, g) + n / 16;
            if s < best.1 {
                best = ((o, g), s);
            }
        }
    }
    best.0
}

/// THE LIGHT MAP: for each tile of a plane, how the light changed since the frame
/// before: an offset, and a gain (of 64). It is written (told from the same tile's of
/// the frame before) or read, and then laid on the frame before as the band sees it:
/// before' = clamp((gain * before) >> 6) + offset). Only what the guessers start from
/// changes; the misses still lead to the real samples, so nothing is lost.
#[allow(clippy::too_many_arguments)]
fn relight<C: Bits>(c: &mut C, m: &mut Model, pk: usize, w: usize, rows: usize, cur: &[u8], mv: &mut [u8], gain: bool) {
    let sw = w + 2 * PAD;
    let side = if pk == 0 { TILE } else { TILE / 2 };
    let (across, down) = (w.div_ceil(side), rows.div_ceil(side));
    let mut was = std::mem::take(&mut m.lit[pk]);
    if was.len() != across * down {
        was = vec![PLAIN; across * down];
    }
    let mut map = vec![PLAIN; across * down];
    let what = (3, 3);
    let mut lit = 0;
    for ty in 0..down {
        for tx in 0..across {
            let k = ty * across + tx;
            let want = if C::WRITE { light_of(cur, w, mv, (tx * side, ((tx + 1) * side).min(w), ty * side, ((ty + 1) * side).min(rows)), gain) } else { PLAIN };
            let at = (tx > 0 && map[k - 1] != PLAIN) as usize + 2 * (was[k] != PLAIN) as usize;
            if c.plain(&mut m.lit_on[pk][at], want != PLAIN, what) {
                map[k] = (was[k].0 + number(c, &mut m.lit_num[pk][0], want.0 - was[k].0), was[k].1 + number(c, &mut m.lit_num[pk][1], want.1 - was[k].1));
                lit += 1;
            }
        }
    }
    if C::WRITE {
        LIT[0].fetch_add(lit, Ordering::Relaxed);
        LIT[1].fetch_add((across * down) as u64, Ordering::Relaxed);
    }
    // Laid on the frame before, its room around it too.
    for r in 0..rows + PAD + 1 {
        let ty = (r.saturating_sub(PAD)).min(rows - 1) / side;
        for tx in 0..across {
            let (o, g) = map[ty * across + tx];
            if (o, g) == PLAIN {
                continue;
            }
            let (a, b) = (if tx == 0 { 0 } else { tx * side + PAD }, if tx + 1 == across { sw } else { (tx + 1) * side + PAD });
            for v in &mut mv[r * sw + a..r * sw + b] {
                *v = (((g * *v as i32) >> 6) + o).clamp(0, 255) as u8;
            }
        }
    }
    m.lit[pk] = map;
}

/// The chroma rows that go with the light's rows `y0..y1`.
fn colour_rows(geo: &Geometry, y0: usize, y1: usize) -> (usize, usize) {
    (y0 / 2, if y1 >= geo.height { geo.planes[1].h } else { y1 / 2 })
}

/// One band: its vectors, its light, its two colours; written, or read into `cur`.
#[allow(clippy::too_many_arguments)]
fn band<C: Bits>(geo: &Geometry, c: &mut C, m: &mut Model, cur: [&mut [u8]; 3], prev: Option<Before>, mut vectors: Vec<(i32, i32)>, y0: usize, y1: usize, how: How) {
    let (w, h) = (geo.width, geo.height);
    let (cw, ch) = (geo.planes[1].w, geo.planes[1].h);
    let (c0, _) = colour_rows(geo, y0, y1);
    let across = w.div_ceil(BLOCK);
    if prev.is_some() {
        // The vectors, each told from the one to its left.
        let blocks = across * (y1 - y0).div_ceil(BLOCK);
        vectors.resize(blocks, (0, 0));
        // (in whole pixels when the frame has no vectors between them)
        let unit = if how.quarter { 1 } else { 4 };
        for k in 0..blocks {
            let from = if k % across > 0 { vectors[k - 1] } else { (0, 0) };
            vectors[k] = (from.0 + unit * number(c, &mut m.vector[0], (vectors[k].0 - from.0) / unit), from.1 + unit * number(c, &mut m.vector[1], (vectors[k].1 - from.1) / unit));
        }
    }
    let [light, first, second] = cur;
    let (rows, colour_rows) = (light.len() / w, first.len() / cw);
    let mut room = std::mem::take(&mut m.room);
    let Room { px, moved: mv, res, ec, eu } = &mut room;
    for (r, len) in res.iter_mut().zip([(rows + PAD) * (w + 2 * PAD), (colour_rows + PAD) * (cw + 2 * PAD), (colour_rows + PAD) * (cw + 2 * PAD)]) {
        if r.len() != len {
            r.clear();
            r.resize(len, 0);
        }
    }
    let [res_light, res_first, res_second] = res;
    match prev {
        Some(p) => {
            moved(mv, p.planes[0], p.first[0], (w, h), y0, rows, &vectors, across, 4, false);
            if how.light > 0 {
                relight(c, m, 0, w, rows, light, mv, how.light > 1);
            }
            plane::<C, true>(c, m, 0, w, light, mv, (&mut *px, &mut *ec, &mut *eu), res_light, None, how.two);
            moved(mv, p.planes[1], p.first[1], (cw, ch), c0, colour_rows, &vectors, across, 3, true);
            if how.light > 0 {
                relight(c, m, 1, cw, colour_rows, first, mv, how.light > 1);
            }
            plane::<C, true>(c, m, 1, cw, first, mv, (&mut *px, &mut *ec, &mut *eu), res_first, None, how.two);
            moved(mv, p.planes[2], p.first[2], (cw, ch), c0, colour_rows, &vectors, across, 3, true);
            if how.light > 0 {
                relight(c, m, 2, cw, colour_rows, second, mv, how.light > 1);
            }
            plane::<C, true>(c, m, 2, cw, second, mv, (&mut *px, &mut *ec, &mut *eu), res_second, Some(res_first), how.two);
        }
        None => {
            plane::<C, false>(c, m, 0, w, light, &[], (&mut *px, &mut *ec, &mut *eu), res_light, None, how.two);
            plane::<C, false>(c, m, 1, cw, first, &[], (&mut *px, &mut *ec, &mut *eu), res_first, None, how.two);
            plane::<C, false>(c, m, 2, cw, second, &[], (&mut *px, &mut *ec, &mut *eu), res_second, Some(res_first), how.two);
        }
    }
    m.room = room;
}

const FROM_BEFORE: u8 = 2;
const STILL: u8 = 4;
const LIGHT: u8 = 8;
const TWO: u8 = 16;
const QUARTER: u8 = 32;
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
pub fn pack(geo: &Geometry, frame: &[u8], before: Option<&[u8]>, bands: usize, memory: &mut Memory, how: How) -> Vec<u8> {
    let mut out = vec![if before.is_some() { FROM_BEFORE } else { 0 } | how.flags()];
    out.extend((bands as u16).to_le_bytes());
    if before == Some(frame) {
        out[0] |= STILL;
        return out;
    }
    let (real, prev) = (planes(geo, frame), before.map(|b| Before { planes: planes(geo, b), first: [0; 3] }));
    let rows = band_rows(geo, bands);
    let starts: Vec<usize> = (0..geo.height).step_by(rows).collect();
    memory.ready(starts.len(), before.is_none());
    let done: Vec<(Vec<u8>, [f64; 7])> = starts
        .par_iter()
        .zip(memory.0.par_iter_mut())
        .map(|(y0, m)| {
            let y0 = *y0;
            let y1 = (y0 + rows).min(geo.height);
            let vectors = prev.map_or(Vec::new(), |p| find(geo.width, geo.height, real[0], p.planes[0], y0, y1, how.quarter));
            let [mut a, mut b, mut d] = cut(geo, real, y0, y1).map(|p| p.to_vec());
            let mut c = Writer(Encoder::new(), [0.0; 7]);
            band(geo, &mut c, m, [&mut a, &mut b, &mut d], prev, vectors, y0, y1, how);
            (c.0.finish(), c.1)
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
    let prev = before.map(|b| Before { planes: planes(geo, b), first: [0; 3] });
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
            let [a, b, d] = &mut cur;
            let mut c = Reader(Decoder::new(bytes));
            band(geo, &mut c, m, [a, b, d], prev, Vec::new(), *y0, y1, How::of(payload[0]));
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

/// What a frame's jobs wait for.
#[derive(Clone, Copy, PartialEq)]
enum Waits {
    /// The same band of the frame before.
    Band,
    /// That, and the bands above and below it.
    Around,
}

/// How a strike went: its time, the time the workers spent in jobs (summed), the
/// workers, the jobs.
#[derive(Default, Clone, Copy, Debug)]
pub struct Struck {
    pub wall: f64,
    pub busy: f64,
    pub workers: usize,
    pub jobs: usize,
}

impl Struck {
    /// The share of the strike's time the workers were in a job.
    pub fn busy_share(&self) -> f64 {
        self.busy / (self.wall * self.workers as f64).max(1e-9)
    }
    pub fn add(&mut self, o: &Struck) {
        *self = Struck { wall: self.wall + o.wall, busy: self.busy + o.busy, workers: o.workers.max(self.workers), jobs: self.jobs + o.jobs };
    }
}

/// THE STRIKE. The jobs of a film are mapped first: one job is one band of one frame,
/// and it waits for the same band of the frame before (what the band has learned) and,
/// when it reads that frame, for the bands above and below it there (their samples).
/// Then every job whose waiting is over is struck at once, on every core, and each one
/// that ends lets the next go. Frames are let in a rolling window ahead of the last
/// frame handed over, so the memory in use stays what the window holds.
///
/// `n` frames of `bands` bands. `admit(f)` lets frame f into the window (in order, on
/// this thread) and says what its jobs wait for; `run(f, k)` is the job of band k (any
/// worker), and `run(f, bands)` the frame's last job, once its bands are done;
/// `deliver(f)` hands the finished frame over (in order, on this thread; false stops).
fn strike(n: usize, bands: usize, window: usize, admit: &mut dyn FnMut(usize) -> Result<Waits, String>, run: &(dyn Fn(usize, usize) -> Result<(), String> + Sync), deliver: &mut dyn FnMut(usize) -> Result<bool, String>) -> Result<Struck, String> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;
    use std::sync::{Condvar, Mutex, mpsc};
    let began = std::time::Instant::now();
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get());
    let queue: (Mutex<(BinaryHeap<Reverse<(usize, usize)>>, bool)>, Condvar) = (Mutex::new((BinaryHeap::new(), false)), Condvar::new());
    let (tx, rx) = mpsc::channel::<(usize, usize, Result<(), String>, f64)>();
    let mut struck = Struck { workers, ..Struck::default() };
    let result = std::thread::scope(|scope| {
        for _ in 0..workers {
            let (tx, queue) = (tx.clone(), &queue);
            scope.spawn(move || {
                loop {
                    let job = {
                        let mut q = queue.0.lock().unwrap();
                        loop {
                            if q.1 {
                                return;
                            }
                            if let Some(Reverse(job)) = q.0.pop() {
                                break job;
                            }
                            q = queue.1.wait(q).unwrap();
                        }
                    };
                    let from = std::time::Instant::now();
                    let r = run(job.0, job.1);
                    if tx.send((job.0, job.1, r, from.elapsed().as_secs_f64())).is_err() {
                        return;
                    }
                }
            });
        }
        let push = |f: usize, k: usize| {
            queue.0.lock().unwrap().0.push(Reverse((f, k)));
            queue.1.notify_one();
        };
        // What each job still waits for, which are done, and what each frame waits for.
        let mut waiting = vec![0u8; n * bands];
        let mut done = vec![false; n * bands];
        let mut left = vec![bands + 1; n];
        let mut waits = Vec::with_capacity(n);
        let (mut admitted, mut delivered) = (0usize, 0usize);
        let mut go = || -> Result<(), String> {
            loop {
                while admitted < n && admitted < delivered + window.max(1) {
                    let f = admitted;
                    let w = admit(f)?;
                    waits.push(w);
                    for k in 0..bands {
                        let lo = if w == Waits::Around { k.saturating_sub(1) } else { k };
                        let hi = if w == Waits::Around { (k + 1).min(bands - 1) } else { k };
                        let not_yet = if f == 0 { 0 } else { (lo..=hi).filter(|j| !done[(f - 1) * bands + j]).count() };
                        waiting[f * bands + k] = not_yet as u8;
                        if not_yet == 0 {
                            push(f, k);
                        }
                    }
                    admitted += 1;
                }
                if delivered == n {
                    return Ok(());
                }
                let (f, k, r, took) = rx.recv().map_err(|_| "the strike's workers stopped".to_string())?;
                struck.busy += took;
                struck.jobs += 1;
                r?;
                left[f] -= 1;
                if k < bands {
                    done[f * bands + k] = true;
                    if left[f] == 1 {
                        push(f, bands);
                    }
                    if f + 1 < admitted {
                        let around = waits[f + 1] == Waits::Around;
                        for j in if around { k.saturating_sub(1) } else { k }..=if around { (k + 1).min(bands - 1) } else { k } {
                            waiting[(f + 1) * bands + j] -= 1;
                            if waiting[(f + 1) * bands + j] == 0 {
                                push(f + 1, j);
                            }
                        }
                    }
                }
                while delivered < admitted && left[delivered] == 0 {
                    if !deliver(delivered)? {
                        return Ok(());
                    }
                    delivered += 1;
                }
            }
        };
        let result = go();
        queue.0.lock().unwrap().1 = true;
        queue.1.notify_all();
        result
    });
    struck.wall = began.elapsed().as_secs_f64();
    result.map(|_| struck)
}

/// Places other threads write and read by agreement: each is written by one job, and
/// read only by jobs that wait for it.
struct Places<T>(Vec<std::cell::UnsafeCell<T>>);
unsafe impl<T: Send> Sync for Places<T> {}
impl<T> Places<T> {
    fn new(n: usize, make: impl Fn() -> T) -> Places<T> {
        Places((0..n).map(|_| std::cell::UnsafeCell::new(make())).collect())
    }
    /// # Safety
    /// Nobody else may be writing the place.
    unsafe fn get(&self, i: usize) -> &T {
        unsafe { &*self.0[i].get() }
    }
    /// # Safety
    /// Nobody else may be reading or writing the place.
    #[allow(clippy::mut_from_ref)]
    unsafe fn get_mut(&self, i: usize) -> &mut T {
        unsafe { &mut *self.0[i].get() }
    }
}

/// A frame the bands of a strike write side by side, each its own rows.
struct Sheet(*mut u8, usize);
unsafe impl Send for Sheet {}
impl Sheet {
    fn of(mut v: Vec<u8>) -> Sheet {
        v.shrink_to_fit();
        let s = Sheet(v.as_mut_ptr(), v.len());
        std::mem::forget(v);
        s
    }
    fn none() -> Sheet {
        Sheet(std::ptr::null_mut(), 0)
    }
    /// The sheet as the frame it is (when no job writes it any more).
    fn take(&mut self) -> Vec<u8> {
        let v = if self.0.is_null() { Vec::new() } else { unsafe { Vec::from_raw_parts(self.0, self.1, self.1) } };
        (self.0, self.1) = (std::ptr::null_mut(), 0);
        v
    }
}
impl Drop for Sheet {
    fn drop(&mut self) {
        self.take();
    }
}

/// Where a band's rows of each plane lie in a frame: (offset, length), and the rows of
/// the frame before it may read (its own and 16 of light on each side): (offset,
/// length, the first row).
#[allow(clippy::type_complexity)]
fn band_places(geo: &Geometry, y0: usize, y1: usize) -> ([(usize, usize); 3], [(usize, usize, usize); 3]) {
    let at = geo.offsets();
    let (c0, c1) = colour_rows(geo, y0, y1);
    let rows = [(y0, y1, geo.height), (c0, c1, geo.planes[1].h), (c0, c1, geo.planes[1].h)];
    let own = std::array::from_fn(|p| (at[p] + rows[p].0 * geo.planes[p].w, (rows[p].1 - rows[p].0) * geo.planes[p].w));
    let near = std::array::from_fn(|p| {
        let reach = if p == 0 { BLOCK } else { BLOCK / 2 };
        let (a, b) = (rows[p].0.saturating_sub(reach), (rows[p].1 + reach).min(rows[p].2));
        (at[p] + a * geo.planes[p].w, (b - a) * geo.planes[p].w, a)
    });
    (own, near)
}

/// The player's strike: `n` frames, each frame's bytes from `payload` (asked for as the
/// frame enters the window), every finished frame checked by `check` (on any worker)
/// and handed to `sink` in order (false stops). `before`: the frame before the first
/// (when the strike goes on from one).
#[allow(clippy::too_many_arguments)]
pub fn strike_unpack(geo: &Geometry, n: usize, window: usize, before: Option<&[u8]>, memory: &mut Memory, payload: &mut dyn FnMut(usize) -> Result<Vec<u8>, String>, check: &(dyn Fn(usize, &[u8]) -> Result<(), String> + Sync), sink: &mut dyn FnMut(usize, Vec<u8>) -> Result<bool, String>) -> Result<Struck, String> {
    if n == 0 {
        return Ok(Struck::default());
    }
    if !crate::block::fits(geo) {
        return Err("not a mixed frame".into());
    }
    let first = payload(0)?;
    if first.len() < HEAD {
        return Err("not a mixed frame".into());
    }
    let bands_said = u16::from_le_bytes([first[1], first[2]]);
    let rows = band_rows(geo, bands_said as usize);
    let starts: Vec<usize> = (0..geo.height).step_by(rows).collect();
    let bands = starts.len();
    memory.ready(bands, false);
    let models: Vec<std::sync::Mutex<Model>> = std::mem::take(&mut memory.0).into_iter().map(std::sync::Mutex::new).collect();
    // Each frame's bytes with where its bands lie, and the frame itself.
    let coded: Places<(Vec<u8>, Vec<(usize, usize)>)> = Places::new(n, || (Vec::new(), Vec::new()));
    let sheets: Places<Sheet> = Places::new(n, Sheet::none);
    let mut first = Some(first);
    let spare: std::cell::RefCell<Vec<Vec<u8>>> = Default::default();
    let mut admit = |f: usize| -> Result<Waits, String> {
        let p = match first.take() {
            Some(p) => p,
            None => payload(f)?,
        };
        if p.len() < HEAD || u16::from_le_bytes([p[1], p[2]]) != bands_said {
            return Err("a mixed frame is cut".into());
        }
        if from_before(&p) && f == 0 && !before.is_some_and(|b| b.len() == geo.frame_len()) {
            return Err("a mixed frame was rebuilt without the frame before it".into());
        }
        let mut places = Vec::new();
        if p[0] & STILL == 0 {
            let mut data = HEAD + 4 * bands;
            for i in 0..bands {
                let len = p.get(HEAD + 4 * i..HEAD + 4 * i + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize).ok_or("a mixed frame is cut")?;
                if data + len > p.len() {
                    return Err("a mixed frame is cut".into());
                }
                places.push((data, len));
                data += len;
            }
        }
        let waits = if from_before(&p) && p[0] & STILL == 0 { Waits::Around } else { Waits::Band };
        // Nobody reads these before the frame's jobs are let go, after this.
        unsafe {
            *coded.get_mut(f) = (p, places);
            *sheets.get_mut(f) = Sheet::of(spare.borrow_mut().pop().unwrap_or_else(|| vec![0u8; geo.frame_len()]));
        }
        Ok(waits)
    };
    let run = |f: usize, k: usize| -> Result<(), String> {
        // The frame's own sheet: this job writes its band's rows; the frame before is done where it is read.
        let (p, places) = unsafe { coded.get(f) };
        let sheet = unsafe { sheets.get(f) };
        if k == bands {
            return check(f, unsafe { std::slice::from_raw_parts(sheet.0, sheet.1) });
        }
        let (y0, y1) = (starts[k], (starts[k] + rows).min(geo.height));
        let (own, near) = band_places(geo, y0, y1);
        let cur: [&mut [u8]; 3] = own.map(|(at, len)| unsafe { std::slice::from_raw_parts_mut(sheet.0.add(at), len) });
        let prev: Option<Before> = if !from_before(p) {
            None
        } else {
            let base: *const u8 = if f == 0 { before.unwrap().as_ptr() } else { unsafe { sheets.get(f - 1) }.0 };
            Some(Before { planes: near.map(|(at, len, _)| unsafe { std::slice::from_raw_parts(base.add(at), len) }), first: near.map(|n| n.2) })
        };
        let mut m = models[k].lock().unwrap();
        if p[0] & STILL != 0 {
            let before = prev.ok_or("a still frame was rebuilt without the frame before it")?;
            for (p, to) in cur.into_iter().enumerate() {
                let skip = own[p].0 - near[p].0;
                to.copy_from_slice(&before.planes[p][skip..skip + to.len()]);
            }
            return Ok(());
        }
        if prev.is_none() {
            *m = Model::new();
        }
        let mut c = Reader(Decoder::new(&p[places[k].0..places[k].0 + places[k].1]));
        band(geo, &mut c, &mut m, cur, prev, Vec::new(), y0, y1, How::of(p[0]));
        Ok(())
    };
    let mut deliver = |f: usize| -> Result<bool, String> {
        // The frame before this one is read by nobody now: its sheet is used again.
        unsafe {
            if f > 0 {
                let old = sheets.get_mut(f - 1).take();
                if old.len() == geo.frame_len() {
                    spare.borrow_mut().push(old);
                }
                *coded.get_mut(f - 1) = (Vec::new(), Vec::new());
            }
            let sheet = sheets.get(f);
            sink(f, std::slice::from_raw_parts(sheet.0, sheet.1).to_vec())
        }
    };
    let struck = strike(n, bands, window, &mut admit, &run, &mut deliver);
    memory.0 = models.into_iter().map(|m| m.into_inner().unwrap()).collect();
    struck
}

/// The packer's strike: the frames packed exactly, each from the one before it
/// (`before` for the first; `alone`: every frame on its own). The same bytes as
/// `pack` gives frame by frame.
pub fn strike_pack(geo: &Geometry, frames: &[&[u8]], before: Option<&[u8]>, alone: bool, bands_said: usize, memory: &mut Memory, how: How) -> Result<(Vec<Vec<u8>>, Struck), String> {
    let n = frames.len();
    let rows = band_rows(geo, bands_said);
    let starts: Vec<usize> = (0..geo.height).step_by(rows).collect();
    let bands = starts.len();
    memory.ready(bands, false);
    let models: Vec<std::sync::Mutex<Model>> = std::mem::take(&mut memory.0).into_iter().map(std::sync::Mutex::new).collect();
    let prev_of = |f: usize| -> Option<&[u8]> {
        if alone {
            None
        } else if f > 0 {
            Some(frames[f - 1])
        } else {
            before
        }
    };
    let still: Vec<bool> = (0..n).into_par_iter().map(|f| prev_of(f) == Some(frames[f])).collect();
    let done: Places<(Vec<u8>, [f64; 7])> = Places::new(n * bands, || (Vec::new(), [0.0; 7]));
    let run = |f: usize, k: usize| -> Result<(), String> {
        if k == bands || still[f] {
            return Ok(());
        }
        let (y0, y1) = (starts[k], (starts[k] + rows).min(geo.height));
        let real = planes(geo, frames[f]);
        let prev = prev_of(f).map(|b| Before { planes: planes(geo, b), first: [0; 3] });
        let mut m = models[k].lock().unwrap();
        if prev.is_none() {
            *m = Model::new();
        }
        let vectors = prev.map_or(Vec::new(), |p| find(geo.width, geo.height, real[0], p.planes[0], y0, y1, how.quarter));
        let [mut a, mut b, mut d] = cut(geo, real, y0, y1).map(|p| p.to_vec());
        let mut c = Writer(Encoder::new(), [0.0; 7]);
        band(geo, &mut c, &mut m, [&mut a, &mut b, &mut d], prev, vectors, y0, y1, how);
        // This job alone writes the place.
        unsafe { *done.get_mut(f * bands + k) = (c.0.finish(), c.1) };
        Ok(())
    };
    let struck = strike(n, bands, n, &mut |_| Ok(Waits::Band), &run, &mut |_| Ok(true))?;
    memory.0 = models.into_iter().map(|m| m.into_inner().unwrap()).collect();
    let mut out = Vec::with_capacity(n);
    for f in 0..n {
        let mut bytes = vec![if prev_of(f).is_some() { FROM_BEFORE } else { 0 } | how.flags()];
        bytes.extend((bands_said as u16).to_le_bytes());
        if still[f] {
            bytes[0] |= STILL;
            out.push(bytes);
            continue;
        }
        let parts: Vec<&(Vec<u8>, [f64; 7])> = (0..bands).map(|k| unsafe { done.get(f * bands + k) }).collect();
        for (part, spent) in &parts {
            bytes.extend((part.len() as u32).to_le_bytes());
            for (total, s) in SPENT.iter().zip(spent) {
                total.fetch_add((s * 1000.0) as u64, Ordering::Relaxed);
            }
        }
        for (part, _) in &parts {
            bytes.extend(part);
        }
        out.push(bytes);
    }
    Ok((out, struck))
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
            for (i, v) in b.iter_mut().enumerate().take(w * h / 2) {
                if i % w < w / 2 {
                    *v = v.saturating_add(9);
                }
            }
            let wild = noise(false);
            for (bands, how) in [(1, How::default()), (3, How::default()), (16, How::default()), (3, How { light: 2, two: true, quarter: true }), (16, How { light: 2, two: false, quarter: false }), (1, How { light: 0, two: false, quarter: true })] {
                // The player goes through the frames in the packer's order, with its own memory.
                let (mut packer, mut player) = (Memory::new(), Memory::new());
                let first = pack(&geo, &a, None, bands, &mut packer, how);
                assert_eq!(unpack(&geo, &first, None, &mut player).unwrap(), a, "{w}x{h} alone, {bands} bands");
                let second = pack(&geo, &b, Some(&a), bands, &mut packer, how);
                assert_eq!(unpack(&geo, &second, Some(&a), &mut player).unwrap(), b, "{w}x{h} from before, {bands} bands");
                let same = pack(&geo, &b, Some(&b), bands, &mut packer, how);
                assert_eq!(same.len(), HEAD);
                assert_eq!(unpack(&geo, &same, Some(&b), &mut player).unwrap(), b);
                let third = pack(&geo, &wild, Some(&b), bands, &mut packer, how);
                assert_eq!(unpack(&geo, &third, Some(&b), &mut player).unwrap(), wild, "{w}x{h} noise, {bands} bands");
                // The strike gives the same bytes and the same frames, whatever the window.
                let mut slid = a.clone();
                slid.copy_within(0..w * h - 3, 3);
                let film = vec![a.clone(), b.clone(), b.clone(), wild.clone(), a.clone(), slid];
                let (struck, _) = strike_pack(&geo, &film.iter().map(|f| &f[..]).collect::<Vec<_>>(), None, false, bands, &mut Memory::new(), how).unwrap();
                assert_eq!(struck[..4], [first.clone(), second.clone(), same.clone(), third.clone()], "{w}x{h}, {bands} bands: the strike packs as the packer does");
                for window in [1, 2, 8] {
                    let mut back = Vec::new();
                    strike_unpack(&geo, film.len(), window, None, &mut Memory::new(), &mut |f| Ok(struck[f].clone()), &|_, _| Ok(()), &mut |_, frame| {
                        back.push(frame);
                        Ok(true)
                    })
                    .unwrap();
                    assert_eq!(back, film, "{w}x{h}, {bands} bands, a window of {window}");
                }
            }
        }
    }
}
