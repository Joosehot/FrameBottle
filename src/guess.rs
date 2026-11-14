//! GUESS and CORRECT: a frame is guessed, and only where the guess was wrong
//! is anything kept.
//!
//! The guess starts from as little as there is: the frame before (all of
//! it, already rebuilt) and the pixels of this frame already rebuilt, to
//! the left and above. Three guessers offer a value for each sample:
//!
//! - `still`: the same sample of the frame before;
//! - `moved`: the frame before, changed as the pixels around changed
//!   (frame before + the median way the left, upper and upper-left
//!   neighbours differ from theirs);
//! - `near`: from this frame alone, the median of left, above and their
//!   slope (for the first frame there is nothing else);
//! - `golden`: the frame before, made lighter or darker by one exact
//!   fraction per plane. Nuome's Golden Answer derives the fraction (the
//!   light of this plane over the light of the same plane before, in its
//!   logical kind) when the frame is packed, and the fraction is kept in
//!   the bottle, so the player needs no Nuome to make the same guess;
//! - `hammer`: the Hammer's stack. Every frame so far lies on the others,
//!   so each pixel is one line of values through time, the top frame
//!   first. The stack holds, for each pixel, where its line has settled
//!   (each new frame moves it a quarter of the way to its value). A pixel
//!   that stays what it was is guessed right from its line, and the grain
//!   of a single frame does not shake the guess as it shakes `still`.
//!
//! The one that was least wrong on the neighbours just rebuilt makes the
//! guess (HotMotor's way: variants, and a judge that scores them where they
//! stand). The correction is what the real sample differs by. A frame whose
//! every correction is zero keeps only a flag. Corrections are written with
//! an adaptive range coder that learns, per context (how large the
//! corrections nearby were, which guesser spoke), how likely a zero, a sign
//! and a size are: a right guess costs a hundredth of a bit.
//!
//! The player makes the same guesses from the same rebuilt pixels and adds
//! the corrections, so every sample comes back exactly. A plane is cut
//! into bands of rows that are guessed side by side on all cores; a band
//! reads the frame before freely, and of this frame only itself.

use crate::motor::Rules;
use crate::pack::{Geometry, Plane};
use rayon::prelude::*;

/// The guesser as it is now. A change to how it guesses or codes changes this, so that
/// the lightning does not strike a pack made by an older guesser.
pub const VERSION: &str = "guess 21";

const TOP: u32 = 1 << 24;
const BITS: u32 = 12;
const HALF: u16 = 1 << (BITS - 1);
const MOVE: u32 = 5;

pub(crate) struct Encoder {
    low: u64,
    range: u32,
    cache: u8,
    pending: u64,
    out: Vec<u8>,
}

impl Encoder {
    pub(crate) fn new() -> Encoder {
        Encoder { low: 0, range: u32::MAX, cache: 0, pending: 1, out: Vec::new() }
    }
    fn shift(&mut self) {
        if (self.low as u32) < 0xFF00_0000 || (self.low >> 32) != 0 {
            let carry = (self.low >> 32) as u8;
            let mut byte = self.cache;
            loop {
                self.out.push(byte.wrapping_add(carry));
                byte = 0xFF;
                self.pending -= 1;
                if self.pending == 0 {
                    break;
                }
            }
            self.cache = (self.low >> 24) as u8;
        }
        self.pending += 1;
        self.low = (self.low & 0x00FF_FFFF) << 8;
    }
    #[inline(always)]
    pub(crate) fn bit(&mut self, p: &mut u16, bit: bool) {
        let bound = (self.range >> BITS) * *p as u32;
        if !bit {
            self.range = bound;
            *p += (((1u32 << BITS) - *p as u32) >> MOVE) as u16;
        } else {
            self.low += bound as u64;
            self.range -= bound;
            *p -= *p >> MOVE;
        }
        while self.range < TOP {
            self.range <<= 8;
            self.shift();
        }
    }
    pub(crate) fn finish(mut self) -> Vec<u8> {
        for _ in 0..5 {
            self.shift();
        }
        self.out
    }
}

pub(crate) struct Decoder<'a> {
    data: &'a [u8],
    at: usize,
    code: u32,
    range: u32,
}

impl<'a> Decoder<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Decoder<'a> {
        let mut d = Decoder { data, at: 1, code: 0, range: u32::MAX };
        for _ in 0..4 {
            d.code = (d.code << 8) | d.next() as u32;
        }
        d
    }
    #[inline(always)]
    fn next(&mut self) -> u8 {
        let b = self.data.get(self.at).copied().unwrap_or(0);
        self.at += 1;
        b
    }
    #[inline(always)]
    pub(crate) fn bit(&mut self, p: &mut u16) -> bool {
        let bound = (self.range >> BITS) * *p as u32;
        let bit = if self.code < bound {
            self.range = bound;
            *p += (((1u32 << BITS) - *p as u32) >> MOVE) as u16;
            false
        } else {
            self.code -= bound;
            self.range -= bound;
            *p -= *p >> MOVE;
            true
        };
        while self.range < TOP {
            self.range <<= 8;
            self.code = (self.code << 8) | self.next() as u32;
        }
        bit
    }
}

/// How large the corrections nearby were, in 9 steps.
const STEPS: usize = 9;
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

/// What the coder has learned so far in one band.
struct Learned {
    zero: [u16; STEPS * 5 * 3 * 2],
    sign: [u16; 9],
    length: [u16; STEPS * 8],
    digits: [u16; 9 * 8],
    /// A tile's flag, by how many of the tiles to its left and above were guessed perfectly.
    tile: [u16; 3],
}

impl Learned {
    fn new() -> Learned {
        Learned { zero: [HALF; STEPS * 5 * 3 * 2], sign: [HALF; 9], length: [HALF; STEPS * 8], digits: [HALF; 9 * 8], tile: [HALF; 3] }
    }
}

/// Where a sample stands for the coder: the step of the corrections nearby,
/// which guesser spoke, how many of the two neighbours needed none, and
/// their signs.
struct Place {
    step: usize,
    who: usize,
    quiet: usize,
    /// How long the Hammer says this sample has stood still, for 8 frames or more (1) or not (0).
    still: usize,
    signs: usize,
}

fn write(e: &mut Encoder, l: &mut Learned, at: &Place, v: i32) {
    e.bit(&mut l.zero[((at.step * 5 + at.who) * 3 + at.quiet) * 2 + at.still], v != 0);
    if v == 0 {
        return;
    }
    e.bit(&mut l.sign[at.signs], v < 0);
    // The size, 1..=128: how many binary digits it has, then the digits under the first.
    let n = v.unsigned_abs();
    let digits = 32 - n.leading_zeros() as usize;
    for i in 1..digits {
        e.bit(&mut l.length[at.step * 8 + i - 1], true);
    }
    if digits < 8 {
        e.bit(&mut l.length[at.step * 8 + digits - 1], false);
    }
    for j in (0..digits - 1).rev() {
        e.bit(&mut l.digits[digits * 8 + j], (n >> j) & 1 == 1);
    }
}

fn read(d: &mut Decoder, l: &mut Learned, at: &Place) -> i32 {
    if !d.bit(&mut l.zero[((at.step * 5 + at.who) * 3 + at.quiet) * 2 + at.still]) {
        return 0;
    }
    let negative = d.bit(&mut l.sign[at.signs]);
    let mut digits = 1;
    while digits < 8 && d.bit(&mut l.length[at.step * 8 + digits - 1]) {
        digits += 1;
    }
    let mut n = 1u32;
    for j in (0..digits - 1).rev() {
        n = (n << 1) | d.bit(&mut l.digits[digits * 8 + j]) as u32;
    }
    if negative { -(n as i32) } else { n as i32 }
}

#[inline(always)]
fn median(a: i32, b: i32, c: i32) -> i32 {
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    if c >= hi { lo } else if c <= lo { hi } else { a + b - c }
}

/// The rows a band has rebuilt, and how wrong each guesser was on them.
struct Band<'a> {
    px: usize,
    /// This row and the one above, as rebuilt.
    row: Vec<u8>,
    up: Vec<u8>,
    has_up: bool,
    /// The corrections of this row and the one above.
    fix: Vec<i8>,
    fix_up: Vec<i8>,
    /// How wrong each guesser (still, moved, near, golden) was, this row and above.
    wrong: [Vec<u8>; 5],
    wrong_up: [Vec<u8>; 5],
    /// The same row of the Hammer's stack, and of how long each sample has stood still.
    stack: Option<&'a [u8]>,
    still: Option<&'a [u8]>,
    /// Golden's guess for each value a sample had in the frame before; None when it says "the same".
    golden: Option<[u8; 256]>,
    /// The same row of the frame before, and the row above it there.
    before: Option<&'a [u8]>,
    before_up: Option<&'a [u8]>,
    offers: [u8; 5],
}

impl<'a> Band<'a> {
    fn new(p: &Plane, golden: Option<[u8; 256]>) -> Band<'a> {
        let n = p.stride();
        Band {
            px: p.px(),
            row: vec![0; n],
            up: vec![0; n],
            has_up: false,
            fix: vec![0; n],
            fix_up: vec![0; n],
            wrong: [vec![0; n], vec![0; n], vec![0; n], vec![0; n], vec![0; n]],
            wrong_up: [vec![0; n], vec![0; n], vec![0; n], vec![0; n], vec![0; n]],
            stack: None,
            still: None,
            golden,
            before: None,
            before_up: None,
            offers: [0; 5],
        }
    }

    /// The guess for sample `i` of this row, and where it stands for the coder.
    #[inline(always)]
    fn guess(&mut self, i: usize) -> (u8, Place) {
        let (px, left, up) = (self.px, i >= self.px, self.has_up);
        let a = if left {
            self.row[i - px] as i32
        } else if up {
            self.up[i] as i32
        } else {
            self.before.map_or(128, |p| p[i] as i32)
        };
        let near = if left && up {
            median(a, self.up[i] as i32, self.up[i - px] as i32)
        } else if up {
            self.up[i] as i32
        } else {
            a
        };
        let mut who = 2;
        let mixed;
        if let Some(p) = self.before {
            let still = p[i] as i32;
            let change = match (left, up.then_some(self.before_up).flatten()) {
                (true, Some(q)) => median(a - p[i - px] as i32, self.up[i] as i32 - q[i] as i32, self.up[i - px] as i32 - q[i - px] as i32),
                (true, None) => a - p[i - px] as i32,
                (false, Some(q)) => self.up[i] as i32 - q[i] as i32,
                (false, None) => 0,
            };
            self.offers = [still as u8, (still + change).clamp(0, 255) as u8, near as u8, self.golden.map_or(still as u8, |g| g[still as usize]), self.stack.map_or(still as u8, |h| h[i])];
            // The judge: how wrong each was on the sample to the left and the one above.
            let right = i + px < self.row.len();
            let score = |g: usize| {
                (if left { 2 * self.wrong[g][i - px] as u32 } else { 0 })
                    + (if up { 2 * self.wrong_up[g][i] as u32 } else { 0 })
                    + (if up && left { self.wrong_up[g][i - px] as u32 } else { 0 })
                    + (if up && right { self.wrong_up[g][i + px] as u32 } else { 0 })
            };
            // Golden and the Hammer speak only where they say something else than `still`
            // (else `still` would be heard twice or three times).
            let speaks = [true, true, true, self.golden.is_some() && self.offers[3] != self.offers[0], self.stack.is_some() && self.offers[4] != self.offers[0]];
            let mut best = u32::MAX;
            let (mut sum, mut total) = (0u32, 0u32);
            for g in 0..5 {
                if !speaks[g] {
                    continue;
                }
                let s = score(g);
                if s < best {
                    best = s;
                    who = g;
                }
                // Each speaks as loud as it was right: the guess is their mix.
                let w = ((1u32 << 20) / ((s + 2) * (s + 2))).max(1);
                sum += w;
                total += self.offers[g] as u32 * w;
            }
            mixed = ((total + sum / 2) / sum) as u8;
        } else {
            self.offers = [near as u8; 5];
            mixed = near as u8;
        }
        let (fl, fu) = (if left { self.fix[i - px] as i32 } else { 0 }, if up { self.fix_up[i] as i32 } else { 0 });
        let ful = if up && left { self.fix_up[i - px] as i32 } else { 0 };
        let fur = if up && i + px < self.row.len() { self.fix_up[i + px] as i32 } else { 0 };
        let place = Place {
            step: step((2 * (fl.unsigned_abs() + fu.unsigned_abs()) + ful.unsigned_abs() + fur.unsigned_abs() + 1) / 3),
            who,
            quiet: (fl == 0) as usize + (fu == 0) as usize,
            still: self.still.map_or(0, |a| (a[i] >= 8) as usize),
            signs: ((fl.signum() + 1) * 3 + fu.signum() + 1) as usize,
        };
        (mixed, place)
    }

    /// The real sample is known: remember it, the correction, and how wrong each guesser was.
    #[inline(always)]
    fn learn(&mut self, i: usize, real: u8, fix: i8) {
        self.row[i] = real;
        self.fix[i] = fix;
        for g in 0..5 {
            self.wrong[g][i] = real.abs_diff(self.offers[g]);
        }
    }

    fn next_row(&mut self) {
        std::mem::swap(&mut self.row, &mut self.up);
        std::mem::swap(&mut self.fix, &mut self.fix_up);
        std::mem::swap(&mut self.wrong, &mut self.wrong_up);
        self.has_up = true;
    }

    fn at_row(&mut self, before: Option<&'a [u8]>, y: usize, stride: usize) {
        self.before = before.map(|b| &b[y * stride..(y + 1) * stride]);
        self.before_up = before.filter(|_| y > 0).map(|b| &b[(y - 1) * stride..y * stride]);
    }
}

/// The Hammer: every frame so far, one on the other, so that each pixel is
/// a line of values through time. It starts at the top frame and takes the
/// next and the next until the last. For each pixel the stack keeps where
/// its line has settled; a new frame moves it a quarter of the way to the
/// pixel's new value. The player strikes the same frames in the same order,
/// so its stack is the same.
///
/// The stack also keeps, for each pixel, how many frames in a row it has been exactly
/// what it was (up to 255): a pixel that has stood still for long mostly stands still
/// again, and the coder is told so.
pub struct Hammer {
    /// Where each line has settled, in 64ths of a value.
    stack: Vec<u16>,
    /// The same to the nearest whole value (the Hammer's guess for the next frame), and
    /// after it how long each pixel has stood still.
    flat: Vec<u8>,
    /// The frame on top.
    last: Vec<u8>,
    frames: usize,
}

impl Hammer {
    pub fn new() -> Hammer {
        Hammer { stack: Vec::new(), flat: Vec::new(), last: Vec::new(), frames: 0 }
    }
    /// The next frame is laid on the stack.
    pub fn strike(&mut self, frame: &[u8]) {
        if self.frames == 0 || self.stack.len() != frame.len() {
            self.stack = frame.iter().map(|v| (*v as u16) << 6).collect();
            self.flat = frame.to_vec();
            self.flat.resize(2 * frame.len(), 0);
            self.last = frame.to_vec();
        } else {
            let (flat, still) = self.flat.split_at_mut(frame.len());
            for ((((h, f), age), was), v) in self.stack.iter_mut().zip(flat).zip(still).zip(self.last.iter_mut()).zip(frame) {
                *h = (*h as i32 + ((((*v as i32) << 6) - *h as i32 + 2) >> 2)) as u16;
                *f = ((*h + 32) >> 6) as u8;
                *age = if v == was { age.saturating_add(1) } else { 0 };
                *was = *v;
            }
        }
        self.frames += 1;
    }
    /// What the stack says about the next frame: its guess for every sample, and after
    /// that how long every sample has stood still (twice a frame long). None before the
    /// top frame.
    pub fn guess(&self) -> Option<&[u8]> {
        (self.frames > 0).then_some(&self.flat[..])
    }
    /// Only the stack's picture (a frame long).
    pub fn picture(&self) -> Option<&[u8]> {
        (self.frames > 0).then(|| &self.flat[..self.flat.len() / 2])
    }
}

/// MEND: what a tolerance lost, put right as far as it can be without one more byte.
///
/// A frame rebuilt with a tolerance has every sample within `tol` of the real one, so
/// the real one lies between `v - tol` and `v + tol`. Which value in that span it was
/// is not kept, but the samples around it say where it probably was: those of the same
/// surface (within 2 tol of it) missed to both sides, and their mean is nearer the real
/// surface than any one of them. The mended sample is that mean, held inside the span.
/// The player does this to what it shows; the guesses are still made from the frames
/// as rebuilt, so every player rebuilds the same. (The Hammer's stack was tried here
/// too and made it worse: it is made of rebuilt frames, whose misses lean the same way.)
pub fn mend(geo: &Geometry, frame: &[u8], tol: u8) -> Vec<u8> {
    let mut out = frame.to_vec();
    if tol == 0 {
        return out;
    }
    let (t, reach) = (tol as i32, 2 * tol as i32);
    let mut rest = &mut out[..];
    for (p, at) in geo.planes.iter().zip(geo.offsets()) {
        let (plane, r) = rest.split_at_mut(p.len());
        rest = r;
        if p.bps != 1 {
            continue;
        }
        let (src, stride, px) = (&frame[at..at + p.len()], p.stride(), p.px());
        plane.par_chunks_mut(stride).enumerate().for_each(|(y, row)| {
            for (i, o) in row.iter_mut().enumerate() {
                let v = src[y * stride + i] as i32;
                let (mut sum, mut n) = (2 * v, 2);
                for yy in y.saturating_sub(1)..(y + 2).min(p.h) {
                    for k in [-1i32, 0, 1] {
                        let ii = i as i32 + k * px as i32;
                        if ii < 0 || ii >= stride as i32 || (yy == y && k == 0) {
                            continue;
                        }
                        let u = src[yy * stride + ii as usize] as i32;
                        if (u - v).abs() <= reach {
                            sum += u;
                            n += 1;
                        }
                    }
                }
                *o = ((sum + n / 2) / n).clamp(v - t, v + t).clamp(0, 255) as u8;
            }
        });
    }
    out
}

/// One plane of a frame, if there is a frame.
fn plane_of(frame: Option<&[u8]>, at: usize, len: usize) -> Option<&[u8]> {
    frame.map(|f| &f[at..at + len])
}

/// A frame is cut into tiles of 16 x 16 pixels (8 x 8 where a plane is halved).
const TILE: usize = 16;

/// A tile's width in bytes and height in rows, in this plane.
fn tile(p: &Plane) -> (usize, usize) {
    (((TILE >> p.sx).max(1)) * p.px(), (TILE >> p.sy).max(1))
}

/// The corrections of rows `y0..y1` of a plane, coded. Each row of tiles
/// starts with one flag per tile: guessed perfectly, or not. Only the tiles
/// that were not have corrections after it. Returns the bytes, the tiles
/// guessed perfectly, and the tiles.
///
/// With a tolerance, a guess that is wrong by no more than it is left as it is, and a
/// larger miss is corrected to within it; the band as the player will rebuild it is
/// returned too (empty without a tolerance: then it is the real band).
#[allow(clippy::too_many_arguments)]
fn correct(p: &Plane, real: &[u8], before: Option<&[u8]>, hammer: Option<&[u8]>, still: Option<&[u8]>, golden: Option<[u8; 256]>, tol: u8, y0: usize, y1: usize) -> (Vec<u8>, usize, usize, Vec<u8>) {
    let stride = p.stride();
    let (tw, th) = tile(p);
    let across = stride.div_ceil(tw);
    // First the guesses, which need only what the player will have too.
    let mut band = Band::new(p, golden);
    let mut fixes: Vec<i8> = Vec::with_capacity((y1 - y0) * stride);
    let mut places: Vec<Place> = Vec::with_capacity((y1 - y0) * stride);
    let mut rebuilt: Vec<u8> = Vec::new();
    for y in y0..y1 {
        band.at_row(before, y, stride);
        band.stack = hammer.map(|h| &h[y * stride..(y + 1) * stride]);
        band.still = still.map(|h| &h[y * stride..(y + 1) * stride]);
        let row = &real[y * stride..(y + 1) * stride];
        for (i, x) in row.iter().enumerate() {
            let (g, place) = band.guess(i);
            let (fix, value) = within(tol, g, *x);
            fixes.push(fix);
            places.push(place);
            // The guesses that follow are made from what the player will have, not from the real sample.
            band.learn(i, value, fix);
            if tol > 0 {
                rebuilt.push(value);
            }
        }
        band.next_row();
    }
    // Then the flags and the corrections, a row of tiles at a time.
    let (mut e, mut l) = (Encoder::new(), Learned::new());
    let mut above = vec![false; across];
    let (mut perfect, mut tiles) = (0, 0);
    for top in (0..y1 - y0).step_by(th) {
        let rows = top..(top + th).min(y1 - y0);
        let flags: Vec<bool> = (0..across).map(|t| rows.clone().all(|y| fixes[y * stride + t * tw..y * stride + ((t + 1) * tw).min(stride)].iter().all(|f| *f == 0))).collect();
        for t in 0..across {
            e.bit(&mut l.tile[above[t] as usize + (t > 0 && flags[t - 1]) as usize], flags[t]);
        }
        perfect += flags.iter().filter(|f| **f).count();
        tiles += across;
        for y in rows {
            for i in 0..stride {
                if !flags[i / tw] {
                    write(&mut e, &mut l, &places[y * stride + i], fixes[y * stride + i] as i32);
                }
            }
        }
        above = flags;
    }
    (e.finish(), perfect, tiles, rebuilt)
}

/// The correction of a guess `g` for the real sample `x`, and the sample as it is
/// rebuilt. Without a tolerance the correction is the whole difference (it wraps
/// round, as a byte does) and the sample comes back exactly. With one, the
/// difference is counted in steps of 2 tol + 1: a miss of tol or less is no step.
#[inline(always)]
fn within(tol: u8, g: u8, x: u8) -> (i8, u8) {
    if tol == 0 {
        return (x.wrapping_sub(g) as i8, x);
    }
    let (t, s) = (tol as i32, 2 * tol as i32 + 1);
    let miss = x as i32 - g as i32;
    let steps = if miss >= 0 { (miss + t) / s } else { -((-miss + t) / s) };
    (steps as i8, (g as i32 + steps * s).clamp(0, 255) as u8)
}

/// The sample a correction rebuilds from the guess.
#[inline(always)]
fn apply(tol: u8, g: u8, fix: i8) -> u8 {
    if tol == 0 { g.wrapping_add(fix as u8) } else { (g as i32 + fix as i32 * (2 * tol as i32 + 1)).clamp(0, 255) as u8 }
}

/// Rows `y0..y1` of a plane rebuilt into `out` (those rows only): the same
/// guesses, and the corrections added where a tile's flag says it has any.
/// `coded` None means no tile needed one.
#[allow(clippy::too_many_arguments)]
fn rebuild(p: &Plane, coded: Option<&[u8]>, before: Option<&[u8]>, hammer: Option<&[u8]>, still: Option<&[u8]>, golden: Option<[u8; 256]>, tol: u8, y0: usize, y1: usize, out: &mut [u8]) {
    let stride = p.stride();
    let (tw, th) = tile(p);
    let across = stride.div_ceil(tw);
    let (mut band, mut l) = (Band::new(p, golden), Learned::new());
    let mut d = coded.map(Decoder::new);
    // Without corrections every tile was guessed perfectly; with them, nothing is above the first row.
    let mut flags = vec![d.is_none(); across];
    for y in y0..y1 {
        if (y - y0) % th == 0 {
            if let Some(d) = d.as_mut() {
                let above = std::mem::replace(&mut flags, vec![false; across]);
                for t in 0..across {
                    flags[t] = d.bit(&mut l.tile[above[t] as usize + (t > 0 && flags[t - 1]) as usize]);
                }
            }
        }
        band.at_row(before, y, stride);
        band.stack = hammer.map(|h| &h[y * stride..(y + 1) * stride]);
        band.still = still.map(|h| &h[y * stride..(y + 1) * stride]);
        let row = &mut out[(y - y0) * stride..(y - y0 + 1) * stride];
        for (i, x) in row.iter_mut().enumerate() {
            let (g, place) = band.guess(i);
            let fix = if flags[i / tw] { 0 } else { read(d.as_mut().unwrap(), &mut l, &place) as i8 };
            *x = apply(tol, g, fix);
            band.learn(i, *x, fix);
        }
        band.next_row();
    }
}

/// The rows of a band: a whole number of rows of tiles.
fn band_rows(p: &Plane, bands: usize) -> usize {
    let th = tile(p).1;
    p.h.div_ceil(bands.max(1)).max(1).div_ceil(th) * th
}

const PERFECT: u8 = 1;
const FROM_BEFORE: u8 = 2;
/// The whole frame is the frame before it: the `still` guesser alone was right everywhere.
const STILL: u8 = 4;

/// What a guessed frame starts with: flags, the number of bands, the tiles guessed
/// perfectly, the tiles, the tolerance.
const HEAD: usize = 12;

/// (tiles guessed perfectly, tiles) of a guessed frame.
pub fn tiles(payload: &[u8]) -> (usize, usize) {
    let n = |at: usize| payload.get(at..at + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()) as usize);
    (n(3), n(7))
}

/// Golden's guess for a plane: each value of the frame before times num / den, to the
/// nearest whole value. None when the fraction is 1 (Golden says "the same").
fn golden_table(gain: (u32, u32)) -> Option<[u8; 256]> {
    if gain.0 == gain.1 || gain.1 == 0 {
        return None;
    }
    let mut t = [0u8; 256];
    for (v, slot) in t.iter_mut().enumerate() {
        *slot = ((v as u64 * gain.0 as u64 + gain.1 as u64 / 2) / gain.1 as u64).min(255) as u8;
    }
    Some(t)
}

/// Golden Answer is asked how much lighter or darker each plane is than the same plane
/// of the frame before: an exact fraction, derived (logical kind). A plane it cannot
/// answer for (no Nuome here, a black plane, samples of two bytes) gets 1.
fn ask_golden(geo: &Geometry, frame: &[u8], before: &[u8]) -> Vec<(u32, u32)> {
    let at = geo.offsets();
    geo.planes
        .par_iter()
        .zip(at)
        .map(|(p, at)| {
            if p.bps != 1 {
                return (1, 1);
            }
            let sum = |f: &[u8]| f[at..at + p.len()].iter().map(|v| *v as u64).sum::<u64>();
            let (now, then) = (sum(frame), sum(before));
            if now == 0 || then == 0 || now == then {
                return (1, 1);
            }
            let Ok(answer) = crate::golden::ask(&format!("What is {now} / {then}?")) else { return (1, 1) };
            let (num, den) = answer.split_once('/').unwrap_or((answer.as_str(), "1"));
            match (num.trim().parse::<u32>(), den.trim().parse::<u32>()) {
                (Ok(n), Ok(d)) if n > 0 && d > 0 => (n, d),
                _ => (1, 1),
            }
        })
        .collect()
}

/// The planes of a guessed frame for which Golden's guesser spoke (its fraction is not 1).
pub fn golden_planes(geo: &Geometry, payload: &[u8]) -> usize {
    if perfect(payload) {
        return 0;
    }
    (0..geo.planes.len()).filter(|i| payload.get(HEAD + 8 * i..HEAD + 8 * i + 8).is_some_and(|b| b[..4] != b[4..])).count()
}

/// Was the frame guessed perfectly (no correction at all)?
pub fn perfect(payload: &[u8]) -> bool {
    payload.first().is_some_and(|f| f & PERFECT != 0)
}

/// Does rebuilding this frame need the frame before it?
pub fn from_before(payload: &[u8]) -> bool {
    payload.first().is_some_and(|f| f & FROM_BEFORE != 0)
}

/// GUESS + CORRECT + the coder: flags, the number of bands, the tiles guessed
/// perfectly and the tiles, then each band's length and bytes, plane by
/// plane. A frame guessed perfectly keeps those first eleven bytes only.
pub fn pack(geo: &Geometry, frame: &[u8], before: Option<&[u8]>, hammer: Option<&[u8]>, bands: usize) -> Vec<u8> {
    pack_within(geo, frame, before, hammer, bands, 0).0
}

/// The same with a tolerance: a guess wrong by no more than `tol` is left as it is.
/// `before` and `hammer` must then be what the player will have (the frames as
/// rebuilt, not the real ones). Returns the frame as the player will rebuild it too
/// (empty when `tol` is 0: then it is `frame`).
pub fn pack_within(geo: &Geometry, frame: &[u8], before: Option<&[u8]>, hammer: Option<&[u8]>, bands: usize, tol: u8) -> (Vec<u8>, Vec<u8>) {
    let hammer = hammer.filter(|h| before.is_some() && h.len() == 2 * frame.len());
    let gains: Vec<(u32, u32)> = match before {
        Some(b) if b != frame => ask_golden(geo, frame, b),
        _ => vec![(1, 1); geo.planes.len()],
    };
    if before == Some(frame) {
        let tiles: usize = geo.planes.iter().map(|p| p.stride().div_ceil(tile(p).0) * p.h.div_ceil(tile(p).1)).sum();
        let mut out = vec![PERFECT | FROM_BEFORE | STILL];
        out.extend((bands as u16).to_le_bytes());
        out.extend((tiles as u32).to_le_bytes());
        out.extend((tiles as u32).to_le_bytes());
        out.push(tol);
        return (out, if tol > 0 { frame.to_vec() } else { Vec::new() });
    }
    let jobs: Vec<(usize, usize, usize)> = geo
        .planes
        .iter()
        .enumerate()
        .flat_map(|(i, p)| {
            let rows = band_rows(p, bands);
            (0..p.h).step_by(rows).map(move |y| (i, y, (y + rows).min(p.h)))
        })
        .collect();
    let at = geo.offsets();
    let done: Vec<(Vec<u8>, usize, usize, Vec<u8>)> = jobs
        .par_iter()
        .map(|(i, y0, y1)| {
            let p = &geo.planes[*i];
            let part = |f| plane_of(f, at[*i], p.len());
            let still = plane_of(hammer, geo.frame_len() + at[*i], p.len());
            correct(p, &frame[at[*i]..at[*i] + p.len()], part(before), part(hammer), still, golden_table(gains[*i]), tol, *y0, *y1)
        })
        .collect();
    let (perfect, tiles) = done.iter().fold((0, 0), |n, d| (n.0 + d.1, n.1 + d.2));
    // A frame keeps its corrections' bytes also when Golden spoke: its fractions are in them.
    let any = perfect < tiles || gains.iter().any(|g| g.0 != g.1);
    let mut out = vec![(if any { 0 } else { PERFECT }) | (if before.is_some() { FROM_BEFORE } else { 0 })];
    out.extend((bands as u16).to_le_bytes());
    out.extend((perfect as u32).to_le_bytes());
    out.extend((tiles as u32).to_le_bytes());
    out.push(tol);
    if any {
        for g in &gains {
            out.extend(g.0.to_le_bytes());
            out.extend(g.1.to_le_bytes());
        }
        for d in &done {
            out.extend((d.0.len() as u32).to_le_bytes());
        }
        for d in &done {
            out.extend(&d.0);
        }
    }
    // The bands were made plane by plane, top to bottom: together they are the frame.
    let rebuilt = done.iter().flat_map(|d| d.3.iter().copied()).collect();
    (out, rebuilt)
}

/// UNPACK: the frame rebuilt from the same guesses and its corrections.
pub fn unpack(geo: &Geometry, payload: &[u8], before: Option<&[u8]>, hammer: Option<&[u8]>) -> Result<Vec<u8>, String> {
    let hammer = hammer.filter(|h| from_before(payload) && h.len() == 2 * geo.frame_len());
    if payload.len() < HEAD {
        return Err("not a guessed frame".into());
    }
    let before = if from_before(payload) { Some(before.filter(|b| b.len() == geo.frame_len()).ok_or("a guessed frame was rebuilt without the frame before it")?) } else { None };
    if payload[0] & STILL != 0 {
        return Ok(before.ok_or("a still frame was rebuilt without the frame before it")?.to_vec());
    }
    let bands = u16::from_le_bytes([payload[1], payload[2]]) as usize;
    let tol = payload[11];
    let at = geo.offsets();
    let mut out = vec![0u8; geo.frame_len()];
    // Each band's bytes, in the order they were written.
    let count: usize = geo.planes.iter().map(|p| p.h.div_ceil(band_rows(p, bands))).sum();
    let mut coded: Vec<Option<&[u8]>> = vec![None; count];
    let mut golden = vec![None; geo.planes.len()];
    if !perfect(payload) {
        // Golden's fraction for each plane, then each band's length, then the bands.
        let word = |at: usize| payload.get(at..at + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).ok_or("a guessed frame is cut");
        for (i, slot) in golden.iter_mut().enumerate() {
            *slot = golden_table((word(HEAD + 8 * i)?, word(HEAD + 8 * i + 4)?));
        }
        let payload = &payload[8 * geo.planes.len()..];
        let mut data = HEAD + 4 * count;
        for (i, slot) in coded.iter_mut().enumerate() {
            let len = payload.get(HEAD + 4 * i..HEAD + 4 + 4 * i).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize).ok_or("a guessed frame is cut")?;
            *slot = Some(payload.get(data..data + len).ok_or("a guessed frame is cut")?);
            data += len;
        }
    }
    let mut rest = &mut out[..];
    let mut first = 0;
    let mut work = Vec::new();
    for (i, p) in geo.planes.iter().enumerate() {
        let (plane, r) = rest.split_at_mut(p.len());
        rest = r;
        let rows = band_rows(p, bands);
        for (k, chunk) in plane.chunks_mut(rows * p.stride()).enumerate() {
            work.push((i, k * rows, coded[first + k], chunk));
        }
        first += p.h.div_ceil(rows);
    }
    work.into_par_iter().for_each(|(i, y0, coded, chunk)| {
        let p = &geo.planes[i];
        let y1 = y0 + chunk.len() / p.stride();
        let part = |f| plane_of(f, at[i], p.len());
        let still = plane_of(hammer, geo.frame_len() + at[i], p.len());
        rebuild(p, coded, part(before), part(hammer), still, golden[i], tol, y0, y1, chunk);
    });
    Ok(out)
}

/// For HotMotor's size judge: the sample bands of the tile, guessed and
/// corrected as a pack would; returns (coded bytes, sample bytes).
pub fn sample(geo: &Geometry, frame: &[u8], before: Option<&[u8]>, hammer: Option<&[u8]>, rules: &Rules) -> (usize, usize) {
    let hammer = hammer.filter(|h| before.is_some() && h.len() == 2 * frame.len());
    let at = geo.offsets();
    let mut jobs = Vec::new();
    for (i, p) in geo.planes.iter().enumerate() {
        let (bands, rows) = (rules.sample_bands, rules.band_rows.min(p.h));
        for k in 0..bands {
            let y0 = if bands > 1 { k * (p.h - rows) / (bands - 1) } else { 0 };
            jobs.push((i, y0, y0 + rows));
        }
    }
    let coded: usize = jobs
        .par_iter()
        .map(|(i, y0, y1)| {
            let p = &geo.planes[*i];
            let part = |f| plane_of(f, at[*i], p.len());
            let still = plane_of(hammer, geo.frame_len() + at[*i], p.len());
            correct(p, &frame[at[*i]..at[*i] + p.len()], part(before), part(hammer), still, None, 0, *y0, *y1).0.len()
        })
        .sum();
    let raw: usize = jobs.iter().map(|(i, y0, y1)| (y1 - y0) * geo.planes[*i].stride()).sum();
    (coded, raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stack as the Hammer gives it: its picture, then how long each sample has stood still.
    fn twice(a: &[u8]) -> Vec<u8> {
        let mut t = a.to_vec();
        t.extend((0..a.len()).map(|i| (i % 9) as u8));
        t
    }

    fn noise(n: usize, seed: u32, flat: bool) -> Vec<u8> {
        let mut s = seed;
        (0..n)
            .map(|i| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                if flat && (i / 97) % 3 != 0 { 100 } else { (s >> 24) as u8 }
            })
            .collect()
    }

    #[test]
    fn a_tolerance_is_kept_and_the_player_rebuilds_the_same() {
        let geo = Geometry::of("yuv420p", 70, 50).unwrap();
        let a = noise(geo.frame_len(), 1, true);
        let b = noise(geo.frame_len(), 2, true);
        for tol in [1u8, 2, 5] {
            let (first, a2) = pack_within(&geo, &a, None, None, 3, tol);
            assert_eq!(unpack(&geo, &first, None, None).unwrap(), a2);
            let (second, b2) = pack_within(&geo, &b, Some(&a2), Some(&twice(&a2)), 3, tol);
            assert_eq!(unpack(&geo, &second, Some(&a2), Some(&twice(&a2))).unwrap(), b2);
            for (real, got) in [(&a, &a2), (&b, &b2)] {
                assert!(real.iter().zip(got).all(|(x, y)| x.abs_diff(*y) <= tol), "no sample is further than {tol} from the real one");
            }
        }
    }

    #[test]
    fn a_frame_comes_back_exactly() {
        for fmt in ["yuv420p", "rgba", "gray", "yuv420p10le"] {
            let geo = Geometry::of(fmt, 70, 50).unwrap();
            let a = noise(geo.frame_len(), 1, true);
            let mut b = a.clone();
            for i in (0..b.len()).step_by(7) {
                b[i] = b[i].wrapping_add(3);
            }
            for bands in [1, 3, 16] {
                let first = pack(&geo, &a, None, None, bands);
                assert_eq!(unpack(&geo, &first, None, None).unwrap(), a, "{fmt} first, {bands} bands");
                let second = pack(&geo, &b, Some(&a), Some(&twice(&a)), bands);
                assert_eq!(unpack(&geo, &second, Some(&a), Some(&twice(&a))).unwrap(), b, "{fmt} second, {bands} bands");
                let same = pack(&geo, &a, Some(&a), Some(&twice(&a)), bands);
                assert!(perfect(&same) && same.len() == HEAD, "{fmt}: a frame equal to the one before is guessed perfectly");
                assert_eq!(unpack(&geo, &same, Some(&a), Some(&twice(&a))).unwrap(), a);
            }
        }
    }
}
