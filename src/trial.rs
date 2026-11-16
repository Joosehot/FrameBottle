//! A trial bench for the exact packers: frames from a raw yuv420p file, packed and
//! unpacked in memory by the strike, with the size, the times and whether every sample
//! came back. FB_HOW: the light map (0, 1, 2), then t (two guessers), q (vectors to a
//! quarter), g (fixed weights), s (a miss as bit sheets). FB_CHUNK: the frames of a stretch (each starts with a
//! frame that stands alone). FB_WINDOW: the player's window.

use crate::mix;
use crate::pack::Geometry;
use std::io::Read;
use std::time::Instant;

pub fn run(path: &std::path::Path, w: usize, h: usize, frames: usize, bands: usize, alone: bool) -> Result<(), String> {
    let geo = Geometry::of("yuv420p", w, h).ok_or("no such geometry")?;
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut all = Vec::new();
    for _ in 0..frames {
        let mut f = vec![0u8; geo.frame_len()];
        file.read_exact(&mut f).map_err(|e| e.to_string())?;
        all.push(f);
    }
    let said = std::env::var("FB_HOW").unwrap_or_default();
    let how = mix::How { light: said.chars().next().and_then(|c| c.to_digit(10)).unwrap_or(0) as u8, two: said.contains('t'), quarter: said.contains('q'), gold: said.contains('g'), sheets: said.contains('s') };
    let chunk: usize = std::env::var("FB_CHUNK").ok().and_then(|v| v.parse().ok()).unwrap_or(frames).max(1);
    let window: usize = std::env::var("FB_WINDOW").ok().and_then(|v| v.parse().ok()).unwrap_or(32);
    let refs: Vec<&[u8]> = all.iter().map(|f| &f[..]).collect();
    let from = Instant::now();
    let (mut packed, mut packing) = (Vec::new(), mix::Struck::default());
    for stretch in refs.chunks(chunk) {
        let (p, how_it_went) = mix::strike_pack(&geo, stretch, None, alone, bands, &mut mix::Memory::new(), how)?;
        packed.extend(p);
        packing.add(&how_it_went);
    }
    let pack_time = from.elapsed().as_secs_f64();
    let from = Instant::now();
    let mut same = 0;
    let unpacking = mix::strike_unpack(&geo, frames, window, None, &mut mix::Memory::new(), &mut |f| Ok(packed[f].clone()), &|_, _| Ok(()), &mut |f, frame| {
        same += (frame == all[f]) as usize;
        Ok(true)
    })?;
    let unpack_time = from.elapsed().as_secs_f64();
    let bytes: usize = packed.iter().map(|p| p.len()).sum();
    println!(
        "{how:?}, {bands} bands, stretches of {chunk}, a window of {window}: {bytes} bytes for {frames} frames; pack {:.1} ms a frame (vectors {:.1}, weights {:.1}; busy {:.0} %), unpack {:.1} ms a frame ({} workers, busy {:.0} %); {same} of {frames} identical",
        1000.0 * pack_time / frames as f64,
        1000.0 * packing.map / frames as f64,
        1000.0 * packing.solve / frames as f64,
        100.0 * packing.busy_share(),
        1000.0 * unpack_time / frames as f64,
        unpacking.workers,
        100.0 * unpacking.busy_share()
    );
    let lit: Vec<u64> = mix::LIT.iter().map(|a| a.load(std::sync::atomic::Ordering::Relaxed)).collect();
    if lit[1] > 0 {
        println!("the light map: {} of {} tiles put right ({:.1} %)", lit[0], lit[1], 100.0 * lit[0] as f64 / lit[1] as f64);
    }
    if let Some(p) = mix::profile() {
        println!("{p}");
    }
    Ok(())
}
