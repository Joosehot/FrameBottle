//! A trial, apart from the packers: STACKED BIT SHEETS. Every sample is split into its
//! eight bits, sheet 7 (the top bit) down to sheet 0, and each bit is written by a
//! learning yes/no coder, two ways: seeing only the same bit of the frame before, or
//! seeing four other bits: the same bit of the frame before,
//! the bits to the left and above in the same sheet, and this sample's bit one sheet
//! higher. The player sees the same four, so nothing else is kept.
//!
//! Tried on the samples themselves and on what the mix's guess missed by (0, -1, 1, -2
//! ... told as 0, 1, 2, 3 ...). First it measures, with no coder: how often a bit is
//! what it was in the frame before, and what size that alone would give.
//!
//! The frames are cut into bands of rows, each band a job of its own through the whole
//! film, on every core. The misses' sheets are timed without the guess that a real
//! player would have to make for every sample before it could read them.

use crate::guess::{Decoder, Encoder};
use crate::mix;
use crate::pack::Geometry;
use rayon::prelude::*;
use std::io::Read;
use std::time::Instant;

/// How likely "yes" is (of 65536), and how often it was asked.
type Learned = [u16; 2];
const SLOW: u16 = 254;
/// A bit's surroundings: the four bits seen, and whether there is a frame before.
const AROUND: usize = 32;
const ALL_FOUR: usize = 31;
const BEFORE_ONLY: usize = 16 | 8;

trait Bits {
    fn bit(&mut self, stream: usize, said: u32, v: bool) -> bool;
}
struct Out(Vec<Encoder>);
struct In<'a>(Vec<Decoder<'a>>);
impl Bits for Out {
    #[inline(always)]
    fn bit(&mut self, stream: usize, said: u32, v: bool) -> bool {
        self.0[stream].fixed(4096 - said, v);
        v
    }
}
impl Bits for In<'_> {
    #[inline(always)]
    fn bit(&mut self, stream: usize, said: u32, _: bool) -> bool {
        self.0[stream].fixed(4096 - said)
    }
}

/// The rows of plane `k` that belong to the band of light rows `y0..y1`.
fn rows_of(geo: &Geometry, k: usize, y0: usize, y1: usize) -> (usize, usize) {
    if k == 0 {
        (y0, y1)
    } else {
        (y0 / 2, if y1 >= geo.height { geo.planes[k].h } else { y1 / 2 })
    }
}

/// One band through the whole film, written or read (stream: plane * 8 + sheet).
/// `seen`: which of the surroundings count (BEFORE_ONLY: the bit of the frame before alone).
/// Gives how many samples did not come out as they are in `frames`.
fn band<C: Bits>(c: &mut C, geo: &Geometry, frames: &[Vec<u8>], y0: usize, y1: usize, part: &[i32; 256], seen: usize) -> usize {
    let at = geo.offsets();
    let mut learned = vec![[[[32768u16, 0]; AROUND]; 8]; 3];
    let mut now: [Vec<u8>; 3] = std::array::from_fn(|k| vec![0u8; geo.planes[k].w * (rows_of(geo, k, y0, y1).1 - rows_of(geo, k, y0, y1).0)]);
    let mut was = now.clone();
    let mut wrong = 0;
    for (f, frame) in frames.iter().enumerate() {
        for k in 0..3 {
            let (w, (r0, r1)) = (geo.planes[k].w, rows_of(geo, k, y0, y1));
            let real = &frame[at[k] + r0 * w..at[k] + r1 * w];
            let (cur, before, l) = (&mut now[k], &was[k], &mut learned[k]);
            let later = if f > 0 { 16 } else { 0 };
            for y in 0..r1 - r0 {
                for x in 0..w {
                    let i = y * w + x;
                    let (p, left, above, want) = (before[i], if x > 0 { cur[i - 1] } else { 0 }, if y > 0 { cur[i - w] } else { 0 }, real[i]);
                    let mut v = 0u8;
                    for b in (0..8).rev() {
                        let higher = if b < 7 { (v >> (b + 1)) & 1 } else { 0 };
                        let around = seen & (later | ((((p >> b) & 1) << 3) | (((left >> b) & 1) << 2) | (((above >> b) & 1) << 1) | higher) as usize);
                        let s: &mut Learned = &mut l[b][around];
                        let bit = c.bit(k * 8 + b, ((s[0] >> 4) as u32).clamp(1, 4095), (want >> b) & 1 == 1);
                        let target = if bit { 65535i32 } else { 0 };
                        *s = [(s[0] as i32 + (((target - s[0] as i32) * part[s[1] as usize]) >> 16)) as u16, (s[1] + 1).min(SLOW)];
                        v |= (bit as u8) << b;
                    }
                    cur[i] = v;
                    wrong += (v != want) as usize;
                }
            }
        }
        std::mem::swap(&mut now, &mut was);
    }
    wrong
}

/// For each kind of plane (light, colour) and sheet: bits that are 1, bits the same as
/// in the frame before, and bits that have a frame before.
type Counts = [[[u64; 3]; 8]; 2];

fn count(geo: &Geometry, frame: &[u8], before: Option<&[u8]>) -> Counts {
    let mut n: Counts = Default::default();
    let colour = geo.planes[0].len();
    for (i, v) in frame.iter().enumerate() {
        let kind = (i >= colour) as usize;
        for b in 0..8 {
            n[kind][b][0] += ((v >> b) & 1) as u64;
        }
        if let Some(p) = before {
            let x = v ^ p[i];
            for b in 0..8 {
                n[kind][b][1] += (1 - ((x >> b) & 1)) as u64;
                n[kind][b][2] += 1;
            }
        }
    }
    n
}

/// The bits a yes/no of likelihood `p` takes at the least.
fn entropy(p: f64) -> f64 {
    if p <= 0.0 || p >= 1.0 {
        0.0
    } else {
        -p * p.log2() - (1.0 - p) * (1.0 - p).log2()
    }
}

/// Measures the sheets of `frames`, then writes and reads them: each bit told from the
/// frame before alone, then from all four. Gives the bytes written each way.
fn study(name: &str, geo: &Geometry, frames: &[Vec<u8>], bands: usize) -> [usize; 2] {
    println!("\n== SHEETS of {name}");
    // 1. The measure, with no coder.
    let counts: Counts = (0..frames.len())
        .into_par_iter()
        .map(|f| count(geo, &frames[f], if f > 0 { Some(&frames[f - 1][..]) } else { None }))
        .reduce(Counts::default, |mut a, b| {
            for (x, y) in a.iter_mut().flatten().flatten().zip(b.iter().flatten().flatten()) {
                *x += y;
            }
            a
        });
    let samples: [u64; 2] = [(geo.planes[0].len() * frames.len()) as u64, ((geo.planes[1].len() + geo.planes[2].len()) * frames.len()) as u64];
    println!("the measure (frames 2 to {}; a sheet of noise is the same as before 50 % of the time):", frames.len());
    println!("  sheet   ones: light  colour   same as before: light  colour   least bytes by that alone   bits a bit");
    let mut least_all = 0.0;
    for b in (0..8).rev() {
        let (mut least, mut bits) = (0.0, 0u64);
        let mut share = [[0.0; 2]; 2];
        for kind in 0..2 {
            let [ones, same, had] = counts[kind][b];
            share[kind] = [ones as f64 / samples[kind] as f64, same as f64 / had.max(1) as f64];
            least += had as f64 * entropy(share[kind][1]) / 8.0;
            bits += had;
        }
        least_all += least;
        println!("  {b}            {:6.2} % {:6.2} %                  {:6.2} % {:6.2} %   {:>14.0}              {:.3}", 100.0 * share[0][0], 100.0 * share[1][0], 100.0 * share[0][1], 100.0 * share[1][1], least, 8.0 * least / bits.max(1) as f64);
    }
    println!("  all sheets: {least_all:.0} bytes at the least by that alone (the first frame not counted)");
    // 2. Written and read back, the two ways.
    [("the bit of the frame before alone", BEFORE_ONLY), ("the frame before, left, above and the sheet above", ALL_FOUR)].map(|(way, seen)| coded(way, seen, geo, frames, bands, samples))
}

fn coded(way: &str, seen: usize, geo: &Geometry, frames: &[Vec<u8>], bands: usize, samples: [u64; 2]) -> usize {
    let part: [i32; 256] = std::array::from_fn(|n| 65536 / (n as i32 + 2));
    let rows = geo.height.div_ceil(bands.max(1)).max(2).next_multiple_of(2);
    let starts: Vec<usize> = (0..geo.height).step_by(rows).collect();
    let from = Instant::now();
    let written: Vec<Vec<Vec<u8>>> = starts
        .par_iter()
        .map(|y0| {
            let mut c = Out((0..24).map(|_| Encoder::new()).collect());
            band(&mut c, geo, frames, *y0, (*y0 + rows).min(geo.height), &part, seen);
            c.0.into_iter().map(|e| e.finish()).collect()
        })
        .collect();
    let write_time = from.elapsed().as_secs_f64();
    let from = Instant::now();
    let wrong: usize = starts
        .par_iter()
        .zip(&written)
        .map(|(y0, streams)| {
            let mut c = In(streams.iter().map(|s| Decoder::new(s)).collect());
            band(&mut c, geo, frames, *y0, (*y0 + rows).min(geo.height), &part, seen)
        })
        .sum();
    let read_time = from.elapsed().as_secs_f64();
    println!("written and read, each bit told from {way} ({} bands, each through the whole film; {} threads):", starts.len(), rayon::current_num_threads());
    println!("  sheet     bytes: light        colour          both   bits a bit");
    let mut all = 0;
    for b in (0..8).rev() {
        let of = |planes: std::ops::Range<usize>| -> usize { written.iter().map(|streams| planes.clone().map(|k| streams[k * 8 + b].len()).sum::<usize>()).sum() };
        let (light, colour) = (of(0..1), of(1..3));
        all += light + colour;
        println!("  {b}        {light:>13} {colour:>13} {:>13}        {:.3}", light + colour, 8.0 * (light + colour) as f64 / (samples[0] + samples[1]) as f64);
    }
    let n = frames.len() as f64;
    println!("  all sheets: {all} bytes, {:.3} bits a sample", 8.0 * all as f64 / (samples[0] + samples[1]) as f64);
    println!("  writing {write_time:.1} s ({:.1} ms a frame), reading {read_time:.1} s ({:.1} ms a frame); {}", 1000.0 * write_time / n, 1000.0 * read_time / n, if wrong == 0 { "every sample came back".to_string() } else { format!("{wrong} samples came back WRONG") });
    all
}

pub fn run(path: &std::path::Path, w: usize, h: usize, frames: usize, bands: usize) -> Result<(), String> {
    let geo = Geometry::of("yuv420p", w, h).ok_or("no such geometry")?;
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut all = Vec::new();
    for _ in 0..frames {
        let mut f = vec![0u8; geo.frame_len()];
        file.read_exact(&mut f).map_err(|e| e.to_string())?;
        all.push(f);
    }
    println!("{frames} frames of {w} x {h}, yuv420p as they are (already light and two colours: no colour transform is made)");
    let raw = study("the samples themselves", &geo, &all, bands);
    // What the mix's guess missed by, frame by frame (vectors to a quarter, the mix that learns).
    let how = mix::How { light: 0, two: false, quarter: true, gold: false, tones: false, pyramid: false };
    let mut memory = mix::Memory::new();
    let from = Instant::now();
    let (mut mixed, mut misses) = (0, Vec::new());
    for f in 0..frames {
        let before = if f > 0 { Some(&all[f - 1][..]) } else { None };
        mixed += mix::pack(&geo, &all[f], before, bands, &mut memory, how).len();
        misses.push(if before == Some(&all[f][..]) { vec![0u8; geo.frame_len()] } else { memory.misses(&geo, bands).iter().map(|r| ((*r as i32) << 1 ^ (*r as i32) >> 7) as u8).collect() });
    }
    let mix_time = from.elapsed().as_secs_f64();
    let missed = study("what the mix's guess missed by", &geo, &misses, bands);
    println!("\n== TOGETHER ({frames} frames)");
    println!("                                 from the frame before alone   from all four");
    println!("  sheets of the samples                 {:>12} bytes    {:>12} bytes", raw[0], raw[1]);
    println!("  sheets of the mix's misses            {:>12} bytes    {:>12} bytes (and the guess to make on top)", missed[0], missed[1]);
    println!("  the mix as it is                      {mixed:>12} bytes (vectors to a quarter; packed here frame by frame in {mix_time:.1} s)");
    Ok(())
}
