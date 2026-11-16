//! THE FAR CACHE, as a bench of its own (`fbottle far pack`, `fbottle far unpack`): the
//! learning mix with a long memory. Both sides keep the frames rebuilt so far, and a
//! block may be guessed from any frame kept; which one is told for each block.
//!
//! - The past: frames up to `past` before the one being told are kept, the oldest dropped.
//! - Thinned: with `sparse`, of the frames more than `near` before only every other one is kept.
//! - Ahead: with a `period` above 1 the frames are told out of order (frame 0, then every
//!   `period`th frame with the frames between it and the one before halved again and
//!   again), so a frame may be guessed from frames up to `period` after it too.
//!
//! The file is this bench's own (not a bottle): a head, then every frame's bytes in the
//! order told. The order is the head's `period`, so nothing more is kept for it.

use crate::mix;
use crate::pack::Geometry;
use std::io::{Read, Seek, SeekFrom, Write};
use std::time::Instant;

#[derive(Clone, Copy, Debug)]
pub struct Plan {
    pub past: usize,
    pub near: usize,
    pub period: usize,
    pub sparse: bool,
}

impl Plan {
    /// May frame `d` be guessed from frame `e` (when `e` is told before it)?
    fn reads(&self, d: usize, e: usize) -> bool {
        e != d && e + self.past >= d && (e < d || e - d <= self.period) && (d.abs_diff(e) <= self.near || e % 2 == 0 || !self.sparse)
    }
    /// Is frame `e` of no use any more, when every frame before `lowest` is told?
    fn over(&self, e: usize, lowest: usize) -> bool {
        e + self.past < lowest || (self.sparse && e % 2 == 1 && e + self.near < lowest)
    }
    /// The order the frames are told in.
    fn order(&self, n: usize) -> Vec<usize> {
        fn between(out: &mut Vec<usize>, lo: usize, hi: usize) {
            if hi - lo > 1 {
                let mid = (lo + hi) / 2;
                out.push(mid);
                between(out, lo, mid);
                between(out, mid, hi);
            }
        }
        let mut out = Vec::with_capacity(n);
        if n > 0 {
            out.push(0);
        }
        let mut at = 0;
        while at + 1 < n {
            let next = (at + self.period.max(1)).min(n - 1);
            out.push(next);
            between(&mut out, at, next);
            at = next;
        }
        out
    }
}

/// The frames kept: each with its place in the film (and, in the packer, its small picture).
struct Kept {
    frames: Vec<(usize, Vec<u8>, Vec<u8>)>,
    told: Vec<bool>,
    lowest: usize,
    most: usize,
}

impl Kept {
    fn new(n: usize) -> Kept {
        Kept { frames: Vec::new(), told: vec![false; n], lowest: 0, most: 0 }
    }
    /// The frames `d` may be guessed from, the nearest first (of two as near, the earlier).
    fn for_frame(&self, plan: &Plan, d: usize) -> Vec<&(usize, Vec<u8>, Vec<u8>)> {
        let mut list: Vec<_> = self.frames.iter().filter(|f| plan.reads(d, f.0)).collect();
        list.sort_by_key(|f| (d.abs_diff(f.0), f.0 > d));
        list
    }
    fn keep(&mut self, plan: &Plan, d: usize, frame: Vec<u8>, small: Vec<u8>) {
        self.frames.push((d, frame, small));
        self.most = self.most.max(self.frames.len());
        self.told[d] = true;
        while self.lowest < self.told.len() && self.told[self.lowest] {
            self.lowest += 1;
        }
        let lowest = self.lowest;
        self.frames.retain(|f| !plan.over(f.0, lowest));
    }
}

/// The most memory this run has held at once, in bytes (Windows; 0 elsewhere).
fn peak_memory() -> usize {
    #[cfg(windows)]
    {
        #[repr(C)]
        struct Counters {
            cb: u32,
            faults: u32,
            peak: usize,
            rest: [usize; 7],
        }
        unsafe extern "system" {
            fn GetCurrentProcess() -> isize;
            fn K32GetProcessMemoryInfo(process: isize, counters: *mut Counters, cb: u32) -> i32;
        }
        let mut c = Counters { cb: std::mem::size_of::<Counters>() as u32, faults: 0, peak: 0, rest: [0; 7] };
        if unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) } != 0 {
            return c.peak;
        }
    }
    0
}

const MARK: &[u8; 8] = b"FBFAR 1\n";

pub fn pack(yuv: &std::path::Path, w: usize, h: usize, n: usize, bands: usize, plan: Plan, out: &std::path::Path) -> Result<(), String> {
    let geo = Geometry::of("yuv420p", w, h).ok_or("no such geometry")?;
    let how = mix::How { light: 0, two: false, quarter: true, gold: false, tones: false, pyramid: false, plates: 0 };
    let mut file = std::fs::File::open(yuv).map_err(|e| e.to_string())?;
    let mut to = std::io::BufWriter::new(std::fs::File::create(out).map_err(|e| e.to_string())?);
    let began = Instant::now();
    to.write_all(MARK).map_err(|e| e.to_string())?;
    for v in [w, h, n, bands, plan.past, plan.near, plan.period, plan.sparse as usize] {
        to.write_all(&(v as u32).to_le_bytes()).map_err(|e| e.to_string())?;
    }
    let mut kept = Kept::new(n);
    let mut memory = mix::Memory::new();
    let (mut bytes, mut work) = (8 + 32u64, 0.0);
    // Blocks: all, not from the nearest, from more than `near` before, from a frame after.
    let mut blocks = [0u64; 4];
    for d in plan.order(n) {
        let mut frame = vec![0u8; geo.frame_len()];
        file.seek(SeekFrom::Start((d * geo.frame_len()) as u64)).map_err(|e| e.to_string())?;
        file.read_exact(&mut frame).map_err(|e| e.to_string())?;
        let from = Instant::now();
        let small = mix::thumb(&geo, &frame);
        let list = kept.for_frame(&plan, d);
        let refs: Vec<&[u8]> = list.iter().map(|f| &f.1[..]).collect();
        let smalls: Vec<&[u8]> = list.iter().map(|f| &f.2[..]).collect();
        let (packed, which) = mix::pack_far(&geo, &frame, &refs, &small, &smalls, bands, &mut memory, how);
        for r in &which {
            let e = list[*r as usize].0;
            blocks[0] += 1;
            blocks[1] += (*r > 0) as u64;
            blocks[2] += (e + plan.near < d) as u64;
            blocks[3] += (e > d) as u64;
        }
        work += from.elapsed().as_secs_f64();
        to.write_all(&(packed.len() as u32).to_le_bytes()).and_then(|_| to.write_all(&packed)).map_err(|e| e.to_string())?;
        bytes += 4 + packed.len() as u64;
        kept.keep(&plan, d, frame, small);
    }
    to.flush().map_err(|e| e.to_string())?;
    let share = |k: usize| 100.0 * blocks[k] as f64 / blocks[0].max(1) as f64;
    println!("{plan:?}");
    println!("packed {n} frames of {w} x {h} into {bytes} bytes ({:.4} bits a pixel) in {:.1} s ({:.1} s of it packing, {:.1} ms a frame)", 8.0 * bytes as f64 / (n * w * h) as f64, began.elapsed().as_secs_f64(), work, 1000.0 * work / n as f64);
    println!("blocks: {} told; {:.3} % not from the nearest frame kept, {:.3} % from a frame more than {} before, {:.3} % from a frame after", blocks[0], share(1), share(2), plan.near, share(3));
    println!("frames kept at once: {} at most ({} MB of frames); the most memory held: {} MB", kept.most, kept.most * geo.frame_len() >> 20, peak_memory() >> 20);
    Ok(())
}

pub fn unpack(packed: &std::path::Path, yuv: &std::path::Path) -> Result<(), String> {
    let mut file = std::io::BufReader::new(std::fs::File::open(packed).map_err(|e| e.to_string())?);
    let mut head = [0u8; 40];
    file.read_exact(&mut head).map_err(|e| e.to_string())?;
    if &head[..8] != MARK {
        return Err("not a far cache bench file".into());
    }
    let said: Vec<usize> = head[8..].chunks(4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize).collect();
    let (w, h, n) = (said[0], said[1], said[2]);
    let plan = Plan { past: said[4], near: said[5], period: said[6], sparse: said[7] != 0 };
    let geo = Geometry::of("yuv420p", w, h).ok_or("no such geometry")?;
    let mut real = std::fs::File::open(yuv).map_err(|e| e.to_string())?;
    let began = Instant::now();
    let mut kept = Kept::new(n);
    let mut memory = mix::Memory::new();
    let (mut same, mut work) = (0, 0.0);
    let mut want = vec![0u8; geo.frame_len()];
    for d in plan.order(n) {
        let from = Instant::now();
        let mut len = [0u8; 4];
        file.read_exact(&mut len).map_err(|e| e.to_string())?;
        let mut payload = vec![0u8; u32::from_le_bytes(len) as usize];
        file.read_exact(&mut payload).map_err(|e| e.to_string())?;
        let list = kept.for_frame(&plan, d);
        let refs: Vec<&[u8]> = list.iter().map(|f| &f.1[..]).collect();
        let frame = mix::unpack_far(&geo, &payload, &refs, &mut memory)?;
        work += from.elapsed().as_secs_f64();
        real.seek(SeekFrom::Start((d * geo.frame_len()) as u64)).map_err(|e| e.to_string())?;
        real.read_exact(&mut want).map_err(|e| e.to_string())?;
        same += (frame == want) as usize;
        kept.keep(&plan, d, frame, Vec::new());
    }
    println!("{plan:?}");
    println!("unpacked {n} frames in {:.1} s ({:.1} s of it unpacking, {:.1} ms a frame; the rest is reading the film to check against); {same} of {n} identical to the film: {}", began.elapsed().as_secs_f64(), work, 1000.0 * work / n as f64, if same == n { "identical" } else { "NOT identical" });
    println!("frames kept at once: {} at most ({} MB of frames); the most memory held: {} MB", kept.most, kept.most * geo.frame_len() >> 20, peak_memory() >> 20);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_frame_is_told_once_and_its_frames_are_kept() {
        for (n, plan) in [(1, Plan { past: 5, near: 2, period: 1, sparse: false }), (23, Plan { past: 6, near: 2, period: 1, sparse: true }), (23, Plan { past: 6, near: 2, period: 4, sparse: true }), (10, Plan { past: 3, near: 3, period: 8, sparse: false })] {
            let order = plan.order(n);
            let mut sorted = order.clone();
            sorted.sort_unstable();
            assert_eq!(sorted, (0..n).collect::<Vec<_>>());
            let mut kept = Kept::new(n);
            for d in order {
                // Every frame told already that `d` may read is still kept.
                let told: Vec<usize> = (0..n).filter(|e| kept.told[*e] && plan.reads(d, *e)).collect();
                let mut have: Vec<usize> = kept.for_frame(&plan, d).iter().map(|f| f.0).collect();
                have.sort_unstable();
                assert_eq!(have, told, "{plan:?}, frame {d}");
                kept.keep(&plan, d, Vec::new(), Vec::new());
            }
        }
    }
}
