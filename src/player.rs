//! PLAYER: from node to node along the bonds, frame to frame.
//!
//! The nodes are loaded into memory when the bottle is opened. The frames
//! live in Nimbus, in a rolling window around the playhead: the nodes a
//! little ahead are unpacked from their particles just before they are
//! played, and the frames between each two of them are guessed then too
//! (live, from vectors); the frames the playhead has left behind leave
//! Nimbus. A frame that is already in the window is struck from it: nothing
//! is unpacked or guessed twice.

use crate::bottle::{self, Bottle};
use crate::fstrike as nimbus;
use crate::motor::Rules;
use crate::smooth;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// What the window holds for one node: its frame, and the guessed frames that lead to it.
#[derive(Default, Clone, Serialize, Deserialize)]
struct Held {
    frame: String,
    /// (brand key, the guessed frame's hash)
    guesses: Vec<(String, String)>,
}

#[derive(Default, Debug)]
pub struct Stats {
    pub nodes: usize,
    pub guessed_frames: usize,
    /// Nodes unpacked from their particles / struck from the window.
    pub unpacked: usize,
    pub struck: usize,
    /// Guessed frames made / struck from the window.
    pub guessed: usize,
    pub guesses_struck: usize,
    /// Frames that left Nimbus as the window rolled.
    pub left: usize,
    /// The most frames the window held at once.
    pub window_peak: usize,
    /// Blocks with a vector that is not zero, of all blocks searched.
    pub moving_blocks: usize,
    pub blocks: usize,
    /// Time spent making frames ready (not waiting for the screen).
    pub work: Duration,
    /// Where the work went, summed over the threads: unpacking nodes, putting frames in
    /// Nimbus, finding vectors, making the guessed frames.
    pub spent: [Duration; 4],
    pub wall: Duration,
}

impl Stats {
    pub fn frames_out(&self) -> usize {
        self.nodes + self.guessed_frames
    }
    /// The time one step along the film takes, in milliseconds.
    pub fn ms_per_frame(&self) -> f64 {
        self.work.as_secs_f64() * 1000.0 / self.frames_out().max(1) as f64
    }
}

fn window_file(b: &Bottle) -> std::path::PathBuf {
    bottle::dir("window").join(format!("{}.json", b.id))
}

fn guess_key(b: &Bottle, rules: &Rules, a: &str, bb: &str, k: usize, n: usize) -> String {
    bottle::sha(format!("fbottle guess 1\n{} {} {} {}\n{} {}x{}\n{a}\n{bb}\n{k}/{n}", rules.block, rules.range, rules.zero_bias, rules.poor_match, b.index.pix_fmt, b.index.width, b.index.height).as_bytes())
}

fn leave(h: &Held, stats: &mut Stats) {
    bottle::leave_nimbus(&h.frame);
    stats.left += 1;
    for (key, hash) in &h.guesses {
        bottle::leave_nimbus(hash);
        let _ = std::fs::remove_file(bottle::dir("guess").join(key));
        stats.left += 1;
    }
}

/// Empties the bottle's window in Nimbus.
pub fn forget(b: &Bottle) -> usize {
    let mut stats = Stats::default();
    if let Ok(text) = std::fs::read_to_string(window_file(b)) {
        if let Ok(held) = serde_json::from_str::<BTreeMap<u32, Held>>(&text) {
            for h in held.values() {
                leave(h, &mut stats);
            }
        }
    }
    let _ = std::fs::remove_file(window_file(b));
    stats.left
}

/// Plays the bottle at `smooth` times its own frame rate: `smooth - 1`
/// guessed frames between each two nodes. With `show` the frames go to a
/// window on the screen (ffplay); without, they are only made, and timed.
pub fn play(b: &Bottle, rules: &Rules, smooth: usize, show: bool) -> Result<Stats, String> {
    let n = b.index.nodes.len();
    let len = b.geo.frame_len();
    let mut stats = Stats::default();
    let mut held: BTreeMap<u32, Held> = std::fs::read_to_string(window_file(b)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let mut screen = None;
    if show {
        let rate = format!("{}/{}", b.index.fps.0 * smooth as u64, b.index.fps.1);
        let size = format!("{}x{}", b.index.width, b.index.height);
        let mut child = Command::new("ffplay")
            .args(["-v", "error", "-autoexit", "-window_title", "Frame Bottle", "-f", "rawvideo", "-pixel_format", &b.index.pix_fmt, "-video_size", &size, "-framerate", &rate, "-i", "-"])
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|e| format!("ffplay: {e}"))?;
        let pipe = child.stdin.take().unwrap();
        screen = Some((child, pipe));
    }
    let began = Instant::now();
    let spent: [std::sync::atomic::AtomicU64; 4] = Default::default();
    let add = |i: usize, t: Instant| spent[i].fetch_add(t.elapsed().as_nanos() as u64, std::sync::atomic::Ordering::Relaxed);
    let mut prev: Option<Vec<u8>> = None;
    let mut closed = false;
    for start in (0..n).step_by(rules.ahead) {
        let t = Instant::now();
        let end = (start + rules.ahead).min(n);
        // The window rolls: what is behind it (or far ahead, from an earlier play) leaves Nimbus.
        let lo = start.saturating_sub(rules.behind) as u32;
        let out: Vec<u32> = held.keys().copied().filter(|k| *k < lo || *k >= end as u32).collect();
        for k in out {
            leave(&held.remove(&k).unwrap(), &mut stats);
        }
        // The nodes ahead: struck from the window, or unpacked from their particles into it.
        let frames: Vec<(Vec<u8>, bool)> = (start..end)
            .into_par_iter()
            .map(|i| {
                let raw = &b.index.nodes[i].raw;
                if let Some(f) = bottle::from_nimbus(raw).filter(|f| f.len() == len) {
                    return Ok((f, true));
                }
                let t = Instant::now();
                let f = b.frame(i)?;
                add(0, t);
                let t = Instant::now();
                bottle::to_nimbus(&f, raw)?;
                add(1, t);
                Ok((f, false))
            })
            .collect::<Result<_, String>>()?;
        for (i, (_, struck)) in frames.iter().enumerate() {
            if *struck {
                stats.struck += 1;
            } else {
                stats.unpacked += 1;
            }
            held.entry((start + i) as u32).or_default().frame = b.index.nodes[start + i].raw.clone();
        }
        // The frames between each node and the one before it, guessed now.
        type Guessed = (Vec<(String, String)>, Vec<Vec<u8>>, bool, usize, usize);
        let guesses: Vec<Guessed> = (start..end)
            .into_par_iter()
            .map(|i| -> Result<Guessed, String> {
                if i == 0 || smooth < 2 {
                    return Ok((Vec::new(), Vec::new(), true, 0, 0));
                }
                let a: &[u8] = if i == start { prev.as_deref().unwrap() } else { &frames[i - 1 - start].0 };
                let bb: &[u8] = &frames[i - start].0;
                let keys: Vec<String> = (1..smooth).map(|k| guess_key(b, rules, &b.index.nodes[i - 1].raw, &b.index.nodes[i].raw, k, smooth)).collect();
                let known: Option<Vec<(String, Vec<u8>)>> = keys
                    .iter()
                    .map(|key| {
                        let hash = bottle::branded("guess", key)?;
                        Some((hash.clone(), bottle::from_nimbus(&hash).filter(|f| f.len() == len)?))
                    })
                    .collect();
                if let Some(known) = known {
                    let (hashes, made): (Vec<String>, Vec<Vec<u8>>) = known.into_iter().unzip();
                    return Ok((keys.into_iter().zip(hashes).collect(), made, true, 0, 0));
                }
                let t = Instant::now();
                let v = smooth::vectors(&b.geo, a, bb, rules);
                add(2, t);
                let mut made = Vec::new();
                let mut names = Vec::new();
                for (k, key) in (1..smooth).zip(keys) {
                    let t = Instant::now();
                    let f = smooth::between(&b.geo, a, bb, &v, k, smooth);
                    add(3, t);
                    let t = Instant::now();
                    let hash = nimbus::put_bytes(&f)?;
                    add(1, t);
                    bottle::brand("guess", &key, &hash);
                    names.push((key, hash));
                    made.push(f);
                }
                Ok((names, made, false, v.moving(), v.d.len()))
            })
            .collect::<Result<_, String>>()?;
        for (i, g) in guesses.iter().enumerate() {
            if g.2 {
                stats.guesses_struck += g.1.len();
            } else {
                stats.guessed += g.1.len();
            }
            stats.moving_blocks += g.3;
            stats.blocks += g.4;
            held.entry((start + i) as u32).or_default().guesses = g.0.clone();
        }
        let in_window: usize = held.values().map(|h| 1 + h.guesses.len()).sum();
        stats.window_peak = stats.window_peak.max(in_window);
        let _ = std::fs::create_dir_all(bottle::dir("window"));
        let _ = std::fs::write(window_file(b), serde_json::to_vec(&held).unwrap());
        stats.work += t.elapsed();
        // Node to node: the guessed frames that lead to a node, then the node.
        let mut last = Vec::new();
        for ((frame, _), g) in frames.into_iter().zip(guesses) {
            stats.guessed_frames += g.1.len();
            stats.nodes += 1;
            if let Some((_, pipe)) = screen.as_mut() {
                if !closed {
                    // The window on the screen was closed: stop there.
                    closed = g.1.iter().chain([&frame]).any(|f| pipe.write_all(f).is_err());
                }
            }
            last = frame;
        }
        prev = Some(last);
        if closed {
            break;
        }
    }
    if let Some((mut child, pipe)) = screen {
        drop(pipe);
        let _ = child.wait();
    }
    stats.wall = began.elapsed();
    for (i, s) in spent.iter().enumerate() {
        stats.spent[i] = Duration::from_nanos(s.load(std::sync::atomic::Ordering::Relaxed));
    }
    Ok(stats)
}
