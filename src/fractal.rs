//! FRACTAL MODE: a frame as rules that make it out of itself and out of the
//! frames before it. No vectors-and-waves: a rule is all there is.
//!
//! A frame is cut into RANGE blocks: 16 x 16 where one rule does, split in
//! four down to 4 x 4 where it does not. Each range is made by one rule:
//!
//! - `skip`: the same place in the frame before, as it is;
//! - `moved`: a block of the same size in one of the last two rebuilt
//!   frames, a few pixels away, its contrast scaled by s and its brightness
//!   shifted by o;
//! - `own`: a DOMAIN block of this same frame, twice the size, made half
//!   as large, turned or flipped one of 8 ways, scaled by s (always less
//!   than 1, so that the rules settle) and shifted by o: the classic
//!   fractal rule (Jacquin);
//! - `flat`: one value.
//!
//! range = s * (block - its mean) + o, in whole numbers (s in 16ths), so
//! that every player gets the same. For a block of another frame o is a
//! shift on the block's own mean; for a domain it is the range's own
//! brightness, so nothing of where the player started is left.
//!
//! **The player** starts a frame that stands alone from flat grey and
//! applies the rules 10 times; any other frame starts from the frame before
//! and applies them 3 times (the rules that read other frames need only
//! once).
//!
//! **The wave** (Devil's Clockwork's, `--wave N`): when the rules have made
//! the frame, a wave starts at every seam of a range made anew (its
//! sources) and moves out a pixel a tick, weaker each tick; the pixels on
//! the two sides of a seam pull each other level, all at once. A step of N
//! or more across a seam is a real edge of the picture: there the two sides
//! cancel and nothing moves. The wave is the player's: it costs no byte
//! but the two that say how it runs. The packer rebuilds with it, so the
//! next frame is made from the frame as the wave left it.
//!
//! **HotMotor does the search.** For each range the decisions are taken in
//! order: where from (the four sources, each with its scores in
//! rules.toml), which place, which turn, what contrast and brightness. At
//! each decision the variants are judged and the beam keeps the best few:
//! a quick judge first (how unlike the block is; whether a domain has
//! contrast enough; whether its bright and dark quarters lie as the
//! range's do), then the exact judge: the squares of what is still wrong
//! plus `lambda` for every bit of the rule. A range is split when its four
//! quarters weigh less together than it does whole.
//!
//! **HotMotor packs the rules.** For each frame it chooses how the rules
//! are written (vectors plain or told from the rule before; brightness
//! plain or told from the rule before) by writing them each way and
//! keeping the smallest: `motor::search_over` with a judge that measures
//! bytes.
//!
//! **Strike** keeps the result: the frame's rules lie in Nimbus under their
//! hash, and the lightning brands the pack by everything that decides it,
//! so a frame searched once is struck the next time (see `main.rs`).

use crate::guess::{Decoder, Encoder};
use crate::motor::{self, Rules};
use crate::pack::Geometry;
use rayon::prelude::*;
use std::sync::atomic::{AtomicU64, Ordering};

const ROOT: usize = 16;
const HALF: u16 = 1 << 11;

/// Thousandths of a bit spent on: the quadtree and the kinds of rule; domain places
/// and vectors; turns; contrast and brightness.
pub static SPENT: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
/// Ranges by rule: skip, moved, own, flat.
pub static RANGES: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

#[derive(Clone, Copy, Debug, PartialEq)]
enum Rule {
    Skip,
    /// From a rebuilt frame (`older`: the one before the last), (dx, dy) pixels away.
    Moved { older: bool, dx: i32, dy: i32, s: i32, o: [i32; 3] },
    /// From this frame at half size, (dx, dy) half-size pixels from the range's own
    /// place there, turned `turn`.
    Own { dx: i32, dy: i32, turn: u8, s: i32, o: [i32; 3] },
    Flat([i32; 3]),
}

#[derive(Clone, Debug)]
enum Node {
    Leaf(Rule),
    Split(Box<[Option<Node>; 4]>),
}

/// One plane: its samples, width and height. A place outside it is its nearest edge.
#[derive(Clone, Copy)]
struct Pl<'a> {
    d: &'a [u8],
    w: usize,
    h: usize,
}

impl Pl<'_> {
    #[inline(always)]
    fn at(&self, x: i32, y: i32) -> i32 {
        self.d[y.clamp(0, self.h as i32 - 1) as usize * self.w + x.clamp(0, self.w as i32 - 1) as usize] as i32
    }
    /// The block of `w` x `h` samples at (x, y).
    fn block(&self, x: i32, y: i32, w: usize, h: usize) -> Vec<i32> {
        let mut out = Vec::with_capacity(w * h);
        for j in 0..h as i32 {
            for i in 0..w as i32 {
                out.push(self.at(x + i, y + j));
            }
        }
        out
    }
}

fn planes<'a>(geo: &Geometry, f: &'a [u8]) -> [Pl<'a>; 3] {
    let at = geo.offsets();
    std::array::from_fn(|i| Pl { d: &f[at[i]..at[i] + geo.planes[i].len()], w: geo.planes[i].w, h: geo.planes[i].h })
}

/// A plane at half size: each sample the mean of four.
fn halved(p: &Pl) -> (Vec<u8>, usize, usize) {
    let (w, h) = ((p.w / 2).max(1), (p.h / 2).max(1));
    let mut out = vec![0u8; w * h];
    for y in 0..h {
        for x in 0..w {
            let (x0, y0) = (2 * x as i32, 2 * y as i32);
            out[y * w + x] = ((p.at(x0, y0) + p.at(x0 + 1, y0) + p.at(x0, y0 + 1) + p.at(x0 + 1, y0 + 1) + 2) / 4) as u8;
        }
    }
    (out, w, h)
}

/// Where sample (i, j) of a block `n` wide comes from in a block turned `turn` (8 ways).
#[inline(always)]
fn turned(turn: u8, n: usize, i: usize, j: usize) -> (usize, usize) {
    let m = n - 1;
    match turn {
        0 => (i, j),
        1 => (j, m - i),
        2 => (m - i, m - j),
        3 => (m - j, i),
        4 => (m - i, j),
        5 => (i, m - j),
        6 => (j, i),
        _ => (m - j, m - i),
    }
}

/// The range a rule makes out of a block: s (16ths) times (the block minus its mean),
/// plus o. For a block of another frame its mean is added too (o is a shift); for a
/// domain of the frame itself it is not (`own`: o is the range's own brightness), so
/// that nothing of the start is left after a few rounds.
fn made(d: &[i32], s: i32, o: i32, own: bool) -> Vec<i32> {
    let n = d.len() as i32;
    let mean = (d.iter().sum::<i32>() + n / 2) / n.max(1);
    let base = if own { o } else { mean + o };
    d.iter().map(|v| (((s * (v - mean) + 8) >> 4) + base).clamp(0, 255)).collect()
}

fn wrong(a: &[i32], b: &[i32]) -> f64 {
    a.iter().zip(b).map(|(x, y)| ((x - y) * (x - y)) as f64).sum()
}

/// The best contrast (in 16ths, within `span`) and brightness for making `r` out of `d`.
fn fit(r: &[i32], d: &[i32], span: (i32, i32), own: bool) -> (i32, i32) {
    let n = r.len() as f64;
    let (rm, dm) = (r.iter().sum::<i32>() as f64 / n, ((d.iter().sum::<i32>() + d.len() as i32 / 2) / d.len() as i32) as f64);
    let (mut up, mut down) = (0f64, 0f64);
    for (x, y) in r.iter().zip(d) {
        up += (*y as f64 - dm) * (*x as f64 - rm);
        down += (*y as f64 - dm) * (*y as f64 - dm);
    }
    let s = if down > 0.0 { ((16.0 * up / down).round() as i32).clamp(span.0, span.1) } else { 0 };
    (s, brightness(r, d, s, own))
}

/// The brightness shift that brings the block scaled by s nearest to `r`.
fn brightness(r: &[i32], d: &[i32], s: i32, own: bool) -> i32 {
    let m = made(d, s, 0, own);
    let diff: i32 = r.iter().zip(&m).map(|(x, y)| x - y).sum();
    let n = r.len() as i32;
    ((2 * diff + n * diff.signum()) / (2 * n)).clamp(-255, 255)
}

/// About how many bits a whole number takes when written as the coder writes it.
fn bits_of(v: i32) -> f64 {
    if v == 0 { 1.0 } else { 3.0 + 2.0 * (v.unsigned_abs().ilog2() as f64) }
}

/// The settings of the fractal mode, from rules.toml and the command.
#[derive(Clone, Debug)]
pub struct How {
    /// What a bit is worth in squares of error (the command's --fractal).
    pub lambda: f64,
    /// How many partial rules the beam keeps after each decision.
    pub beam: usize,
    /// How far a block is looked for in the frames before (pixels each way), and how
    /// far a domain is looked for in this frame (half-size pixels each way).
    pub range: i32,
    pub window: i32,
    /// How many times the rules are applied to a frame that stands alone, and to another.
    pub rounds_alone: u8,
    pub rounds: u8,
    /// The wave: how many ticks it moves out from a seam (0: no wave), and how high a
    /// step across a seam is an edge of the picture, which it leaves alone.
    pub wave: u8,
    pub wave_edge: u8,
    /// The sources' scores (pack, play, size) from rules.toml: skip, moved, own, flat.
    pub sources: [[f64; 3]; 4],
    pub source_weight: f64,
    /// The profile the scores are read with.
    pub profile: [f64; 3],
}

/// What the packer knows while it searches one frame.
struct Search<'a> {
    how: &'a How,
    real: [Pl<'a>; 3],
    before: Option<[Pl<'a>; 3]>,
    older: Option<[Pl<'a>; 3]>,
    /// The real frame at half size (the domains are looked for there), and running
    /// sums over its brightness for a block's mean and spread at once.
    half: [(Vec<u8>, usize, usize); 3],
    sums: Vec<i64>,
    squares: Vec<i64>,
}

impl Search<'_> {
    /// The sum and the sum of squares of the half-size brightness in a rectangle.
    fn area(&self, x: usize, y: usize, w: usize, h: usize) -> (i64, i64) {
        let s = self.half[0].1 + 1;
        let at = |t: &Vec<i64>| t[(y + h) * s + x + w] - t[y * s + x + w] - t[(y + h) * s + x] + t[y * s + x];
        (at(&self.sums), at(&self.squares))
    }

    /// The source's own score from rules.toml, as bits off the rule's price.
    fn bonus(&self, source: usize) -> f64 {
        self.how.source_weight * self.how.sources[source].iter().zip(&self.how.profile).map(|(a, p)| a * p).sum::<f64>()
    }

    /// The colour blocks of a range, and of the block a rule takes them from.
    fn colour(&self, pl: &Pl, x: i32, y: i32, n: usize) -> Vec<i32> {
        let (w, h) = ((n / 2).min(pl.w.saturating_sub(x.max(0) as usize)).max(1), (n / 2).min(pl.h.saturating_sub(y.max(0) as usize)).max(1));
        pl.block(x, y, w, h)
    }

    /// HotMotor's search for the rule of the range at (x, y), `n` wide: the rule, what
    /// it weighs (error + lambda * bits), and its error alone.
    fn rule(&self, x: usize, y: usize, n: usize, near: (i32, i32)) -> (Rule, f64, f64) {
        let how = self.how;
        let (bw, bh) = (n.min(self.real[0].w - x), n.min(self.real[0].h - y));
        let (xi, yi) = (x as i32, y as i32);
        let r = self.real[0].block(xi, yi, bw, bh);
        let rc: [Vec<i32>; 2] = [self.colour(&self.real[1], xi / 2, yi / 2, n), self.colour(&self.real[2], xi / 2, yi / 2, n)];
        let (cw, ch) = ((n / 2).min(self.real[1].w.saturating_sub(x / 2)).max(1), (n / 2).min(self.real[1].h.saturating_sub(y / 2)).max(1));
        let weigh = |err: f64, bits: f64, source: usize| err + how.lambda * (bits - self.bonus(source)).max(0.1);
        let mut best: (Rule, f64, f64) = (Rule::Flat([128; 3]), f64::MAX, f64::MAX);
        let offer = |rule: Rule, err: f64, bits: f64, source: usize, best: &mut (Rule, f64, f64)| {
            let cost = weigh(err, bits, source);
            if cost < best.1 {
                *best = (rule, cost, err);
            }
        };
        // Decision 1, the source. Skip and flat are whole rules at once.
        if let Some(b) = &self.before {
            let err = wrong(&r, &b[0].block(xi, yi, bw, bh)) + wrong(&rc[0], &b[1].block(xi / 2, yi / 2, cw, ch)) + wrong(&rc[1], &b[2].block(xi / 2, yi / 2, cw, ch));
            offer(Rule::Skip, err, 0.3, 0, &mut best);
            // A range that is where it was to within the price of a few bits is not looked at further.
            if err <= how.lambda * 6.0 {
                return best;
            }
        }
        let mean = |v: &[i32]| (v.iter().sum::<i32>() + v.len() as i32 / 2) / v.len().max(1) as i32;
        let flat = [mean(&r), mean(&rc[0]), mean(&rc[1])];
        let flat_err = r.iter().map(|v| ((v - flat[0]) * (v - flat[0])) as f64).sum::<f64>() + rc[0].iter().map(|v| ((v - flat[1]) * (v - flat[1])) as f64).sum::<f64>() + rc[1].iter().map(|v| ((v - flat[2]) * (v - flat[2])) as f64).sum::<f64>();
        offer(Rule::Flat(flat), flat_err, 3.0 + 12.0, 3, &mut best);
        // Decision 2, the place, for a block from a frame before: the quick judge is how
        // unlike the brightness is; the beam keeps the nearest few.
        if let Some(b) = &self.before {
            let reach = if n >= 8 { how.range } else { (how.range / 2).max(1) };
            let mut places: Vec<(u32, i32, i32)> = Vec::new();
            let unlike = |dx: i32, dy: i32| -> u32 { r.iter().zip(b[0].block(xi + dx, yi + dy, bw, bh)).map(|(p, q)| p.abs_diff(q)).sum() };
            for dy in -reach..=reach {
                for dx in -reach..=reach {
                    places.push((unlike(dx, dy), dx, dy));
                }
            }
            if near.0.abs() > reach || near.1.abs() > reach {
                places.push((unlike(near.0, near.1), near.0, near.1));
            }
            places.sort();
            places.truncate(how.beam.max(1));
            // Decision 3, contrast and brightness, and the exact judge; the frame before
            // the last is offered at the best place too.
            for (rank, (_, dx, dy)) in places.iter().enumerate() {
                for (older, frames) in [(false, Some(b)), (true, self.older.as_ref())] {
                    let Some(fr) = frames else { continue };
                    if older && rank > 0 {
                        continue;
                    }
                    let d = fr[0].block(xi + dx, yi + dy, bw, bh);
                    let dc = [fr[1].block(xi / 2 + (dx >> 1), yi / 2 + (dy >> 1), cw, ch), fr[2].block(xi / 2 + (dx >> 1), yi / 2 + (dy >> 1), cw, ch)];
                    let (fitted, _) = fit(&r, &d, (8, 24), false);
                    for s in [16, fitted] {
                        let o = [brightness(&r, &d, s, false), brightness(&rc[0], &dc[0], s, false), brightness(&rc[1], &dc[1], s, false)];
                        let err = wrong(&r, &made(&d, s, o[0], false)) + wrong(&rc[0], &made(&dc[0], s, o[1], false)) + wrong(&rc[1], &made(&dc[1], s, o[2], false));
                        let bits = 3.0 + bits_of(dx - near.0) + bits_of(dy - near.1) + bits_of(s - 16) + o.iter().map(|v| bits_of(*v)).sum::<f64>();
                        offer(Rule::Moved { older, dx: *dx, dy: *dy, s, o }, err, bits, 1, &mut best);
                        if fitted == 16 {
                            break;
                        }
                    }
                }
            }
        }
        // The classic fractal rule: a domain of this frame at half size. Only for a
        // whole range, and only where what is found so far still costs much.
        if bw == n && bh == n && best.2 > how.lambda * 24.0 {
            let (hw, hh) = (self.half[0].1, self.half[0].2);
            let (u0, v0) = ((x / 2) as i32, (y / 2) as i32);
            let q = n / 2;
            // The range's spread, and how bright each of its quarters is.
            let quarters = |get: &dyn Fn(usize, usize) -> i64| -> [i64; 4] { [get(0, 0), get(q, 0), get(0, q), get(q, q)] };
            let rq = quarters(&|qx, qy| (0..q).flat_map(|j| (0..q).map(move |i| (i, j))).map(|(i, j)| r[(qy + j) * n + qx + i] as i64).sum());
            let order = |v: [i64; 4]| -> [usize; 4] {
                let mut idx = [0usize, 1, 2, 3];
                idx.sort_by_key(|i| (v[*i], *i));
                idx
            };
            let r_order = order(rq);
            let r_spread = {
                let (sum, sq) = (r.iter().map(|v| *v as i64).sum::<i64>(), r.iter().map(|v| (*v as i64) * (*v as i64)).sum::<i64>());
                sq - sum * sum / (n * n) as i64
            };
            // Decision 2, the place and the turn. Quick judges: the domain must have
            // contrast enough (s stays under 1), and turned, its quarters must lie from
            // dark to bright as the range's do. The beam keeps those nearest in spread.
            let mut places: Vec<(i64, i32, i32, u8)> = Vec::new();
            let mut dv = -how.window;
            while dv <= how.window {
                let mut du = -how.window;
                while du <= how.window {
                    let (u, v) = (u0 + du, v0 + dv);
                    if u >= 0 && v >= 0 && u as usize + n <= hw && v as usize + n <= hh {
                        let (sum, sq) = self.area(u as usize, v as usize, n, n);
                        let spread = sq - sum * sum / (n * n) as i64;
                        if 4 * spread >= 3 * r_spread {
                            let dq = quarters(&|qx, qy| self.area(u as usize + qx, v as usize + qy, q, q).0);
                            for turn in 0..8u8 {
                                // Which quarter of the domain lands in each quarter of the range.
                                let lands: [i64; 4] = std::array::from_fn(|k| {
                                    let (si, sj) = turned(turn, 2, k % 2, k / 2);
                                    dq[sj * 2 + si]
                                });
                                if order(lands) == r_order {
                                    places.push(((spread - r_spread).abs(), du, dv, turn));
                                }
                            }
                        }
                    }
                    du += 2;
                }
                dv += 2;
            }
            places.sort();
            places.truncate(how.beam.max(1) * 4);
            // Decision 3 and the exact judge.
            let hp: [Pl; 3] = std::array::from_fn(|i| Pl { d: &self.half[i].0, w: self.half[i].1, h: self.half[i].2 });
            for (_, du, dv, turn) in places {
                let take = |pl: &Pl, bx: i32, by: i32, m: usize| -> Vec<i32> {
                    let block = pl.block(bx, by, m, m);
                    (0..m * m).map(|k| { let (si, sj) = turned(turn, m, k % m, k / m); block[sj * m + si] }).collect()
                };
                let d = take(&hp[0], u0 + du, v0 + dv, n);
                let (s, o0) = fit(&r, &d, (-15, 15), true);
                let mut err = wrong(&r, &made(&d, s, o0, true));
                let mut o = [o0, 0, 0];
                // The colours follow the same rule in their own planes, where the range is whole there.
                if cw == q && ch == q && q >= 2 {
                    for c in 0..2 {
                        let dc = take(&hp[c + 1], u0 / 2 + (du >> 1), v0 / 2 + (dv >> 1), q);
                        o[c + 1] = brightness(&rc[c], &dc, s, true);
                        err += wrong(&rc[c], &made(&dc, s, o[c + 1], true));
                    }
                } else {
                    continue;
                }
                let bits = 4.0 + bits_of(du / 2) + bits_of(dv / 2) + 3.0 + bits_of(s) + 12.0;
                offer(Rule::Own { dx: du, dy: dv, turn, s, o }, err, bits, 2, &mut best);
            }
        }
        best
    }

    /// The range at (x, y), `n` wide, as one rule or split in four: whichever weighs less.
    fn node(&self, x: usize, y: usize, n: usize, near: &mut (i32, i32)) -> (Node, f64) {
        let (rule, cost, err) = self.rule(x, y, n, *near);
        if n > 4 && err > self.how.lambda * 8.0 {
            let mut inner = *near;
            let mut parts: [Option<Node>; 4] = [None, None, None, None];
            let mut sum = self.how.lambda;
            for (k, part) in parts.iter_mut().enumerate() {
                let (px, py) = (x + (k % 2) * n / 2, y + (k / 2) * n / 2);
                if px < self.real[0].w && py < self.real[0].h {
                    let (node, c) = self.node(px, py, n / 2, &mut inner);
                    sum += c;
                    *part = Some(node);
                }
            }
            if sum < cost {
                *near = inner;
                return (Node::Split(Box::new(parts)), sum);
            }
        }
        if let Rule::Moved { dx, dy, .. } = rule {
            *near = (dx, dy);
        }
        (Node::Leaf(rule), cost)
    }
}

/// How the rules of a frame are written: HotMotor's two decisions.
#[derive(Clone, Copy, Default)]
struct Writing {
    /// A vector is told from the rule before (else plain).
    vectors_told: bool,
    /// A brightness shift is told from the rule before (else plain).
    shifts_told: bool,
}

/// One way through the syntax for the packer and the player.
enum Coder<'a> {
    Write(Encoder, [f64; 4]),
    Read(Decoder<'a>),
}

/// What the coder learns in one band.
struct Learned {
    split: [u16; 3],
    kind: [u16; 3 * 3],
    older: u16,
    turn: [u16; 8],
    number: [([u16; 32], [u16; 2]); 6],
}

impl Learned {
    fn new() -> Learned {
        Learned { split: [HALF; 3], kind: [HALF; 9], older: HALF, turn: [HALF; 8], number: [([HALF; 32], [HALF; 2]); 6] }
    }
}

impl Coder<'_> {
    /// One yes/no; `what` says what it is for (see SPENT).
    #[inline(always)]
    fn bit(&mut self, p: &mut u16, v: bool, what: usize) -> bool {
        match self {
            Coder::Write(e, spent) => {
                let no = *p as f64 / 4096.0;
                spent[what] -= (if v { 1.0 - no } else { no }).log2();
                e.bit(p, v);
                v
            }
            Coder::Read(d) => d.bit(p),
        }
    }
    /// A whole number with a sign: is it nothing, its sign, how many digits, the digits.
    fn number(&mut self, p: &mut ([u16; 32], [u16; 2]), v: i32, what: usize) -> i32 {
        if !self.bit(&mut p.1[0], v != 0, what) {
            return 0;
        }
        let negative = self.bit(&mut p.1[1], v < 0, what);
        let n = v.unsigned_abs().clamp(1, 65535);
        let want = 32 - n.leading_zeros() as usize;
        let mut digits = 1;
        while digits < 16 && self.bit(&mut p.0[digits - 1], digits < want, what) {
            digits += 1;
        }
        let mut value = 1u32;
        for j in (0..digits - 1).rev() {
            value = (value << 1) | self.bit(&mut p.0[16 + j], (n >> j) & 1 == 1, what) as u32;
        }
        if negative { -(value as i32) } else { value as i32 }
    }
}

/// What the rule before left, for telling the next one from it.
#[derive(Default)]
struct Before {
    vector: (i32, i32),
    shift: i32,
    flat: [i32; 3],
}

/// A range and all below it: written, or read. `want` is the packer's node.
#[allow(clippy::too_many_arguments)]
fn code(geo: &Geometry, c: &mut Coder, l: &mut Learned, w: Writing, last: &mut Before, x: usize, y: usize, n: usize, has_before: bool, has_older: bool, want: Option<&Node>) -> Node {
    let depth = match n {
        16 => 0,
        8 => 1,
        _ => 2,
    };
    if depth < 2 && c.bit(&mut l.split[depth], matches!(want, Some(Node::Split(_))), 0) {
        let parts = if let Some(Node::Split(p)) = want { Some(p) } else { None };
        let mut out: [Option<Node>; 4] = [None, None, None, None];
        // Only the quarters that lie inside the picture are there.
        for (k, ok) in inside(geo, x, y, n).into_iter().enumerate() {
            if ok {
                out[k] = Some(code(geo, c, l, w, last, x + (k % 2) * n / 2, y + (k / 2) * n / 2, n / 2, has_before, has_older, parts.and_then(|p| p[k].as_ref())));
            }
        }
        return Node::Split(Box::new(out));
    }
    let rule = if let Some(Node::Leaf(r)) = want { *r } else { Rule::Skip };
    let k = depth * 3;
    let told = |c: &mut Coder, l: &mut Learned, i: usize, v: i32, from: i32, on: bool, what: usize| -> i32 {
        if on { from + c.number(&mut l.number[i], v - from, what) } else { c.number(&mut l.number[i], v, what) }
    };
    if has_before && c.bit(&mut l.kind[k], rule == Rule::Skip, 0) {
        return Node::Leaf(Rule::Skip);
    }
    if has_before && c.bit(&mut l.kind[k + 1], matches!(rule, Rule::Moved { .. }), 0) {
        let (wo, wdx, wdy, ws, wsh) = if let Rule::Moved { older, dx, dy, s, o } = rule { (older, dx, dy, s, o) } else { (false, 0, 0, 16, [0; 3]) };
        let older = has_older && c.bit(&mut l.older, wo, 0);
        let dx = told(c, l, 0, wdx, last.vector.0, w.vectors_told, 1);
        let dy = told(c, l, 0, wdy, last.vector.1, w.vectors_told, 1);
        let s = 16 + c.number(&mut l.number[1], ws - 16, 3);
        let o0 = told(c, l, 2, wsh[0], last.shift, w.shifts_told, 3);
        let o = [o0, c.number(&mut l.number[3], wsh[1], 3), c.number(&mut l.number[3], wsh[2], 3)];
        last.vector = (dx, dy);
        last.shift = o0;
        return Node::Leaf(Rule::Moved { older, dx, dy, s, o });
    }
    if c.bit(&mut l.kind[k + 2], matches!(rule, Rule::Own { .. }), 0) {
        let (wdx, wdy, wt, ws, wsh) = if let Rule::Own { dx, dy, turn, s, o } = rule { (dx, dy, turn, s, o) } else { (0, 0, 0, 0, [0; 3]) };
        let dx = 2 * c.number(&mut l.number[4], wdx / 2, 1);
        let dy = 2 * c.number(&mut l.number[4], wdy / 2, 1);
        let mut turn = 0u8;
        let mut node = 1usize;
        for b in (0..3).rev() {
            let bit = c.bit(&mut l.turn[node], (wt >> b) & 1 == 1, 2);
            node = (node * 2 + bit as usize) & 7;
            turn = (turn << 1) | bit as u8;
        }
        let s = c.number(&mut l.number[1], ws, 3);
        let o: [i32; 3] = std::array::from_fn(|i| last.flat[i] + c.number(&mut l.number[5], wsh[i] - last.flat[i], 3));
        last.flat = o;
        return Node::Leaf(Rule::Own { dx, dy, turn, s, o });
    }
    let wf = if let Rule::Flat(f) = rule { f } else { [128; 3] };
    let f: [i32; 3] = std::array::from_fn(|i| last.flat[i] + c.number(&mut l.number[5], wf[i] - last.flat[i], 3));
    last.flat = f;
    Node::Leaf(Rule::Flat(f))
}

/// The rules of a band of roots, in order, with each one's place and size.
fn leaves(node: &Node, x: usize, y: usize, n: usize, out: &mut Vec<(usize, usize, usize, Rule)>) {
    match node {
        Node::Leaf(r) => out.push((x, y, n, *r)),
        Node::Split(parts) => {
            for (k, p) in parts.iter().enumerate() {
                if let Some(p) = p {
                    leaves(p, x + (k % 2) * n / 2, y + (k / 2) * n / 2, n / 2, out);
                }
            }
        }
    }
}

/// The luma rows of a band of roots.
fn band_rows(geo: &Geometry, bands: usize) -> usize {
    geo.height.div_ceil(bands.max(1)).max(1).div_ceil(ROOT) * ROOT
}

/// Which quarters of a root or of a range at (x, y), `n` wide, lie inside the picture.
fn inside(geo: &Geometry, x: usize, y: usize, n: usize) -> [bool; 4] {
    std::array::from_fn(|k| x + (k % 2) * n / 2 < geo.width && y + (k / 2) * n / 2 < geo.height)
}

/// A band's roots written: the bytes, and what the bits went to.
fn write_band(geo: &Geometry, nodes: &[Node], y0: usize, w: Writing, has_before: bool, has_older: bool) -> (Vec<u8>, [f64; 4]) {
    let mut c = Coder::Write(Encoder::new(), [0.0; 4]);
    let (mut l, mut last) = (Learned::new(), Before { flat: [128; 3], ..Default::default() });
    let across = geo.width.div_ceil(ROOT);
    for (i, node) in nodes.iter().enumerate() {
        let (x, y) = ((i % across) * ROOT, y0 + (i / across) * ROOT);
        code(geo, &mut c, &mut l, w, &mut last, x, y, ROOT, has_before, has_older, Some(node));
    }
    match c {
        Coder::Write(e, spent) => (e.finish(), spent),
        Coder::Read(_) => unreachable!(),
    }
}

const ALONE: u8 = 1;
const OLDER: u8 = 2;
const VECTORS_TOLD: u8 = 4;
const SHIFTS_TOLD: u8 = 8;
const WAVE: u8 = 16;
const HEAD: usize = 4;

/// The wave over a frame the rules have made. `rules` are its ranges; the seams of
/// those made anew are the sources. Every pixel is moved at once, from the frame as
/// the rules left it: across a seam with the step d, the pixel k away from it moves
/// d * (2 (ticks - k) - 1) / (4 ticks) toward the other side.
fn wave(geo: &Geometry, cur: &mut [u8], rules: &[(usize, usize, usize, Rule)], ticks: usize, edge: i32) {
    let at = geo.offsets();
    for p in 0..3 {
        let (w, h) = (geo.planes[p].w, geo.planes[p].h);
        // The colours' ranges are half as wide: the wave moves half as far there.
        let t = if p == 0 { ticks } else { ticks.div_ceil(2) };
        if t == 0 || w < 2 * t || h < 2 * t {
            continue;
        }
        // A seam left of a pixel, and one above it.
        let (mut left, mut above) = (vec![false; w * h], vec![false; w * h]);
        for (x, y, n, r) in rules {
            if *r == Rule::Skip {
                continue;
            }
            let (px, py, pn) = if p == 0 { (*x, *y, *n) } else { (x / 2, y / 2, n / 2) };
            let (x1, y1) = ((px + pn).min(w), (py + pn).min(h));
            for yy in py..y1 {
                left[yy * w + px] = px > 0;
                if x1 < w {
                    left[yy * w + x1] = true;
                }
            }
            for xx in px..x1 {
                above[py * w + xx] = py > 0;
                if y1 < h {
                    above[y1 * w + xx] = true;
                }
            }
        }
        let plane = &mut cur[at[p]..at[p] + w * h];
        let was: Vec<i32> = plane.iter().map(|v| *v as i32).collect();
        let mut moved = vec![0i32; w * h];
        // One seam: `q` is the first pixel past it, `step` leads away from it.
        let mut seam = |q: usize, step: usize| {
            let p0 = q - step;
            let d = was[q] - was[p0];
            // A real edge, or a side that is not level itself: the two cancel.
            if d == 0 || d.abs() >= edge || 2 * (was[p0] - was[p0 - (t - 1) * step]).abs() >= edge || 2 * (was[q] - was[q + (t - 1) * step]).abs() >= edge {
                return;
            }
            for k in 0..t {
                let c = d * (2 * (t - k) as i32 - 1);
                moved[p0 - k * step] += c;
                moved[q + k * step] -= c;
            }
        };
        for y in 0..h {
            for x in t..=w - t {
                if x < w && left[y * w + x] {
                    seam(y * w + x, 1);
                }
            }
        }
        for y in t..=h - t {
            for x in 0..w {
                if y < h && above[y * w + x] {
                    seam(y * w + x, w);
                }
            }
        }
        let scale = 4 * t as i32;
        for (v, (o, m)) in plane.iter_mut().zip(was.iter().zip(&moved)) {
            *v = (o + (m + scale / 2).div_euclid(scale)).clamp(0, 255) as u8;
        }
    }
}

/// Does the frame stand alone (no frame before it is needed)?
pub fn alone(payload: &[u8]) -> bool {
    payload.first().is_some_and(|f| f & ALONE != 0)
}

/// Packs a frame as rules. `before` and `older` are the last two frames as the player
/// rebuilt them (none: the frame stands alone). Returns the bytes: flags, the rounds,
/// the number of bands, each band's length, the bands.
pub fn pack(geo: &Geometry, frame: &[u8], before: Option<&[u8]>, older: Option<&[u8]>, bands: usize, how: &How, rules: &Rules) -> Vec<u8> {
    let real = planes(geo, frame);
    let half: [(Vec<u8>, usize, usize); 3] = std::array::from_fn(|i| halved(&real[i]));
    // Running sums over the half-size brightness.
    let (hw, hh) = (half[0].1, half[0].2);
    let side = hw + 1;
    let (mut sums, mut squares) = (vec![0i64; side * (hh + 1)], vec![0i64; side * (hh + 1)]);
    for y in 0..hh {
        let (mut row, mut row_sq) = (0i64, 0i64);
        for x in 0..hw {
            let v = half[0].0[y * hw + x] as i64;
            row += v;
            row_sq += v * v;
            sums[(y + 1) * side + x + 1] = sums[y * side + x + 1] + row;
            squares[(y + 1) * side + x + 1] = squares[y * side + x + 1] + row_sq;
        }
    }
    let search = Search { how, real, before: before.map(|b| planes(geo, b)), older: older.filter(|_| before.is_some()).map(|o| planes(geo, o)), half, sums, squares };
    let rows = band_rows(geo, bands);
    let across = geo.width.div_ceil(ROOT);
    // The search, band by band on every core: HotMotor's beam for each range.
    let found: Vec<(usize, Vec<Node>)> = (0..geo.height)
        .into_par_iter()
        .step_by(rows)
        .map(|y0| {
            let mut near = (0, 0);
            let mut nodes = Vec::new();
            for y in (y0..(y0 + rows).min(geo.height)).step_by(ROOT) {
                for tx in 0..across {
                    nodes.push(search.node(tx * ROOT, y, ROOT, &mut near).0);
                }
            }
            (y0, nodes)
        })
        .collect();
    let (has_before, has_older) = (search.before.is_some(), search.older.is_some());
    // HotMotor packs the rules: each way of writing them is tried, the judge measures bytes.
    let write_all = |w: Writing| -> Vec<(Vec<u8>, [f64; 4])> { found.par_iter().map(|(y0, nodes)| write_band(geo, nodes, *y0, w, has_before, has_older)).collect() };
    let as_writing = |choice: &motor::Choice| Writing { vectors_told: motor::get(choice, "vectors") != Some("plain"), shifts_told: motor::get(choice, "shifts") != Some("plain") };
    let profile = how.profile;
    let mut judge = |choice: &motor::Choice| {
        let bytes: usize = write_all(as_writing(choice)).iter().map(|b| b.0.len()).sum();
        // Fewer bytes is the whole of it: the score is what is saved against a thousand bytes a band.
        (-(bytes as f64) / 1000.0, vec![format!("{bytes} bytes")])
    };
    let chosen = motor::search_over(&rules.fractal_writing, rules.beam, &profile, &mut judge).remove(0);
    let w = as_writing(&chosen.choice);
    let written = write_all(w);
    for (_, spent) in &written {
        for (total, s) in SPENT.iter().zip(spent) {
            total.fetch_add((s * 1000.0) as u64, Ordering::Relaxed);
        }
    }
    for (y0, nodes) in &found {
        let mut all = Vec::new();
        for (i, n) in nodes.iter().enumerate() {
            leaves(n, (i % across) * ROOT, y0 + (i / across) * ROOT, ROOT, &mut all);
        }
        for (_, _, _, r) in all {
            let k = match r {
                Rule::Skip => 0,
                Rule::Moved { .. } => 1,
                Rule::Own { .. } => 2,
                Rule::Flat(_) => 3,
            };
            RANGES[k].fetch_add(1, Ordering::Relaxed);
        }
    }
    let mut out = vec![(if how.wave > 0 { WAVE } else { 0 }) | (if has_before { 0 } else { ALONE }) | (if has_older { OLDER } else { 0 }) | (if w.vectors_told { VECTORS_TOLD } else { 0 }) | (if w.shifts_told { SHIFTS_TOLD } else { 0 })];
    out.push(if has_before { how.rounds } else { how.rounds_alone });
    out.extend((bands as u16).to_le_bytes());
    if how.wave > 0 {
        out.extend([how.wave, how.wave_edge]);
    }
    for (bytes, _) in &written {
        out.extend((bytes.len() as u32).to_le_bytes());
    }
    for (bytes, _) in &written {
        out.extend(bytes);
    }
    out
}

/// The player: the frame the rules make.
pub fn unpack(geo: &Geometry, payload: &[u8], before: Option<&[u8]>, older: Option<&[u8]>) -> Result<Vec<u8>, String> {
    if payload.len() < HEAD || geo.planes.len() != 3 {
        return Err("not a frame of fractal rules".into());
    }
    let flags = payload[0];
    let has_before = flags & ALONE == 0;
    let before = if has_before { Some(before.filter(|b| b.len() == geo.frame_len()).ok_or("a frame of rules was rebuilt without the frame before it")?) } else { None };
    let older = if flags & OLDER != 0 { Some(older.filter(|b| b.len() == geo.frame_len()).ok_or("a frame of rules was rebuilt without the frame before the last")?) } else { None };
    let (rounds, bands) = (payload[1].max(1), u16::from_le_bytes([payload[2], payload[3]]) as usize);
    let w = Writing { vectors_told: flags & VECTORS_TOLD != 0, shifts_told: flags & SHIFTS_TOLD != 0 };
    let rows = band_rows(geo, bands);
    let across = geo.width.div_ceil(ROOT);
    let starts: Vec<usize> = (0..geo.height).step_by(rows).collect();
    // A frame with a wave says how it runs, after the head.
    let (head, waves) = if flags & WAVE != 0 { (HEAD + 2, payload.get(HEAD..HEAD + 2).map(|b| (b[0] as usize, b[1] as i32)).ok_or("a frame of rules is cut")?) } else { (HEAD, (0, 0)) };
    let mut data = head + 4 * starts.len();
    let mut coded = Vec::new();
    for i in 0..starts.len() {
        let len = payload.get(head + 4 * i..head + 4 * i + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize).ok_or("a frame of rules is cut")?;
        coded.push(payload.get(data..data + len).ok_or("a frame of rules is cut")?);
        data += len;
    }
    // The rules of every band, read on every core.
    let rules: Vec<(usize, usize, usize, Rule)> = starts
        .par_iter()
        .zip(coded)
        .flat_map_iter(|(y0, bytes)| {
            let mut c = Coder::Read(Decoder::new(bytes));
            let (mut l, mut last) = (Learned::new(), Before { flat: [128; 3], ..Default::default() });
            let mut all = Vec::new();
            for y in (*y0..(*y0 + rows).min(geo.height)).step_by(ROOT) {
                for tx in 0..across {
                    let node = code(geo, &mut c, &mut l, w, &mut last, tx * ROOT, y, ROOT, has_before, older.is_some(), None);
                    leaves(&node, tx * ROOT, y, ROOT, &mut all);
                }
            }
            all
        })
        .collect();
    // The start: the frame before, or flat grey for a frame that stands alone.
    let mut cur = match before {
        Some(b) => b.to_vec(),
        None => vec![128u8; geo.frame_len()],
    };
    let at = geo.offsets();
    let (bp, op) = (before.map(|b| planes(geo, b)), older.map(|o| planes(geo, o)));
    // A rule's three blocks (brightness and the two colours), each with its place and size.
    let blocks_of = |x: usize, y: usize, n: usize, rule: &Rule, halves: Option<&[(Vec<u8>, usize, usize); 3]>| -> [(usize, usize, usize, usize, Vec<u8>); 3] {
        std::array::from_fn(|p| {
            let pl = &geo.planes[p];
            let (px, py, pn) = if p == 0 { (x, y, n) } else { (x / 2, y / 2, n / 2) };
            let (bw, bh) = (pn.min(pl.w.saturating_sub(px)), pn.min(pl.h.saturating_sub(py)));
            if bw == 0 || bh == 0 {
                return (px, py, 0, 0, Vec::new());
            }
            let vals: Vec<i32> = match rule {
                Rule::Skip => bp.as_ref().unwrap()[p].block(px as i32, py as i32, bw, bh),
                Rule::Moved { older, dx, dy, s, o } => {
                    let fr = if *older { op.as_ref().or(bp.as_ref()) } else { bp.as_ref() }.unwrap();
                    let (vx, vy) = if p == 0 { (*dx, *dy) } else { (*dx >> 1, *dy >> 1) };
                    made(&fr[p].block(px as i32 + vx, py as i32 + vy, bw, bh), *s, o[p], false)
                }
                Rule::Own { dx, dy, turn, s, o } => {
                    let h = &halves.unwrap()[p];
                    let hp = Pl { d: &h.0, w: h.1, h: h.2 };
                    let (vx, vy) = if p == 0 { (*dx, *dy) } else { (*dx >> 1, *dy >> 1) };
                    let block = hp.block((px / 2) as i32 + vx, (py / 2) as i32 + vy, pn, pn);
                    let turned_block: Vec<i32> = (0..pn * pn).map(|k| { let (si, sj) = turned(*turn, pn, k % pn, k / pn); block[sj * pn + si] }).collect();
                    let whole = made(&turned_block, *s, o[p], true);
                    (0..bh).flat_map(|j| (0..bw).map(move |i| (i, j))).map(|(i, j)| whole[j * pn + i]).collect()
                }
                Rule::Flat(f) => vec![f[p]; bw * bh],
            };
            (px, py, bw, bh, vals.into_iter().map(|v| v.clamp(0, 255) as u8).collect())
        })
    };
    let put = |cur: &mut [u8], blocks: [(usize, usize, usize, usize, Vec<u8>); 3]| {
        for (p, (px, py, bw, bh, vals)) in blocks.into_iter().enumerate() {
            let w = geo.planes[p].w;
            for j in 0..bh {
                cur[at[p] + (py + j) * w + px..at[p] + (py + j) * w + px + bw].copy_from_slice(&vals[j * bw..(j + 1) * bw]);
            }
        }
    };
    // The rules that read other frames, or nothing, give the same every round: once.
    let fixed: Vec<_> = rules.par_iter().filter(|r| !matches!(r.3, Rule::Own { .. } | Rule::Skip)).map(|(x, y, n, r)| blocks_of(*x, *y, *n, r, None)).collect();
    for b in fixed {
        put(&mut cur, b);
    }
    // The rules that read this frame: round after round, each from the frame as it stood.
    let own: Vec<&(usize, usize, usize, Rule)> = rules.iter().filter(|r| matches!(r.3, Rule::Own { .. })).collect();
    if !own.is_empty() {
        for _ in 0..rounds {
            let now = planes(geo, &cur);
            let halves: [(Vec<u8>, usize, usize); 3] = std::array::from_fn(|i| halved(&now[i]));
            let next: Vec<_> = own.par_iter().map(|(x, y, n, r)| blocks_of(*x, *y, *n, r, Some(&halves))).collect();
            for b in next {
                put(&mut cur, b);
            }
        }
    }
    if waves.0 > 0 {
        wave(geo, &mut cur, &rules, waves.0.min(8), waves.1);
    }
    Ok(cur)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_player_makes_what_the_rules_say_every_time() {
        let geo = Geometry::of("yuv420p", 72, 56).unwrap();
        let rules = Rules::parse(motor::DEFAULT_RULES).unwrap();
        let how = How { lambda: 10.0, ..rules.fractal.clone() };
        let mut s = 9u32;
        // Slopes with a little grain; the colours are slopes of their own.
        let light = 72 * 56;
        let a: Vec<u8> = (0..geo.frame_len())
            .map(|i| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                if i < light { ((i % 72) as u32 * 2 + ((i / 72) as u32 % 16) * 5 + (s >> 30)) as u8 } else { (100 + ((i - light) % 36) as u32 + ((i - light) / 36 % 28) as u32) as u8 }
            })
            .collect();
        let b: Vec<u8> = a.iter().enumerate().map(|(i, v)| if i < light && i % 72 > 30 { v.wrapping_add(9) } else { *v }).collect();
        // A frame that stands alone: flat grey, the rules applied round after round.
        let first = pack(&geo, &a, None, None, 3, &how, &rules);
        assert!(alone(&first));
        let a2 = unpack(&geo, &first, None, None).unwrap();
        assert_eq!(a2, unpack(&geo, &first, None, None).unwrap(), "the same rules make the same frame");
        let far: f64 = a.iter().zip(&a2).map(|(x, y)| (*x as f64 - *y as f64).powi(2)).sum::<f64>() / a.len() as f64;
        assert!(far < 400.0, "the rules make something like the frame (mean square {far})");
        // A frame from the frame before.
        let second = pack(&geo, &b, Some(&a2), None, 3, &how, &rules);
        assert!(!alone(&second));
        let b2 = unpack(&geo, &second, Some(&a2), None).unwrap();
        assert_eq!(b2, unpack(&geo, &second, Some(&a2), None).unwrap());
        // The same frame again is all skips: a few bytes.
        let third = pack(&geo, &b2, Some(&b2), Some(&a2), 3, &how, &rules);
        assert_eq!(unpack(&geo, &third, Some(&b2), Some(&a2)).unwrap(), b2);
        assert!(third.len() < 60, "a frame that is the frame before is nearly nothing ({} bytes)", third.len());
    }
}
