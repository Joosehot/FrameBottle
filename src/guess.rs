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
//!   slope (for the first frame there is nothing else).
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
pub const VERSION: &str = "guess 11";

const TOP: u32 = 1 << 24;
const BITS: u32 = 12;
const HALF: u16 = 1 << (BITS - 1);
const MOVE: u32 = 5;

struct Encoder {
    low: u64,
    range: u32,
    cache: u8,
    pending: u64,
    out: Vec<u8>,
}

impl Encoder {
    fn new() -> Encoder {
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
    fn bit(&mut self, p: &mut u16, bit: bool) {
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
    fn finish(mut self) -> Vec<u8> {
        for _ in 0..5 {
            self.shift();
        }
        self.out
    }
}

struct Decoder<'a> {
    data: &'a [u8],
    at: usize,
    code: u32,
    range: u32,
}

impl<'a> Decoder<'a> {
    fn new(data: &'a [u8]) -> Decoder<'a> {
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
    fn bit(&mut self, p: &mut u16) -> bool {
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
    zero: [u16; STEPS * 3 * 3],
    sign: [u16; 9],
    length: [u16; STEPS * 8],
    digits: [u16; 9 * 8],
    /// A tile's flag, by how many of the tiles to its left and above were guessed perfectly.
    tile: [u16; 3],
}

impl Learned {
    fn new() -> Learned {
        Learned { zero: [HALF; STEPS * 3 * 3], sign: [HALF; 9], length: [HALF; STEPS * 8], digits: [HALF; 9 * 8], tile: [HALF; 3] }
    }
}

/// Where a sample stands for the coder: the step of the corrections nearby,
/// which guesser spoke, how many of the two neighbours needed none, and
/// their signs.
struct Place {
    step: usize,
    who: usize,
    quiet: usize,
    signs: usize,
}

fn write(e: &mut Encoder, l: &mut Learned, at: &Place, v: i32) {
    e.bit(&mut l.zero[(at.step * 3 + at.who) * 3 + at.quiet], v != 0);
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
    if !d.bit(&mut l.zero[(at.step * 3 + at.who) * 3 + at.quiet]) {
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
    /// How wrong each guesser (still, moved, near) was, this row and above.
    wrong: [Vec<u8>; 3],
    wrong_up: [Vec<u8>; 3],
    /// The same row of the frame before, and the row above it there.
    before: Option<&'a [u8]>,
    before_up: Option<&'a [u8]>,
    offers: [u8; 3],
}

impl<'a> Band<'a> {
    fn new(p: &Plane) -> Band<'a> {
        let n = p.stride();
        Band {
            px: p.px(),
            row: vec![0; n],
            up: vec![0; n],
            has_up: false,
            fix: vec![0; n],
            fix_up: vec![0; n],
            wrong: [vec![0; n], vec![0; n], vec![0; n]],
            wrong_up: [vec![0; n], vec![0; n], vec![0; n]],
            before: None,
            before_up: None,
            offers: [0; 3],
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
            self.offers = [still as u8, (still + change).clamp(0, 255) as u8, near as u8];
            // The judge: how wrong each was on the sample to the left and the one above.
            let right = i + px < self.row.len();
            let score = |g: usize| {
                (if left { 2 * self.wrong[g][i - px] as u32 } else { 0 })
                    + (if up { 2 * self.wrong_up[g][i] as u32 } else { 0 })
                    + (if up && left { self.wrong_up[g][i - px] as u32 } else { 0 })
                    + (if up && right { self.wrong_up[g][i + px] as u32 } else { 0 })
            };
            let (s0, s1, s2) = (score(0), score(1), score(2));
            who = if s0 <= s1 && s0 <= s2 { 0 } else if s1 <= s2 { 1 } else { 2 };
            // Each speaks as loud as it was right: the guess is their mix.
            let w = |s: u32| ((1u32 << 20) / ((s + 2) * (s + 2))).max(1);
            let (w0, w1, w2) = (w(s0), w(s1), w(s2));
            let sum = w0 + w1 + w2;
            mixed = ((self.offers[0] as u32 * w0 + self.offers[1] as u32 * w1 + self.offers[2] as u32 * w2 + sum / 2) / sum) as u8;
        } else {
            self.offers = [near as u8; 3];
            mixed = near as u8;
        }
        let (fl, fu) = (if left { self.fix[i - px] as i32 } else { 0 }, if up { self.fix_up[i] as i32 } else { 0 });
        let ful = if up && left { self.fix_up[i - px] as i32 } else { 0 };
        let fur = if up && i + px < self.row.len() { self.fix_up[i + px] as i32 } else { 0 };
        let place = Place {
            step: step((2 * (fl.unsigned_abs() + fu.unsigned_abs()) + ful.unsigned_abs() + fur.unsigned_abs() + 1) / 3),
            who,
            quiet: (fl == 0) as usize + (fu == 0) as usize,
            signs: ((fl.signum() + 1) * 3 + fu.signum() + 1) as usize,
        };
        (mixed, place)
    }

    /// The real sample is known: remember it, the correction, and how wrong each guesser was.
    #[inline(always)]
    fn learn(&mut self, i: usize, real: u8, fix: i8) {
        self.row[i] = real;
        self.fix[i] = fix;
        for g in 0..3 {
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
fn correct(p: &Plane, real: &[u8], before: Option<&[u8]>, y0: usize, y1: usize) -> (Vec<u8>, usize, usize) {
    let stride = p.stride();
    let (tw, th) = tile(p);
    let across = stride.div_ceil(tw);
    // First the guesses, which need only what the player will have too.
    let mut band = Band::new(p);
    let mut fixes: Vec<i8> = Vec::with_capacity((y1 - y0) * stride);
    let mut places: Vec<Place> = Vec::with_capacity((y1 - y0) * stride);
    for y in y0..y1 {
        band.at_row(before, y, stride);
        let row = &real[y * stride..(y + 1) * stride];
        for (i, x) in row.iter().enumerate() {
            let (g, place) = band.guess(i);
            let fix = x.wrapping_sub(g) as i8;
            fixes.push(fix);
            places.push(place);
            band.learn(i, *x, fix);
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
    (e.finish(), perfect, tiles)
}

/// Rows `y0..y1` of a plane rebuilt into `out` (those rows only): the same
/// guesses, and the corrections added where a tile's flag says it has any.
/// `coded` None means no tile needed one.
fn rebuild(p: &Plane, coded: Option<&[u8]>, before: Option<&[u8]>, y0: usize, y1: usize, out: &mut [u8]) {
    let stride = p.stride();
    let (tw, th) = tile(p);
    let across = stride.div_ceil(tw);
    let (mut band, mut l) = (Band::new(p), Learned::new());
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
        let row = &mut out[(y - y0) * stride..(y - y0 + 1) * stride];
        for (i, x) in row.iter_mut().enumerate() {
            let (g, place) = band.guess(i);
            let fix = if flags[i / tw] { 0 } else { read(d.as_mut().unwrap(), &mut l, &place) as i8 };
            *x = g.wrapping_add(fix as u8);
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

/// What a guessed frame starts with: flags, the number of bands, the tiles guessed perfectly, the tiles.
const HEAD: usize = 11;

/// (tiles guessed perfectly, tiles) of a guessed frame.
pub fn tiles(payload: &[u8]) -> (usize, usize) {
    let n = |at: usize| payload.get(at..at + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()) as usize);
    (n(3), n(7))
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
pub fn pack(geo: &Geometry, frame: &[u8], before: Option<&[u8]>, bands: usize) -> Vec<u8> {
    if before == Some(frame) {
        let tiles: usize = geo.planes.iter().map(|p| p.stride().div_ceil(tile(p).0) * p.h.div_ceil(tile(p).1)).sum();
        let mut out = vec![PERFECT | FROM_BEFORE | STILL];
        out.extend((bands as u16).to_le_bytes());
        out.extend((tiles as u32).to_le_bytes());
        out.extend((tiles as u32).to_le_bytes());
        return out;
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
    let done: Vec<(Vec<u8>, usize, usize)> = jobs
        .par_iter()
        .map(|(i, y0, y1)| {
            let p = &geo.planes[*i];
            correct(p, &frame[at[*i]..at[*i] + p.len()], before.map(|b| &b[at[*i]..at[*i] + p.len()]), *y0, *y1)
        })
        .collect();
    let (perfect, tiles) = done.iter().fold((0, 0), |n, d| (n.0 + d.1, n.1 + d.2));
    let any = perfect < tiles;
    let mut out = vec![(if any { 0 } else { PERFECT }) | (if before.is_some() { FROM_BEFORE } else { 0 })];
    out.extend((bands as u16).to_le_bytes());
    out.extend((perfect as u32).to_le_bytes());
    out.extend((tiles as u32).to_le_bytes());
    if any {
        for d in &done {
            out.extend((d.0.len() as u32).to_le_bytes());
        }
        for d in &done {
            out.extend(&d.0);
        }
    }
    out
}

/// UNPACK: the frame rebuilt from the same guesses and its corrections.
pub fn unpack(geo: &Geometry, payload: &[u8], before: Option<&[u8]>) -> Result<Vec<u8>, String> {
    if payload.len() < HEAD {
        return Err("not a guessed frame".into());
    }
    let before = if from_before(payload) { Some(before.filter(|b| b.len() == geo.frame_len()).ok_or("a guessed frame was rebuilt without the frame before it")?) } else { None };
    if payload[0] & STILL != 0 {
        return Ok(before.ok_or("a still frame was rebuilt without the frame before it")?.to_vec());
    }
    let bands = u16::from_le_bytes([payload[1], payload[2]]) as usize;
    let at = geo.offsets();
    let mut out = vec![0u8; geo.frame_len()];
    // Each band's bytes, in the order they were written.
    let count: usize = geo.planes.iter().map(|p| p.h.div_ceil(band_rows(p, bands))).sum();
    let mut coded: Vec<Option<&[u8]>> = vec![None; count];
    if !perfect(payload) {
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
        rebuild(p, coded, before.map(|b| &b[at[i]..at[i] + p.len()]), y0, y1, chunk);
    });
    Ok(out)
}

/// For HotMotor's size judge: the sample bands of the tile, guessed and
/// corrected as a pack would; returns (coded bytes, sample bytes).
pub fn sample(geo: &Geometry, frame: &[u8], before: Option<&[u8]>, rules: &Rules) -> (usize, usize) {
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
            correct(p, &frame[at[*i]..at[*i] + p.len()], before.map(|b| &b[at[*i]..at[*i] + p.len()]), *y0, *y1).0.len()
        })
        .sum();
    let raw: usize = jobs.iter().map(|(i, y0, y1)| (y1 - y0) * geo.planes[*i].stride()).sum();
    (coded, raw)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn a_frame_comes_back_exactly() {
        for fmt in ["yuv420p", "rgba", "gray", "yuv420p10le"] {
            let geo = Geometry::of(fmt, 70, 50).unwrap();
            let a = noise(geo.frame_len(), 1, true);
            let mut b = a.clone();
            for i in (0..b.len()).step_by(7) {
                b[i] = b[i].wrapping_add(3);
            }
            for bands in [1, 3, 16] {
                let first = pack(&geo, &a, None, bands);
                assert_eq!(unpack(&geo, &first, None).unwrap(), a, "{fmt} first, {bands} bands");
                let second = pack(&geo, &b, Some(&a), bands);
                assert_eq!(unpack(&geo, &second, Some(&a)).unwrap(), b, "{fmt} second, {bands} bands");
                let same = pack(&geo, &a, Some(&a), bands);
                assert!(perfect(&same) && same.len() == HEAD, "{fmt}: a frame equal to the one before is guessed perfectly");
                assert_eq!(unpack(&geo, &same, Some(&a)).unwrap(), a);
            }
        }
    }
}
