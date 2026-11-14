//! BLOCKS: AV1's way of keeping less than every pixel.
//!
//! A frame is cut into the same 16 x 16 tiles. For each tile the packer
//! looks for the same picture in the frame before, as the player rebuilt
//! it, a few pixels away in any direction, to half a pixel; if it is there,
//! only the vector to it is kept (`moved`). If it is not, the tile is guessed as one flat
//! value from the pixels above and to the left of it (`flat`). What the
//! guess misses is not corrected pixel by pixel: each 8 x 8 block of it is
//! turned into 64 waves (a cosine transform, in whole numbers so that every
//! player gets the same), each wave's size is counted in steps (the finer
//! the wave, the longer its step: `coarse` sets the length), and a wave
//! smaller than a step is left out. Most waves of most blocks are. The
//! steps that are left are written with the adaptive range coder.
//!
//! The frame the player rebuilds is not the movie's frame; how near it is
//! is measured and reported (PSNR). Only 8-bit YUV 4:2:0 is cut this way.

use crate::guess::{Decoder, Encoder};
use crate::pack::Geometry;
use rayon::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};

/// Which 8 x 8 blocks of each plane were made anew in this frame (a flat guess, a
/// vector that is not zero, or any wave left). A block that is only the same block
/// of the frame before is not marked.
type Marks = [Vec<AtomicBool>; 3];

fn marks(geo: &Geometry) -> Marks {
    let plane = |i: usize| (0..geo.planes[i].w.div_ceil(8) * geo.planes[i].h.div_ceil(8)).map(|_| AtomicBool::new(false)).collect();
    [plane(0), plane(1), plane(2)]
}

const HALF: u16 = 1 << 11;
const TILE: usize = 16;

/// One way through the syntax for the packer and the player: the packer
/// gives the value and it is written; the player gets the value read.
enum Coder<'a> {
    Write(Encoder),
    Read(Decoder<'a>),
    /// The corrections as plain bits, one after the other, nothing learned and nothing
    /// squeezed: what the pattern dictionary is given when it comes before the final packing.
    Bits(Vec<u8>, usize),
    ReadBits(&'a [u8], usize),
}

impl Coder<'_> {
    #[inline(always)]
    fn bit(&mut self, p: &mut u16, v: bool) -> bool {
        match self {
            Coder::Write(e) => {
                e.bit(p, v);
                v
            }
            Coder::Read(d) => d.bit(p),
            Coder::Bits(bytes, n) => {
                if *n % 8 == 0 {
                    bytes.push(0);
                }
                if v {
                    *bytes.last_mut().unwrap() |= 0x80 >> (*n % 8);
                }
                *n += 1;
                v
            }
            Coder::ReadBits(bytes, n) => {
                let bit = bytes.get(*n / 8).is_some_and(|b| b & (0x80 >> (*n % 8)) != 0);
                *n += 1;
                bit
            }
        }
    }
    /// A whole number from 1 to 65535: how many binary digits, then the digits under the first.
    fn count(&mut self, p: &mut [u16; 32], n: u32) -> u32 {
        let want = 32 - n.max(1).leading_zeros() as usize;
        let mut digits = 1;
        while digits < 16 && self.bit(&mut p[digits - 1], digits < want) {
            digits += 1;
        }
        let mut value = 1u32;
        for j in (0..digits - 1).rev() {
            value = (value << 1) | self.bit(&mut p[16 + j], (n >> j) & 1 == 1) as u32;
        }
        value
    }
    /// A whole number with a sign.
    fn signed(&mut self, p: &mut ([u16; 32], [u16; 2]), v: i32) -> i32 {
        if !self.bit(&mut p.1[0], v != 0) {
            return 0;
        }
        let negative = self.bit(&mut p.1[1], v < 0);
        let n = self.count(&mut p.0, v.unsigned_abs().min(65535)) as i32;
        if negative { -n } else { n }
    }
}

/// What the coder has learned so far in one band.
struct Learned {
    /// Is the tile `moved`? By whether the tile to the left was (2: there is none).
    moved: [u16; 3],
    /// Is a tile that is not `moved` from the Hammer's stack? By whether the one to the left was.
    stack: [u16; 2],
    vector: [([u16; 32], [u16; 2]); 2],
    /// Has the block any wave left? By light or colour, and by flat or moved.
    any: [u16; 4],
    /// Where the last wave left is (6 binary digits), by light or colour.
    last: [[u16; 64]; 2],
    /// Is this wave left? By light or colour, how fine the wave is, and whether the one before was left.
    left: [u16; 2 * 6 * 2],
    sign: [u16; 2],
    size: [[u16; 32]; 12],
}

impl Learned {
    fn new() -> Learned {
        Learned { moved: [HALF; 3], stack: [HALF; 2], vector: [([HALF; 32], [HALF; 2]); 2], any: [HALF; 4], last: [[HALF; 64]; 2], left: [HALF; 24], sign: [HALF; 2], size: [[HALF; 32]; 12] }
    }
}

/// The fixed tables: the order waves are written in (coarse to fine), the
/// cosine table, and each wave's step.
struct Tables {
    order: [usize; 64],
    cosine: [[i32; 8]; 8],
    step: [i32; 64],
}

impl Tables {
    fn new(coarse: u8, slope: u8) -> Tables {
        // The zigzag from the flattest wave to the finest.
        let mut order = [0usize; 64];
        let (mut x, mut y) = (0i32, 0i32);
        for slot in order.iter_mut() {
            *slot = (y * 8 + x) as usize;
            if (x + y) % 2 == 0 {
                if x == 7 { y += 1 } else if y == 0 { x += 1 } else { x += 1; y -= 1 }
            } else if y == 7 {
                x += 1
            } else if x == 0 {
                y += 1
            } else {
                x -= 1;
                y += 1
            }
        }
        // cos(k pi / 16) in 4096ths, k = 0..8; row 0 is the mean (1 / sqrt 2).
        const C: [i32; 9] = [4096, 4017, 3784, 3406, 2896, 2276, 1567, 799, 0];
        let mut cosine = [[0i32; 8]; 8];
        for (k, row) in cosine.iter_mut().enumerate() {
            for (n, v) in row.iter_mut().enumerate() {
                let t = ((2 * n + 1) * k) % 32;
                let t = if t > 16 { 32 - t } else { t };
                *v = if k == 0 { 2896 } else if t <= 8 { C[t] } else { -C[16 - t] };
            }
        }
        // A finer wave has a longer step (the eye misses it least); `slope` says how much longer.
        let mut step = [1i32; 64];
        for (i, s) in step.iter_mut().enumerate() {
            let (u, v) = (order[i] % 8, order[i] / 8);
            *s = (coarse as i32 * (slope.max(1) as i32 + u as i32 + v as i32) / slope.max(1) as i32).max(1);
        }
        Tables { order, cosine, step }
    }

    /// 64 samples into 64 waves.
    fn waves(&self, x: &[i32; 64]) -> [i32; 64] {
        let (t, mut mid, mut out) = (&self.cosine, [0i32; 64], [0i32; 64]);
        for r in 0..8 {
            for k in 0..8 {
                mid[r * 8 + k] = ((0..8).map(|n| t[k][n] * x[r * 8 + n]).sum::<i32>() + 4096) >> 13;
            }
        }
        for c in 0..8 {
            for k in 0..8 {
                out[k * 8 + c] = ((0..8).map(|n| t[k][n] * mid[n * 8 + c]).sum::<i32>() + 4096) >> 13;
            }
        }
        out
    }

    /// 64 waves back into 64 samples.
    fn samples(&self, y: &[i32; 64]) -> [i32; 64] {
        let (t, mut mid, mut out) = (&self.cosine, [0i32; 64], [0i32; 64]);
        for c in 0..8 {
            for n in 0..8 {
                mid[n * 8 + c] = ((0..8).map(|k| t[k][n] * y[k * 8 + c]).sum::<i32>() + 4096) >> 13;
            }
        }
        for r in 0..8 {
            for n in 0..8 {
                out[r * 8 + n] = ((0..8).map(|k| t[k][n] * mid[r * 8 + k]).sum::<i32>() + 4096) >> 13;
            }
        }
        out
    }
}

/// A block's corrections as a pattern: which waves are left, and each one's steps.
type Waves = Vec<(u8, i16)>;

/// The pattern dictionary on the corrections themselves, before the coder: a book of
/// whole blocks' corrections that have come up before. A block whose corrections are
/// in the book is kept as the pattern's label. The book works as the bit dictionary
/// does: a pattern enters the second time it is seen, the most used have the lowest
/// ranks, at most `BOOK` are held and the one not used for the longest is dropped.
/// It is never stored: the player learns each block's corrections as it rebuilds
/// them and keeps the same book. Each band of a frame has its own, carried from
/// frame to frame.
pub struct Book {
    entries: Vec<(Waves, u32, u64, usize)>,
    find: std::collections::HashMap<Waves, usize>,
    ranks: Vec<usize>,
    oldest: std::collections::BTreeSet<(u64, usize)>,
    waiting: std::collections::HashSet<Waves>,
    ticks: u64,
    hit: [u16; 2],
    rank_bits: [u16; 32],
    /// A book of four patterns put in ready-made, which learns no others.
    four: bool,
    /// Blocks with corrections, and how many of them were in the book.
    pub blocks: u64,
    pub hits: u64,
}

const BOOK: usize = 1024;

/// The books hold only the four ready-made patterns (the command's --four).
pub static FOUR_ONLY: AtomicBool = AtomicBool::new(false);

impl Book {
    pub fn new() -> Book {
        Book { entries: Vec::new(), find: Default::default(), ranks: Vec::new(), oldest: Default::default(), waiting: Default::default(), ticks: 0, hit: [HALF; 2], rank_bits: [HALF; 32], four: false, blocks: 0, hits: 0 }
    }
    /// The four commonest patterns of block corrections, ready-made: the flattest wave one
    /// step down or up, and the next wave one step down or up (measured on the guitar clip:
    /// 19.5 %, 9.7 %, 3.2 % and 3.1 % of the blocks that have corrections). Nothing is learned.
    pub fn four() -> Book {
        let mut b = Book::new();
        b.four = true;
        for w in [vec![(0u8, -1i16)], vec![(0, 1)], vec![(1, -1)], vec![(1, 1)]] {
            b.entries.push((w.clone(), 1, 0, b.ranks.len()));
            b.ranks.push(b.entries.len() - 1);
            b.find.insert(w, b.entries.len() - 1);
            b.oldest.insert((0, b.entries.len() - 1));
        }
        b
    }
    pub fn patterns(&self) -> usize {
        self.entries.len()
    }
    /// The patterns with how often each was used (for looking at which are common).
    pub fn counts(&self) -> Vec<(Vec<(u8, i16)>, u32)> {
        self.entries.iter().map(|e| (e.0.clone(), e.1)).collect()
    }
    fn used(&mut self, e: usize) {
        self.oldest.remove(&(self.entries[e].2, e));
        self.entries[e].2 = self.ticks;
        self.oldest.insert((self.ticks, e));
        self.entries[e].1 += 1;
        let mut r = self.entries[e].3;
        while r > 0 && self.entries[self.ranks[r - 1]].1 < self.entries[e].1 {
            let other = self.ranks[r - 1];
            self.ranks.swap(r - 1, r);
            self.entries[other].3 = r;
            r -= 1;
        }
        self.entries[e].3 = r;
    }
    /// A block's corrections that were not in the book: the second time, they enter it.
    fn saw(&mut self, w: Waves) {
        if self.four {
            return;
        }
        if !self.waiting.remove(&w) {
            if self.waiting.len() >= 8192 {
                self.waiting.clear();
            }
            self.waiting.insert(w);
            return;
        }
        let e = if self.entries.len() < BOOK {
            self.entries.push((w.clone(), 1, self.ticks, self.ranks.len()));
            self.ranks.push(self.entries.len() - 1);
            self.entries.len() - 1
        } else {
            let (at, e) = *self.oldest.iter().next().unwrap();
            self.oldest.remove(&(at, e));
            let old = std::mem::replace(&mut self.entries[e], (w.clone(), 1, self.ticks, 0));
            self.find.remove(&old.0);
            self.entries[e].3 = old.3;
            e
        };
        self.find.insert(w, e);
        self.oldest.insert((self.ticks, e));
    }
}

fn waves_of(steps: &[i32; 64]) -> Waves {
    steps.iter().enumerate().filter(|(_, s)| **s != 0).map(|(i, s)| (i as u8, (*s).clamp(-32767, 32767) as i16)).collect()
}

/// The steps of one block's waves, coarse to fine: written, or read into `steps`.
fn block(c: &mut Coder, l: &mut Learned, book: &mut Option<&mut Book>, kind: usize, moved: usize, steps: &mut [i32; 64]) {
    let want = steps.iter().rposition(|v| *v != 0).map_or(0, |i| i + 1);
    if !c.bit(&mut l.any[kind * 2 + moved], want > 0) {
        *steps = [0; 64];
        return;
    }
    if let Some(book) = book.as_deref_mut() {
        // Is this block's whole pattern of corrections in the book? Then only its label is kept.
        book.ticks += 1;
        book.blocks += 1;
        let found = if matches!(c, Coder::Write(_)) { book.find.get(&waves_of(steps)).copied() } else { None };
        if c.bit(&mut book.hit[kind], found.is_some()) {
            let rank = c.count(&mut book.rank_bits, found.map_or(1, |e| book.entries[e].3 as u32 + 1)) as usize - 1;
            let e = book.ranks.get(rank).copied().unwrap_or(0);
            *steps = [0; 64];
            if let Some(entry) = book.entries.get(e) {
                for (i, s) in &entry.0 {
                    steps[*i as usize] = *s as i32;
                }
                book.used(e);
            }
            book.hits += 1;
            return;
        }
    }
    let (mut node, mut end) = (1usize, 0usize);
    for b in (0..6).rev() {
        let bit = c.bit(&mut l.last[kind][node], (want.wrapping_sub(1) >> b) & 1 == 1);
        node = (node * 2 + bit as usize) & 63;
        end = end * 2 + bit as usize;
    }
    let end = end + 1;
    let mut before = 1usize;
    for i in 0..64 {
        if i >= end {
            steps[i] = 0;
            continue;
        }
        let fine = kind * 6 + l_fine(i);
        // The last wave left is left: that is what "last" said.
        if i + 1 < end && !c.bit(&mut l.left[fine * 2 + before], steps[i] != 0) {
            steps[i] = 0;
            before = 0;
            continue;
        }
        let negative = c.bit(&mut l.sign[kind], steps[i] < 0);
        let n = c.count(&mut l.size[fine], steps[i].unsigned_abs().clamp(1, 65535)) as i32;
        steps[i] = if negative { -n } else { n };
        before = 1;
    }
    if let Some(book) = book.as_deref_mut() {
        book.saw(waves_of(steps));
    }
}

#[inline(always)]
fn l_fine(i: usize) -> usize {
    match i {
        0 => 0,
        1..=2 => 1,
        3..=5 => 2,
        6..=14 => 3,
        15..=35 => 4,
        _ => 5,
    }
}

/// A plane: its samples, width and height.
#[derive(Clone, Copy)]
struct Pic<'a> {
    data: &'a [u8],
    w: usize,
    h: usize,
}

impl Pic<'_> {
    #[inline(always)]
    fn at(&self, x: i32, y: i32) -> u8 {
        self.data[y.clamp(0, self.h as i32 - 1) as usize * self.w + x.clamp(0, self.w as i32 - 1) as usize]
    }
}

impl Pic<'_> {
    /// The plane at a place counted in half pixels: between two pixels their mean,
    /// between four the mean of the four.
    #[inline(always)]
    fn half(&self, x2: i32, y2: i32) -> i32 {
        let (x, y) = (x2 >> 1, y2 >> 1);
        match (x2 & 1, y2 & 1) {
            (0, 0) => self.at(x, y) as i32,
            (1, 0) => (self.at(x, y) as i32 + self.at(x + 1, y) as i32 + 1) >> 1,
            (0, _) => (self.at(x, y) as i32 + self.at(x, y + 1) as i32 + 1) >> 1,
            _ => (self.at(x, y) as i32 + self.at(x + 1, y) as i32 + self.at(x, y + 1) as i32 + self.at(x + 1, y + 1) as i32 + 2) >> 2,
        }
    }
}

/// Plane `i` of a frame.
fn pic<'a>(geo: &Geometry, at: &[usize], f: &'a [u8], i: usize) -> Pic<'a> {
    Pic { data: &f[at[i]..at[i] + geo.planes[i].len()], w: geo.planes[i].w, h: geo.planes[i].h }
}

/// How unlike the 16 x 16 tile `s` and the frame before at (x, y) are; stops counting past `limit`.
fn unlike(s: &[u8; 256], before: &Pic, x: i32, y: i32, limit: u32) -> u32 {
    let mut sum = 0u32;
    let inside = x >= 0 && x as usize + 16 <= before.w;
    for j in 0..16 {
        let row = (y + j).clamp(0, before.h as i32 - 1) as usize * before.w;
        if inside {
            let b = &before.data[row + x as usize..][..16];
            sum += s[j as usize * 16..][..16].iter().zip(b).map(|(p, q)| p.abs_diff(*q) as u32).sum::<u32>();
        } else {
            for i in 0..16 {
                sum += s[(j * 16 + i) as usize].abs_diff(before.at(x + i, y + j)) as u32;
            }
        }
        if sum > limit {
            return sum;
        }
    }
    sum
}

/// Where in the frame before the tile is: the vector in half pixels, and how unlike
/// it still is. Whole pixels are looked through first, then the eight half-pixel
/// places around the best.
fn find(s: &[u8; 256], before: &Pic, x0: i32, y0: i32, (range, bias): (i32, u32), left: (i32, i32)) -> (i32, i32, u32) {
    let still = unlike(s, before, x0, y0, u32::MAX);
    let (mut dx, mut dy, mut whole) = find_whole(s, before, x0, y0, range, (left.0 >> 1, left.1 >> 1));
    // A tile is taken to have moved only when that is clearly more like it than standing
    // still: a vector that follows the grain blurs a still picture a little more each frame.
    if (dx, dy) != (0, 0) && whole + bias >= still {
        (dx, dy, whole) = (0, 0, still);
    }
    let mut best = (2 * dx, 2 * dy, whole);
    if whole <= 256 {
        return best;
    }
    let whole_best = whole;
    for hy in -1..=1 {
        for hx in -1..=1 {
            if (hx, hy) == (0, 0) {
                continue;
            }
            let mut sum = 0u32;
            'tile: for j in 0..16 {
                for i in 0..16 {
                    sum += (s[(j * 16 + i) as usize] as i32).abs_diff(before.half(2 * (x0 + dx + i) + hx, 2 * (y0 + dy + j) + hy));
                }
                if sum >= best.2 {
                    break 'tile;
                }
            }
            if sum < best.2 {
                best = (2 * dx + hx, 2 * dy + hy, sum);
            }
        }
    }
    if best.2 + bias / 2 >= whole_best {
        best = (2 * dx, 2 * dy, whole_best);
    }
    best
}

fn find_whole(s: &[u8; 256], before: &Pic, x0: i32, y0: i32, range: i32, left: (i32, i32)) -> (i32, i32, u32) {
    let mut best = (0, 0, unlike(s, before, x0, y0, u32::MAX));
    // A tile that is where it was (within one step of grey a pixel) is not looked for.
    if best.2 <= 256 {
        return best;
    }
    let look = |dx: i32, dy: i32, best: &mut (i32, i32, u32)| {
        let u = unlike(s, before, x0 + dx, y0 + dy, best.2);
        if u < best.2 {
            *best = (dx, dy, u);
        }
    };
    look(left.0, left.1, &mut best);
    for dy in -range..=range {
        for dx in -range..=range {
            if (dx, dy) != (0, 0) {
                look(dx, dy, &mut best);
            }
        }
    }
    best
}

/// One band of tile rows `ty0..ty1`: packed (with `src`) or rebuilt (without).
/// `bufs` are the band's rows of the three planes as the player has them.
#[allow(clippy::too_many_arguments)]
fn band(geo: &Geometry, t: &Tables, src: Option<&[u8]>, before: Option<&[u8]>, stack: Option<&[u8]>, (range, dead, still): (i32, i32, u32), ty0: usize, ty1: usize, bufs: [&mut [u8]; 3], made: &Marks, mut book: Option<&mut Book>, c: &mut Coder) {
    let at = geo.offsets();
    let mut l = Learned::new();
    let across = geo.width.div_ceil(TILE);
    for ty in ty0..ty1 {
        let mut left: Option<(i32, i32)> = None;
        let (mut any_left, mut left_stack) = (false, false);
        for tx in 0..across {
            let (x0, y0) = ((tx * TILE) as i32, (ty * TILE) as i32);
            // The packer decides: is the tile somewhere in the frame before, or flat?
            let mut want: Option<(i32, i32)> = None;
            let mut want_stack = false;
            if let (Some(src), Some(before)) = (src, before) {
                let (sp, bp) = (pic(geo, &at, src, 0), pic(geo, &at, before, 0));
                let mut s = [0u8; 256];
                for j in 0..16 {
                    for i in 0..16 {
                        s[j * 16 + i] = sp.at(x0 + i as i32, y0 + j as i32);
                    }
                }
                let (dx, dy, moved) = find(&s, &bp, x0, y0, (range, still), left.unwrap_or((0, 0)));
                let mean = (s.iter().map(|v| *v as u32).sum::<u32>() + 128) / 256;
                let flat: u32 = s.iter().map(|v| (*v as u32).abs_diff(mean)).sum();
                // The frame before is taken unless a flat guess is far nearer: a tile from the
                // frame before keeps what earlier frames paid for, a flat one starts again.
                if moved <= flat * 4 + 2048 {
                    want = Some((dx, dy));
                }
                // The Hammer's stack: every frame so far, one on the other. Where the picture
                // stands still the misses of single frames have evened out in it, and the
                // tile is taken from there when the stack is more like it than the frame before.
                if let Some(stack) = stack {
                    let from_stack = unlike(&s, &pic(geo, &at, stack, 0), x0, y0, u32::MAX);
                    // "More like it" by a clear margin (one grey level a pixel): measured, a tile
                    // taken from the stack for less than that costs more bytes than it mends.
                    let margin = 256;
                    if from_stack + margin < moved && from_stack <= flat * 4 + 2048 {
                        want = None;
                        want_stack = true;
                    }
                }
            }
            let mut vector: Option<(i32, i32)> = None;
            let mut from_stack = false;
            if before.is_some() {
                let ctx = if !any_left { 2 } else { left.is_some() as usize };
                if c.bit(&mut l.moved[ctx], want.is_some()) {
                    let (from, w) = (left.unwrap_or((0, 0)), want.unwrap_or((0, 0)));
                    vector = Some((from.0 + c.signed(&mut l.vector[0], w.0 - from.0), from.1 + c.signed(&mut l.vector[1], w.1 - from.1)));
                } else if stack.is_some() {
                    from_stack = c.bit(&mut l.stack[left_stack as usize], want_stack);
                }
            }
            for i in 0..3 {
                let p = &geo.planes[i];
                let size = TILE >> p.sx;
                let (px0, py0) = (tx * size, ty * size);
                if px0 >= p.w || py0 >= p.h {
                    continue;
                }
                let band_top = ty0 * (TILE >> p.sy);
                let buf = &mut *bufs[i];
                // The flat guess: the mean of the row above the tile and the column to its left.
                let flat = {
                    let (mut sum, mut n) = (0u32, 0u32);
                    if py0 > band_top {
                        for x in px0..(px0 + size).min(p.w) {
                            sum += buf[(py0 - 1 - band_top) * p.w + x] as u32;
                            n += 1;
                        }
                    }
                    if px0 > 0 {
                        for y in py0..(py0 + size).min(p.h) {
                            sum += buf[(y - band_top) * p.w + px0 - 1] as u32;
                            n += 1;
                        }
                    }
                    if n == 0 { 128 } else { ((sum + n / 2) / n) as i32 }
                };
                let from = if from_stack { Some((pic(geo, &at, stack.unwrap(), i), 0, 0)) } else { vector.map(|(dx, dy)| (pic(geo, &at, before.unwrap(), i), dx >> p.sx, dy >> p.sy)) };
                for b in 0..(size / 8) * (size / 8) {
                    let (bx, by) = (px0 + (b % (size / 8)) * 8, py0 + (b / (size / 8)) * 8);
                    if bx >= p.w || by >= p.h {
                        continue;
                    }
                    let mut guess = [flat; 64];
                    if let Some((bp, dx, dy)) = &from {
                        for j in 0..8 {
                            for k in 0..8 {
                                guess[j * 8 + k] = bp.half(2 * (bx + k) as i32 + dx, 2 * (by + j) as i32 + dy);
                            }
                        }
                    }
                    let mut steps = [0i32; 64];
                    if let Some(src) = src {
                        let sp = pic(geo, &at, src, i);
                        let mut miss = [0i32; 64];
                        for j in 0..8 {
                            for k in 0..8 {
                                miss[j * 8 + k] = sp.at((bx + k) as i32, (by + j) as i32) as i32 - guess[j * 8 + k];
                            }
                        }
                        let w = t.waves(&miss);
                        for (n, s) in steps.iter_mut().enumerate() {
                            // A wave is counted in whole steps, rounded down but for `dead`: one
                            // that is not nearly a step long is grain more often than picture.
                            let (v, step) = (w[t.order[n]], t.step[n]);
                            *s = v.signum() * ((v.abs() + step / dead) / step);
                        }
                    }
                    block(c, &mut l, &mut book, (i > 0) as usize, (vector.is_some() || from_stack) as usize, &mut steps);
                    let mut back = [0i32; 64];
                    let waves_left = steps.iter().any(|s| *s != 0);
                    if waves_left || !(from_stack || matches!(vector, Some((0, 0)))) {
                        made[i][(by / 8) * p.w.div_ceil(8) + bx / 8].store(true, Ordering::Relaxed);
                    }
                    if waves_left {
                        let mut w = [0i32; 64];
                        for (n, s) in steps.iter().enumerate() {
                            w[t.order[n]] = s * t.step[n];
                        }
                        back = t.samples(&w);
                    }
                    for j in 0..8.min(p.h - by) {
                        for k in 0..8.min(p.w - bx) {
                            buf[(by + j - band_top) * p.w + bx + k] = (guess[j * 8 + k] + back[j * 8 + k]).clamp(0, 255) as u8;
                        }
                    }
                }
            }
            left = vector;
            left_stack = from_stack;
            any_left = true;
        }
    }
}

/// The seams between 8 x 8 blocks are smoothed once the whole frame is rebuilt, by the
/// packer and the player alike, so the smoothed frame is what the next one is found in.
/// A seam is smoothed only where both sides are calm and the jump over it is no more
/// than a step could have made: a real edge of the picture is left as it is. And only
/// where a block on either side was made anew in this frame: a block that is just the
/// same block of the frame before was smoothed when it was made, and smoothing it
/// again every frame would blur a still picture a little more each time.
fn smooth_seams(geo: &Geometry, frame: &mut [u8], coarse: u8, made: &Marks) {
    // Measured on the 10 s clip: with these, coarse 22 went from 39.0 to 40.7 dB and got 4 % smaller.
    let c = coarse as i32;
    let (jump, calm, most) = (2 * c, c * 3 / 4 + 2, c / 4 + 1);
    #[inline(always)]
    fn seam(p1: u8, p0: &mut u8, q0: &mut u8, q1: u8, (jump, calm, most): (i32, i32, i32)) {
        let (a, b, c, d) = (p1 as i32, *p0 as i32, *q0 as i32, q1 as i32);
        if (b - c).abs() < jump && (a - b).abs() < calm && (d - c).abs() < calm {
            let move_by = (((c - b) * 4 + (a - d) + 4) >> 3).clamp(-most, most);
            *p0 = (b + move_by).clamp(0, 255) as u8;
            *q0 = (c - move_by).clamp(0, 255) as u8;
        }
    }
    let how = (jump, calm, most);
    let mut rest = &mut frame[..];
    for (i, p) in geo.planes.iter().enumerate() {
        let (plane, r) = rest.split_at_mut(p.len());
        rest = r;
        let (w, h, across) = (p.w, p.h, p.w.div_ceil(8));
        let anew = |bx: usize, by: usize| made[i][by * across + bx].load(Ordering::Relaxed);
        plane.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
            for x in (8..w.saturating_sub(1)).step_by(8) {
                if !anew(x / 8 - 1, y / 8) && !anew(x / 8, y / 8) {
                    continue;
                }
                let (l, r) = row.split_at_mut(x);
                let (far_left, far_right) = (l[x - 2], r[1]);
                seam(far_left, &mut l[x - 1], &mut r[0], far_right, how);
            }
        });
        for y in (8..h.saturating_sub(1)).step_by(8) {
            let (top, bottom) = plane.split_at_mut(y * w);
            let (above2, above) = top[(y - 2) * w..].split_at_mut(w);
            let (below, below2) = bottom[..2 * w].split_at_mut(w);
            for x in 0..w {
                if anew(x / 8, y / 8 - 1) || anew(x / 8, y / 8) {
                    seam(above2[x], &mut above[x], &mut below[x], below2[x], how);
                }
            }
        }
    }
}

const FROM_BEFORE: u8 = 2;
/// The frame's blocks were kept with the book of patterns.
const BOOKS: u8 = 4;
/// The books hold four ready-made patterns and learn no others.
const FOUR: u8 = 8;
/// The bands are plain bits (for the pattern dictionary to work on), not coded.
const PLAIN: u8 = 16;

/// Frames are packed as plain bits, and the dictionary does the final packing
/// (the command's --dictionary-first).
pub static PLAIN_BITS: AtomicBool = AtomicBool::new(false);

/// One place for a book (or none) for each band.
fn slots(books: Option<&mut Vec<Book>>, bands: usize, four: bool) -> Vec<Option<&mut Book>> {
    match books {
        Some(b) => {
            if b.len() != bands || b.iter().any(|x| x.four != four) {
                *b = (0..bands).map(|_| if four { Book::four() } else { Book::new() }).collect();
            }
            b.iter_mut().map(Some).collect()
        }
        None => (0..bands).map(|_| None).collect(),
    }
}
const HEAD: usize = 5;

pub fn fits(geo: &Geometry) -> bool {
    geo.planes.len() == 3 && geo.planes.iter().all(|p| p.bps == 1 && p.ch == 1) && geo.planes[1].sx == 1 && geo.planes[1].sy == 1
}

pub fn from_before(payload: &[u8]) -> bool {
    payload.first().is_some_and(|f| f & FROM_BEFORE != 0)
}

/// The luma rows of a band: a whole number of rows of tiles.
fn band_rows(geo: &Geometry, bands: usize) -> usize {
    geo.height.div_ceil(bands.max(1)).max(1).div_ceil(TILE) * TILE
}

/// The frame as three planes cut into bands, each with its first row of tiles.
fn cut<'a>(geo: &Geometry, out: &'a mut [u8], rows: usize) -> Vec<(usize, [&'a mut [u8]; 3])> {
    let (py, pu) = (&geo.planes[0], &geo.planes[1]);
    let (y, rest) = out.split_at_mut(py.len());
    let (u, v) = rest.split_at_mut(pu.len());
    y.chunks_mut(rows * py.w).zip(u.chunks_mut(rows / 2 * pu.w)).zip(v.chunks_mut(rows / 2 * pu.w)).enumerate().map(|(k, ((y, u), v))| (k * rows / TILE, [y, u, v])).collect()
}

/// The packer's numbers from rules.toml: how far a tile is looked for, how much longer a
/// finer wave's step is (kept in the bottle: the player needs it), and the dead zone.
#[derive(Clone, Debug)]
pub struct How {
    pub range: usize,
    pub slope: u8,
    pub dead: u8,
    /// How much more like the tile a moved place must be than the same place (summed over
    /// the tile's 256 pixels) before the tile counts as moved.
    pub still: u32,
}

/// Packs a frame in AV1's way. `before` is the frame before as the player
/// rebuilt it. Returns the bytes (flags, bands, coarse, slope, each band's
/// length, the bands) and the frame as the player will rebuild it.
pub fn pack(geo: &Geometry, frame: &[u8], before: Option<&[u8]>, stack: Option<&[u8]>, bands: usize, coarse: u8, how: &How, books: Option<&mut Vec<Book>>) -> (Vec<u8>, Vec<u8>) {
    let plain = PLAIN_BITS.load(Ordering::Relaxed);
    let books = if plain { None } else { books };
    let with_books = books.is_some();
    let four = with_books && FOUR_ONLY.load(Ordering::Relaxed);
    let stack = stack.filter(|s| before.is_some() && s.len() == frame.len());
    // The top frame is what every still tile after it is a copy of: it is kept finer.
    let coarse = if before.is_none() { (coarse / 3).max(1) } else { coarse };
    let t = Tables::new(coarse, how.slope);
    let rows = band_rows(geo, bands);
    let tile_rows = geo.height.div_ceil(TILE);
    let mut rebuilt = vec![0u8; geo.frame_len()];
    let made = marks(geo);
    let work = cut(geo, &mut rebuilt, rows);
    let places = slots(books, work.len(), four);
    let done: Vec<Vec<u8>> = work
        .into_par_iter()
        .zip(places)
        .map(|((ty0, bufs), book)| {
            let mut c = if plain { Coder::Bits(Vec::new(), 0) } else { Coder::Write(Encoder::new()) };
            band(geo, &t, Some(frame), before, stack, (how.range as i32, how.dead.max(1) as i32, how.still), ty0, (ty0 + rows / TILE).min(tile_rows), bufs, &made, book, &mut c);
            match c {
                Coder::Write(e) => e.finish(),
                Coder::Bits(bytes, _) => bytes,
                _ => unreachable!(),
            }
        })
        .collect();
    smooth_seams(geo, &mut rebuilt, coarse, &made);
    let mut out = vec![(if before.is_some() { FROM_BEFORE } else { 0 }) | (if with_books { BOOKS } else { 0 }) | (if four { FOUR } else { 0 }) | (if plain { PLAIN } else { 0 })];
    out.extend((bands as u16).to_le_bytes());
    out.push(coarse);
    out.push(how.slope);
    for d in &done {
        out.extend((d.len() as u32).to_le_bytes());
    }
    for d in &done {
        out.extend(d);
    }
    (out, rebuilt)
}

/// The frame as the player rebuilds it.
pub fn unpack(geo: &Geometry, payload: &[u8], before: Option<&[u8]>, stack: Option<&[u8]>, books: &mut Vec<Book>) -> Result<Vec<u8>, String> {
    if payload.len() < HEAD || !fits(geo) {
        return Err("not a frame of blocks".into());
    }
    let before = if from_before(payload) { Some(before.filter(|b| b.len() == geo.frame_len()).ok_or("a frame of blocks was rebuilt without the frame before it")?) } else { None };
    let stack = stack.filter(|s| before.is_some() && s.len() == geo.frame_len());
    let (bands, t) = (u16::from_le_bytes([payload[1], payload[2]]) as usize, Tables::new(payload[3], payload[4]));
    let rows = band_rows(geo, bands);
    let tile_rows = geo.height.div_ceil(TILE);
    let mut out = vec![0u8; geo.frame_len()];
    let made = marks(geo);
    let work = cut(geo, &mut out, rows);
    let mut data = HEAD + 4 * work.len();
    let mut coded = Vec::new();
    for i in 0..work.len() {
        let len = payload.get(HEAD + 4 * i..HEAD + 4 * i + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize).ok_or("a frame of blocks is cut")?;
        coded.push(payload.get(data..data + len).ok_or("a frame of blocks is cut")?);
        data += len;
    }
    let places = slots(if payload[0] & BOOKS != 0 { Some(books) } else { None }, work.len(), payload[0] & FOUR != 0);
    work.into_par_iter().zip(coded).zip(places).for_each(|(((ty0, bufs), bytes), book)| {
        let mut c = if payload[0] & PLAIN != 0 { Coder::ReadBits(bytes, 0) } else { Coder::Read(Decoder::new(bytes)) };
        band(geo, &t, None, before, stack, (0, 1, 0), ty0, (ty0 + rows / TILE).min(tile_rows), bufs, &made, book, &mut c);
    });
    smooth_seams(geo, &mut out, payload[3], &made);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_player_rebuilds_what_the_packer_did() {
        let geo = Geometry::of("yuv420p", 70, 50).unwrap();
        let mut s = 7u32;
        let a: Vec<u8> = (0..geo.frame_len()).map(|i| { s = s.wrapping_mul(1664525).wrapping_add(1013904223); ((i % 70) as u32 * 3 + (s >> 29)) as u8 }).collect();
        let b: Vec<u8> = a.iter().map(|v| v.wrapping_add(2)).collect();
        let how = How { range: 7, slope: 32, dead: 12, still: 256 };
        for (coarse, with) in [(1u8, false), (6, true), (20, true), (6, false)] {
            let (mut packer, mut player): (Vec<Book>, Vec<Book>) = (Vec::new(), Vec::new());
            let (first, a2) = pack(&geo, &a, None, None, 3, coarse, &how, with.then_some(&mut packer));
            assert_eq!(unpack(&geo, &first, None, None, &mut player).unwrap(), a2);
            let (second, b2) = pack(&geo, &b, Some(&a2), Some(&a2), 3, coarse, &how, with.then_some(&mut packer));
            assert_eq!(unpack(&geo, &second, Some(&a2), Some(&a2), &mut player).unwrap(), b2);
        }
    }
}
