//! A trial, apart from the packers: the COLOUR CHEAT SHEET. Each channel has a sheet of
//! its tones; a common tone gets a short code, a rare one a long code, and every sample
//! is written as its tone's code. Three ways:
//!
//! - a sheet for every frame (canonical Huffman codes; the sheet is the 256 code lengths,
//!   half a byte each),
//! - one sheet for the whole scene (the same),
//! - a sheet that is never kept: a learning yes/no coder that counts the tones as they
//!   come (the tone told bit by bit, the top one first, each bit knowing those above).
//!
//! Tried on the samples themselves, on what the mix's guess missed by (0, -1, 1, -2 ...
//! told as 0, 1, 2, 3 ...), and on the mix's packed bytes (one channel: the bytes).
//! The clip is one scene.

use crate::guess::{Decoder, Encoder};
use crate::mix;
use crate::pack::Geometry;
use rayon::prelude::*;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::io::Read;
use std::time::Instant;

/// The longest code, in bits.
const LONGEST: usize = 15;
/// A sheet as it is kept: 256 code lengths, half a byte each.
const SHEET: usize = 128;
/// The parts a channel is cut into for the learning coder, each a job through the film.
const PARTS: usize = 16;

fn count(data: &[u8]) -> [u64; 256] {
    let mut n = [0u64; 256];
    for v in data {
        n[*v as usize] += 1;
    }
    n
}

/// Huffman's code lengths for the tones (0: the tone does not occur).
fn huffman(counts: &[u64; 256]) -> [u8; 256] {
    let mut parent = [usize::MAX; 511];
    let mut heap: BinaryHeap<Reverse<(u64, usize)>> = (0..256).filter(|s| counts[*s] > 0).map(|s| Reverse((counts[s], s))).collect();
    let mut len = [0u8; 256];
    if heap.len() == 1 {
        len[heap.pop().unwrap().0.1] = 1;
        return len;
    }
    let mut next = 256;
    while heap.len() > 1 {
        let (Reverse(a), Reverse(b)) = (heap.pop().unwrap(), heap.pop().unwrap());
        (parent[a.1], parent[b.1]) = (next, next);
        heap.push(Reverse((a.0 + b.0, next)));
        next += 1;
    }
    for s in (0..256).filter(|s| counts[*s] > 0) {
        let (mut at, mut depth) = (s, 0);
        while parent[at] != usize::MAX {
            at = parent[at];
            depth += 1;
        }
        len[s] = depth.min(255) as u8;
    }
    len
}

/// The sheet: code lengths of at most LONGEST bits (rare tones are counted up until they fit).
fn sheet_of(counts: &[u64; 256]) -> [u8; 256] {
    let mut c = *counts;
    loop {
        let len = huffman(&c);
        if len.iter().all(|l| *l as usize <= LONGEST) {
            return len;
        }
        for x in c.iter_mut().filter(|x| **x > 0) {
            *x = *x / 2 + 1;
        }
    }
}

/// The canonical codes of a sheet: shorter first, then by tone.
fn codes_of(sheet: &[u8; 256]) -> [u16; 256] {
    let mut order: Vec<usize> = (0..256).filter(|s| sheet[*s] > 0).collect();
    order.sort_by_key(|s| (sheet[*s], *s));
    let (mut codes, mut code, mut before) = ([0u16; 256], 0u32, 0u8);
    for s in order {
        code <<= sheet[s] - before.min(sheet[s]);
        before = sheet[s];
        codes[s] = code as u16;
        code += 1;
    }
    codes
}

fn write(data: &[u8], sheet: &[u8; 256]) -> Vec<u8> {
    let codes = codes_of(sheet);
    let mut out = Vec::with_capacity(data.len() / 2);
    let (mut acc, mut held) = (0u64, 0u32);
    for v in data {
        let l = sheet[*v as usize] as u32;
        acc = (acc << l) | codes[*v as usize] as u64;
        held += l;
        while held >= 8 {
            held -= 8;
            out.push((acc >> held) as u8);
        }
    }
    if held > 0 {
        out.push((acc << (8 - held)) as u8);
    }
    out
}

fn read(bytes: &[u8], sheet: &[u8; 256], n: usize) -> Vec<u8> {
    let codes = codes_of(sheet);
    // For every run of LONGEST bits: the tone it starts with, and its code's length.
    let mut table = vec![(0u8, 0u8); 1 << LONGEST];
    for s in (0..256).filter(|s| sheet[*s] > 0) {
        let l = sheet[s] as usize;
        let first = (codes[s] as usize) << (LONGEST - l);
        table[first..first + (1 << (LONGEST - l))].fill((s as u8, l as u8));
    }
    let mut out = Vec::with_capacity(n);
    let (mut acc, mut held, mut at) = (0u64, 0u32, 0usize);
    for _ in 0..n {
        while held < LONGEST as u32 {
            acc = (acc << 8) | bytes.get(at).copied().unwrap_or(0) as u64;
            at += 1;
            held += 8;
        }
        let (tone, l) = table[((acc >> (held - LONGEST as u32)) & ((1 << LONGEST) - 1)) as usize];
        held -= l as u32;
        out.push(tone);
    }
    out
}

trait Bits {
    fn bit(&mut self, said: u32, v: bool) -> bool;
}
impl Bits for Encoder {
    #[inline(always)]
    fn bit(&mut self, said: u32, v: bool) -> bool {
        self.fixed(4096 - said, v);
        v
    }
}
impl Bits for Decoder<'_> {
    #[inline(always)]
    fn bit(&mut self, said: u32, _: bool) -> bool {
        self.fixed(4096 - said)
    }
}

/// One part of a channel through the whole film by the learning coder, written or read.
/// Gives how many samples did not come out as they are in `frames`.
fn learned<C: Bits>(c: &mut C, frames: &[Vec<u8>], cuts: &(dyn Fn(usize) -> Vec<(usize, usize)> + Sync), k: usize, part: usize, step: &[i32; 256]) -> usize {
    // For each run of bits above: how likely the next bit is 1 (of 65536), how often asked.
    let mut tree = [[32768u16, 0]; 256];
    let mut wrong = 0;
    for (f, frame) in frames.iter().enumerate() {
        let (a, b) = cuts(f)[k];
        let n = b - a;
        for want in &frame[a + n * part / PARTS..a + n * (part + 1) / PARTS] {
            let mut above = 1usize;
            for sheet in (0..8).rev() {
                let s = &mut tree[above];
                let bit = c.bit(((s[0] >> 4) as u32).clamp(1, 4095), (want >> sheet) & 1 == 1);
                let target = if bit { 65535i32 } else { 0 };
                *s = [(s[0] as i32 + (((target - s[0] as i32) * step[s[1] as usize]) >> 16)) as u16, (s[1] + 1).min(254)];
                above = (above << 1) | bit as usize;
            }
            wrong += (above as u8 != *want) as usize;
        }
    }
    wrong
}

struct Way {
    name: &'static str,
    /// Bytes for each channel, the sheets with them; and the sheets alone.
    bytes: Vec<usize>,
    sheets: usize,
    times: (f64, f64),
    wrong: usize,
}

/// The three ways on `frames`, whose channels lie at `cuts(frame)`. Gives the bytes each way.
fn study(name: &str, channels: &[&str], frames: &[Vec<u8>], cuts: &(dyn Fn(usize) -> Vec<(usize, usize)> + Sync), pixels: u64) -> Vec<usize> {
    let ch = channels.len();
    let samples: Vec<u64> = (0..ch).map(|k| (0..frames.len()).map(|f| (cuts(f)[k].1 - cuts(f)[k].0) as u64).sum()).collect();
    let mut ways = Vec::new();
    // What the tones themselves allow at the least (no code shorter than its likelihood).
    let all: Vec<[u64; 256]> = (0..ch)
        .map(|k| {
            (0..frames.len()).into_par_iter().map(|f| count(&frames[f][cuts(f)[k].0..cuts(f)[k].1])).reduce(
                || [0u64; 256],
                |mut a, b| {
                    for (x, y) in a.iter_mut().zip(b) {
                        *x += y;
                    }
                    a
                },
            )
        })
        .collect();
    for (title, per_frame) in [("a sheet for every frame", true), ("one sheet for the scene", false)] {
        let scene: Vec<[u8; 256]> = all.iter().map(sheet_of).collect();
        let from = Instant::now();
        let written: Vec<Vec<([u8; 256], Vec<u8>)>> = (0..frames.len())
            .into_par_iter()
            .map(|f| {
                (0..ch)
                    .map(|k| {
                        let data = &frames[f][cuts(f)[k].0..cuts(f)[k].1];
                        let sheet = if per_frame { sheet_of(&count(data)) } else { scene[k] };
                        (sheet, write(data, &sheet))
                    })
                    .collect()
            })
            .collect();
        let write_time = from.elapsed().as_secs_f64();
        let from = Instant::now();
        let wrong: usize = (0..frames.len())
            .into_par_iter()
            .map(|f| (0..ch).map(|k| if read(&written[f][k].1, &written[f][k].0, cuts(f)[k].1 - cuts(f)[k].0) == frames[f][cuts(f)[k].0..cuts(f)[k].1] { 0 } else { 1 }).sum::<usize>())
            .sum();
        let read_time = from.elapsed().as_secs_f64();
        let sheets = if per_frame { SHEET * frames.len() } else { SHEET };
        ways.push(Way { name: title, bytes: (0..ch).map(|k| written.iter().map(|w| w[k].1.len()).sum::<usize>() + sheets).collect(), sheets: sheets * ch, times: (write_time, read_time), wrong });
    }
    let step: [i32; 256] = std::array::from_fn(|n| 65536 / (n as i32 + 2));
    let jobs: Vec<(usize, usize)> = (0..ch).flat_map(|k| (0..PARTS).map(move |p| (k, p))).collect();
    let from = Instant::now();
    let written: Vec<Vec<u8>> = jobs
        .par_iter()
        .map(|(k, p)| {
            let mut c = Encoder::new();
            learned(&mut c, frames, cuts, *k, *p, &step);
            c.finish()
        })
        .collect();
    let write_time = from.elapsed().as_secs_f64();
    let from = Instant::now();
    let wrong: usize = jobs.par_iter().zip(&written).map(|((k, p), bytes)| learned(&mut Decoder::new(bytes), frames, cuts, *k, *p, &step)).sum();
    let read_time = from.elapsed().as_secs_f64();
    ways.push(Way { name: "a learning coder, no sheet kept", bytes: (0..ch).map(|k| jobs.iter().zip(&written).filter(|(j, _)| j.0 == k).map(|(_, b)| b.len()).sum()).collect(), sheets: 0, times: (write_time, read_time), wrong });

    println!("\n== CHEAT SHEETS of {name}");
    let least: Vec<f64> = all.iter().map(|c| { let n: u64 = c.iter().sum(); c.iter().filter(|x| **x > 0).map(|x| -(*x as f64) * (*x as f64 / n as f64).log2()).sum::<f64>() / 8.0 }).collect();
    println!("  {:<34}{}        all   bits a pixel", "bytes (bits a sample):", channels.iter().map(|c| format!("{c:>22}")).collect::<String>());
    println!("  {:<34}{} {:>10}", "as they are", samples.iter().map(|s| format!("{s:>14} (8.000)")).collect::<String>(), samples.iter().sum::<u64>());
    println!("  {:<34}{} {:>10.0}        {:.3}", "the least one scene sheet allows", least.iter().zip(&samples).map(|(l, s)| format!("{l:>14.0} ({:.3})", 8.0 * l / *s as f64)).collect::<String>(), least.iter().sum::<f64>(), 8.0 * least.iter().sum::<f64>() / pixels as f64);
    for w in &ways {
        let total: usize = w.bytes.iter().sum();
        println!(
            "  {:<34}{} {total:>10}        {:.3}   the sheets {} bytes; writing {:.1} s, reading {:.1} s; {}",
            w.name,
            w.bytes.iter().zip(&samples).map(|(b, s)| format!("{b:>14} ({:.3})", 8.0 * *b as f64 / *s as f64)).collect::<String>(),
            8.0 * total as f64 / pixels as f64,
            w.sheets,
            w.times.0,
            w.times.1,
            if w.wrong == 0 { "every sample came back".to_string() } else { format!("{} came back WRONG", w.wrong) }
        );
    }
    ways.iter().map(|w| w.bytes.iter().sum()).collect()
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
    let pixels = (w * h * frames) as u64;
    println!("{frames} frames of {w} x {h}, yuv420p as they are: the channels are light and two colours (no red, green and blue to try), 12 bits a pixel");
    let at = geo.offsets();
    let planes: Vec<(usize, usize)> = (0..3).map(|k| (at[k], at[k] + geo.planes[k].len())).collect();
    let raw = study("the samples themselves", &["light", "colour 1", "colour 2"], &all, &|_| planes.clone(), pixels);
    let how = mix::How { light: 0, two: false, quarter: true, gold: false, tones: false, pyramid: false };
    let mut memory = mix::Memory::new();
    let from = Instant::now();
    let (mut packed, mut misses) = (Vec::new(), Vec::new());
    for f in 0..frames {
        let before = if f > 0 { Some(&all[f - 1][..]) } else { None };
        packed.push(mix::pack(&geo, &all[f], before, bands, &mut memory, how));
        misses.push(if before == Some(&all[f][..]) { vec![0u8; geo.frame_len()] } else { memory.misses(&geo, bands).iter().map(|r| ((*r as i32) << 1 ^ (*r as i32) >> 7) as u8).collect() });
    }
    let mix_time = from.elapsed().as_secs_f64();
    let mixed: usize = packed.iter().map(|p| p.len()).sum();
    let missed = study("what the mix's guess missed by (before the mix writes it)", &["light", "colour 1", "colour 2"], &misses, &|_| planes.clone(), pixels);
    let on_top = study("the mix's packed bytes", &["the bytes"], &packed, &|f| vec![(0, packed[f].len())], pixels);
    println!("\n== TOGETHER ({frames} frames; bytes, bits a pixel)");
    let line = |name: &str, bytes: usize| println!("  {name:<58}{bytes:>12}   {:.3}", 8.0 * bytes as f64 / pixels as f64);
    line("the samples as they are", geo.frame_len() * frames);
    for (k, way) in ["a sheet for every frame", "one sheet for the scene", "a learning coder"].iter().enumerate() {
        line(&format!("the samples by cheat sheet: {way}"), raw[k]);
    }
    line(&format!("the mix as it is (packed here frame by frame in {mix_time:.1} s)"), mixed);
    for (k, way) in ["a sheet for every frame", "one sheet for the scene", "a learning coder"].iter().enumerate() {
        line(&format!("the mix's packed bytes by cheat sheet: {way}"), on_top[k]);
    }
    for (k, way) in ["a sheet for every frame", "one sheet for the scene", "a learning coder"].iter().enumerate() {
        line(&format!("the mix's misses by cheat sheet: {way}"), missed[k]);
    }
    Ok(())
}
