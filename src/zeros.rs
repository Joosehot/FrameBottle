//! A measure, apart from the packers: what x264's ways of skipping would find in the
//! mix's misses. How many blocks are exactly what they were in the frame before (x264
//! skips such a block with one yes/no), how many blocks of misses are all nothing (x264
//! says so with one flag for the block), and what the mix spends on a miss's sign (x264
//! writes a sign as one plain bit).

use crate::mix;
use crate::pack::Geometry;
use std::io::Read;
use std::sync::atomic::Ordering;

pub fn run(path: &std::path::Path, w: usize, h: usize, frames: usize, bands: usize) -> Result<(), String> {
    let geo = Geometry::of("yuv420p", w, h).ok_or("no such geometry")?;
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let how = mix::How { light: 0, two: false, quarter: true, gold: false, tones: false };
    let mut memory = mix::Memory::new();
    let at = geo.offsets();
    let (mut before, mut bytes) = (Vec::new(), 0usize);
    // For each plane: samples, misses that are nothing; for each block side (4, 8, 16): blocks, blocks all nothing.
    let mut n = [[0u64; 8]; 3];
    // Blocks of 16 x 16 of light, and those exactly what they were (their colours too).
    let mut still = [0u64; 2];
    for f in 0..frames {
        let mut frame = vec![0u8; geo.frame_len()];
        file.read_exact(&mut frame).map_err(|e| e.to_string())?;
        bytes += mix::pack(&geo, &frame, if f > 0 { Some(&before[..]) } else { None }, bands, &mut memory, how).len();
        let misses = memory.misses(&geo, bands);
        for k in 0..3 {
            let (pw, ph) = (geo.planes[k].w, geo.planes[k].h);
            let m = &misses[at[k]..at[k] + pw * ph];
            n[k][0] += m.len() as u64;
            n[k][1] += m.iter().filter(|v| **v == 0).count() as u64;
            for (j, side) in [4usize, 8, 16].into_iter().enumerate() {
                for by in (0..ph).step_by(side) {
                    for bx in (0..pw).step_by(side) {
                        n[k][2 + 2 * j] += 1;
                        n[k][3 + 2 * j] += (by..(by + side).min(ph)).all(|y| m[y * pw + bx..y * pw + (bx + side).min(pw)].iter().all(|v| *v == 0)) as u64;
                    }
                }
            }
        }
        if f > 0 {
            let same = |k: usize, x0: usize, y0: usize, side: usize| {
                let (pw, ph) = (geo.planes[k].w, geo.planes[k].h);
                (y0..(y0 + side).min(ph)).all(|y| frame[at[k] + y * pw + x0..at[k] + y * pw + (x0 + side).min(pw)] == before[at[k] + y * pw + x0..at[k] + y * pw + (x0 + side).min(pw)])
            };
            for by in (0..h).step_by(16) {
                for bx in (0..w).step_by(16) {
                    still[0] += 1;
                    still[1] += (same(0, bx, by, 16) && same(1, bx / 2, by / 2, 8) && same(2, bx / 2, by / 2, 8)) as u64;
                }
            }
        }
        before = frame;
    }
    let pct = |a: u64, b: u64| 100.0 * a as f64 / b.max(1) as f64;
    println!("{frames} frames, the mix {bytes} bytes");
    println!("blocks of 16 x 16 exactly what they were in the frame before (light and colours): {} of {} ({:.2} %)", still[1], still[0], pct(still[1], still[0]));
    for (k, name) in ["light", "colour 1", "colour 2"].iter().enumerate() {
        println!("{name}: misses that are nothing {:.1} %; blocks of misses all nothing: 4 x 4 {:.2} %, 8 x 8 {:.2} %, 16 x 16 {:.2} %", pct(n[k][1], n[k][0]), pct(n[k][3], n[k][2]), pct(n[k][5], n[k][4]), pct(n[k][7], n[k][6]));
    }
    let spent: Vec<f64> = mix::SPENT.iter().map(|a| a.load(Ordering::Relaxed) as f64 / 1000.0).collect();
    let signs: u64 = (0..3).map(|k| n[k][0] - n[k][1]).sum();
    println!("bits: is there a miss {:.0} bytes, its sign {:.0} bytes ({:.3} bits a sign; a plain bit each would be {:.0} bytes), its size {:.0} bytes, the vectors {:.0} bytes", spent[4] / 8.0, spent[5] / 8.0, spent[5] / signs.max(1) as f64, signs as f64 / 8.0, spent[6] / 8.0, spent[3] / 8.0);
    Ok(())
}
