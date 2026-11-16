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
/// The sum of what the guesses missed by (without the sign), as the packer counted it.
pub static MISSED: AtomicU64 = AtomicU64::new(0);

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
const NP: usize = 5;
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
    /// The packer's count of how far a guess was off.
    #[inline(always)]
    fn missed(&mut self, _by: u32) {}
}

struct Writer(Encoder, [f64; 7], u64);
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
    #[inline(always)]
    fn missed(&mut self, by: u32) {
        self.2 += by as u64;
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
    /// The far cache: which kept frame a block is guessed from, by whether the block to its left was guessed from the nearest.
    far: [([u16; 32], [u16; 2]); 2],
    /// For the profile: the ticks the range coder took.
    coder_ticks: u64,
    /// The band's working room, made once and used for every frame.
    room: Room,
    /// The light map: is a tile put right (by its left neighbour and the frame before), its
    /// offset and gain, and the frame before's map for each plane.
    lit_on: [[u16; 4]; 3],
    lit_num: [[([u16; 32], [u16; 2]); 2]; 3],
    lit: [Vec<(i32, i32)>; 3],
    /// The fixed weights of the stretch of film the band is in.
    gold: Gold,
    /// The pyramid's learners (made when first asked), and whether the band is told as one.
    pyr: Vec<[u16; 2]>,
    pyramid: bool,
    /// The plates' way for the band (see How).
    plates: u8,
    /// The pyramid the mix's way: the learning guessers' weights for each kind of pass and
    /// plane, what guesses in the same surroundings missed by, and how far each guesser was
    /// off on each sample of the plane being told.
    pyr_lw: Vec<[[i32; FEATS]; 3]>,
    pyr_slow: Vec<[[i32; FEATS]; 3]>,
    pyr_bias: Vec<(i32, i32)>,
    pyr_off: Vec<[u8; 4]>,
    /// TRIAL: how often each tone came in each plane of the band so far (halved after every
    /// frame), and from that how common it is, in 4 steps.
    tone: [[u32; 256]; 3],
    tone_class: [[u8; 256]; 3],
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
            a: vec![[32768, 0]; 4 * A_ALL * SLOTS],
            tone: [[0; 256]; 3],
            tone_class: [[0; 256]; 3],
            d: vec![[32768, 0]; D_ALL * SLOTS],
            e: vec![[32768, 0]; E_ALL * SLOTS],
            w: vec![[26000, 26000, 26000, 0]; SETS],
            w2: vec![[26000, 26000, 26000, 0]; SLOTS * 3 * 5],
            extra: [[2048; 8]; 14],
            bias: vec![(0, 0); A_SIZE],
            vector: [([2048; 32], [2048; 2]); 2],
            far: [([2048; 32], [2048; 2]); 2],
            coder_ticks: 0,
            room: Room::default(),
            lit_on: [[2048; 4]; 3],
            lit_num: [[([2048; 32], [2048; 2]); 2]; 3],
            lit: Default::default(),
            gold: [[0; FEATS]; GOLD_SETS],
            pyr: Vec::new(),
            pyramid: false,
            plates: 0,
            pyr_lw: Vec::new(),
            pyr_slow: Vec::new(),
            pyr_bias: Vec::new(),
            pyr_off: Vec::new(),
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
fn plane<C: Bits, const T: bool>(c: &mut C, m: &mut Model, pk: usize, w: usize, cur: &mut [u8], before: &[u8], (px, ec, eu): (&mut Vec<u8>, &mut Vec<[u8; NP]>, &mut Vec<[u8; NP]>), res: &mut [i8], first: Option<&[i8]>, two: bool, tones: bool) {
    if m.pyramid {
        return plane_pyr_learn::<C, T>(c, m, pk, w, cur, before, res, first);
    }
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
    let plates = m.plates;
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
            // The plate's guess: the fixed weights of where the sample stands, on what the fixed
            // weights weigh (the learning guesser's own, without CALIC's guess).
            let plate = if plates > 0 {
                let weighed: [i32; FEATS] = if T { std::array::from_fn(|k| if k < 9 { feats[k] } else if k < 13 { feats[k + 1] } else { 0 }) } else { std::array::from_fn(|k| if k < 7 { feats[k] } else { 0 }) };
                let weights = &m.gold[PLATE_SETS + (if T { pk } else { 3 + pk }) * PLATES + plate_of(west, north, nw, ne, ww, nn, nne, if T { Some([feats[0], feats[1], feats[2], feats[3]]) } else { None })];
                let mut lean = 0i32;
                for k in 0..FEATS {
                    lean = lean.wrapping_add(weights[k].wrapping_mul(weighed[k]));
                }
                (base + (lean.wrapping_add(32768) >> 16)).clamp(0, 255)
            } else {
                0
            };
            let (mut lean, mut lean_slow, mut norm) = (0i32, 0i32, 32i32);
            for k in 0..if plates == 1 { 0 } else { FEATS } {
                lean = lean.wrapping_add(lw[k].wrapping_mul(feats[k]));
                lean_slow = lean_slow.wrapping_add(lw_slow[k].wrapping_mul(feats[k]));
                norm += feats[k] * feats[k];
            }
            let learned = (base + (lean.wrapping_add(32768) >> 16)).clamp(0, 255);
            let learned_slow = (base + (lean_slow.wrapping_add(32768) >> 16)).clamp(0, 255);
            // Plates in place of the guessers: nothing is learned for the guess.
            let (learned, learned_slow) = if plates == 1 { (plate, plate) } else { (learned, learned_slow) };
            let offers = [med, gap, learned, learned_slow, plate];
            lap!(1);
            // The mix: each speaks as loud as it was right on the neighbours.
            let (e_w, e_n, e_nw, e_ne) = (ec[x], eu[x + 1], eu[x], eu[x + 2]);
            let (mut sum, mut total) = (0u32, 0u32);
            for g in if two { 2 } else { 0 }..if plates == 2 { NP } else { 4 } {
                let wt = t.weight[2 * e_w[g] as usize + 2 * e_n[g] as usize + e_nw[g] as usize + e_ne[g] as usize];
                sum += wt;
                total += wt * offers[g] as u32;
            }
            let mixed = if plates == 1 { plate } else { ((total + sum / 2) / sum) as i32 };
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
            let common = if tones { m.tone_class[pk][mixed as usize] as usize * A_ALL } else { 0 };
            let got = m.miss(c, t, pk, [common + at_a, at_d, at_e], common + signs, (if near < 2 { 0 } else if near < 5 { 1 } else { 2 }, stayed), want);
            lap!(4);
            c.missed(got.unsigned_abs());
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
            for k in 0..if plates == 1 { 0 } else { FEATS } {
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
    if tones {
        let tone = &mut m.tone[pk];
        for v in cur.iter() {
            tone[*v as usize] += 1;
        }
        // Against a tone as common as every other (1 of 256): under an eighth, under one, under four, more.
        let all: u64 = tone.iter().map(|n| *n as u64).sum();
        for (class, n) in m.tone_class[pk].iter_mut().zip(tone.iter_mut()) {
            let share = *n as u64 * 256 * 8 / all.max(1);
            *class = if share < 1 { 0 } else if share < 8 { 1 } else if share < 32 { 2 } else { 3 };
            *n /= 2;
        }
    }
    if prof {
        ticks[5] = m.coder_ticks - coder_before;
        ticks[4] -= ticks[5];
        ticks[7] = (rows * w) as u64;
        for (total, t) in TICKS.iter().zip(ticks) {
            total.fetch_add(t, Ordering::Relaxed);
        }
    }
}

/// GOLD: the fixed weights of a stretch of film, worked out once by the packer and kept
/// in the head of the stretch's first frame: for each plane the weights of a frame that
/// leans on the one before, then of a frame that stands alone (65536 is one).
pub type Gold = [[i32; FEATS]; GOLD_SETS];
/// The sets of weights: 6 for the rows told one after the other, then the pyramid's.
const GOLD_SETS: usize = PLATE_SETS + 6 * PLATES;
/// For each set of weights: the 16 x 16 sums of products of the things weighed, then
/// the 16 sums of each with what they should have summed to.
type Sums = Vec<[f64; 17 * FEATS]>;
/// How large the misses nearby were, in 5 steps (of the 9).
const NEAR5: [usize; 9] = [0, 1, 2, 2, 3, 3, 3, 4, 4];

/// The sets a frame's head carries: the plates, the pyramid's, or the six of the rows.
fn gold_sets(how: How) -> std::ops::Range<usize> {
    if how.plates > 0 {
        PLATE_SETS..GOLD_SETS
    } else if how.pyramid {
        PYR_SETS..PLATE_SETS
    } else {
        0..PYR_SETS
    }
}

fn gold_bytes(g: &Gold, how: How) -> Vec<u8> {
    g[gold_sets(how)].iter().flatten().flat_map(|w| w.to_le_bytes()).collect()
}

fn gold_of(bytes: &[u8], how: How) -> Gold {
    let mut g = [[0; FEATS]; GOLD_SETS];
    for (s, set) in g[gold_sets(how)].iter_mut().enumerate() {
        *set = std::array::from_fn(|k| i32::from_le_bytes(bytes[4 * (s * FEATS + k)..4 * (s * FEATS + k) + 4].try_into().unwrap()));
    }
    g
}

/// Where a frame's table of band lengths starts: after the head, and the weights when
/// the frame carries them.
fn table_at(head: &[u8]) -> usize {
    let how = how_of(head);
    HEAD + if (how.gold || how.plates > 0) && head[0] & FROM_BEFORE == 0 { gold_sets(how).len() * FEATS * 4 } else { 0 }
}

/// A frame's head: its flags, then the bands said, with the plates' way in the top two bits.
fn head_of(flags: u8, bands: usize, how: How) -> [u8; HEAD] {
    let said = (bands as u16 & 0x3FFF) | ((how.plates as u16) << 14);
    [flags | how.flags(), said as u8, (said >> 8) as u8]
}

fn how_of(head: &[u8]) -> How {
    How { plates: head[2] >> 6, ..How::of(head[0]) }
}

fn bands_of(head: &[u8]) -> usize {
    (u16::from_le_bytes([head[1], head[2]]) & 0x3FFF) as usize
}

/// The weights that make the squares of the misses smallest, from the sums (the
/// answer to 14 equations in 14 letters).
fn solve(sums: &[f64; 17 * FEATS]) -> [i32; FEATS] {
    let n = FEATS;
    let mut a = vec![vec![0f64; n + 1]; n];
    for i in 0..n {
        for j in 0..n {
            a[i][j] = sums[i * FEATS + j];
        }
        a[i][i] += 1.0;
        a[i][n] = sums[16 * FEATS + i];
    }
    for i in 0..n {
        let p = (i..n).max_by(|x, y| a[*x][i].abs().partial_cmp(&a[*y][i].abs()).unwrap()).unwrap();
        a.swap(i, p);
        for r in 0..n {
            if r != i && a[i][i] != 0.0 {
                let k = a[r][i] / a[i][i];
                for c in i..=n {
                    a[r][c] -= k * a[i][c];
                }
            }
        }
    }
    let mut w = [0i32; FEATS];
    for i in 0..n {
        w[i] = if a[i][i] != 0.0 { (65536.0 * a[i][n] / a[i][i]).round().clamp(-524288.0, 524288.0) as i32 } else { 0 };
    }
    w
}

/// PRESSURE PLATES: fixed weights chosen by where the sample stands. What lies around a
/// sample (how it slopes, along which way an edge runs, how much the neighbours changed
/// since the frame before) is told as one of PLATES numbers, and each number has its own
/// set of weights (its plate), worked out by the packer for the stretch of film and kept
/// in the head of the stretch's first frame. The guess is one lookup and one weighed sum;
/// the player learns nothing for it.
const PLATES: usize = 60;
/// Where the plates' sets of weights start: for each plane, with a frame before, then alone.
const PLATE_SETS: usize = PYR_SETS + 6 * KINDS;

/// Which plate: the edge's way in 5 steps (as CALIC tells it), then with a frame before
/// how much the four neighbours changed (4 steps) and how rough it is here (3 steps);
/// alone, how rough in 4 steps.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn plate_of(west: i32, north: i32, nw: i32, ne: i32, ww: i32, nn: i32, nne: i32, changed: Option<[i32; 4]>) -> usize {
    let (dh, dv) = ((west - ww).abs() + (north - nw).abs() + (north - ne).abs(), (west - nw).abs() + (north - nn).abs() + (ne - nne).abs());
    let edge = if dv - dh > 32 {
        0
    } else if dv - dh > 8 {
        1
    } else if dh - dv > 32 {
        4
    } else if dh - dv > 8 {
        3
    } else {
        2
    };
    let rough = dh + dv;
    match changed {
        Some(c) => {
            let by = c[0].abs() + c[1].abs() + c[2].abs() + c[3].abs();
            let moved = if by == 0 { 0 } else if by <= 2 { 1 } else if by <= 6 { 2 } else { 3 };
            (edge * 4 + moved) * 3 + if rough < 6 { 0 } else if rough < 20 { 1 } else { 2 }
        }
        None => edge * 4 + if rough < 4 { 0 } else if rough < 12 { 1 } else if rough < 32 { 2 } else { 3 },
    }
}

/// PYRAMID: with fixed weights, a plane is told coarse first: every eighth sample of
/// every eighth row, then the samples between those (the middle of four, then the two
/// kinds beside them), then again at half the step, down to every sample. A sample is
/// guessed from the four around it that are already told (on all its sides) and the
/// frame before; nothing of its own pass is read, so a pass's guesses do not wait for
/// one another.
const TOP: usize = 8;
/// The kinds of pass (the coarse one, then three for each halving), and where the
/// pyramid's sets of weights start: for each plane and kind, with a frame before, then alone.
const KINDS: usize = 10;
const PYR_SETS: usize = 6;
const PYR_AT: usize = 5 * 5 * 5 * 5 * KINDS * 3;
const SPREAD: [u8; 16] = [0, 1, 1, 2, 2, 2, 3, 3, 3, 3, 3, 3, 4, 4, 4, 4];

impl Model {
    /// A miss of the pyramid, written or read, each yes/no by one learner.
    #[inline(always)]
    fn miss_pyr<C: Bits>(&mut self, c: &mut C, t: &Tables, pk: usize, at: usize, signs: usize, v: i32) -> i32 {
        if !one(c, t, &mut self.pyr[at * SLOTS], v != 0, (pk, 4), &mut self.coder_ticks) {
            return 0;
        }
        let negative = one(c, t, &mut self.pyr[PYR_AT * SLOTS + signs], v < 0, (pk, 5), &mut self.coder_ticks);
        let size = v.unsigned_abs().clamp(1, 128);
        let want = if C::WRITE { LO.iter().rposition(|lo| *lo <= size).unwrap_or(0).min(13) } else { 0 };
        let mut b = 0;
        while b < 13 && one(c, t, &mut self.pyr[at * SLOTS + 2 + b], want > b, (pk, 6), &mut self.coder_ticks) {
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

/// One pass of the pyramid over a plane of a band: the samples at `xs.0, xs.0 + xs.1 ...`
/// of the rows `ys.0, ys.0 + ys.1 ...`, each guessed from the four (or eight) at `around`
/// (those outside the band count for nothing).
#[allow(clippy::too_many_arguments)]
fn pass<C: Bits, const T: bool>(c: &mut C, m: &mut Model, pk: usize, kind: usize, w: usize, cur: &mut [u8], before: &[u8], res: &mut [i8], first: Option<&[i8]>, sums: &mut Option<&mut Sums>, xs: (usize, usize), ys: (usize, usize), around: &[(i32, i32)]) {
    let t = tables();
    let rows = cur.len() / w;
    let sw = w + 2 * PAD;
    let set = PYR_SETS + if T { 0 } else { 3 * KINDS } + pk * KINDS + kind;
    let weights = m.gold[set];
    // A pass of the finest step is sampled thinly for the sums, a coarse one wholly.
    let thin = if xs.1 <= 2 { 3 } else if xs.1 <= 4 { 1 } else { 0 };
    for y in (ys.0..rows).step_by(ys.1) {
        for x in (xs.0..w).step_by(xs.1) {
            let (i, ip) = (y * w + x, (y + PAD) * sw + PAD + x);
            // The four around: the sample, the frame before there, the miss there.
            let (mut now, mut was, mut miss, mut have) = ([0i32; 8], [0i32; 8], [0i32; 8], [false; 8]);
            for (j, (dx, dy)) in around.iter().copied().enumerate() {
                let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                if nx >= 0 && ny >= 0 && (nx as usize) < w && (ny as usize) < rows {
                    let np = (ny as usize + PAD) * sw + PAD + nx as usize;
                    (now[j], miss[j], have[j]) = (cur[ny as usize * w + nx as usize] as i32, res[np] as i32, true);
                    if T {
                        was[j] = before[np] as i32;
                    }
                }
            }
            let (base, stayed, spread, feats): (i32, usize, i32, [i32; FEATS]) = if T {
                // How each of the four changed since the frame before, and how the frame before slopes to it.
                let t0 = before[ip] as i32;
                let d: [i32; 8] = std::array::from_fn(|j| if have[j] { now[j] - was[j] } else { 0 });
                let e: [i32; 8] = std::array::from_fn(|j| if have[j] { was[j] - t0 } else { 0 });
                let stayed = (0..4).filter(|j| have[*j] && d[*j] == 0).count();
                (t0, stayed, d[..4].iter().max().unwrap() - d[..4].iter().min().unwrap(), std::array::from_fn(|k| if k < 8 { d[k] } else { e[k - 8] }))
            } else {
                let n = have[..4].iter().filter(|h| **h).count() as i32;
                let base = if n == 0 { 128 } else { ((0..4).filter(|j| have[*j]).map(|j| now[j]).sum::<i32>() + n / 2) / n };
                let f: [i32; 8] = std::array::from_fn(|j| if have[j] { now[j] - base } else { 0 });
                (base, 0, (f[..4].iter().max().unwrap() - f[..4].iter().min().unwrap()) / 2, std::array::from_fn(|k| if k < 8 { f[k] } else { 0 }))
            };
            let mut lean = 0i32;
            for k in 0..FEATS {
                lean = lean.wrapping_add(weights[k].wrapping_mul(feats[k]));
            }
            let guess = (base + (lean.wrapping_add(32768) >> 16)).clamp(0, 255);
            // Where the sample stands: how the base and the mean of the four stand from the guess
            // (alone: how unlike the four are), how large their misses were, how many stayed, and
            // the kind of pass.
            let near = NEAR5[STEP[((miss[..4].iter().map(|r| r.unsigned_abs()).sum::<u32>() as usize + 1) / 2).min(23)] as usize];
            let five = |r: i32| (r.clamp(-2, 2) + 2) as usize;
            let n4 = have[..4].iter().filter(|h| **h).count() as i32;
            let mean4 = if n4 == 0 { guess } else { ((0..4).filter(|j| have[*j]).map(|j| now[j]).sum::<i32>() + n4 / 2) / n4 };
            let at = ((((five(base - guess) * 5 + if T { five(mean4 - guess) } else { SPREAD[(spread as usize).min(15)] as usize }) * 5 + near) * 5 + stayed) * KINDS + kind) * 3 + pk;
            let sign_first = first.map_or(1, |f| (f[ip] as i32).signum() + 1);
            let signs = ((((miss[..4].iter().sum::<i32>().signum() + 1) * 3 + sign_first) as usize) * KINDS + kind) * 3 + pk;
            let real = if C::WRITE { cur[i] } else { 0 };
            let want = if C::WRITE { real.wrapping_sub(guess as u8) as i8 as i32 } else { 0 };
            let got = m.miss_pyr(c, t, pk, at, signs, want);
            let real = if C::WRITE { real } else { (guess as u8).wrapping_add(got as u8) };
            cur[i] = real;
            res[ip] = got as i8;
            if let Some(all) = sums.as_deref_mut() {
                if ((x / xs.1) ^ (y / ys.1)) & thin == 0 && (y / ys.1) & thin == 0 {
                    let (s, want) = (&mut all[set], (real as i32 - base) as f64);
                    for j in 0..FEATS {
                        let fj = feats[j] as f64;
                        for k in 0..FEATS {
                            s[j * FEATS + k] += fj * feats[k] as f64;
                        }
                        s[16 * FEATS + j] += fj * want;
                    }
                }
            }
        }
    }
}

/// One plane of a band as a pyramid.
#[allow(clippy::too_many_arguments)]
fn plane_pyr<C: Bits, const T: bool>(c: &mut C, m: &mut Model, pk: usize, w: usize, cur: &mut [u8], before: &[u8], res: &mut [i8], first: Option<&[i8]>, mut sums: Option<&mut Sums>) {
    if m.pyr.is_empty() {
        m.pyr = vec![[32768, 0]; PYR_AT * SLOTS + 9 * KINDS * 3];
    }
    let s = TOP as i32;
    pass::<C, T>(c, m, pk, 0, w, cur, before, res, first, &mut sums, (0, TOP), (0, TOP), &[(-s, 0), (0, -s), (-s, -s), (s, -s)]);
    for (k, step) in [TOP, TOP / 2, TOP / 4].into_iter().enumerate() {
        let (half, h) = (step / 2, step as i32 / 2);
        // The middle of four; then beside it in a row of the step; then in a row between, where
        // the four cornerwise are told by now too.
        pass::<C, T>(c, m, pk, 1 + 3 * k, w, cur, before, res, first, &mut sums, (half, step), (half, step), &[(-h, -h), (h, -h), (-h, h), (h, h)]);
        pass::<C, T>(c, m, pk, 2 + 3 * k, w, cur, before, res, first, &mut sums, (half, step), (0, step), &[(-h, 0), (h, 0), (0, -h), (0, h)]);
        pass::<C, T>(c, m, pk, 3 + 3 * k, w, cur, before, res, first, &mut sums, (0, step), (half, step), &[(0, -h), (0, h), (-h, 0), (h, 0), (-h, -h), (h, -h), (-h, h), (h, h)]);
    }
}

/// The pyramid with the mix's own way of guessing and writing (TRIAL): the passes of
/// `plane_pyr`, but a sample is guessed by four guessers mixed (the mean of the four
/// around it, the frame before moved by how the four changed, and two learning guessers,
/// with weights of their own for each kind of pass), put right by its surroundings, and
/// its miss is written by the three learners mixed.
const PYR_A: usize = 9 * 6 * KINDS * 3;
const PYR_A_ALL: usize = PYR_A + 27 * KINDS * 3;
const PYR_D: usize = 625 * 3 * 3;

#[allow(clippy::too_many_arguments)]
fn pass_learn<C: Bits, const T: bool>(c: &mut C, m: &mut Model, pk: usize, kind: usize, w: usize, cur: &mut [u8], before: &[u8], res: &mut [i8], first: Option<&[i8]>, xs: (usize, usize), ys: (usize, usize), around: &[(i32, i32)]) {
    let t = tables();
    let rows = cur.len() / w;
    let sw = w + 2 * PAD;
    let (mut lw, mut lw_slow) = (m.pyr_lw[kind][pk], m.pyr_slow[kind][pk]);
    let mut off = std::mem::take(&mut m.pyr_off);
    // The coarse passes, the middles of the finest step, the rest of the finest step.
    let group = if kind < KINDS - 3 { 0 } else if kind == KINDS - 3 { 1 } else { 2 };
    for y in (ys.0..rows).step_by(ys.1) {
        for x in (xs.0..w).step_by(xs.1) {
            let (i, ip) = (y * w + x, (y + PAD) * sw + PAD + x);
            // Those around: the sample, the frame before there, the miss there, how far each guesser was off there.
            let (mut now, mut was, mut miss, mut have, mut were) = ([0i32; 8], [0i32; 8], [0i32; 8], [false; 8], [0u32; 4]);
            for (j, (dx, dy)) in around.iter().copied().enumerate() {
                let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                if nx >= 0 && ny >= 0 && (nx as usize) < w && (ny as usize) < rows {
                    let (ni, np) = (ny as usize * w + nx as usize, (ny as usize + PAD) * sw + PAD + nx as usize);
                    (now[j], miss[j], have[j]) = (cur[ni] as i32, res[np] as i32, true);
                    if T {
                        was[j] = before[np] as i32;
                    }
                    if j < 4 {
                        for g in 0..4 {
                            were[g] += off[ni][g] as u32;
                        }
                    }
                }
            }
            let n4 = have[..4].iter().filter(|h| **h).count() as i32;
            let mean_of = |v: &[i32; 8]| if n4 == 0 { 0 } else { ((0..4).filter(|j| have[*j]).map(|j| v[j]).sum::<i32>() + 2 * n4 + n4 / 2).div_euclid(n4) - 2 };
            let (base, stayed, moved, feats): (i32, usize, i32, [i32; FEATS]) = if T {
                let t0 = before[ip] as i32;
                let d: [i32; 8] = std::array::from_fn(|j| if have[j] { now[j] - was[j] } else { 0 });
                let e: [i32; 8] = std::array::from_fn(|j| if have[j] { was[j] - t0 } else { 0 });
                let stayed = (0..4).filter(|j| have[*j] && d[*j] == 0).count();
                let by = if n4 == 0 { 0 } else { ((0..4).map(|j| d[j]).sum::<i32>() + 512 * n4 + n4 / 2) / n4 - 512 };
                (t0, stayed, (t0 + by).clamp(0, 255), std::array::from_fn(|k| if k < 8 { d[k] } else { e[k - 8] }))
            } else {
                let base = if n4 == 0 { 128 } else { mean_of(&now) };
                let f: [i32; 8] = std::array::from_fn(|j| if have[j] { now[j] - base } else { 0 });
                (base, 0, base, std::array::from_fn(|k| if k < 8 { f[k] } else { 0 }))
            };
            let mean4 = if n4 == 0 { base } else { mean_of(&now) };
            let (mut lean, mut lean_slow, mut norm) = (0i32, 0i32, 32i32);
            for k in 0..FEATS {
                lean = lean.wrapping_add(lw[k].wrapping_mul(feats[k]));
                lean_slow = lean_slow.wrapping_add(lw_slow[k].wrapping_mul(feats[k]));
                norm += feats[k] * feats[k];
            }
            let learned = (base + (lean.wrapping_add(32768) >> 16)).clamp(0, 255);
            let learned_slow = (base + (lean_slow.wrapping_add(32768) >> 16)).clamp(0, 255);
            let offers = [mean4, moved, learned, learned_slow];
            // The mix: each speaks as loud as it was right on those around.
            let (mut sum, mut total) = (0u32, 0u32);
            for g in 0..4 {
                let wt = t.weight[(3 * were[g] as usize / 2).min(1535)];
                sum += wt;
                total += wt * offers[g] as u32;
            }
            let mixed = ((total + sum / 2) / sum) as i32;
            let low = mean4.min(moved).min(learned).min(learned_slow);
            let high = mean4.max(moved).max(learned).max(learned_slow);
            // Where the sample stands.
            let near = STEP[((3 * miss[..4].iter().map(|r| r.unsigned_abs()).sum::<u32>() as usize / 2 + 1) / 3).min(23)] as usize;
            let at_a = ((near * 6 + APART[((high - low) as usize).min(25)] as usize) * KINDS + kind) * 3 + pk;
            let sign_first = first.map_or(1, |f| (f[ip] as i32).signum() + 1);
            let signs = PYR_A + ((((miss[0].signum() + 1) * 3 + miss[1].signum() + 1) * 3 + sign_first) as usize * KINDS + kind) * 3 + pk;
            let b = m.pyr_bias[at_a];
            let lean = ((2 * b.0 + b.1 * b.0.signum()).unsigned_abs() * t.half[b.1 as usize]) >> 24;
            let guess = (mixed + if b.0 < 0 { -(lean as i32) } else { lean as i32 }).clamp(0, 255);
            let five = |r: i32| (r.clamp(-2, 2) + 2) as usize;
            let at_d = ((((five(miss[0]) * 5 + five(miss[1])) * 5 + five(miss[2])) * 5 + five(miss[3])) * 3 + group) * 3 + pk;
            let at_e = ((((five(base - guess) * 5 + five(mean4 - guess)) * 5 + five(learned - guess)) * 5 + stayed) * 3 + group) * 3 + pk;
            let real = if C::WRITE { cur[i] } else { 0 };
            let want = if C::WRITE { real.wrapping_sub(guess as u8) as i8 as i32 } else { 0 };
            let got = m.miss(c, t, pk, [at_a, at_d, at_e], signs, (if near < 2 { 0 } else if near < 5 { 1 } else { 2 }, stayed), want);
            c.missed(got.unsigned_abs());
            let real = if C::WRITE { real } else { (guess as u8).wrapping_add(got as u8) };
            cur[i] = real;
            res[ip] = got as i8;
            let b = &mut m.pyr_bias[at_a];
            b.0 += got as i8 as i32;
            b.1 += 1;
            if b.1 == 32 {
                *b = (b.0 / 2, 16);
            }
            off[i] = offers.map(|o| (real as i32 - o).unsigned_abs() as u8);
            let over = ((1u32 << 24) / norm as u32) as i64;
            let pull = ((((real as i32 - learned) * LEARN) as i64 * over) >> 21) as i32;
            let pull_slow = ((((real as i32 - learned_slow) * (LEARN / 16)) as i64 * over) >> 17) as i32;
            for k in 0..FEATS {
                lw[k] = lw[k].wrapping_add(pull.wrapping_mul(feats[k]) >> 3);
                lw_slow[k] = lw_slow[k].wrapping_add(pull_slow.wrapping_mul(feats[k]) >> 7);
            }
        }
    }
    (m.pyr_lw[kind][pk], m.pyr_slow[kind][pk]) = (lw, lw_slow);
    m.pyr_off = off;
}

/// One plane of a band as a pyramid, the mix's way.
fn plane_pyr_learn<C: Bits, const T: bool>(c: &mut C, m: &mut Model, pk: usize, w: usize, cur: &mut [u8], before: &[u8], res: &mut [i8], first: Option<&[i8]>) {
    if m.pyr_lw.is_empty() {
        m.pyr_lw = vec![[[0; FEATS]; 3]; KINDS];
        m.pyr_slow = vec![[[0; FEATS]; 3]; KINDS];
        m.pyr_bias = vec![(0, 0); PYR_A];
        m.a = vec![[32768, 0]; PYR_A_ALL * SLOTS];
        m.d = vec![[32768, 0]; PYR_D * SLOTS];
        m.e = vec![[32768, 0]; PYR_D * SLOTS];
    }
    m.pyr_off.clear();
    m.pyr_off.resize(cur.len(), [0u8; 4]);
    let s = TOP as i32;
    pass_learn::<C, T>(c, m, pk, 0, w, cur, before, res, first, (0, TOP), (0, TOP), &[(-s, 0), (0, -s), (-s, -s), (s, -s)]);
    for (k, step) in [TOP, TOP / 2, TOP / 4].into_iter().enumerate() {
        let (half, h) = (step / 2, step as i32 / 2);
        pass_learn::<C, T>(c, m, pk, 1 + 3 * k, w, cur, before, res, first, (half, step), (half, step), &[(-h, -h), (h, -h), (-h, h), (h, h)]);
        pass_learn::<C, T>(c, m, pk, 2 + 3 * k, w, cur, before, res, first, (half, step), (0, step), &[(-h, 0), (h, 0), (0, -h), (0, h), (-h, -2 * h), (h, -2 * h), (-h, 2 * h), (h, 2 * h)]);
        pass_learn::<C, T>(c, m, pk, 3 + 3 * k, w, cur, before, res, first, (0, step), (half, step), &[(0, -h), (0, h), (-h, 0), (h, 0), (-h, -h), (h, -h), (-h, h), (h, h)]);
    }
}

/// The packer's dry run: nothing is written, the yes/nos are what they are.
struct Dry;
impl Bits for Dry {
    const WRITE: bool = true;
    #[inline(always)]
    fn bit(&mut self, _: i32, v: bool, _: (usize, usize)) -> bool {
        v
    }
    #[inline(always)]
    fn plain(&mut self, _: &mut u16, v: bool, _: (usize, usize)) -> bool {
        v
    }
}

/// One yes/no said by one learner alone (no mixing), and the learner taught.
#[inline(always)]
fn one<C: Bits>(c: &mut C, t: &Tables, p: &mut [u16; 2], v: bool, what: (usize, usize), ticks: &mut u64) -> bool {
    let said = ((p[0] >> 4) as i32).clamp(1, 4095);
    let bit = if profiled() {
        let from = now();
        let bit = c.bit(said, v, what);
        *ticks += now() - from;
        bit
    } else {
        c.bit(said, v, what)
    };
    let target = if bit { 65535i32 } else { 0 };
    *p = [(p[0] as i32 + (((target - p[0] as i32) * t.part[p[1] as usize]) >> 16)) as u16, (p[1] + 1).min(SLOW)];
    bit
}

impl Model {
    /// A miss, written or read, each yes/no by one learner.
    #[inline(always)]
    fn miss_gold<C: Bits>(&mut self, c: &mut C, t: &Tables, pk: usize, at: usize, signs: usize, v: i32) -> i32 {
        if !one(c, t, &mut self.e[at * SLOTS], v != 0, (pk, 4), &mut self.coder_ticks) {
            return 0;
        }
        let negative = one(c, t, &mut self.a[signs * SLOTS + 1], v < 0, (pk, 5), &mut self.coder_ticks);
        let size = v.unsigned_abs().clamp(1, 128);
        let want = if C::WRITE { LO.iter().rposition(|lo| *lo <= size).unwrap_or(0).min(13) } else { 0 };
        let mut b = 0;
        while b < 13 && one(c, t, &mut self.e[at * SLOTS + 2 + b], want > b, (pk, 6), &mut self.coder_ticks) {
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

/// One plane of a band with the fixed weights: the guess is one weighed sum, nothing is
/// learned for it, and each yes/no of a miss is said by one learner. `sums`: the packer
/// adding up what the weights are worked out from (every fourth sample of every fourth row).
#[allow(clippy::too_many_arguments)]
fn plane_gold<C: Bits, const T: bool>(c: &mut C, m: &mut Model, pk: usize, w: usize, cur: &mut [u8], before: &[u8], px: &mut Vec<u8>, res: &mut [i8], first: Option<&[i8]>, mut sums: Option<&mut Sums>) {
    if m.pyramid {
        return plane_pyr::<C, T>(c, m, pk, w, cur, before, res, first, sums);
    }
    let t = tables();
    let rows = cur.len() / w;
    let sw = w + 2 * PAD;
    px.resize((rows + PAD) * sw, 0);
    if T {
        px[..PAD * sw].copy_from_slice(&before[..PAD * sw]);
    } else {
        px[..PAD * sw].fill(128);
    }
    let set = if T { pk } else { 3 + pk };
    let weights = m.gold[set];
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
    for yy in 0..rows {
        let o = (yy + PAD) * sw + PAD;
        (px[o - 1], px[o - 2]) = (px[o - sw], px[o - sw]);
        for x in 0..w {
            let i = o + x;
            let p = |k: usize| px[k] as i32;
            let (west, north, nw, ne, ww, nn, nne) = (p(i - 1), p(i - sw), p(i - sw - 1), p(i - sw + 1), p(i - 2), p(i - 2 * sw), p(i - 2 * sw + 1));
            let med = median(west, north, west + north - nw);
            let (base, stayed, feats): (i32, usize, [i32; FEATS]) = if T {
                let q = |k: usize| before[k] as i32;
                let t0 = q(i);
                let (cw, cn, cnw, cne) = (west - q(i - 1), north - q(i - sw), nw - q(i - sw - 1), ne - q(i - sw + 1));
                let stayed = (cw == 0) as usize + (cn == 0) as usize + (cnw == 0) as usize + (cne == 0) as usize;
                (t0, stayed, [cw, cn, cnw, cne, q(i + 1) - t0, q(i + sw) - t0, q(i - 1) - t0, q(i - sw) - t0, med - t0, q(i + sw + 1) - t0, q(i + sw - 1) - t0, ww - q(i - 2), nn - q(i - 2 * sw), 0, 0, 0])
            } else {
                (west, 0, [north - west, nw - west, ne - west, ww - west, nn - west, p(i - 2 * sw + 1) - west, med - west, 0, 0, 0, 0, 0, 0, 0, 0, 0])
            };
            lap!(0);
            // With plates: the set of weights is the plate of where the sample stands.
            let (set, weights) = if m.plates > 0 {
                let s = PLATE_SETS + (if T { pk } else { 3 + pk }) * PLATES + plate_of(west, north, nw, ne, ww, nn, nne, if T { Some([feats[0], feats[1], feats[2], feats[3]]) } else { None });
                (s, m.gold[s])
            } else {
                (set, weights)
            };
            let mut lean = 0i32;
            for k in 0..FEATS {
                lean = lean.wrapping_add(weights[k].wrapping_mul(feats[k]));
            }
            let guess = (base + (lean.wrapping_add(32768) >> 16)).clamp(0, 255);
            lap!(1);
            // Where the sample stands: how the frame before (alone: what is above) and the median
            // stand from the guess, how large the misses nearby were, how many neighbours stayed.
            let r = |k: usize| res[k] as i32;
            let (rw, rn, rnw, rne) = (r(i - 1), r(i - sw), r(i - sw - 1), r(i - sw + 1));
            let near = STEP[(((2 * (rw.unsigned_abs() + rn.unsigned_abs()) + rnw.unsigned_abs() + rne.unsigned_abs() + 1) / 3) as usize).min(23)] as usize;
            let five = |r: i32| (r.clamp(-2, 2) + 2) as usize;
            let at = (((five((if T { base } else { north }) - guess) * 5 + five(med - guess)) * 5 + NEAR5[near]) * 5 + stayed) * 3 + pk;
            let sign_first = first.map_or(1, |f| (f[i] as i32).signum() + 1);
            let signs = A_SIZE + (((rw.signum() + 1) * 3 + rn.signum() + 1) * 3 + sign_first) as usize * 3 + pk;
            let real = if C::WRITE { cur[yy * w + x] } else { 0 };
            let want = if C::WRITE { real.wrapping_sub(guess as u8) as i8 as i32 } else { 0 };
            lap!(3);
            let got = m.miss_gold(c, t, pk, at, signs, want);
            lap!(4);
            c.missed(got.unsigned_abs());
            let real = if C::WRITE { real } else { (guess as u8).wrapping_add(got as u8) };
            px[i] = real;
            res[i] = got as i8;
            if let Some(all) = sums.as_deref_mut() {
                if x & 3 == 0 && yy & 3 == 0 {
                    let (s, want) = (&mut all[set], (real as i32 - base) as f64);
                    for j in 0..FEATS {
                        let fj = feats[j] as f64;
                        for k in 0..FEATS {
                            s[j * FEATS + k] += fj * feats[k] as f64;
                        }
                        s[16 * FEATS + j] += fj * want;
                    }
                }
            }
        }
        (px[o + w], px[o + w + 1]) = (px[o + w - 1], px[o + w - 1]);
        if !C::WRITE {
            cur[yy * w..(yy + 1) * w].copy_from_slice(&px[o..o + w]);
        }
    }
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
            let best = find_block(w, h, real, prev, bx, by, quarter);
            out.push((best.1, best.2));
        }
    }
    out
}

/// The block at (bx, by): how unlike the best place in `prev` is (with what its vector
/// costs), and the vector.
fn find_block(w: usize, h: usize, real: &[u8], prev: &[u8], bx: usize, by: usize, quarter: bool) -> (u32, i32, i32) {
    let (bw, bh) = (BLOCK.min(w - bx), BLOCK.min(h - by));
    let unlike = |dx: i32, dy: i32, limit: u32| -> u32 {
        let mut s = 0u32;
        let (x, y) = (bx as i32 + dx, by as i32 + dy);
        if x >= 0 && y >= 0 && x as usize + bw <= w && y as usize + bh <= h {
            // Wholly inside the frame before: row against row.
            let (x, y) = (x as usize, y as usize);
            for j in 0..bh {
                let (a, b) = (&real[(by + j) * w + bx..][..bw], &prev[(y + j) * w + x..][..bw]);
                s += a.iter().zip(b).map(|(a, b)| a.abs_diff(*b) as u32).sum::<u32>();
                if s >= limit {
                    return s;
                }
            }
            return s;
        }
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
            // The frame before around the best place, one pixel wider on every side, and its
            // three half-way pictures (to the right of a pixel, below it, the middle of four),
            // worked out once: every place tried is one of these or the mean of two.
            let g = |x: i32, y: i32| prev[y.clamp(0, h as i32 - 1) as usize * w + x.clamp(0, w as i32 - 1) as usize] as i32;
            let (x0, y0) = (bx as i32 + best.1 / 4 - 1, by as i32 + best.2 / 4 - 1);
            let (aw, ah) = (bw + 2, bh + 2);
            let clip = |v: i32| v.clamp(0, 255);
            let mut rows = [[0i32; BLOCK + 2]; BLOCK + 8];
            for (j, row) in rows.iter_mut().enumerate().take(ah + 6) {
                let y = y0 + j as i32 - 2;
                for (k, v) in row.iter_mut().enumerate().take(aw) {
                    let x = x0 + k as i32;
                    *v = g(x - 2, y) - 5 * g(x - 1, y) + 20 * g(x, y) + 20 * g(x + 1, y) - 5 * g(x + 2, y) + g(x + 3, y);
                }
            }
            let mut whole = [[0u8; BLOCK + 2]; BLOCK + 2];
            let (mut right, mut below, mut middle) = (whole, whole, whole);
            for j in 0..ah {
                for k in 0..aw {
                    let (x, y) = (x0 + k as i32, y0 + j as i32);
                    whole[j][k] = g(x, y) as u8;
                    right[j][k] = clip((rows[j + 2][k] + 16) >> 5) as u8;
                    below[j][k] = clip((g(x, y - 2) - 5 * g(x, y - 1) + 20 * g(x, y) + 20 * g(x, y + 1) - 5 * g(x, y + 2) + g(x, y + 3) + 16) >> 5) as u8;
                    middle[j][k] = clip((rows[j][k] - 5 * rows[j + 1][k] + 20 * rows[j + 2][k] + 20 * rows[j + 3][k] - 5 * rows[j + 4][k] + rows[j + 5][k] + 512) >> 10) as u8;
                }
            }
            let centre = (best.1, best.2);
            let unlike = |qx: i32, qy: i32, limit: u32| -> u32 {
                // Where the place lies from the best whole place: a pixel of the pictures, and quarters past it.
                let (ox, oy) = (qx - centre.0 + 4, qy - centre.1 + 4);
                let (ix, iy, fx, fy) = ((ox >> 2) as usize, (oy >> 2) as usize, ox & 3, oy & 3);
                let mean = |a: u8, b: u8| (a as i32 + b as i32 + 1) >> 1;
                let mut s = 0u32;
                for j in 0..bh {
                    for k in 0..bw {
                        let (y, x) = (iy + j, ix + k);
                        let there = match (fx, fy) {
                            (0, 0) => whole[y][x] as i32,
                            (1, 0) => mean(whole[y][x], right[y][x]),
                            (2, 0) => right[y][x] as i32,
                            (3, 0) => mean(whole[y][x + 1], right[y][x]),
                            (0, 1) => mean(whole[y][x], below[y][x]),
                            (0, 2) => below[y][x] as i32,
                            (0, 3) => mean(whole[y + 1][x], below[y][x]),
                            (2, 2) => middle[y][x] as i32,
                            (2, 1) => mean(right[y][x], middle[y][x]),
                            (2, 3) => mean(middle[y][x], right[y + 1][x]),
                            (1, 2) => mean(below[y][x], middle[y][x]),
                            (3, 2) => mean(middle[y][x], below[y][x + 1]),
                            (1, 1) => mean(right[y][x], below[y][x]),
                            (3, 1) => mean(right[y][x], below[y][x + 1]),
                            (1, 3) => mean(below[y][x], right[y + 1][x]),
                            _ => mean(below[y][x + 1], right[y + 1][x]),
                        };
                        s += (real[(by + j) * w + bx + k] as i32 - there).unsigned_abs();
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
    best
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
    /// Fixed weights (worked out by the packer for a stretch of film, kept in its first
    /// frame's head) in place of the learning and the weighing, and one learner for each
    /// yes/no in place of three mixed.
    pub gold: bool,
    /// TRIAL: how common the guessed tone is in its channel, as a part of where the sample stands.
    pub tones: bool,
    /// TRIAL, with fixed weights: a plane is told coarse first, then finer (a pyramid).
    pub pyramid: bool,
    /// Pressure plates (--plates N; the weights are fixed for a stretch as with gold). With
    /// gold: the plates are the guess. Without: 1 the plates are the guess in place of the
    /// four guessers, 2 the plates are a fifth guesser.
    pub plates: u8,
}

impl How {
    fn flags(self) -> u8 {
        (if self.light > 0 { LIGHT } else { 0 }) | (if self.two { TWO } else { 0 }) | (if self.quarter { QUARTER } else { 0 }) | (if self.gold { GOLD } else { 0 }) | (if self.tones { TONES } else { 0 }) | (if self.pyramid { PYRAMID } else { 0 })
    }
    fn of(flags: u8) -> How {
        How { light: if flags & LIGHT != 0 { 2 } else { 0 }, two: flags & TWO != 0, quarter: flags & QUARTER != 0, gold: flags & GOLD != 0, tones: flags & TONES != 0, pyramid: flags & PYRAMID != 0, plates: 0 }
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
fn band<C: Bits>(geo: &Geometry, c: &mut C, m: &mut Model, cur: [&mut [u8]; 3], prev: Option<Before>, mut vectors: Vec<(i32, i32)>, y0: usize, y1: usize, how: How, mut gather: Option<&mut Sums>) {
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
    m.plates = how.plates;
    m.pyramid = how.pyramid;
    if how.gold {
        match prev {
            Some(p) => {
                moved(mv, p.planes[0], p.first[0], (w, h), y0, rows, &vectors, across, 4, false);
                if how.light > 0 {
                    relight(c, m, 0, w, rows, light, mv, how.light > 1);
                }
                plane_gold::<C, true>(c, m, 0, w, light, mv, &mut *px, res_light, None, gather.as_deref_mut());
                moved(mv, p.planes[1], p.first[1], (cw, ch), c0, colour_rows, &vectors, across, 3, true);
                if how.light > 0 {
                    relight(c, m, 1, cw, colour_rows, first, mv, how.light > 1);
                }
                plane_gold::<C, true>(c, m, 1, cw, first, mv, &mut *px, res_first, None, gather.as_deref_mut());
                moved(mv, p.planes[2], p.first[2], (cw, ch), c0, colour_rows, &vectors, across, 3, true);
                if how.light > 0 {
                    relight(c, m, 2, cw, colour_rows, second, mv, how.light > 1);
                }
                plane_gold::<C, true>(c, m, 2, cw, second, mv, &mut *px, res_second, Some(res_first), gather.as_deref_mut());
            }
            None => {
                plane_gold::<C, false>(c, m, 0, w, light, &[], &mut *px, res_light, None, gather.as_deref_mut());
                plane_gold::<C, false>(c, m, 1, cw, first, &[], &mut *px, res_first, None, gather.as_deref_mut());
                plane_gold::<C, false>(c, m, 2, cw, second, &[], &mut *px, res_second, Some(res_first), gather.as_deref_mut());
            }
        }
        m.room = room;
        return;
    }
    match prev {
        Some(p) => {
            moved(mv, p.planes[0], p.first[0], (w, h), y0, rows, &vectors, across, 4, false);
            if how.light > 0 {
                relight(c, m, 0, w, rows, light, mv, how.light > 1);
            }
            plane::<C, true>(c, m, 0, w, light, mv, (&mut *px, &mut *ec, &mut *eu), res_light, None, how.two, how.tones);
            moved(mv, p.planes[1], p.first[1], (cw, ch), c0, colour_rows, &vectors, across, 3, true);
            if how.light > 0 {
                relight(c, m, 1, cw, colour_rows, first, mv, how.light > 1);
            }
            plane::<C, true>(c, m, 1, cw, first, mv, (&mut *px, &mut *ec, &mut *eu), res_first, None, how.two, how.tones);
            moved(mv, p.planes[2], p.first[2], (cw, ch), c0, colour_rows, &vectors, across, 3, true);
            if how.light > 0 {
                relight(c, m, 2, cw, colour_rows, second, mv, how.light > 1);
            }
            plane::<C, true>(c, m, 2, cw, second, mv, (&mut *px, &mut *ec, &mut *eu), res_second, Some(res_first), how.two, how.tones);
        }
        None => {
            plane::<C, false>(c, m, 0, w, light, &[], (&mut *px, &mut *ec, &mut *eu), res_light, None, how.two, how.tones);
            plane::<C, false>(c, m, 1, cw, first, &[], (&mut *px, &mut *ec, &mut *eu), res_first, None, how.two, how.tones);
            plane::<C, false>(c, m, 2, cw, second, &[], (&mut *px, &mut *ec, &mut *eu), res_second, Some(res_first), how.two, how.tones);
        }
    }
    m.room = room;
}

const FROM_BEFORE: u8 = 2;
const STILL: u8 = 4;
const LIGHT: u8 = 8;
const TWO: u8 = 16;
const QUARTER: u8 = 32;
const GOLD: u8 = 64;
const TONES: u8 = 128;
const PYRAMID: u8 = 1;
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
    /// For the trials: what the guess missed by on every sample of the frame packed last
    /// (by `pack`, with the same `bands` said), laid as the frame's planes lie.
    pub fn misses(&self, geo: &Geometry, bands: usize) -> Vec<i8> {
        let mut out = vec![0i8; geo.frame_len()];
        let (rows, at) = (band_rows(geo, bands), geo.offsets());
        for (m, y0) in self.0.iter().zip((0..geo.height).step_by(rows)) {
            let y1 = (y0 + rows).min(geo.height);
            let (c0, c1) = colour_rows(geo, y0, y1);
            for (k, (first, n)) in [(y0, y1 - y0), (c0, c1 - c0), (c0, c1 - c0)].into_iter().enumerate() {
                let w = geo.planes[k].w;
                for yy in 0..n {
                    let o = (yy + PAD) * (w + 2 * PAD) + PAD;
                    out[at[k] + (first + yy) * w..][..w].copy_from_slice(&m.room.res[k][o..o + w]);
                }
            }
        }
        out
    }
}

/// Packs a frame exactly. `before`: the frame before it (None: the frame stands alone).
/// The bytes: flags, the number of bands, each band's length, the bands.
#[allow(dead_code)]
pub fn pack(geo: &Geometry, frame: &[u8], before: Option<&[u8]>, bands: usize, memory: &mut Memory, how: How) -> Vec<u8> {
    assert!(!how.gold, "fixed weights are worked out for a stretch of frames: strike_pack");
    assert!(how.plates == 0, "plates are worked out for a stretch of frames: strike_pack");
    let mut out = head_of(if before.is_some() { FROM_BEFORE } else { 0 }, bands, how).to_vec();
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
            let mut c = Writer(Encoder::new(), [0.0; 7], 0);
            band(geo, &mut c, m, [&mut a, &mut b, &mut d], prev, vectors, y0, y1, how, None);
            MISSED.fetch_add(c.2, Ordering::Relaxed);
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
    let rows = band_rows(geo, bands_of(payload));
    let starts: Vec<usize> = (0..geo.height).step_by(rows).collect();
    let table = table_at(payload);
    let gold = if table > HEAD { Some(gold_of(payload.get(HEAD..table).ok_or("a mixed frame is cut")?, how_of(payload))) } else { None };
    let mut data = table + 4 * starts.len();
    let mut coded = Vec::new();
    for i in 0..starts.len() {
        let len = payload.get(table + 4 * i..table + 4 * i + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize).ok_or("a mixed frame is cut")?;
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
            if let Some(g) = gold {
                m.gold = g;
            }
            let mut c = Reader(Decoder::new(bytes));
            band(geo, &mut c, m, [a, b, d], prev, Vec::new(), *y0, y1, how_of(payload), None);
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
    /// Nothing: the frame stands alone.
    Alone,
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
    /// The packer, with fixed weights: the time of finding every vector, and of working out the weights.
    pub map: f64,
    pub solve: f64,
}

impl Struck {
    /// The share of the strike's time the workers were in a job.
    pub fn busy_share(&self) -> f64 {
        self.busy / (self.wall * self.workers as f64).max(1e-9)
    }
    pub fn add(&mut self, o: &Struck) {
        *self = Struck { wall: self.wall + o.wall, busy: self.busy + o.busy, workers: o.workers.max(self.workers), jobs: self.jobs + o.jobs, map: self.map + o.map, solve: self.solve + o.solve };
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
                        let not_yet = if f == 0 || w == Waits::Alone { 0 } else { (lo..=hi).filter(|j| !done[(f - 1) * bands + j]).count() };
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
                    if f + 1 < admitted && waits[f + 1] != Waits::Alone {
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
    let rows = band_rows(geo, bands_of(&first));
    let starts: Vec<usize> = (0..geo.height).step_by(rows).collect();
    let bands = starts.len();
    memory.ready(bands, false);
    // What the bands have learned: a set for each stretch of film in the window (a frame that
    // stands alone starts a stretch, and its jobs wait for nothing).
    let ring = window.max(1) + 1;
    let models: Vec<std::sync::Mutex<Option<Model>>> = std::mem::take(&mut memory.0).into_iter().map(Some).chain((0..(ring - 1) * bands).map(|_| None)).map(std::sync::Mutex::new).collect();
    let stretch: Places<usize> = Places::new(n, || 0);
    let mut stretches = 0usize;
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
        let table = table_at(&p);
        if p.len() < table {
            return Err("a mixed frame is cut".into());
        }
        let mut places = Vec::new();
        if p[0] & STILL == 0 {
            let mut data = table + 4 * bands;
            for i in 0..bands {
                let len = p.get(table + 4 * i..table + 4 * i + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize).ok_or("a mixed frame is cut")?;
                if data + len > p.len() {
                    return Err("a mixed frame is cut".into());
                }
                places.push((data, len));
                data += len;
            }
        }
        let waits = if !from_before(&p) {
            stretches += 1;
            Waits::Alone
        } else if p[0] & STILL == 0 {
            Waits::Around
        } else {
            Waits::Band
        };
        unsafe { *stretch.get_mut(f) = stretches };
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
        let mut slot = models[(unsafe { *stretch.get(f) } % ring) * bands + k].lock().unwrap();
        if p[0] & STILL != 0 {
            let before = prev.ok_or("a still frame was rebuilt without the frame before it")?;
            for (p, to) in cur.into_iter().enumerate() {
                let skip = own[p].0 - near[p].0;
                to.copy_from_slice(&before.planes[p][skip..skip + to.len()]);
            }
            return Ok(());
        }
        if prev.is_none() {
            let mut fresh = Model::new();
            if table_at(p) > HEAD {
                fresh.gold = gold_of(&p[HEAD..table_at(p)], how_of(p));
            }
            *slot = Some(fresh);
        }
        let m = slot.as_mut().ok_or("a mixed frame was rebuilt without the frames before it")?;
        let mut c = Reader(Decoder::new(&p[places[k].0..places[k].0 + places[k].1]));
        band(geo, &mut c, m, cur, prev, Vec::new(), y0, y1, how_of(p), None);
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
    // What the last stretch's bands have learned goes on to the next strike.
    let last = stretches % ring;
    memory.0 = models.into_iter().skip(last * bands).take(bands).filter_map(|m| m.into_inner().unwrap()).collect();
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
    // Fixed weights: every vector is found first (the map), then every fourth frame's sums are
    // added up (and the frames that stand alone), and the weights worked out from them.
    let (mut map_time, mut solve_time) = (0.0, 0.0);
    let mut found: Vec<Vec<(i32, i32)>> = Vec::new();
    let mut gold: Gold = [[0; FEATS]; GOLD_SETS];
    let fixed = how.gold || how.plates > 0;
    if fixed {
        if before.is_some() && !alone {
            return Err("a stretch of fixed weights starts with a frame that stands alone".into());
        }
        let from = std::time::Instant::now();
        found = (0..n * bands)
            .into_par_iter()
            .map(|j| {
                let (f, y0) = (j / bands, starts[j % bands]);
                prev_of(f).map_or(Vec::new(), |p| find(geo.width, geo.height, planes(geo, frames[f])[0], planes(geo, p)[0], y0, (y0 + rows).min(geo.height), how.quarter))
            })
            .collect();
        map_time = from.elapsed().as_secs_f64();
        let from = std::time::Instant::now();
        let sampled: Vec<usize> = (0..n).filter(|f| prev_of(*f).is_none() || f % 4 == 1).flat_map(|f| (0..bands).map(move |k| f * bands + k)).collect();
        let sums: Sums = sampled
            .par_iter()
            .map(|j| {
                let (f, k) = (j / bands, j % bands);
                let (y0, y1) = (starts[k], (starts[k] + rows).min(geo.height));
                let prev = prev_of(f).map(|b| Before { planes: planes(geo, b), first: [0; 3] });
                let [mut a, mut b, mut d] = cut(geo, planes(geo, frames[f]), y0, y1).map(|p| p.to_vec());
                let mut sums: Sums = vec![[0.0; 17 * FEATS]; GOLD_SETS];
                band(geo, &mut Dry, &mut Model::new(), [&mut a, &mut b, &mut d], prev, found[*j].clone(), y0, y1, How { light: 0, gold: true, ..how }, Some(&mut sums));
                sums
            })
            .reduce(
                || vec![[0.0; 17 * FEATS]; GOLD_SETS],
                |mut a, b| {
                    for (x, y) in a.iter_mut().flatten().zip(b.iter().flatten()) {
                        *x += y;
                    }
                    a
                },
            );
        gold = std::array::from_fn(|s| solve(&sums[s]));
        solve_time = from.elapsed().as_secs_f64();
    }
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
        if fixed {
            m.gold = gold;
        }
        let vectors = if fixed { found[f * bands + k].clone() } else { prev.map_or(Vec::new(), |p| find(geo.width, geo.height, real[0], p.planes[0], y0, y1, how.quarter)) };
        let [mut a, mut b, mut d] = cut(geo, real, y0, y1).map(|p| p.to_vec());
        let mut c = Writer(Encoder::new(), [0.0; 7], 0);
        band(geo, &mut c, &mut m, [&mut a, &mut b, &mut d], prev, vectors, y0, y1, how, None);
        MISSED.fetch_add(c.2, Ordering::Relaxed);
        // This job alone writes the place.
        unsafe { *done.get_mut(f * bands + k) = (c.0.finish(), c.1) };
        Ok(())
    };
    let mut struck = strike(n, bands, n, &mut |_| Ok(Waits::Band), &run, &mut |_| Ok(true))?;
    (struck.map, struck.solve) = (map_time, solve_time);
    memory.0 = models.into_iter().map(|m| m.into_inner().unwrap()).collect();
    let mut out = Vec::with_capacity(n);
    for f in 0..n {
        let mut bytes = head_of(if prev_of(f).is_some() { FROM_BEFORE } else { 0 }, bands_said, how).to_vec();
        if fixed && prev_of(f).is_none() {
            bytes.extend(gold_bytes(&gold, how));
        }
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

/// THE FAR CACHE (--past, --ahead; `fbottle far`): a block may be guessed from any of
/// the frames kept, not only the frame before. Which one is told for each block.

/// A frame's light at a quarter of its size each way (the mean of every 4 x 4): by
/// these the packer picks the kept frames worth a real look.
pub fn thumb(geo: &Geometry, frame: &[u8]) -> Vec<u8> {
    let (w, h) = (geo.width, geo.height);
    let mut out = Vec::with_capacity(w.div_ceil(4) * h.div_ceil(4));
    for y in (0..h).step_by(4) {
        for x in (0..w).step_by(4) {
            let (mut sum, mut n) = (0u32, 0u32);
            for j in y..(y + 4).min(h) {
                for v in &frame[j * w + x..j * w + (x + 4).min(w)] {
                    sum += *v as u32;
                    n += 1;
                }
            }
            out.push(((sum + n / 2) / n) as u8);
        }
    }
    out
}

/// For each block of light in rows `y0..y1`: which kept frame it is guessed from (0 is
/// the nearest) and its vector there. The nearest is searched always; of the others the
/// two whose small pictures are most like the block's (and more like it than the
/// nearest's) are searched, and one is taken when it is alike by `eighths` eighths of
/// the nearest's unlikeness or less.
#[allow(clippy::too_many_arguments)]
fn choose(w: usize, h: usize, real: &[u8], refs: &[&[u8]], small: &[u8], smalls: &[&[u8]], y0: usize, y1: usize, quarter: bool, eighths: u32) -> (Vec<u16>, Vec<(i32, i32)>) {
    let (mut which, mut vectors) = (Vec::new(), Vec::new());
    let tw = w.div_ceil(4);
    for by in (y0..y1).step_by(BLOCK) {
        for bx in (0..w).step_by(BLOCK) {
            let (bw, bh) = (BLOCK.min(w - bx), BLOCK.min(h - by));
            let near = find_block(w, h, real, refs[0], bx, by, quarter);
            let mut best = (near.0, 0u16, near.1, near.2);
            if refs.len() > 1 && near.0 > (bw * bh) as u32 {
                let unlike = |r: usize| -> u32 {
                    let mut s = 0;
                    for j in by / 4..(by + bh).div_ceil(4) {
                        let (a, b) = (&small[j * tw + bx / 4..j * tw + (bx + bw).div_ceil(4)], &smalls[r][j * tw + bx / 4..j * tw + (bx + bw).div_ceil(4)]);
                        s += a.iter().zip(b).map(|(a, b)| a.abs_diff(*b) as u32).sum::<u32>();
                    }
                    s
                };
                let nearest = unlike(0);
                let mut top = [(u32::MAX, 0usize); 2];
                for r in 1..refs.len() {
                    let s = unlike(r);
                    if s < nearest && s < top[1].0 {
                        top[1] = (s, r);
                        if top[1].0 < top[0].0 {
                            top.swap(0, 1);
                        }
                    }
                }
                for (s, r) in top {
                    if s != u32::MAX {
                        let f = find_block(w, h, real, refs[r], bx, by, quarter);
                        if 8 * (f.0 + 32) < eighths * near.0 && f.0 < best.0 {
                            best = (f.0, r as u16, f.1, f.2);
                        }
                    }
                }
            }
            which.push(best.1);
            vectors.push((best.2, best.3));
        }
    }
    (which, vectors)
}

/// The kept frames as the band's blocks see them: each block's samples from the frame
/// it is guessed from, where its vector points (as `moved` lays them).
#[allow(clippy::too_many_arguments)]
fn moved_far(mv: &mut Vec<u8>, part: &mut Vec<u8>, refs: &[[&[u8]; 3]], p: usize, used: &[u16], which: &[u16], (w, h): (usize, usize), y0: usize, rows: usize, vectors: &[(i32, i32)], across: usize, shift: usize, halve: bool) {
    if used.len() == 1 {
        return moved(mv, refs[used[0] as usize][p], 0, (w, h), y0, rows, vectors, across, shift, halve);
    }
    let sw = w + 2 * PAD;
    mv.clear();
    mv.resize((rows + PAD + 1) * sw, 0);
    let (block_rows, block_cols) = ((vectors.len() / across).max(1), w.div_ceil(1 << shift).min(across));
    let mut own = Vec::with_capacity(vectors.len());
    for r in used {
        // The blocks of other frames stay where they are here (their samples are not taken).
        own.clear();
        own.extend(vectors.iter().zip(which).map(|(v, x)| if x == r { *v } else { (0, 0) }));
        moved(part, refs[*r as usize][p], 0, (w, h), y0, rows, &own, across, shift, halve);
        for row in -(PAD as i32)..=rows as i32 {
            let br = ((row.clamp(0, rows as i32 - 1) as usize) >> shift).min(block_rows - 1);
            let o = (row + PAD as i32) as usize * sw + PAD;
            for bc in (0..block_cols).filter(|bc| which[br * across + bc] == *r) {
                let (a, b) = (if bc == 0 { o - PAD } else { o + (bc << shift) }, if bc == block_cols - 1 { o + w + PAD } else { o + ((bc + 1) << shift) });
                mv[a..b].copy_from_slice(&part[a..b]);
            }
        }
    }
}

/// One band guessed from the kept frames (`refs`, the nearest first): which frame each
/// block is guessed from (told from the block to its left, when there is more than one
/// frame), its vectors, its light, its two colours; written, or read into `cur`. Gives
/// which frame each block was guessed from.
#[allow(clippy::too_many_arguments)]
fn band_far<C: Bits>(geo: &Geometry, c: &mut C, m: &mut Model, cur: [&mut [u8]; 3], refs: &[[&[u8]; 3]], mut which: Vec<u16>, mut vectors: Vec<(i32, i32)>, y0: usize, y1: usize, how: How) -> Vec<u16> {
    let (w, h) = (geo.width, geo.height);
    let (cw, ch) = (geo.planes[1].w, geo.planes[1].h);
    let (c0, _) = colour_rows(geo, y0, y1);
    let across = w.div_ceil(BLOCK);
    let blocks = across * (y1 - y0).div_ceil(BLOCK);
    which.resize(blocks, 0);
    vectors.resize(blocks, (0, 0));
    if refs.len() > 1 {
        for k in 0..blocks {
            let left = if k % across > 0 { which[k - 1] as i32 } else { 0 };
            which[k] = (left + number(c, &mut m.far[(left > 0) as usize], which[k] as i32 - left)).clamp(0, refs.len() as i32 - 1) as u16;
        }
    }
    let unit = if how.quarter { 1 } else { 4 };
    for k in 0..blocks {
        let from = if k % across > 0 { vectors[k - 1] } else { (0, 0) };
        vectors[k] = (from.0 + unit * number(c, &mut m.vector[0], (vectors[k].0 - from.0) / unit), from.1 + unit * number(c, &mut m.vector[1], (vectors[k].1 - from.1) / unit));
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
    (m.plates, m.pyramid) = (0, false);
    let mut used = which.clone();
    used.sort_unstable();
    used.dedup();
    let mut part = Vec::new();
    moved_far(mv, &mut part, refs, 0, &used, &which, (w, h), y0, rows, &vectors, across, 4, false);
    plane::<C, true>(c, m, 0, w, light, mv, (&mut *px, &mut *ec, &mut *eu), res_light, None, how.two, how.tones);
    moved_far(mv, &mut part, refs, 1, &used, &which, (cw, ch), c0, colour_rows, &vectors, across, 3, true);
    plane::<C, true>(c, m, 1, cw, first, mv, (&mut *px, &mut *ec, &mut *eu), res_first, None, how.two, how.tones);
    moved_far(mv, &mut part, refs, 2, &used, &which, (cw, ch), c0, colour_rows, &vectors, across, 3, true);
    plane::<C, true>(c, m, 2, cw, second, mv, (&mut *px, &mut *ec, &mut *eu), res_second, Some(res_first), how.two, how.tones);
    m.room = room;
    which
}

/// Packs a frame exactly from the kept frames `refs` (the nearest first; none: the frame
/// stands alone), whose small pictures are `smalls` (`small`: the frame's own). With one
/// kept frame the bytes are `pack`'s. Gives the bytes, and which kept frame each block
/// was guessed from (nothing for a frame that stands alone or is the nearest over again).
#[allow(clippy::too_many_arguments)]
pub fn pack_far(geo: &Geometry, frame: &[u8], refs: &[&[u8]], small: &[u8], smalls: &[&[u8]], bands: usize, memory: &mut Memory, how: How) -> (Vec<u8>, Vec<u16>) {
    assert!(!how.gold && how.plates == 0 && how.light == 0, "the far cache goes with the learning mix, without the light map");
    if refs.is_empty() {
        return (pack(geo, frame, None, bands, memory, how), Vec::new());
    }
    let mut out = head_of(FROM_BEFORE, bands, how).to_vec();
    if refs[0] == frame {
        out[0] |= STILL;
        return (out, Vec::new());
    }
    let real = planes(geo, frame);
    let far: Vec<[&[u8]; 3]> = refs.iter().map(|r| planes(geo, r)).collect();
    let lights: Vec<&[u8]> = far.iter().map(|p| p[0]).collect();
    let rows = band_rows(geo, bands);
    let starts: Vec<usize> = (0..geo.height).step_by(rows).collect();
    memory.ready(starts.len(), false);
    let eighths: u32 = std::env::var("FB_FAR_EIGHTHS").ok().and_then(|v| v.parse().ok()).unwrap_or(7);
    let done: Vec<(Vec<u8>, Vec<u16>)> = starts
        .par_iter()
        .zip(memory.0.par_iter_mut())
        .map(|(y0, m)| {
            let y0 = *y0;
            let y1 = (y0 + rows).min(geo.height);
            let (which, vectors) = choose(geo.width, geo.height, real[0], &lights, small, smalls, y0, y1, how.quarter, eighths);
            let [mut a, mut b, mut d] = cut(geo, real, y0, y1).map(|p| p.to_vec());
            let mut c = Writer(Encoder::new(), [0.0; 7], 0);
            let which = band_far(geo, &mut c, m, [&mut a, &mut b, &mut d], &far, which, vectors, y0, y1, how);
            MISSED.fetch_add(c.2, Ordering::Relaxed);
            (c.0.finish(), which)
        })
        .collect();
    for (bytes, _) in &done {
        out.extend((bytes.len() as u32).to_le_bytes());
    }
    for (bytes, _) in &done {
        out.extend(bytes);
    }
    (out, done.into_iter().flat_map(|d| d.1).collect())
}

/// The player for `pack_far`: the frame, exactly, from the same kept frames.
pub fn unpack_far(geo: &Geometry, payload: &[u8], refs: &[&[u8]], memory: &mut Memory) -> Result<Vec<u8>, String> {
    if !from_before(payload) {
        return unpack(geo, payload, None, memory);
    }
    if payload.len() < HEAD || !crate::block::fits(geo) || refs.is_empty() || refs.iter().any(|r| r.len() != geo.frame_len()) {
        return Err("a mixed frame was rebuilt without the frames it is guessed from".into());
    }
    if payload[0] & STILL != 0 {
        return Ok(refs[0].to_vec());
    }
    let rows = band_rows(geo, bands_of(payload));
    let starts: Vec<usize> = (0..geo.height).step_by(rows).collect();
    let mut data = HEAD + 4 * starts.len();
    let mut coded = Vec::new();
    for i in 0..starts.len() {
        let len = payload.get(HEAD + 4 * i..HEAD + 4 * i + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize).ok_or("a mixed frame is cut")?;
        coded.push(payload.get(data..data + len).ok_or("a mixed frame is cut")?);
        data += len;
    }
    let far: Vec<[&[u8]; 3]> = refs.iter().map(|r| planes(geo, r)).collect();
    let cw = geo.planes[1].w;
    memory.ready(starts.len(), false);
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
            band_far(geo, &mut c, m, [a, b, d], &far, Vec::new(), Vec::new(), *y0, y1, how_of(payload));
            cur
        })
        .collect();
    let mut out = Vec::with_capacity(geo.frame_len());
    for p in 0..3 {
        for b in &made {
            out.extend(&b[p]);
        }
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
            for (i, v) in b.iter_mut().enumerate().take(w * h / 2) {
                if i % w < w / 2 {
                    *v = v.saturating_add(9);
                }
            }
            let wild = noise(false);
            for (bands, how) in [(1, How::default()), (3, How::default()), (16, How::default()), (3, How { light: 2, two: true, quarter: true, gold: false, tones: false, pyramid: false, plates: 0 }), (16, How { light: 2, two: false, quarter: false, gold: false, tones: false, pyramid: false, plates: 0 }), (1, How { light: 0, two: false, quarter: true, gold: false, tones: false, pyramid: false, plates: 0 })] {
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
                // The far cache: with one frame kept the bytes are the packer's; with several, every frame comes back.
                if how.light == 0 {
                    let (mut far_packer, mut far_player) = (Memory::new(), Memory::new());
                    let (alone, _) = pack_far(&geo, &a, &[], &thumb(&geo, &a), &[], bands, &mut far_packer, how);
                    assert_eq!(alone, first);
                    assert_eq!(pack_far(&geo, &b, &[&a], &thumb(&geo, &b), &[&thumb(&geo, &a)], bands, &mut far_packer, how).0, second, "{w}x{h}, {bands} bands: one frame kept packs as the packer does");
                    assert_eq!(unpack_far(&geo, &alone, &[], &mut far_player).unwrap(), a);
                    assert_eq!(unpack_far(&geo, &second, &[&a], &mut far_player).unwrap(), b);
                    // A frame made of pieces of three others: the pieces are found in the frames kept.
                    let mut pieces = b.clone();
                    for (i, v) in pieces.iter_mut().enumerate() {
                        let x = (i % w) * 3 / w;
                        *v = [a[i], wild[i], b[i]][x];
                    }
                    let kept: [&[u8]; 3] = [&b, &wild, &a];
                    let smalls: Vec<Vec<u8>> = kept.iter().map(|f| thumb(&geo, f)).collect();
                    let (packed, which) = pack_far(&geo, &pieces, &kept, &thumb(&geo, &pieces), &smalls.iter().map(|s| &s[..]).collect::<Vec<_>>(), bands, &mut far_packer, how);
                    assert!(w < 64 || which.iter().any(|r| *r > 0), "{w}x{h}, {bands} bands: no block was guessed from a frame further off");
                    assert_eq!(unpack_far(&geo, &packed, &kept, &mut far_player).unwrap(), pieces, "{w}x{h}, {bands} bands: the far cache");
                    assert_eq!(unpack_far(&geo, &pack_far(&geo, &b, &kept, &thumb(&geo, &b), &smalls.iter().map(|s| &s[..]).collect::<Vec<_>>(), bands, &mut far_packer, how).0, &kept, &mut far_player).unwrap(), b);
                }
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
                // Fixed weights: two stretches, each starting with a frame that stands alone.
                let gold = How { gold: true, ..how };
                let refs: Vec<&[u8]> = film.iter().map(|f| &f[..]).collect();
                let mut packed = strike_pack(&geo, &refs[..3], None, false, bands, &mut Memory::new(), gold).unwrap().0;
                packed.extend(strike_pack(&geo, &refs[3..], None, false, bands, &mut Memory::new(), gold).unwrap().0);
                for window in [1, 3, 8] {
                    let mut back = Vec::new();
                    strike_unpack(&geo, film.len(), window, None, &mut Memory::new(), &mut |f| Ok(packed[f].clone()), &|_, _| Ok(()), &mut |_, frame| {
                        back.push(frame);
                        Ok(true)
                    })
                    .unwrap();
                    assert_eq!(back, film, "{w}x{h}, {bands} bands, fixed weights, a window of {window}");
                }
                let (mut player, mut last) = (Memory::new(), None::<Vec<u8>>);
                for (f, p) in packed.iter().enumerate() {
                    last = Some(unpack(&geo, p, last.as_deref(), &mut player).unwrap());
                    assert_eq!(last.as_ref(), Some(&film[f]), "{w}x{h}, {bands} bands, fixed weights, frame by frame");
                }
            }
        }
    }
}
