//! A trial bench for the exact packers: frames from a raw yuv420p file, packed and
//! unpacked in memory, with the size, the times and whether every sample came back.

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
    let from = Instant::now();
    let mut memory = mix::Memory::new();
    let mut packed = Vec::new();
    for k in 0..frames {
        let before = if k > 0 && !alone { Some(all[k - 1].as_slice()) } else { None };
        packed.push(mix::pack(&geo, &all[k], before, bands, &mut memory));
    }
    let pack_time = from.elapsed().as_secs_f64();
    let from = Instant::now();
    let mut memory = mix::Memory::new();
    let mut last: Option<Vec<u8>> = None;
    let mut same = 0;
    for k in 0..frames {
        let f = mix::unpack(&geo, &packed[k], last.as_deref(), &mut memory)?;
        same += (f == all[k]) as usize;
        last = Some(f);
    }
    let unpack_time = from.elapsed().as_secs_f64();
    let bytes: usize = packed.iter().map(|p| p.len()).sum();
    let later: usize = packed.iter().skip(frames / 2).map(|p| p.len()).sum();
    println!(
        "mix, {bands} bands{}: {bytes} bytes for {frames} frames ({} a frame in the later half); pack {:.1} ms a frame, unpack {:.1} ms a frame; {same} of {frames} identical",
        if alone { ", alone" } else { "" },
        later / (frames - frames / 2).max(1),
        1000.0 * pack_time / frames as f64,
        1000.0 * unpack_time / frames as f64
    );
    let from = Instant::now();
    let (struck, how) = mix::strike_pack(&geo, &all.iter().map(|f| &f[..]).collect::<Vec<_>>(), None, alone, bands, &mut mix::Memory::new())?;
    let strike_pack_time = from.elapsed().as_secs_f64();
    let window: usize = std::env::var("FB_WINDOW").ok().and_then(|v| v.parse().ok()).unwrap_or(32);
    let from = Instant::now();
    let mut same = 0;
    let unstruck = mix::strike_unpack(&geo, frames, window, None, &mut mix::Memory::new(), &mut |f| Ok(struck[f].clone()), &|_, _| Ok(()), &mut |f, frame| {
        same += (frame == all[f]) as usize;
        Ok(true)
    })?;
    let strike_unpack_time = from.elapsed().as_secs_f64();
    println!(
        "the strike, a window of {window}: the same bytes: {}; pack {:.1} ms a frame ({} workers, busy {:.0} %), unpack {:.1} ms a frame (busy {:.0} %); {same} of {frames} identical",
        struck == packed,
        1000.0 * strike_pack_time / frames as f64,
        how.workers,
        100.0 * how.busy_share(),
        1000.0 * strike_unpack_time / frames as f64,
        100.0 * unstruck.busy_share()
    );
    if let Some(p) = mix::profile() {
        println!("{p}");
    }
    Ok(())
}
