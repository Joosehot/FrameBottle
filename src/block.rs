//! BLOCKS: AV1's way of keeping less than every pixel.
//!
//! A frame is cut into the same 16 x 16 tiles. For each tile the packer
//! looks for the same picture in the frame before, as the player rebuilt
//! it, a few pixels away in any direction, to a quarter of a pixel; if it is there,
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
use crate::refiner::{self, Refiner};
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

/// What the packer spent on the frame it packed last, for the breakdown: thousandths of
/// a bit on tile kinds and flags, on vectors, on waves, on labels of the book; then
/// blocks, blocks with no wave left, tiles, and tiles that are only the same picture
/// from another frame with nothing put right (skipped).
pub static SPENT: [std::sync::atomic::AtomicU64; 8] = [const { std::sync::atomic::AtomicU64::new(0) }; 8];
/// Whether the spending is counted (the command's --breakdown).
pub static COUNTING: AtomicBool = AtomicBool::new(false);
thread_local! {
    /// What the bits written now are for (an index into SPENT).
    static WHAT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn spending(what: usize) {
    WHAT.with(|w| w.set(what));
}

fn count(what: usize, n: u64) {
    if COUNTING.load(Ordering::Relaxed) {
        SPENT[what].fetch_add(n, Ordering::Relaxed);
    }
}

/// The spending of the last frame packed, and the counters set back to nothing.
pub fn spent() -> [u64; 8] {
    std::array::from_fn(|i| SPENT[i].swap(0, Ordering::Relaxed))
}

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
                if COUNTING.load(Ordering::Relaxed) {
                    // What this yes/no costs: the fewer bits, the surer the coder was of it.
                    let no = *p as f64 / 4096.0;
                    let bits = -(if v { 1.0 - no } else { no }).log2();
                    SPENT[WHAT.with(|w| w.get())].fetch_add((bits * 1000.0) as u64, Ordering::Relaxed);
                }
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
    /// Is a moved tile split in four? Is a tile that is not moved between two frames?
    /// Each by whether the tile to the left was.
    split: [u16; 2],
    between: [u16; 2],
    /// Is a tile that is neither from the frame after (for a frame between two)?
    after: u16,
    /// Which guess from the pixels around a tile that is in no other frame.
    around: [u16; 3],
    /// Is the tile skipped (the picture where the engine predicted it, nothing put
    /// right)? By how many of the tiles to its left and above it were.
    skip: [u16; 3],
    /// Is a tile's guess refined by Neuras? By whether the tile to the left's was.
    refined: [u16; 2],
    vector: [([u16; 32], [u16; 2]); 2],
    /// Has the block any wave left? By light or colour, and by flat or moved.
    any: [u16; 4],
    /// Where the last wave left is (6 binary digits), by light or colour.
    last: [[u16; 64]; 2],
    /// Is this wave left? By light or colour, how fine the wave is, and whether the one before was left.
    left: [u16; 2 * 6 * 6],
    /// Is a wave more than one step, more than two? By light or colour and the waves next to it.
    more: [u16; 2 * 4 * 2],
    sign: [u16; 2],
    /// For a tile of light taken as one block of 16 x 16: is it? (by whether the tile to
    /// the left was); has it any wave; where the last one is (8 binary digits); is a wave
    /// left, is it more than one step or two, and how many more.
    big: [u16; 2],
    any16: u16,
    last16: [u16; 256],
    left16: [u16; 7 * 6],
    more16: [u16; 4 * 2],
    size16: [[u16; 32]; 7],
    size: [[u16; 32]; 12],
}

impl Learned {
    fn new() -> Learned {
        Learned { moved: [HALF; 3], stack: [HALF; 2], split: [HALF; 2], between: [HALF; 2], after: HALF, around: [HALF; 3], skip: [HALF; 3], refined: [HALF; 2], vector: [([HALF; 32], [HALF; 2]); 2], any: [HALF; 4], last: [[HALF; 64]; 2], left: [HALF; 72], more: [HALF; 16], sign: [HALF; 2], big: [HALF; 2], any16: HALF, last16: [HALF; 256], left16: [HALF; 42], more16: [HALF; 8], size16: [[HALF; 32]; 7], size: [[HALF; 32]; 12] }
    }
}

/// The fixed tables: the order waves are written in (coarse to fine), the
/// cosine table, and each wave's step.
struct Tables {
    order: [usize; 64],
    cosine: [[i32; 8]; 8],
    step: [i32; 64],
    /// The same for a whole tile of light taken as one block of 16 x 16.
    order16: [usize; 256],
    cosine16: [[i32; 16]; 16],
    step16: [i32; 256],
}

/// The zigzag from the flattest wave to the finest, for a block `n` wide.
fn zigzag(n: usize) -> Vec<usize> {
    let mut order = Vec::with_capacity(n * n);
    let (mut x, mut y, last) = (0usize, 0usize, n - 1);
    for _ in 0..n * n {
        order.push(y * n + x);
        if (x + y) % 2 == 0 {
            if x == last { y += 1 } else if y == 0 { x += 1 } else { x += 1; y -= 1 }
        } else if y == last {
            x += 1
        } else if x == 0 {
            y += 1
        } else {
            x -= 1;
            y += 1
        }
    }
    order
}

impl Tables {
    fn new(coarse: u8, slope: u8) -> Tables {
        let order: [usize; 64] = zigzag(8).try_into().unwrap();
        let order16: [usize; 256] = zigzag(16).try_into().unwrap();
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
        // cos(k pi / 32) in 4096ths, k = 0..16, for the block of 16.
        const C16: [i32; 17] = [4096, 4076, 4017, 3920, 3784, 3612, 3406, 3166, 2896, 2598, 2276, 1931, 1567, 1189, 799, 401, 0];
        let mut cosine16 = [[0i32; 16]; 16];
        for (k, row) in cosine16.iter_mut().enumerate() {
            for (n, v) in row.iter_mut().enumerate() {
                let t = ((2 * n + 1) * k) % 64;
                let t = if t > 32 { 64 - t } else { t };
                *v = if k == 0 { 2896 } else if t <= 16 { C16[t] } else { -C16[32 - t] };
            }
        }
        // A wave of the block of 16 is as fine as the wave of half its number in a block of 8.
        let mut step16 = [1i32; 256];
        for (i, s) in step16.iter_mut().enumerate() {
            let (u, v) = (order16[i] % 16, order16[i] / 16);
            *s = (coarse as i32 * (2 * slope.max(1) as i32 + u as i32 + v as i32) / (2 * slope.max(1) as i32)).max(1);
        }
        Tables { order, cosine, step, order16, cosine16, step16 }
    }

    /// 256 samples (a tile of light) into 256 waves, and back.
    fn waves16(&self, x: &[i32; 256]) -> [i32; 256] {
        let (t, mut mid, mut out) = (&self.cosine16, [0i32; 256], [0i32; 256]);
        for r in 0..16 {
            for k in 0..16 {
                mid[r * 16 + k] = ((0..16).map(|n| t[k][n] * x[r * 16 + n]).sum::<i32>() + (1 << 12)) >> 13;
            }
        }
        for c in 0..16 {
            for k in 0..16 {
                out[k * 16 + c] = ((0..16).map(|n| t[k][n] * mid[n * 16 + c]).sum::<i32>() + (1 << 13)) >> 14;
            }
        }
        out
    }
    fn samples16(&self, y: &[i32; 256]) -> [i32; 256] {
        let (t, mut mid, mut out) = (&self.cosine16, [0i32; 256], [0i32; 256]);
        for c in 0..16 {
            for n in 0..16 {
                mid[n * 16 + c] = ((0..16).map(|k| t[k][n] * y[k * 16 + c]).sum::<i32>() + (1 << 12)) >> 13;
            }
        }
        for r in 0..16 {
            for n in 0..16 {
                out[r * 16 + n] = ((0..16).map(|k| t[k][n] * mid[r * 16 + k]).sum::<i32>() + (1 << 13)) >> 14;
            }
        }
        out
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
fn block(c: &mut Coder, l: &mut Learned, t: &Tables, book: &mut Option<&mut Book>, kind: usize, moved: usize, steps: &mut [i32; 64]) {
    let want = steps.iter().rposition(|v| *v != 0).map_or(0, |i| i + 1);
    spending(2);
    count(4, 1);
    if !c.bit(&mut l.any[kind * 2 + moved], want > 0) {
        *steps = [0; 64];
        count(5, 1);
        return;
    }
    if let Some(book) = book.as_deref_mut() {
        spending(3);
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
        spending(2);
    }
    let (mut node, mut end) = (1usize, 0usize);
    for b in (0..6).rev() {
        let bit = c.bit(&mut l.last[kind][node], (want.wrapping_sub(1) >> b) & 1 == 1);
        node = (node * 2 + bit as usize) & 63;
        end = end * 2 + bit as usize;
    }
    let end = end + 1;
    // From the last wave left back to the flattest (AV1's way): how likely a wave is left, and
    // how large it is, is learned by how large the waves next to it are, to the right and
    // below: finer ones, which are already written.
    let mut size_at = [0u8; 64];
    for i in (0..end).rev() {
        let at = t.order[i];
        let (u, v) = (at % 8, at / 8);
        let get = |du: usize, dv: usize| if u + du < 8 && v + dv < 8 { size_at[(v + dv) * 8 + u + du] as usize } else { 0 };
        let near = get(1, 0) + get(0, 1) + get(1, 1) + get(2, 0) + get(0, 2);
        let fine = kind * 6 + l_fine(i);
        // The last wave left is left: that is what "last" said.
        if i + 1 < end && !c.bit(&mut l.left[fine * 6 + near.min(5)], steps[i] != 0) {
            steps[i] = 0;
            continue;
        }
        let negative = c.bit(&mut l.sign[kind], steps[i] < 0);
        let want = steps[i].unsigned_abs().clamp(1, 65535);
        // One step, two, or more (and then how many more).
        let ctx = (kind * 4 + near.min(3)) * 2;
        let mut n = 1;
        if c.bit(&mut l.more[ctx], want > 1) {
            n = 2;
            if c.bit(&mut l.more[ctx + 1], want > 2) {
                n = 2 + c.count(&mut l.size[fine], want.saturating_sub(2).max(1));
            }
        }
        steps[i] = if negative { -(n as i32) } else { n as i32 };
        size_at[at] = n.min(3) as u8;
    }
    for s in steps.iter_mut().skip(end) {
        *s = 0;
    }
    if let Some(book) = book.as_deref_mut() {
        book.saw(waves_of(steps));
    }
}

/// The steps of the waves of a tile of light taken as one block of 16 x 16: written, or
/// read into `steps`. As `block` does it, with the block's own learning.
fn block16(c: &mut Coder, l: &mut Learned, t: &Tables, steps: &mut [i32; 256]) {
    let want = steps.iter().rposition(|v| *v != 0).map_or(0, |i| i + 1);
    if !c.bit(&mut l.any16, want > 0) {
        *steps = [0; 256];
        return;
    }
    let (mut node, mut end) = (1usize, 0usize);
    for b in (0..8).rev() {
        let bit = c.bit(&mut l.last16[node], (want.wrapping_sub(1) >> b) & 1 == 1);
        node = (node * 2 + bit as usize) & 255;
        end = end * 2 + bit as usize;
    }
    let end = end + 1;
    let mut size_at = [0u8; 256];
    for i in (0..end).rev() {
        let at = t.order16[i];
        let (u, v) = (at % 16, at / 16);
        let get = |du: usize, dv: usize| if u + du < 16 && v + dv < 16 { size_at[(v + dv) * 16 + u + du] as usize } else { 0 };
        let near = get(1, 0) + get(0, 1) + get(1, 1) + get(2, 0) + get(0, 2);
        let fine = if i >= 64 { 6 } else { l_fine(i) };
        if i + 1 < end && !c.bit(&mut l.left16[fine * 6 + near.min(5)], steps[i] != 0) {
            steps[i] = 0;
            continue;
        }
        let negative = c.bit(&mut l.sign[0], steps[i] < 0);
        let want = steps[i].unsigned_abs().clamp(1, 65535);
        let ctx = near.min(3) * 2;
        let mut n = 1;
        if c.bit(&mut l.more16[ctx], want > 1) {
            n = 2;
            if c.bit(&mut l.more16[ctx + 1], want > 2) {
                n = 2 + c.count(&mut l.size16[fine], want.saturating_sub(2).max(1));
            }
        }
        steps[i] = if negative { -(n as i32) } else { n as i32 };
        size_at[at] = n.min(3) as u8;
    }
    for s in steps.iter_mut().skip(end) {
        *s = 0;
    }
}

/// A tile of light's miss as steps of the waves of one block of 16 x 16, and what
/// those steps put back.
fn count_steps16(t: &Tables, miss: &[i32; 256], dead: i32) -> [i32; 256] {
    let w = t.waves16(miss);
    let mut steps = [0i32; 256];
    for (n, s) in steps.iter_mut().enumerate() {
        let (v, step) = (w[t.order16[n]], t.step16[n]);
        *s = v.signum() * ((v.abs() + step / dead) / step);
    }
    steps
}

fn put_back16(t: &Tables, steps: &[i32; 256]) -> [i32; 256] {
    if steps.iter().all(|s| *s == 0) {
        return [0; 256];
    }
    let mut w = [0i32; 256];
    for (n, s) in steps.iter().enumerate() {
        w[t.order16[n]] = s * t.step16[n];
    }
    t.samples16(&w)
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
    /// The light plane at a place counted in quarter pixels.
    fn quarter(&self, x4: i32, y4: i32) -> i32 {
        quarter_of(&|x, y| self.at(x, y) as i32, x4, y4)
    }

    /// A colour plane at a place counted in eighths of a pixel (a quarter of a light
    /// pixel): the four pixels around it, each weighed by how near it is.
    #[inline(always)]
    fn eighth(&self, x8: i32, y8: i32) -> i32 {
        let (x, y, fx, fy) = (x8 >> 3, y8 >> 3, x8 & 7, y8 & 7);
        if fx == 0 && fy == 0 {
            return self.at(x, y) as i32;
        }
        let g = |dx: i32, dy: i32| self.at(x + dx, y + dy) as i32;
        ((8 - fx) * (8 - fy) * g(0, 0) + fx * (8 - fy) * g(1, 0) + (8 - fx) * fy * g(0, 1) + fx * fy * g(1, 1) + 32) >> 6
    }
}

/// A picture at a place counted in quarter pixels; `g` gives its whole pixels.
/// Half way between two pixels is six pixels in a row weighed 1, -5, 20, 20, -5, 1
/// (H.264's own filter: a movie that was H.264 once moved its blocks with exactly
/// this); a quarter is the mean of the two places next to it.
#[inline(always)]
fn quarter_of<G: Fn(i32, i32) -> i32>(g: &G, x4: i32, y4: i32) -> i32 {
    let (x, y, fx, fy) = (x4 >> 2, y4 >> 2, x4 & 3, y4 & 3);
    if fx == 0 && fy == 0 {
        return g(x, y);
    }
    let clip = |v: i32| v.clamp(0, 255);
    let mean = |a: i32, b: i32| (a + b + 1) >> 1;
    let across = |x: i32, y: i32| g(x - 2, y) - 5 * g(x - 1, y) + 20 * g(x, y) + 20 * g(x + 1, y) - 5 * g(x + 2, y) + g(x + 3, y);
    let down = |x: i32, y: i32| g(x, y - 2) - 5 * g(x, y - 1) + 20 * g(x, y) + 20 * g(x, y + 1) - 5 * g(x, y + 2) + g(x, y + 3);
    // Half way to the right of a pixel, half way below it, and the middle of four.
    let right = |dx: i32, dy: i32| clip((across(x + dx, y + dy) + 16) >> 5);
    let below = |dx: i32, dy: i32| clip((down(x + dx, y + dy) + 16) >> 5);
    let middle = || clip((across(x, y - 2) - 5 * across(x, y - 1) + 20 * across(x, y) + 20 * across(x, y + 1) - 5 * across(x, y + 2) + across(x, y + 3) + 512) >> 10);
    match (fx, fy) {
        (1, 0) => mean(g(x, y), right(0, 0)),
        (2, 0) => right(0, 0),
        (3, 0) => mean(g(x + 1, y), right(0, 0)),
        (0, 1) => mean(g(x, y), below(0, 0)),
        (0, 2) => below(0, 0),
        (0, 3) => mean(g(x, y + 1), below(0, 0)),
        (2, 2) => middle(),
        (2, 1) => mean(right(0, 0), middle()),
        (2, 3) => mean(middle(), right(0, 1)),
        (1, 2) => mean(below(0, 0), middle()),
        (3, 2) => mean(middle(), below(1, 0)),
        (1, 1) => mean(right(0, 0), below(0, 0)),
        (3, 1) => mean(right(0, 0), below(1, 0)),
        (1, 3) => mean(below(0, 0), right(0, 1)),
        _ => mean(below(1, 0), right(0, 1)),
    }
}

/// Plane `i` of a frame.
fn pic<'a>(geo: &Geometry, at: &[usize], f: &'a [u8], i: usize) -> Pic<'a> {
    Pic { data: &f[at[i]..at[i] + geo.planes[i].len()], w: geo.planes[i].w, h: geo.planes[i].h }
}

/// How unlike the `n` x `n` tile `s` and the frame before at (x, y) are; stops counting past `limit`.
fn unlike(s: &[u8], n: usize, before: &Pic, x: i32, y: i32, limit: u32) -> u32 {
    let mut sum = 0u32;
    let inside = x >= 0 && x as usize + n <= before.w;
    for j in 0..n as i32 {
        let row = (y + j).clamp(0, before.h as i32 - 1) as usize * before.w;
        if inside {
            let b = &before.data[row + x as usize..][..n];
            sum += s[j as usize * n..][..n].iter().zip(b).map(|(p, q)| p.abs_diff(*q) as u32).sum::<u32>();
        } else {
            for i in 0..n as i32 {
                sum += s[(j as usize) * n + i as usize].abs_diff(before.at(x + i, y + j)) as u32;
            }
        }
        if sum > limit {
            return sum;
        }
    }
    sum
}

/// A frame around a tile at a whole-pixel place: the tile and 4 pixels more on every
/// side, cut out once so that the fine search need not look at the frame's edges.
const AROUND: i32 = 4;
const CUT: usize = 16 + 2 * AROUND as usize;

fn cut_out(frame: &Pic, x: i32, y: i32, n: usize) -> [u8; CUT * CUT] {
    let mut c = [0u8; CUT * CUT];
    for j in 0..(n as i32 + 2 * AROUND) {
        for i in 0..(n as i32 + 2 * AROUND) {
            c[(j as usize) * CUT + i as usize] = frame.at(x - AROUND + i, y - AROUND + j);
        }
    }
    c
}

/// The cut-out at a place counted in quarter pixels from the tile's corner.
#[inline(always)]
fn fine(cut: &[u8; CUT * CUT], x4: i32, y4: i32) -> i32 {
    quarter_of(&|x: i32, y: i32| cut[((y + AROUND) as usize) * CUT + (x + AROUND) as usize] as i32, x4, y4)
}

/// How unlike the tile and the cut-out are, the tile shifted by (qx, qy) quarter pixels.
fn unlike_fine(s: &[u8], n: usize, cut: &[u8; CUT * CUT], qx: i32, qy: i32, limit: u32) -> u32 {
    let mut sum = 0u32;
    for j in 0..n as i32 {
        for i in 0..n as i32 {
            sum += (s[(j as usize) * n + i as usize] as i32).abs_diff(fine(cut, 4 * i + qx, 4 * j + qy));
        }
        if sum >= limit {
            return sum;
        }
    }
    sum
}

/// Where in the frame before the `n` x `n` tile is: the vector in quarter pixels, and
/// how unlike it still is. Whole pixels are looked through first (`range` each way,
/// around no move and around `near`), then the eight half-pixel places around the
/// best, then the eight quarter-pixel places around that.
fn find(s: &[u8], n: usize, before: &Pic, x0: i32, y0: i32, (range, bias): (i32, u32), near: (i32, i32)) -> (i32, i32, u32) {
    let still = unlike(s, n, before, x0, y0, u32::MAX);
    let mut whole = (0, 0, still);
    // A tile that is where it was (within a grey level a pixel) is not looked for.
    if still as usize <= n * n {
        return whole;
    }
    let look = |dx: i32, dy: i32, best: &mut (i32, i32, u32)| {
        let u = unlike(s, n, before, x0 + dx, y0 + dy, best.2);
        if u < best.2 {
            *best = (dx, dy, u);
        }
    };
    let (nx, ny) = (near.0 >> 2, near.1 >> 2);
    look(nx, ny, &mut whole);
    for dy in -range..=range {
        for dx in -range..=range {
            look(dx, dy, &mut whole);
            if (nx, ny) != (0, 0) && n < 16 {
                look(nx + dx, ny + dy, &mut whole);
            }
        }
    }
    // A tile is taken to have moved only when that is clearly more like it than standing
    // still: a vector that follows the grain blurs a still picture a little more each frame.
    if (whole.0, whole.1) != (0, 0) && whole.2 + bias >= still {
        whole = (0, 0, still);
    }
    let (dx, dy) = (whole.0, whole.1);
    let mut best = (4 * dx, 4 * dy, whole.2);
    // Each finer step is kept only if it is clearly more like the tile than the coarser one.
    let cut = cut_out(before, x0 + dx, y0 + dy, n);
    for (step, gain) in [(2, bias / 2), (1, bias / 4)] {
        let (cx, cy, coarser) = best;
        let mut finer = best;
        for hy in -1..=1 {
            for hx in -1..=1 {
                if (hx, hy) == (0, 0) {
                    continue;
                }
                let (vx, vy) = (cx + hx * step, cy + hy * step);
                let sum = unlike_fine(s, n, &cut, vx - 4 * dx, vy - 4 * dy, finer.2);
                if sum < finer.2 {
                    finer = (vx, vy, sum);
                }
            }
        }
        if finer.2 + gain < coarser {
            best = finer;
        }
    }
    best
}

/// For a tile between two frames: the vector d (quarter pixels) for which the frame
/// before at d back and the frame after at d on, mixed half and half, are most like it.
fn find_between(s: &[u8; 256], before: &Pic, after: &Pic, x0: i32, y0: i32) -> (i32, i32) {
    // Whole pixels first, two each way.
    let mut whole = (0, 0, u32::MAX);
    for dy in -2..=2 {
        for dx in -2..=2 {
            let mut sum = 0u32;
            for j in 0..16 {
                for i in 0..16 {
                    let mix = (before.at(x0 + i - dx, y0 + j - dy) as u32 + after.at(x0 + i + dx, y0 + j + dy) as u32 + 1) >> 1;
                    sum += (s[(j * 16 + i) as usize] as u32).abs_diff(mix);
                }
                if sum >= whole.2 {
                    break;
                }
            }
            // No move is taken on a tie.
            if sum < whole.2 || (sum == whole.2 && (dx, dy) == (0, 0)) {
                whole = (dx, dy, sum);
            }
        }
    }
    let (dx, dy) = (whole.0, whole.1);
    let (cb, ca) = (cut_out(before, x0 - dx, y0 - dy, 16), cut_out(after, x0 + dx, y0 + dy, 16));
    let mut best = (4 * dx, 4 * dy, whole.2);
    for step in [2, 1] {
        let (cx, cy) = (best.0, best.1);
        for hy in -1..=1 {
            for hx in -1..=1 {
                if (hx, hy) == (0, 0) {
                    continue;
                }
                let (vx, vy) = (cx + hx * step, cy + hy * step);
                let (qx, qy) = (vx - 4 * dx, vy - 4 * dy);
                let mut sum = 0u32;
                for j in 0..16 {
                    for i in 0..16 {
                        let mix = (fine(&cb, 4 * i - qx, 4 * j - qy) + fine(&ca, 4 * i + qx, 4 * j + qy) + 1) >> 1;
                        sum += (s[(j * 16 + i) as usize] as i32).abs_diff(mix);
                    }
                    if sum >= best.2 {
                        break;
                    }
                }
                if sum < best.2 {
                    best = (vx, vy, sum);
                }
            }
        }
    }
    (best.0, best.1)
}

/// Where a tile's guess comes from.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    /// From the pixels just above each block and just to its left, already rebuilt (for a
    /// tile that is in no other frame): 0 their mean, 1 the row above carried down,
    /// 2 the column to the left carried across, 3 each pixel from the one above it, the
    /// one to its left and the one at the corner, whichever is nearest to above + left -
    /// corner (AV1's Paeth).
    Flat(u8),
    /// The Hammer's stack, at the same place.
    Stack,
    /// The frame before, by a vector for each quarter of the tile (in quarter pixels);
    /// true when the four are not all the same (the tile is split).
    Moved([(i32, i32); 4], bool),
    /// Between the frame before and the frame after: the mean of the frame before one
    /// vector back and the frame after the same vector on.
    Between((i32, i32)),
    /// The frame before by one vector, and the guess then refined by Neuras.
    Refined((i32, i32)),
    /// The frame after, by one vector (for a frame between two).
    After((i32, i32)),
}

/// A vector, in quarter pixels.
type V = (i32, i32);

/// The Predictive Motion Engine's memory: the vector of every tile in the last few
/// frames that others are guessed from, newest first (None where a tile had none).
/// It is made only of what was rebuilt, so the packer's and the player's are the
/// same, and nothing of it is stored. `check` folds in every vector either of them
/// predicted: equal checks mean the two predicted the same all the way.
pub struct Motion {
    fields: std::collections::VecDeque<Vec<Option<V>>>,
    /// The last three frames others are guessed from, as rebuilt, newest first (for the refiner).
    frames: std::collections::VecDeque<Vec<u8>>,
    history: usize,
    /// How many frames the newest field's vectors span.
    span: i32,
    pub check: u64,
}

impl Motion {
    /// A memory of `history` frames (0: the usual 4).
    pub fn new(history: usize) -> Motion {
        Motion { fields: Default::default(), frames: Default::default(), history: if history == 0 { 4 } else { history }, span: 1, check: 0xcbf2_9ce4_8422_2325 }
    }
    /// At a cut the history is forgotten.
    fn forget(&mut self) {
        self.fields.clear();
        self.frames.clear();
    }
}

/// What the engine reads for one frame: the newest field and the one before it, and for
/// a frame between two how far back and on its two lie and how many frames the
/// newest field spans.
#[derive(Clone, Copy)]
struct Trails<'a> {
    last: Option<&'a [Option<V>]>,
    older: Option<&'a [Option<V>]>,
    between: Option<(i32, i32, i32)>,
}

/// A vector for a part of the way: v * part / whole, to the nearest quarter pixel.
fn part_of(v: V, part: i32, whole: i32) -> V {
    let one = |c: i32| {
        let n = 2 * c * part;
        (n + whole.max(1) * n.signum()) / (2 * whole.max(1))
    };
    (one(v.0), one(v.1))
}

/// The frames a tile may be guessed from, as the player has them.
#[derive(Clone, Copy)]
struct From<'a> {
    before: Option<&'a [u8]>,
    after: Option<&'a [u8]>,
    stack: Option<&'a [u8]>,
    /// The vector most tiles of the frame have (the whole picture's move): what a
    /// tile's vector is told apart from when the tile to its left has none.
    whole: (i32, i32),
    /// The motion engine's memory, when it is used.
    trails: Option<Trails<'a>>,
    /// The last three frames others are guessed from (the oldest again where there are
    /// fewer), and the Neuras refiner, when it is used.
    refs: [Option<&'a [u8]>; 3],
    refine: Option<&'a Refiner>,
}

/// The four windows the refiner takes for the tile at (x0, y0): the same place in the
/// last three frames, and the motion engine's guess by the vector `v`.
fn windows_of(geo: &Geometry, at: &[usize], f: &From, x0: i32, y0: i32, v: V) -> Vec<u8> {
    let (side, edge) = (refiner::SIDE as i32, refiner::EDGE as i32);
    let mut w = Vec::with_capacity(refiner::WINDOWS * refiner::SIDE * refiner::SIDE);
    for r in f.refs {
        let p = pic(geo, at, r.expect("three frames for the refiner"), 0);
        for j in 0..side {
            for k in 0..side {
                w.push(p.at(x0 - edge + k, y0 - edge + j));
            }
        }
    }
    let bp = pic(geo, at, f.before.expect("a frame before"), 0);
    for j in 0..side {
        for k in 0..side {
            w.push(bp.quarter(4 * (x0 - edge + k) + v.0, 4 * (y0 - edge + j) + v.1) as u8);
        }
    }
    w
}

/// Tiles the refiner was offered to, and tiles that took it (for the report).
pub static REFINED: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// Where samples for training the refiner are written (the command's --dump), and of
/// how many tiles one is taken.
static DUMP: std::sync::Mutex<Option<std::fs::File>> = std::sync::Mutex::new(None);

pub fn dump_to(path: &std::path::Path) -> Result<(), String> {
    *DUMP.lock().unwrap() = Some(std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?);
    Ok(())
}

/// The guess of a block from the pixels just above it and just to its left. `get`
/// gives a pixel that is already rebuilt, or None where there is none.
fn from_neighbours(mode: u8, get: &dyn Fn(i32, i32) -> Option<i32>, bx: i32, by: i32) -> [i32; 64] {
    // A row or column that runs out of the picture is carried on by its last pixel.
    let line = |along: &dyn Fn(i32) -> Option<i32>| -> Option<[i32; 8]> {
        let mut l = [along(0)?; 8];
        for k in 1..8 {
            l[k] = along(k as i32).unwrap_or(l[k - 1]);
        }
        Some(l)
    };
    let above = line(&|k| get(bx + k, by - 1));
    let left = line(&|k| get(bx - 1, by + k));
    let both: Vec<i32> = above.iter().chain(left.iter()).flatten().copied().collect();
    let mean = if both.is_empty() { 128 } else { (both.iter().sum::<i32>() + both.len() as i32 / 2) / both.len() as i32 };
    let mut guess = [mean; 64];
    match (mode, above, left) {
        (1, Some(a), _) => {
            for j in 0..8 {
                guess[j * 8..j * 8 + 8].copy_from_slice(&a);
            }
        }
        (2, _, Some(l)) => {
            for j in 0..8 {
                guess[j * 8..j * 8 + 8].fill(l[j]);
            }
        }
        (3, Some(a), Some(l)) => {
            let corner = get(bx - 1, by - 1).unwrap_or(a[0]);
            for j in 0..8 {
                for k in 0..8 {
                    let base = a[k] + l[j] - corner;
                    let (da, dl, dc) = ((base - a[k]).abs(), (base - l[j]).abs(), (base - corner).abs());
                    guess[j * 8 + k] = if dl <= da && dl <= dc { l[j] } else if da <= dc { a[k] } else { corner };
                }
            }
        }
        _ => {}
    }
    guess
}

/// The guess for the 8 x 8 block at (bx, by) of plane `i`, in a tile whose corner in
/// that plane is (px0, py0).
#[allow(clippy::too_many_arguments)]
fn guess_of(geo: &Geometry, at: &[usize], f: &From, i: usize, (bx, by): (usize, usize), (px0, py0): (usize, usize), kind: &Kind, fine: Option<&[i32; 256]>) -> [i32; 64] {
    let mut guess = [128; 64];
    // A place in quarters of a light pixel is the same place in eighths of a colour pixel.
    let sample = |p: &Pic, x: usize, y: usize, dx: i32, dy: i32| if i == 0 { p.quarter(4 * x as i32 + dx, 4 * y as i32 + dy) } else { p.eighth(8 * x as i32 + dx, 8 * y as i32 + dy) };
    match kind {
        // A guess from the pixels around the block is made where the block is rebuilt (`from_neighbours`).
        Kind::Flat(_) => {}
        Kind::Stack => {
            let sp = pic(geo, at, f.stack.unwrap(), i);
            for j in 0..8 {
                for k in 0..8 {
                    guess[j * 8 + k] = sp.at((bx + k) as i32, (by + j) as i32) as i32;
                }
            }
        }
        Kind::Refined(v) => {
            let bp = pic(geo, at, f.before.unwrap(), i);
            for j in 0..8 {
                for k in 0..8 {
                    // The light is the refined tile; the colours are the motion engine's guess.
                    guess[j * 8 + k] = match (i, fine) {
                        (0, Some(fine)) => fine[(by - py0 + j) * 16 + bx - px0 + k],
                        _ => sample(&bp, bx + k, by + j, v.0, v.1),
                    };
                }
            }
        }
        Kind::Moved(v, _) => {
            let bp = pic(geo, at, f.before.unwrap(), i);
            for j in 0..8 {
                for k in 0..8 {
                    // Which quarter of the tile the sample is in.
                    let q = if i == 0 { ((by - py0) / 8) * 2 + (bx - px0) / 8 } else { (j / 4) * 2 + k / 4 };
                    guess[j * 8 + k] = sample(&bp, bx + k, by + j, v[q].0, v[q].1);
                }
            }
        }
        Kind::After(v) => {
            let ap = pic(geo, at, f.after.unwrap(), i);
            for j in 0..8 {
                for k in 0..8 {
                    guess[j * 8 + k] = sample(&ap, bx + k, by + j, v.0, v.1);
                }
            }
        }
        Kind::Between(d) => {
            let (bp, ap) = (pic(geo, at, f.before.unwrap(), i), pic(geo, at, f.after.unwrap(), i));
            for j in 0..8 {
                for k in 0..8 {
                    guess[j * 8 + k] = (sample(&bp, bx + k, by + j, -d.0, -d.1) + sample(&ap, bx + k, by + j, d.0, d.1) + 1) >> 1;
                }
            }
        }
    }
    guess
}

/// A miss as steps of its waves, coarse to fine.
fn count_steps(t: &Tables, miss: &[i32; 64], dead: i32, price: f64) -> [i32; 64] {
    let w = t.waves(miss);
    let mut steps = [0i32; 64];
    for (n, s) in steps.iter_mut().enumerate() {
        // A wave is counted in whole steps, rounded down but for `dead`: one that is not
        // nearly a step long is grain more often than picture.
        let (v, step) = (w[t.order[n]], t.step[n]);
        *s = v.signum() * ((v.abs() + step / dead) / step);
    }
    // Then each wave is weighed on its own, finest first: a step less, if what that
    // leaves wrong costs less than the bits it saves (a wave gone saves about five,
    // a step shorter about two).
    for n in (0..64).rev() {
        if steps[n] == 0 {
            continue;
        }
        let (v, step) = (w[t.order[n]] as f64, t.step[n] as f64);
        let less = steps[n] - steps[n].signum();
        let (wrong, wrong_less) = ((v - steps[n] as f64 * step).powi(2), (v - less as f64 * step).powi(2));
        if wrong_less - wrong < price * if less == 0 { 5.0 } else { 2.0 } {
            steps[n] = less;
        }
    }
    steps
}

/// What the steps put back onto the guess.
fn put_back(t: &Tables, steps: &[i32; 64]) -> [i32; 64] {
    if steps.iter().all(|s| *s == 0) {
        return [0; 64];
    }
    let mut w = [0i32; 64];
    for (n, s) in steps.iter().enumerate() {
        w[t.order[n]] = s * t.step[n];
    }
    t.samples(&w)
}

/// About how many bits a vector's difference takes.
fn vector_bits(d: (i32, i32)) -> u32 {
    let one = |c: i32| if c == 0 { 1 } else { 3 + 2 * c.unsigned_abs().ilog2() };
    one(d.0) + one(d.1)
}

/// The blocks of a tile, in the order they are written: (plane, corner of the block,
/// corner of the tile in that plane).
fn blocks_of(geo: &Geometry, tx: usize, ty: usize) -> Vec<(usize, (usize, usize), (usize, usize))> {
    let mut out = Vec::with_capacity(6);
    for (i, p) in geo.planes.iter().enumerate() {
        let size = TILE >> p.sx;
        let (px0, py0) = (tx * size, ty * size);
        for b in 0..(size / 8) * (size / 8) {
            let (bx, by) = (px0 + (b % (size / 8)) * 8, py0 + (b / (size / 8)) * 8);
            if bx < p.w && by < p.h {
                out.push((i, (bx, by), (px0, py0)));
            }
        }
    }
    out
}

/// The packer tries one way of guessing a tile: the steps it would keep for each
/// block, and what it weighs: the squares of what is still wrong, plus `price` for
/// every bit it would take. A block's waves are left out altogether when keeping them
/// costs more than it mends.
#[allow(clippy::too_many_arguments)]
fn weigh(geo: &Geometry, t: &Tables, at: &[usize], f: &From, src: &[u8], kind: &Kind, head_bits: u32, (tx, ty, ty0): (usize, usize, usize), bufs: &[&mut [u8]; 3], (dead, large): (i32, bool), price: f64) -> (f64, Vec<[i32; 64]>, Option<Box<[i32; 256]>>) {
    let mut cost = price * head_bits as f64;
    let mut all = Vec::with_capacity(6);
    // The light of the tile: what its four blocks weigh, their guesses and their misses.
    let (mut light_cost, mut light) = (0f64, Vec::new());
    // The blocks of this tile as this try would rebuild them (the next block is guessed from them).
    let mut tried: Vec<(usize, usize, usize, [u8; 64])> = Vec::new();
    let fine = if let Kind::Refined(v) = kind { Some(f.refine.expect("the refiner").refine(&windows_of(geo, at, f, (tx * TILE) as i32, (ty * TILE) as i32, *v))) } else { None };
    for (i, (bx, by), (px0, py0)) in blocks_of(geo, tx, ty) {
        let p = &geo.planes[i];
        let guess = if let Kind::Flat(mode) = kind {
            let band_top = ty0 * (TILE >> p.sy);
            let get = |x: i32, y: i32| -> Option<i32> {
                if x < 0 || y < 0 || x >= p.w as i32 || y >= p.h as i32 {
                    return None;
                }
                for (pi, tx0, ty0, px) in &tried {
                    if *pi == i && x >= *tx0 as i32 && x < *tx0 as i32 + 8 && y >= *ty0 as i32 && y < *ty0 as i32 + 8 {
                        return Some(px[(y as usize - ty0) * 8 + x as usize - tx0] as i32);
                    }
                }
                (y as usize >= band_top).then(|| bufs[i][(y as usize - band_top) * p.w + x as usize] as i32)
            };
            from_neighbours(*mode, &get, bx as i32, by as i32)
        } else {
            guess_of(geo, at, f, i, (bx, by), (px0, py0), kind, fine.as_ref())
        };
        let sp = pic(geo, at, src, i);
        let mut miss = [0i32; 64];
        for j in 0..8 {
            for k in 0..8 {
                miss[j * 8 + k] = sp.at((bx + k) as i32, (by + j) as i32) as i32 - guess[j * 8 + k];
            }
        }
        let mut steps = count_steps(t, &miss, dead, price);
        let wrong_without: i64 = miss.iter().map(|m| (*m as i64) * (*m as i64)).sum();
        let mut wrong = wrong_without as f64 + price;
        let mut back = [0i32; 64];
        if steps.iter().any(|s| *s != 0) {
            back = put_back(t, &steps);
            let with: i64 = (0..64).map(|n| { let e = (miss[n] - ((guess[n] + back[n]).clamp(0, 255) - guess[n])) as i64; e * e }).sum();
            let bits: u32 = 7 + steps.iter().filter(|s| **s != 0).map(|s| 3 + 2 * s.unsigned_abs().ilog2()).sum::<u32>();
            if (with as f64) + price * bits as f64 <= wrong {
                wrong = with as f64 + price * bits as f64;
            } else {
                steps = [0; 64];
                back = [0; 64];
            }
        }
        if matches!(kind, Kind::Flat(_)) {
            let mut px = [0u8; 64];
            for n in 0..64 {
                px[n] = (guess[n] + back[n]).clamp(0, 255) as u8;
            }
            tried.push((i, bx, by, px));
        }
        cost += wrong;
        if i == 0 {
            light_cost += wrong;
            light.push((bx - px0, by - py0, guess, miss));
        }
        all.push(steps);
    }
    // A tile whose light is whole (not at the picture's edge) and guessed from another
    // frame may keep its light as one block of 16 x 16, if that weighs less.
    let mut big = None;
    if large && light.len() == 4 && !matches!(kind, Kind::Flat(_)) {
        let (mut guess, mut miss) = ([0i32; 256], [0i32; 256]);
        for (dx, dy, g, m) in &light {
            for j in 0..8 {
                for k in 0..8 {
                    guess[(dy + j) * 16 + dx + k] = g[j * 8 + k];
                    miss[(dy + j) * 16 + dx + k] = m[j * 8 + k];
                }
            }
        }
        let steps = count_steps16(t, &miss, dead);
        let mut wrong: f64 = miss.iter().map(|m| (*m as f64) * (*m as f64)).sum::<f64>() + price;
        let mut kept = [0i32; 256];
        if steps.iter().any(|s| *s != 0) {
            let back = put_back16(t, &steps);
            let with: f64 = (0..256).map(|n| ((miss[n] - ((guess[n] + back[n]).clamp(0, 255) - guess[n])) as f64).powi(2)).sum();
            let bits: u32 = 9 + steps.iter().filter(|s| **s != 0).map(|s| 3 + 2 * s.unsigned_abs().ilog2()).sum::<u32>();
            if with + price * bits as f64 <= wrong {
                wrong = with + price * bits as f64;
                kept = steps;
            }
        }
        if wrong < light_cost {
            cost += wrong - light_cost;
            big = Some(Box::new(kept));
        }
    }
    (cost, all, big)
}

/// A tile of `n` x `n` light pixels cut from a frame, its edges held.
fn cut_tile(sp: &Pic, x0: i32, y0: i32, n: usize) -> Vec<u8> {
    (0..n * n).map(|k| sp.at(x0 + (k % n) as i32, y0 + (k / n) as i32)).collect()
}

/// One band of tile rows `ty0..ty1`: packed (with `src`) or rebuilt (without).
/// `bufs` are the band's rows of the three planes as the player has them.
#[allow(clippy::too_many_arguments)]
fn band(geo: &Geometry, t: &Tables, src: Option<&[u8]>, f: &From, how: &How, ty0: usize, ty1: usize, bufs: [&mut [u8]; 3], made: &Marks, mut book: Option<&mut Book>, field: &mut [Option<V>], check: &mut u64, c: &mut Coder) {
    let at = geo.offsets();
    let mut l = Learned::new();
    let across = geo.width.div_ceil(TILE);
    let dead = how.dead.max(1) as i32;
    // What a bit is worth in squares of error: more, the coarser the steps.
    let price = how.weigh * (t.step[0] * t.step[0]) as f64;
    // Which tiles of the row above were skipped (none above the band's first row).
    let mut above_skip = vec![false; across];
    for ty in ty0..ty1 {
        let mut left: Option<(i32, i32)> = None;
        let (mut any_left, mut left_stack, mut left_split, mut left_between, mut left_big) = (false, false, false, false, false);
        let (mut left_skip, mut left_refined) = (false, false);
        let mut left_models: Option<[Option<V>; 4]> = None;
        for tx in 0..across {
            let (x0, y0) = ((tx * TILE) as i32, (ty * TILE) as i32);
            // The engine's models for this tile's vector: the tile to the left (or the
            // whole picture's move); the same tile's vector last time (constant speed);
            // that carried on by how it changed (constant change of speed); and for a
            // frame between two, the part of the group's vector that falls to it.
            let spatial = left.unwrap_or(f.whole);
            let mut models: [Option<V>; 4] = [Some(spatial), None, None, None];
            let (mut guess_between, mut guess_after) = ((0, 0), (0, 0));
            if let Some(tr) = &f.trails {
                let last = tr.last.and_then(|fd| fd[ty * across + tx]);
                if let Some((back, on, span)) = tr.between {
                    models[3] = last.map(|v| part_of(v, back, span));
                    guess_between = last.map_or((0, 0), |v| part_of(v, back + on, 2 * span));
                    guess_after = last.map_or((0, 0), |v| part_of((-v.0, -v.1), on, span));
                } else {
                    models[1] = last;
                    models[2] = last.zip(tr.older.and_then(|fd| fd[ty * across + tx])).map(|(a, b)| (2 * a.0 - b.0, 2 * a.1 - b.1));
                }
            }
            // The model that was nearest for the tile to the left speaks for this one: only
            // what is already rebuilt decides, so the player picks the same.
            let mut pick = [3, 1, 0].into_iter().find(|g| models[*g].is_some()).unwrap_or(0);
            if let (Some(lv), Some(lm)) = (left, &left_models) {
                let mut nearest = i32::MAX;
                for g in [3, 1, 2, 0] {
                    if let (Some(was), Some(_)) = (lm[g], models[g]) {
                        let off = (was.0 - lv.0).abs() + (was.1 - lv.1).abs();
                        if off < nearest {
                            (nearest, pick) = (off, g);
                        }
                    }
                }
            }
            let from = models[pick].unwrap_or(spatial);
            if f.trails.is_some() {
                for v in [from, guess_between, guess_after] {
                    *check = (*check ^ (v.0 as u32 as u64) ^ ((v.1 as u32 as u64) << 32)).wrapping_mul(0x0000_0100_0000_01B3);
                }
            }
            // The packer tries the ways a tile can be guessed and keeps the one that weighs least.
            let mut want = Kind::Flat(0);
            let mut kept: Option<Vec<[i32; 64]>> = None;
            let mut kept_big: Option<Box<[i32; 256]>> = None;
            if let Some(src) = src {
                let mut ways: Vec<(Kind, u32)> = Vec::new();
                let sp = pic(geo, &at, src, 0);
                let s: [u8; 256] = cut_tile(&sp, x0, y0, 16).try_into().unwrap();
                if let Some(before) = f.before {
                    let bp = pic(geo, &at, before, 0);
                    let (dx, dy, moved) = find(&s, 16, &bp, x0, y0, (how.range as i32, how.still), from);
                    ways.push((Kind::Moved([(dx, dy); 4], false), 2 + vector_bits((dx - from.0, dy - from.1))));
                    if f.trails.is_some() && (dx, dy) != from {
                        // Exactly where the engine predicted: no vector to keep.
                        ways.push((Kind::Moved([from; 4], false), 1));
                    }
                    // A tile that is where it was (within a grey level a pixel) is not looked at further.
                    if moved > 256 {
                        // The tile split in four, each quarter with its own vector, found near the tile's.
                        let (mut vs, mut sum, mut bits, mut last) = ([(0, 0); 4], 0u32, 3u32, from);
                        for (q, v) in vs.iter_mut().enumerate() {
                            let (qx, qy) = (x0 + 8 * (q as i32 % 2), y0 + 8 * (q as i32 / 2));
                            let (vx, vy, unlike) = find(&cut_tile(&sp, qx, qy, 8), 8, &bp, qx, qy, (2, how.still / 4), (dx, dy));
                            *v = (vx, vy);
                            sum += unlike;
                            bits += vector_bits((vx - last.0, vy - last.1));
                            last = *v;
                        }
                        if vs.iter().any(|v| *v != vs[0]) && sum + how.still < moved {
                            ways.push((Kind::Moved(vs, true), bits));
                        }
                        if f.refine.is_some() {
                            // The same vector, and the guess refined by Neuras: one flag more.
                            ways.push((Kind::Refined((dx, dy)), 3 + vector_bits((dx - from.0, dy - from.1))));
                            REFINED[0].fetch_add(1, Ordering::Relaxed);
                        }
                        if let Some(after) = f.after {
                            let ap = pic(geo, &at, after, 0);
                            let d = find_between(&s, &bp, &ap, x0, y0);
                            ways.push((Kind::Between(d), 3 + vector_bits((d.0 - guess_between.0, d.1 - guess_between.1))));
                            let (ax, ay, _) = find(&s, 16, &ap, x0, y0, (how.range as i32, how.still), (-dx, -dy));
                            ways.push((Kind::After((ax, ay)), 4 + vector_bits((ax - guess_after.0, ay - guess_after.1))));
                            if f.trails.is_some() {
                                if d != guess_between {
                                    ways.push((Kind::Between(guess_between), 3));
                                }
                                if (ax, ay) != guess_after {
                                    ways.push((Kind::After(guess_after), 4));
                                }
                            }
                        }
                        if f.stack.is_some() {
                            ways.push((Kind::Stack, 3));
                        }
                        for mode in 0..4 {
                            ways.push((Kind::Flat(mode), 5));
                        }
                    }
                } else {
                    for mode in 0..4 {
                        ways.push((Kind::Flat(mode), 2));
                    }
                }
                let mut least = f64::MAX;
                for (kind, bits) in &ways {
                    let (cost, steps, big) = weigh(geo, t, &at, f, src, kind, *bits, (tx, ty, ty0), &bufs, (dead, how.large), price);
                    if cost < least {
                        (least, want, kept, kept_big) = (cost, *kind, Some(steps), big);
                    }
                }
            }
            if matches!(want, Kind::Refined(_)) {
                REFINED[1].fetch_add(1, Ordering::Relaxed);
            }
            // A sample for training the refiner: a tile guessed by one vector that still
            // needed corrections, with its windows and the real window.
            if let (Some(src), Kind::Moved(v, false), true, None) = (src, want, f.refs[0].is_some(), f.after) {
                if (tx + 3 * ty) % 5 == 0 && kept.as_ref().is_some_and(|k| k.iter().take(4).any(|b| b.iter().any(|s| *s != 0))) {
                    if let Some(file) = DUMP.lock().unwrap().as_mut() {
                        use std::io::Write;
                        let mut sample = windows_of(geo, &at, f, x0, y0, v[0]);
                        let sp = pic(geo, &at, src, 0);
                        let (side, edge) = (refiner::SIDE as i32, refiner::EDGE as i32);
                        for j in 0..side {
                            for k in 0..side {
                                sample.push(sp.at(x0 - edge + k, y0 - edge + j));
                            }
                        }
                        let _ = file.write_all(&sample);
                    }
                }
            }
            // The tile's kind and vectors: written, or read.
            spending(0);
            let mut tile_waves = false;
            let mut kind = Kind::Flat(0);
            // Skipped: the picture where the engine predicted it, and nothing put right.
            let want_skip = want == Kind::Moved([from; 4], false) && kept_big.is_none() && kept.as_ref().is_some_and(|k| k.iter().all(|b| b.iter().all(|s| *s == 0)));
            let skip = f.trails.is_some() && f.before.is_some() && c.bit(&mut l.skip[left_skip as usize + above_skip[tx] as usize], want_skip);
            if skip {
                kind = Kind::Moved([from; 4], false);
            }
            if f.before.is_some() && !skip {
                let ctx = if !any_left { 2 } else { left.is_some() as usize };
                if c.bit(&mut l.moved[ctx], matches!(want, Kind::Moved(..) | Kind::Refined(_))) {
                    let (wv, wsplit) = match want {
                        Kind::Moved(v, s) => (v, s),
                        Kind::Refined(v) => ([v; 4], false),
                        _ => ([(0, 0); 4], false),
                    };
                    let split = c.bit(&mut l.split[left_split as usize], wsplit);
                    let (mut vs, mut last) = ([(0, 0); 4], from);
                    spending(1);
                    for q in 0..if split { 4 } else { 1 } {
                        vs[q] = (last.0 + c.signed(&mut l.vector[0], wv[q].0 - last.0), last.1 + c.signed(&mut l.vector[1], wv[q].1 - last.1));
                        last = vs[q];
                    }
                    spending(0);
                    if !split {
                        vs = [vs[0]; 4];
                    }
                    kind = Kind::Moved(vs, split);
                    // Is the guess refined by Neuras?
                    if !split && f.refine.is_some() && c.bit(&mut l.refined[left_refined as usize], matches!(want, Kind::Refined(_))) {
                        kind = Kind::Refined(vs[0]);
                    }
                } else if f.after.is_some() && c.bit(&mut l.between[left_between as usize], matches!(want, Kind::Between(_))) {
                    let wd = if let Kind::Between(d) = want { d } else { (0, 0) };
                    spending(1);
                    kind = Kind::Between((guess_between.0 + c.signed(&mut l.vector[0], wd.0 - guess_between.0), guess_between.1 + c.signed(&mut l.vector[1], wd.1 - guess_between.1)));
                    spending(0);
                } else if f.after.is_some() && c.bit(&mut l.after, matches!(want, Kind::After(_))) {
                    let wv = if let Kind::After(v) = want { v } else { (0, 0) };
                    spending(1);
                    kind = Kind::After((guess_after.0 + c.signed(&mut l.vector[0], wv.0 - guess_after.0), guess_after.1 + c.signed(&mut l.vector[1], wv.1 - guess_after.1)));
                    spending(0);
                } else if f.stack.is_some() && c.bit(&mut l.stack[left_stack as usize], want == Kind::Stack) {
                    kind = Kind::Stack;
                }
            }
            if matches!(kind, Kind::Flat(_)) {
                // Which of the four guesses from the pixels around: two yes/no.
                let wm = if let Kind::Flat(m) = want { m } else { 0 };
                let high = c.bit(&mut l.around[0], wm >= 2);
                let low = c.bit(&mut l.around[1 + high as usize], wm & 1 == 1);
                kind = Kind::Flat((high as u8) << 1 | low as u8);
            }
            let fine = if let Kind::Refined(v) = kind { Some(f.refine.expect("the refiner").refine(&windows_of(geo, &at, f, x0, y0, v))) } else { None };
            // The light of a whole tile guessed from another frame: as one block of 16 x 16?
            let whole_tile = x0 as usize + TILE <= geo.width && y0 as usize + TILE <= geo.height;
            spending(0);
            let big = !skip && !matches!(kind, Kind::Flat(_)) && whole_tile && c.bit(&mut l.big[left_big as usize], kept_big.is_some());
            if big {
                let (p, band_top) = (&geo.planes[0], ty0 * TILE);
                let mut guess = [0i32; 256];
                for b in 0..4 {
                    let (dx, dy) = ((b % 2) * 8, (b / 2) * 8);
                    let g = guess_of(geo, &at, f, 0, (x0 as usize + dx, y0 as usize + dy), (x0 as usize, y0 as usize), &kind, fine.as_ref());
                    for j in 0..8 {
                        guess[(dy + j) * 16 + dx..(dy + j) * 16 + dx + 8].copy_from_slice(&g[j * 8..j * 8 + 8]);
                    }
                }
                let mut steps = kept_big.map_or([0i32; 256], |k| *k);
                spending(2);
                block16(c, &mut l, t, &mut steps);
                let waves_left = steps.iter().any(|s| *s != 0);
                tile_waves |= waves_left;
                let same = match kind {
                    Kind::Stack => true,
                    Kind::Moved(v, _) => v.iter().all(|v| *v == (0, 0)),
                    _ => false,
                };
                if waves_left || !same {
                    for b in 0..4 {
                        made[0][((y0 as usize + (b / 2) * 8) / 8) * p.w.div_ceil(8) + (x0 as usize + (b % 2) * 8) / 8].store(true, Ordering::Relaxed);
                    }
                }
                let back = put_back16(t, &steps);
                let buf = &mut *bufs[0];
                for j in 0..16 {
                    for k in 0..16 {
                        buf[(y0 as usize + j - band_top) * p.w + x0 as usize + k] = (guess[j * 16 + k] + back[j * 16 + k]).clamp(0, 255) as u8;
                    }
                }
            }
            for (n, (i, (bx, by), (px0, py0))) in blocks_of(geo, tx, ty).into_iter().enumerate() {
                if big && i == 0 {
                    continue;
                }
                let p = &geo.planes[i];
                let band_top = ty0 * (TILE >> p.sy);
                let guess = if let Kind::Flat(mode) = kind {
                    let buf = &bufs[i][..];
                    let get = |x: i32, y: i32| -> Option<i32> { (x >= 0 && x < p.w as i32 && y >= band_top as i32 && y < p.h as i32).then(|| buf[(y as usize - band_top) * p.w + x as usize] as i32) };
                    from_neighbours(mode, &get, bx as i32, by as i32)
                } else {
                    guess_of(geo, &at, f, i, (bx, by), (px0, py0), &kind, fine.as_ref())
                };
                let mut steps = kept.as_ref().map_or([0i32; 64], |k| k[n]);
                if skip {
                    steps = [0; 64];
                } else {
                    block(c, &mut l, t, &mut book, (i > 0) as usize, !matches!(kind, Kind::Flat(_)) as usize, &mut steps);
                }
                let waves_left = steps.iter().any(|s| *s != 0);
                tile_waves |= waves_left;
                // A block that is only the same block of the frame before is not made anew.
                let same = match kind {
                    Kind::Stack => true,
                    Kind::Moved(v, _) => v.iter().all(|v| *v == (0, 0)),
                    _ => false,
                };
                if waves_left || !same {
                    made[i][(by / 8) * p.w.div_ceil(8) + bx / 8].store(true, Ordering::Relaxed);
                }
                let back = put_back(t, &steps);
                let buf = &mut *bufs[i];
                for j in 0..8.min(p.h - by) {
                    for k in 0..8.min(p.w - bx) {
                        buf[(by + j - band_top) * p.w + bx + k] = (guess[j * 8 + k] + back[j * 8 + k]).clamp(0, 255) as u8;
                    }
                }
            }
            left = match kind {
                Kind::Moved(v, _) => Some(v[1]),
                Kind::Refined(v) => Some(v),
                _ => None,
            };
            left_refined = matches!(kind, Kind::Refined(_));
            left_split = matches!(kind, Kind::Moved(_, true));
            left_stack = kind == Kind::Stack;
            if src.is_some() {
                count(6, 1);
                if !tile_waves && !matches!(kind, Kind::Flat(_)) {
                    count(7, 1);
                }
            }
            left_skip = skip;
            above_skip[tx] = skip;
            left_models = Some(models);
            field[(ty - ty0) * across + tx] = match kind {
                Kind::Moved(v, _) => Some(v[0]),
                Kind::Refined(v) => Some(v),
                _ => None,
            };
            left_between = matches!(kind, Kind::Between(_));
            left_big = big;
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
/// The frame lies between two others and was guessed from both: it is rebuilt after
/// the frame after it.
pub const BETWEEN: u8 = 32;

pub fn between(payload: &[u8]) -> bool {
    payload.first().is_some_and(|f| f & BETWEEN != 0)
}

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
const HEAD: usize = 9;
/// After the frame is rebuilt its brightness and colours are put right level by level.
const PUT_RIGHT: u8 = 64;
/// The frame was packed with the Predictive Motion Engine.
const PREDICT: u8 = 128;
/// Tiles of the frame may have their guess refined by Neuras.
const REFINER: u8 = 1;

/// The vector most tiles of the frame have: every third tile each way is looked for in
/// the frame before, and the commonest answer is taken.
fn whole_move(geo: &Geometry, frame: &[u8], before: &[u8], how: &How) -> (i32, i32) {
    let at = geo.offsets();
    let (sp, bp) = (pic(geo, &at, frame, 0), pic(geo, &at, before, 0));
    let found: Vec<(i32, i32)> = (0..geo.height.div_ceil(TILE))
        .into_par_iter()
        .step_by(3)
        .flat_map_iter(|ty| {
            let (sp, bp) = (sp, bp);
            (0..geo.width.div_ceil(TILE)).step_by(3).map(move |tx| {
                let (x0, y0) = ((tx * TILE) as i32, (ty * TILE) as i32);
                let v = find(&cut_tile(&sp, x0, y0, 16), 16, &bp, x0, y0, (how.range as i32, how.still), (0, 0));
                (v.0, v.1)
            })
        })
        .collect();
    let mut count: std::collections::BTreeMap<(i32, i32), usize> = Default::default();
    for v in found {
        *count.entry(v).or_default() += 1;
    }
    let (mut most, mut n) = ((0, 0), 0);
    for (v, k) in count {
        if k > n {
            (most, n) = (v, k);
        }
    }
    (most.0.clamp(-127, 127), most.1.clamp(-127, 127))
}

/// How far, on the whole, the rebuilt frame's samples of each level (32 spans of 8
/// levels, in each plane) are from the movie's: the packer measures it, and the frame
/// is put right by it on both sides before anything is guessed from it.
fn levels_off(geo: &Geometry, real: &[u8], rebuilt: &[u8]) -> [[i8; 32]; 3] {
    let mut off = [[0i8; 32]; 3];
    for (i, (p, at)) in geo.planes.iter().zip(geo.offsets()).enumerate().take(3) {
        let mut sum = [(0i64, 0i64); 32];
        for (r, v) in real[at..at + p.len()].iter().zip(&rebuilt[at..at + p.len()]) {
            sum[(*v >> 3) as usize].0 += *r as i64 - *v as i64;
            sum[(*v >> 3) as usize].1 += 1;
        }
        for (o, s) in off[i].iter_mut().zip(sum) {
            if s.1 > 0 {
                *o = ((2 * s.0 + s.1.signum() * s.0.signum() * s.1) / (2 * s.1)).clamp(-8, 8) as i8;
            }
        }
    }
    off
}

fn put_right(geo: &Geometry, frame: &mut [u8], off: &[[i8; 32]; 3]) {
    for (i, (p, at)) in geo.planes.iter().zip(geo.offsets()).enumerate().take(3) {
        frame[at..at + p.len()].par_chunks_mut(4096).for_each(|part| {
            for v in part {
                *v = (*v as i32 + off[i][(*v >> 3) as usize] as i32).clamp(0, 255) as u8;
            }
        });
    }
}

/// For a frame between two: how many frames back and how many frames on the two lie.
pub fn refs(payload: &[u8]) -> (usize, usize) {
    (payload.get(5).copied().unwrap_or(0) as usize, payload.get(6).copied().unwrap_or(0) as usize)
}

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
    /// What a bit is worth against the squares of what is still wrong, for a flattest
    /// step one long (it grows with the square of the step).
    pub weigh: f64,
    /// How much longer the steps of a frame between two are.
    pub between: f64,
    /// With --between: how many frames a group is. The last of a group is guessed from
    /// the last of the group before; the others lie between two, the middle one first.
    pub group: usize,
    /// The packer may keep a tile's light as one block of 16 x 16.
    pub large: bool,
    /// The packer may have a tile's guess refined by Neuras (the command's --neuras).
    pub refine: bool,
}

/// Packs a frame in AV1's way. `before` is the frame before as the player
/// rebuilt it. Returns the bytes (flags, bands, coarse, slope, each band's
/// length, the bands) and the frame as the player will rebuild it.
#[allow(clippy::too_many_arguments)]
pub fn pack(geo: &Geometry, frame: &[u8], before: Option<&[u8]>, after: Option<&[u8]>, stack: Option<&[u8]>, bands: usize, coarse: u8, how: &How, books: Option<&mut Vec<Book>>, refs: (u8, u8), mut motion: Option<&mut Motion>) -> (Vec<u8>, Vec<u8>) {
    let after = after.filter(|_| before.is_some());
    // The whole picture's move, for a frame guessed from the one before it alone.
    let whole = match (before, after) {
        (Some(b), None) => whole_move(geo, frame, b, how),
        _ => (0, 0),
    };
    let predicts = motion.is_some();
    if before.is_none() {
        // A frame that stands alone starts the film or follows a cut: the history is forgotten.
        if let Some(m) = motion.as_deref_mut() {
            m.forget();
        }
    }
    let trails = motion.as_deref().filter(|_| before.is_some()).map(|m| Trails {
        last: m.fields.front().map(|f| &f[..]),
        older: if after.is_some() { None } else { m.fields.get(1).map(|f| &f[..]) },
        between: after.map(|_| (refs.0 as i32, refs.1 as i32, m.span)),
    });
    // The last three frames others are guessed from, and the refiner, for a frame guessed
    // from the one before it alone.
    let last_three: [Option<&[u8]>; 3] = match motion.as_deref() {
        Some(m) if !m.frames.is_empty() => std::array::from_fn(|i| m.frames.get(i).or(m.frames.back()).map(|f| &f[..])),
        _ => [None; 3],
    };
    let refine = if how.refine && before.is_some() && after.is_none() && last_three[0].is_some() { refiner::shipped() } else { None };
    let from = From { before, after, stack: stack.filter(|s| before.is_some() && s.len() == frame.len()), whole, trails, refs: last_three, refine };
    let plain = PLAIN_BITS.load(Ordering::Relaxed);
    let books = if plain { None } else { books };
    let with_books = books.is_some();
    let four = with_books && FOUR_ONLY.load(Ordering::Relaxed);
    // The top frame is what every still tile after it is a copy of: it is kept finer.
    let coarse = if before.is_none() { (coarse / 3).max(1) } else { coarse };
    let t = Tables::new(coarse, how.slope);
    let rows = band_rows(geo, bands);
    let tile_rows = geo.height.div_ceil(TILE);
    let mut rebuilt = vec![0u8; geo.frame_len()];
    let made = marks(geo);
    let work = cut(geo, &mut rebuilt, rows);
    let places = slots(books, work.len(), four);
    let across = geo.width.div_ceil(TILE);
    let mut field: Vec<Option<V>> = vec![None; across * tile_rows];
    let coded: Vec<(Vec<u8>, u64)> = work
        .into_par_iter()
        .zip(places)
        .zip(field.par_chunks_mut(across * rows / TILE))
        .map(|(((ty0, bufs), book), field)| {
            let mut c = if plain { Coder::Bits(Vec::new(), 0) } else { Coder::Write(Encoder::new()) };
            let mut check = 0u64;
            band(geo, &t, Some(frame), &from, how, ty0, (ty0 + rows / TILE).min(tile_rows), bufs, &made, book, field, &mut check, &mut c);
            (
                match c {
                    Coder::Write(e) => e.finish(),
                    Coder::Bits(bytes, _) => bytes,
                    _ => unreachable!(),
                },
                check,
            )
        })
        .collect();
    let checks: Vec<u64> = coded.iter().map(|c| c.1).collect();
    let done: Vec<Vec<u8>> = coded.into_iter().map(|c| c.0).collect();
    let refined = refine.is_some();
    smooth_seams(geo, &mut rebuilt, coarse, &made);
    let off = levels_off(geo, frame, &rebuilt);
    let any_off = off.iter().flatten().any(|o| *o != 0);
    if any_off {
        put_right(geo, &mut rebuilt, &off);
    }
    if let Some(m) = motion {
        remember(m, field, &checks, before.is_some(), after.is_some(), refs.0, &rebuilt);
    }
    let mut out = vec![(if refined { REFINER } else { 0 }) | (if any_off { PUT_RIGHT } else { 0 }) | (if before.is_some() { FROM_BEFORE } else { 0 }) | (if with_books { BOOKS } else { 0 }) | (if four { FOUR } else { 0 }) | (if plain { PLAIN } else { 0 }) | (if after.is_some() { BETWEEN } else { 0 }) | (if predicts { PREDICT } else { 0 })];
    out.extend((bands as u16).to_le_bytes());
    out.push(coarse);
    out.push(how.slope);
    out.push(refs.0);
    out.push(refs.1);
    out.push(whole.0 as i8 as u8);
    out.push(whole.1 as i8 as u8);
    if any_off {
        out.extend(off.iter().flatten().map(|o| *o as u8));
    }
    for d in &done {
        out.extend((d.len() as u32).to_le_bytes());
    }
    for d in &done {
        out.extend(d);
    }
    (out, rebuilt)
}

/// After a frame the engine remembers: its checks are folded in, and the vectors of a
/// frame that others are guessed from become the newest field.
fn remember(m: &mut Motion, field: Vec<Option<V>>, checks: &[u64], has_before: bool, is_between: bool, span: u8, rebuilt: &[u8]) {
    for c in checks {
        m.check = (m.check ^ c).wrapping_mul(0x0000_0100_0000_01B3);
    }
    if !is_between {
        m.frames.push_front(rebuilt.to_vec());
        m.frames.truncate(3);
    }
    if has_before && !is_between {
        m.fields.push_front(field);
        m.fields.truncate(m.history);
        m.span = span.max(1) as i32;
    }
}

/// The frame as the player rebuilds it.
pub fn unpack(geo: &Geometry, payload: &[u8], before: Option<&[u8]>, after: Option<&[u8]>, stack: Option<&[u8]>, books: &mut Vec<Book>, motion: &mut Motion) -> Result<Vec<u8>, String> {
    if payload.len() < HEAD || !fits(geo) {
        return Err("not a frame of blocks".into());
    }
    let before = if from_before(payload) { Some(before.filter(|b| b.len() == geo.frame_len()).ok_or("a frame of blocks was rebuilt without the frame before it")?) } else { None };
    let after = if between(payload) { Some(after.filter(|a| a.len() == geo.frame_len()).ok_or("a frame between two was rebuilt without the frame after it")?) } else { None };
    let predicts = payload[0] & PREDICT != 0;
    if predicts && before.is_none() {
        motion.forget();
    }
    let (back, on) = refs(payload);
    let trails = (predicts && before.is_some()).then(|| Trails {
        last: motion.fields.front().map(|f| &f[..]),
        older: if after.is_some() { None } else { motion.fields.get(1).map(|f| &f[..]) },
        between: after.map(|_| (back as i32, on as i32, motion.span)),
    });
    let last_three: [Option<&[u8]>; 3] = if predicts && !motion.frames.is_empty() { std::array::from_fn(|i| motion.frames.get(i).or(motion.frames.back()).map(|f| &f[..])) } else { [None; 3] };
    let refine = if payload[0] & REFINER != 0 { Some(refiner::shipped().ok_or("this frame's guesses were refined by Neuras, and this player has no refiner")?) } else { None };
    let from = From { before, after, stack: stack.filter(|s| before.is_some() && s.len() == geo.frame_len()), whole: (payload[7] as i8 as i32, payload[8] as i8 as i32), trails, refs: last_three, refine };
    // The levels to put right, if the frame has them, lie before the bands' lengths.
    let mut off = [[0i8; 32]; 3];
    let mut head = HEAD;
    if payload[0] & PUT_RIGHT != 0 {
        let bytes = payload.get(HEAD..HEAD + 96).ok_or("a frame of blocks is cut")?;
        for (o, b) in off.iter_mut().flatten().zip(bytes) {
            *o = *b as i8;
        }
        head += 96;
    }
    let how = How { range: 0, slope: payload[4], dead: 1, still: 0, weigh: 0.0, between: 1.0, group: 1, large: false, refine: false };
    let (bands, t) = (u16::from_le_bytes([payload[1], payload[2]]) as usize, Tables::new(payload[3], payload[4]));
    let rows = band_rows(geo, bands);
    let tile_rows = geo.height.div_ceil(TILE);
    let mut out = vec![0u8; geo.frame_len()];
    let made = marks(geo);
    let work = cut(geo, &mut out, rows);
    let mut data = head + 4 * work.len();
    let mut coded = Vec::new();
    for i in 0..work.len() {
        let len = payload.get(head + 4 * i..head + 4 * i + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize).ok_or("a frame of blocks is cut")?;
        coded.push(payload.get(data..data + len).ok_or("a frame of blocks is cut")?);
        data += len;
    }
    let places = slots(if payload[0] & BOOKS != 0 { Some(books) } else { None }, work.len(), payload[0] & FOUR != 0);
    let across = geo.width.div_ceil(TILE);
    let mut field: Vec<Option<V>> = vec![None; across * tile_rows];
    let checks: Vec<u64> = work
        .into_par_iter()
        .zip(coded)
        .zip(places)
        .zip(field.par_chunks_mut(across * rows / TILE))
        .map(|((((ty0, bufs), bytes), book), field)| {
            let mut c = if payload[0] & PLAIN != 0 { Coder::ReadBits(bytes, 0) } else { Coder::Read(Decoder::new(bytes)) };
            let mut check = 0u64;
            band(geo, &t, None, &from, &how, ty0, (ty0 + rows / TILE).min(tile_rows), bufs, &made, book, field, &mut check, &mut c);
            check
        })
        .collect();
    let (has_before, is_between) = (before.is_some(), after.is_some());
    smooth_seams(geo, &mut out, payload[3], &made);
    if payload[0] & PUT_RIGHT != 0 {
        put_right(geo, &mut out, &off);
    }
    if predicts {
        remember(motion, field, &checks, has_before, is_between, back as u8, &out);
    }
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
        let how = How { range: 7, slope: 32, dead: 12, still: 256, weigh: 0.1, between: 1.0, group: 2, large: true, refine: false };
        for (coarse, with) in [(1u8, false), (6, true), (20, true), (6, false)] {
            let (mut packer, mut player): (Vec<Book>, Vec<Book>) = (Vec::new(), Vec::new());
            let (mut engine, mut engine_of_player) = (Motion::new(4), Motion::new(4));
            let (first, a2) = pack(&geo, &a, None, None, None, 3, coarse, &how, with.then_some(&mut packer), (0, 0), with.then_some(&mut engine));
            assert_eq!(unpack(&geo, &first, None, None, None, &mut player, &mut engine_of_player).unwrap(), a2);
            let (second, b2) = pack(&geo, &b, Some(&a2), None, Some(&a2), 3, coarse, &how, with.then_some(&mut packer), (2, 0), with.then_some(&mut engine));
            assert_eq!(unpack(&geo, &second, Some(&a2), None, Some(&a2), &mut player, &mut engine_of_player).unwrap(), b2);
            // A frame between the two: it is guessed from both.
            let m: Vec<u8> = a.iter().zip(&b).map(|(x, y)| ((*x as u16 + *y as u16) / 2) as u8).collect();
            let (third, m2) = pack(&geo, &m, Some(&a2), Some(&b2), Some(&b2), 3, coarse, &how, with.then_some(&mut packer), (1, 1), with.then_some(&mut engine));
            assert_eq!(refs(&third), (1, 1));
            assert!(between(&third));
            assert_eq!(unpack(&geo, &third, Some(&a2), Some(&b2), Some(&b2), &mut player, &mut engine_of_player).unwrap(), m2);
            // One more frame guessed from `b`, so that the engine has a history to carry on.
            let c3: Vec<u8> = b.iter().map(|v| v.wrapping_add(1)).collect();
            let (fourth, c2) = pack(&geo, &c3, Some(&b2), None, Some(&b2), 3, coarse, &how, with.then_some(&mut packer), (2, 0), with.then_some(&mut engine));
            assert_eq!(unpack(&geo, &fourth, Some(&b2), None, Some(&b2), &mut player, &mut engine_of_player).unwrap(), c2);
            // The player's engine predicted exactly what the packer's did, all the way.
            assert_eq!(engine.check, engine_of_player.check, "the two engines predicted the same vectors");
            if with {
                assert_ne!(engine.check, Motion::new(4).check, "the engine did predict");
            }
        }
    }
}
