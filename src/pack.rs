//! PACK: one tile of the grid (one frame) into smaller bytes, and back,
//! exactly. A tile stands on its own, or leans on the tile before it on
//! the grid: then only what differs from that tile is packed, and it
//! unpacks after that tile. Either way every frame is kept and comes back
//! byte for byte.

use crate::motor::{self, Choice, Rules};
use std::collections::HashMap;
use std::io::{Read, Write};

/// One plane of a frame: `w` x `h` pixels of `ch` samples of `bps` bytes;
/// `sx`, `sy` are how many times it is halved against the frame.
#[derive(Clone, Debug)]
pub struct Plane {
    pub w: usize,
    pub h: usize,
    pub ch: usize,
    pub bps: usize,
    pub sx: u32,
    pub sy: u32,
}

impl Plane {
    pub fn px(&self) -> usize {
        self.ch * self.bps
    }
    pub fn stride(&self) -> usize {
        self.w * self.px()
    }
    pub fn len(&self) -> usize {
        self.stride() * self.h
    }
}

#[derive(Clone, Debug)]
pub struct Geometry {
    pub pix_fmt: String,
    pub width: usize,
    pub height: usize,
    pub planes: Vec<Plane>,
}

impl Geometry {
    /// The layout of a raw frame in this pixel format (named as ffmpeg names them), if Frame Bottle knows it.
    pub fn of(pix_fmt: &str, w: usize, h: usize) -> Option<Geometry> {
        let yuv = |sx: u32, sy: u32, bps: usize| {
            let (cw, chh) = ((w + (1 << sx) - 1) >> sx, (h + (1 << sy) - 1) >> sy);
            vec![
                Plane { w, h, ch: 1, bps, sx: 0, sy: 0 },
                Plane { w: cw, h: chh, ch: 1, bps, sx, sy },
                Plane { w: cw, h: chh, ch: 1, bps, sx, sy },
            ]
        };
        let planes = match pix_fmt {
            "yuv420p" | "yuvj420p" => yuv(1, 1, 1),
            "yuv422p" | "yuvj422p" => yuv(1, 0, 1),
            "yuv444p" | "yuvj444p" => yuv(0, 0, 1),
            "yuv420p10le" => yuv(1, 1, 2),
            "yuv422p10le" => yuv(1, 0, 2),
            "yuv444p10le" => yuv(0, 0, 2),
            "gray" => vec![Plane { w, h, ch: 1, bps: 1, sx: 0, sy: 0 }],
            "rgb24" => vec![Plane { w, h, ch: 3, bps: 1, sx: 0, sy: 0 }],
            "rgba" => vec![Plane { w, h, ch: 4, bps: 1, sx: 0, sy: 0 }],
            _ => return None,
        };
        Some(Geometry { pix_fmt: pix_fmt.to_string(), width: w, height: h, planes })
    }
    pub fn frame_len(&self) -> usize {
        self.planes.iter().map(|p| p.len()).sum()
    }
    /// Where each plane starts in the frame.
    pub fn offsets(&self) -> Vec<usize> {
        let mut at = 0;
        self.planes
            .iter()
            .map(|p| {
                let o = at;
                at += p.len();
                o
            })
            .collect()
    }
}

pub const FROMS: [&str; 3] = ["own", "tile", "guess"];
pub const PREDICTS: [&str; 5] = ["none", "left", "up", "paeth", "med"];
pub const CODERS: [&str; 3] = ["deflate", "zstd", "xz"];

#[inline(always)]
fn guess(predict: u8, a: u8, b: u8, c: u8) -> u8 {
    match predict {
        0 => 0,
        1 => a,
        2 => b,
        3 => {
            let (ia, ib, ic) = (a as i32, b as i32, c as i32);
            let p = ia + ib - ic;
            let (pa, pb, pc) = ((p - ia).abs(), (p - ib).abs(), (p - ic).abs());
            if pa <= pb && pa <= pc {
                a
            } else if pb <= pc {
                b
            } else {
                c
            }
        }
        _ => {
            let (lo, hi) = if a < b { (a, b) } else { (b, a) };
            if c >= hi {
                lo
            } else if c <= lo {
                hi
            } else {
                (a as i32 + b as i32 - c as i32) as u8
            }
        }
    }
}

/// Row `y` of a plane as what is left after the guess from the pixel on the
/// left (a), above (b) and above-left (c); outside the tile they are 0.
fn residual_row(p: &Plane, data: &[u8], y: usize, predict: u8, out: &mut Vec<u8>) {
    let (stride, px) = (p.stride(), p.px());
    let row = &data[y * stride..(y + 1) * stride];
    if predict == 0 {
        out.extend_from_slice(row);
        return;
    }
    let zeros;
    let up: &[u8] = if y > 0 {
        &data[(y - 1) * stride..y * stride]
    } else {
        zeros = vec![0u8; stride];
        &zeros
    };
    for i in 0..px.min(stride) {
        out.push(row[i].wrapping_sub(guess(predict, 0, up[i], 0)));
    }
    for i in px..stride {
        out.push(row[i].wrapping_sub(guess(predict, row[i - px], up[i], up[i - px])));
    }
}

fn restore_plane(p: &Plane, res: &[u8], predict: u8, out: &mut [u8]) {
    let (stride, px) = (p.stride(), p.px());
    if predict == 0 {
        out.copy_from_slice(res);
        return;
    }
    let zeros = vec![0u8; stride];
    for y in 0..p.h {
        let (done, rest) = out.split_at_mut(y * stride);
        let up: &[u8] = if y > 0 { &done[(y - 1) * stride..] } else { &zeros };
        let row = &mut rest[..stride];
        let r = &res[y * stride..(y + 1) * stride];
        for i in 0..px.min(stride) {
            row[i] = r[i].wrapping_add(guess(predict, 0, up[i], 0));
        }
        for i in px..stride {
            row[i] = r[i].wrapping_add(guess(predict, row[i - px], up[i], up[i - px]));
        }
    }
}

fn encode(coder: u8, data: &[u8]) -> Vec<u8> {
    match coder {
        0 => {
            let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(6));
            e.write_all(data).unwrap();
            e.finish().unwrap()
        }
        1 => zstd::bulk::compress(data, 19).unwrap(),
        _ => {
            let mut e = xz2::write::XzEncoder::new(Vec::new(), 6);
            e.write_all(data).unwrap();
            e.finish().unwrap()
        }
    }
}

fn decode(coder: u8, data: &[u8], len: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(len);
    match coder {
        0 => flate2::read::ZlibDecoder::new(data).read_to_end(&mut out).map(|_| ()).map_err(|e| e.to_string())?,
        1 => out = zstd::bulk::decompress(data, len).map_err(|e| e.to_string())?,
        2 => xz2::read::XzDecoder::new(data).read_to_end(&mut out).map(|_| ()).map_err(|e| e.to_string())?,
        c => return Err(format!("unknown coder {c}")),
    }
    if out.len() != len {
        return Err(format!("a tile unpacked to {} bytes, not {len}", out.len()));
    }
    Ok(out)
}

fn id(names: &[&str], value: &str) -> u8 {
    names.iter().position(|n| *n == value).unwrap() as u8
}

/// The rows the size judge reads: bands spread evenly over the plane.
fn sample_rows(h: usize, bands: usize, band_rows: usize) -> Vec<usize> {
    if h <= bands * band_rows || bands < 2 {
        return (0..h).collect();
    }
    let mut rows = Vec::new();
    for k in 0..bands {
        let start = k * (h - band_rows) / (bands - 1);
        rows.extend(start..start + band_rows);
    }
    rows
}

fn sample(geo: &Geometry, frame: &[u8], predict: u8, rules: &Rules) -> Vec<u8> {
    let mut out = Vec::new();
    for (p, at) in geo.planes.iter().zip(geo.offsets()) {
        let data = &frame[at..at + p.len()];
        for y in sample_rows(p.h, rules.sample_bands, rules.band_rows) {
            residual_row(p, data, y, predict, &mut out);
        }
    }
    out
}

/// Bits per byte if every byte were coded by how often it occurs.
fn entropy(data: &[u8]) -> f64 {
    let mut n = [0u64; 256];
    for b in data {
        n[*b as usize] += 1;
    }
    let total = data.len().max(1) as f64;
    n.iter()
        .filter(|c| **c > 0)
        .map(|c| {
            let p = *c as f64 / total;
            -p * p.log2()
        })
        .sum()
}

/// What differs between a tile and the tile before it, byte by byte.
fn differ(frame: &[u8], before: &[u8]) -> Vec<u8> {
    frame.iter().zip(before).map(|(a, b)| a.wrapping_sub(*b)).collect()
}

/// HotMotor chooses how this tile is packed. The size judge reads the tile:
/// until the coder is chosen it scores how few bits the sample would need,
/// with the coder chosen too it packs the sample and scores what it saved.
/// `before` is the tile before this one on the grid, if there is one.
/// `hammer` is what the Hammer's stack guesses for this tile.
pub fn choose(geo: &Geometry, frame: &[u8], before: Option<&[u8]>, hammer: Option<&[u8]>, rules: &Rules, profile: &[f64; 3]) -> Vec<motor::Candidate> {
    let leaning: Option<Vec<u8>> = before.map(|b| differ(frame, b));
    let mut samples: HashMap<(u8, u8), Vec<u8>> = HashMap::new();
    let mut guessed: Option<(usize, usize)> = None;
    let mut judge = |choice: &Choice| {
        let from = id(&FROMS, motor::get(choice, "from").unwrap());
        if from == 2 {
            // A guessed tile: the sample is guessed and corrected as the pack would be.
            let (coded, raw) = *guessed.get_or_insert_with(|| crate::guess::sample(geo, frame, before, hammer, rules));
            let share = coded as f64 / raw.max(1) as f64;
            let bonus = rules.judge_size * profile[2] * (1.0 - share);
            return (bonus, vec![format!("+{bonus:.2} size: the sample guessed and corrected is {:.1}% of itself", share * 100.0)]);
        }
        let data: &[u8] = match (from, &leaning) {
            (0, _) => frame,
            (_, Some(d)) => d,
            (_, None) => return (-1e9, vec!["there is no tile before this one".to_string()]),
        };
        let predict = motor::get(choice, "predict").map_or(0, |p| id(&PREDICTS, p));
        let s = samples.entry((from, predict)).or_insert_with(|| sample(geo, data, predict, rules));
        let (share, how) = match motor::get(choice, "coder") {
            None => (entropy(s) / 8.0, "by how often each byte occurs"),
            Some(c) => (encode(id(&CODERS, c), s).len() as f64 / s.len().max(1) as f64, "packed"),
        };
        let bonus = rules.judge_size * profile[2] * (1.0 - share);
        (bonus, vec![format!("+{bonus:.2} size: the sample {how} is {:.1}% of itself", share * 100.0)])
    };
    motor::search(rules, profile, &mut judge)
}

/// The tile packed by this choice: what it leans on, the guess, the coder, the coder's bytes.
pub fn pack(geo: &Geometry, frame: &[u8], before: Option<&[u8]>, hammer: Option<&[u8]>, choice: &Choice, rules: &Rules) -> Vec<u8> {
    let from = id(&FROMS, motor::get(choice, "from").unwrap());
    if from == 2 {
        let mut out = vec![2, 0, 0];
        out.extend(crate::guess::pack(geo, frame, before, hammer, rules.bands));
        return out;
    }
    let leaning;
    let frame: &[u8] = if from == 1 {
        leaning = differ(frame, before.expect("a leaning tile has a tile before it"));
        &leaning
    } else {
        frame
    };
    let predict = id(&PREDICTS, motor::get(choice, "predict").unwrap());
    let coder = id(&CODERS, motor::get(choice, "coder").unwrap());
    let mut res = Vec::with_capacity(frame.len());
    for (p, at) in geo.planes.iter().zip(geo.offsets()) {
        let data = &frame[at..at + p.len()];
        for y in 0..p.h {
            residual_row(p, data, y, predict, &mut res);
        }
    }
    let mut out = vec![from, predict, coder];
    out.extend(encode(coder, &res));
    out
}

/// Does this packed tile lean on the tile before it?
pub fn leans(packed: &[u8]) -> bool {
    packed.first() == Some(&1) || (fractal(packed) && !crate::fractal::alone(&packed[3..])) || (guessed(packed) && crate::guess::from_before(&packed[3..])) || (blocks(packed) && crate::block::from_before(&packed[3..]))
}

/// Is this packed tile a frame of fractal rules (not exact)?
pub fn fractal(packed: &[u8]) -> bool {
    packed.len() > 3 && packed[0] == 4
}

/// Is this packed tile a frame of blocks (AV1's way, not exact)?
pub fn blocks(packed: &[u8]) -> bool {
    packed.len() > 3 && packed[0] == 3
}

/// Is this packed tile a guess with its corrections?
pub fn guessed(packed: &[u8]) -> bool {
    packed.len() > 3 && packed[0] == 2
}

/// (16 x 16 tiles guessed perfectly, tiles) of a guessed frame; (0, 0) for any other.
pub fn guess_tiles(packed: &[u8]) -> (usize, usize) {
    if guessed(packed) { crate::guess::tiles(&packed[3..]) } else { (0, 0) }
}

/// The planes of a guessed frame for which Golden's guesser spoke.
pub fn golden_planes(geo: &Geometry, packed: &[u8]) -> usize {
    if guessed(packed) { crate::guess::golden_planes(geo, &packed[3..]) } else { 0 }
}

/// Was this tile guessed perfectly (it holds no correction at all)?
pub fn perfect(packed: &[u8]) -> bool {
    guessed(packed) && crate::guess::perfect(&packed[3..])
}

/// The first half of unpacking, which needs no other tile: the coder's bytes back.
pub fn uncode(geo: &Geometry, packed: &[u8]) -> Result<Vec<u8>, String> {
    if packed.len() < 3 || packed[0] as usize >= FROMS.len() || packed[1] as usize >= PREDICTS.len() {
        return Err("not a packed tile".into());
    }
    decode(packed[2], &packed[3..], geo.frame_len())
}

/// The second half, which needs no other tile either: the guesses undone. For a tile
/// that stands on its own this is the frame; for a leaning one, `lean_on` finishes it.
pub fn restore(geo: &Geometry, packed: &[u8], res: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; res.len()];
    let mut rest = &mut out[..];
    for (p, at) in geo.planes.iter().zip(geo.offsets()) {
        let (plane, r) = rest.split_at_mut(p.len());
        restore_plane(p, &res[at..at + p.len()], packed[1], plane);
        rest = r;
    }
    out
}

/// A leaning tile becomes its frame: what differs, put back on the frame before it.
pub fn lean_on(tile: &mut [u8], before: &[u8]) {
    for (o, b) in tile.iter_mut().zip(before) {
        *o = o.wrapping_add(*b);
    }
}

pub fn choice_name(packed: &[u8]) -> String {
    if guessed(packed) {
        return if perfect(packed) { "guess, perfect".into() } else { "guess".into() };
    }
    if blocks(packed) {
        return "blocks".into();
    }
    if fractal(packed) {
        return "fractal rules".into();
    }
    format!("{}+{}+{}", FROMS[packed[0] as usize], PREDICTS[packed[1] as usize], CODERS[packed[2] as usize])
}
