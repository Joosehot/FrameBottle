//! Frame Bottle: a movie as a grid of frames.
//!
//! 1. CHOP     the movie into its frames, every one (src/chop.rs; no outside program)
//! 2. GRID     every frame is a tile of one grid
//! 3. PACK     each tile into smaller bytes, HotMotor choosing how (src/motor.rs, src/pack.rs);
//!             the packed tiles lie in Strike's Nimbus, branded by Strike's lightning
//! 4. NODES    each frame is a node, a particle of the rice bowl's particle maker whose
//!             ports hold the packed tile and whose bond leads to the next frame (src/particle.rs)
//! 5. GLUE     Strike's Nimbus strikes every share into one .fbott file (src/bottle.rs)
//! 6. PLAYER   from node to node, the frames unpacked just before they play, in a
//!             rolling window in Nimbus (src/player.rs)
//! 7. SMOOTHER the frames between two nodes, guessed live from vectors (src/smooth.rs)
//!
//! The lossless packer (src/guess.rs) guesses each frame and keeps only the corrections.
//! Its guessers read the frame before, this frame so far, Golden Answer's fraction and
//! the Hammer: the stack of every pixel's line through all the frames so far.
//!
//! The report's numbers are worked out by Nuome's Golden Answer (src/golden.rs).

// Strike's own code, as it is in Strike: the lightning's home and hashes, and Nimbus.
#[allow(dead_code)]
#[path = "../../Strike/src/lightning.rs"]
mod lightning;
#[allow(dead_code)]
#[path = "../../Strike/src/fstrike.rs"]
mod fstrike;

mod block;
mod bottle;
mod chop;
mod dict;
mod golden;
mod guess;
mod motor;
mod pack;
mod particle;
mod player;
mod screen;
mod smooth;

use bottle::{Bottle, Index, Node};
use motor::Rules;
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

const USAGE: &str = "\
fbottle pack <movie> [-o FILE.fbott] [--for size|play] [--explain] [--tolerance N]
                                         N: a guess wrong by N or less is left uncorrected (0: exact)
             [--coarse N]                AV1's way: blocks, vectors and waves; N is the length of a wave's step
             [--line]                    exact, but each sample first says whether it stayed what it was
             [--dictionary]              common bit patterns of the corrections are kept by label
             [--four]                    the dictionary with only four ready-made patterns of block corrections
fbottle verify <bottle> <movie>          every node against the movie's own frames
fbottle play <bottle> [--smooth N] [--bench]   N times the movie's frame rate (2); --bench: no window, timed
fbottle test <movie> [--smooth N] [--for size|play] [--tolerance N]   pack, verify, play twice without a window, report
fbottle info <bottle>
fbottle forget <bottle>                  empty the bottle's window in Nimbus
fbottle smoother <bottle>                how near a guessed frame comes to a real one";

struct Packed {
    out: PathBuf,
    frames: usize,
    struck: usize,
    seconds: f64,
}

fn pack_key(rules_text: &str, profile: &str, geo: &pack::Geometry, species: &particle::Species, raw: &str, before: &str) -> String {
    bottle::sha(format!("fbottle pack 3 {}\n{rules_text}\n{profile}\n{} {}x{}\n{} {}\n{raw}\n{before}", guess::VERSION, geo.pix_fmt, geo.width, geo.height, species.definition_hash, species.k).as_bytes())
}

fn pack(movie_path: &Path, out: Option<PathBuf>, profile_name: &str, explain: bool, rules: &Rules) -> Result<Packed, String> {
    let began = Instant::now();
    let profile = *rules.profiles.get(profile_name).ok_or(format!("--for {profile_name}: rules.toml has no such profile"))?;
    let movie = chop::probe(movie_path)?;
    let geo = &movie.geo;
    if rules.coarse > 0 && !block::fits(geo) {
        return Err(format!("--coarse cuts 8-bit YUV 4:2:0 only; this movie's frames are {}", geo.pix_fmt));
    }
    let species = particle::choose()?;
    println!("CHOP   {}: {}x{} {} at {}/{} frames a second", movie_path.display(), geo.width, geo.height, geo.pix_fmt, movie.fps.0, movie.fps.1);
    println!(
        "NODES  each node is a copy of the particle {} ({}): {} ports hold the tile, the bond port {} -> port {} of the next (affinity {:.3}) is the link",
        species.id, species.kind, species.k, species.out, species.into, species.affinity
    );
    let mut reader = chop::frames(movie_path, geo)?;
    let batch = rayon::current_num_threads() * 2;
    let mut nodes: Vec<Node> = Vec::new();
    let mut share_ids: HashMap<String, u32> = HashMap::new();
    let mut share_hashes: Vec<String> = Vec::new();
    let (mut struck, mut explained) = (0usize, !explain);
    // The tile before the next one on the grid: the last frame of the batch before.
    let mut carry: Option<(Vec<u8>, String)> = None;
    // The Hammer's stack of every frame so far, and one name for all of them (for the brand).
    let mut hammer = guess::Hammer::new();
    let mut so_far = String::new();
    // The pattern dictionary: every frame's bits go through it, in order.
    let mut dictionary = dict::Dictionary::new();
    let mut books: Vec<block::Book> = Vec::new();
    loop {
        let mut frames = Vec::new();
        while frames.len() < batch {
            match reader.next()? {
                Some(f) => frames.push(f),
                None => break,
            }
        }
        if frames.is_empty() {
            break;
        }
        if !explained {
            explained = true;
            println!("PACK   HotMotor on the first tile (--for {profile_name}), best first:");
            let second = frames.len() > 1;
            if second {
                println!("       (the second tile is shown: the first has no tile before it)");
            }
            let shown = if second { pack::choose(geo, &frames[1], Some(&frames[0]), Some(&frames[0]), rules, &profile) } else { pack::choose(geo, &frames[0], None, None, rules, &profile) };
            for c in shown {
                let name: Vec<&str> = c.choice.iter().map(|(_, v)| v.as_str()).collect();
                println!("         {:14} {:.3} = {:.3} by the rules {:+.3} judged   {}", name.join("+"), c.score(), c.local, c.judged, c.notes.join("; "));
            }
        }
        // PACK: every tile on its own. A tile whose pack is branded is struck from Nimbus.
        if rules.tolerance > 0 || rules.coarse > 0 {
            // Not exact: with a tolerance, guesses wrong by no more than it are left as they
            // are; with --coarse, a frame is kept as blocks, vectors and waves. Either way each
            // frame is made from the frames as the player will have them, so one frame at a
            // time (its bands side by side).
            for frame in &frames {
                let before = carry.as_ref().map(|(f, _)| f.as_slice());
                let (kind, (payload, rebuilt)) = if rules.coarse > 0 {
                    (3, block::pack(geo, frame, before, hammer.picture(), rules.bands, rules.coarse, &rules.blocks, rules.dictionary.then_some(&mut books)))
                } else {
                    (2, guess::pack_within(geo, frame, before, hammer.guess(), rules.bands, rules.tolerance))
                };
                let mut packed = vec![kind, 0, 0];
                packed.extend(payload);
                let leans = pack::leans(&packed);
                if rules.dictionary {
                    packed = dictionary.close(packed);
                }
                let hashes: Vec<String> = particle::shares(&packed, species.k).into_iter().map(fstrike::put_bytes).collect::<Result<_, _>>()?;
                let ports = hashes
                    .into_iter()
                    .map(|h| {
                        *share_ids.entry(h.clone()).or_insert_with(|| {
                            share_hashes.push(h);
                            share_hashes.len() as u32 - 1
                        })
                    })
                    .collect();
                let raw = bottle::sha(&rebuilt);
                nodes.push(Node { frame: nodes.len() as u32, col: 0, row: 0, raw: raw.clone(), ports, next: None, leans });
                hammer.strike(&rebuilt);
                carry = Some((rebuilt, raw));
            }
            continue;
        }
        let raws: Vec<String> = frames.par_iter().map(|f| bottle::sha(f)).collect();
        // The stack as it stands before each frame of the batch, top frame first.
        let mut stacks: Vec<Option<Vec<u8>>> = Vec::new();
        let mut names: Vec<String> = Vec::new();
        for (frame, raw) in frames.iter().zip(&raws) {
            stacks.push(hammer.guess().map(|s| s.to_vec()));
            names.push(so_far.clone());
            hammer.strike(frame);
            so_far = bottle::sha(format!("{so_far}{raw}").as_bytes());
        }
        let done: Vec<(String, Vec<String>, bool, bool)> = frames
            .par_iter()
            .enumerate()
            .map(|(i, frame)| -> Result<_, String> {
                let raw = raws[i].clone();
                let before: Option<(&[u8], &str)> = if i > 0 { Some((&frames[i - 1], &raws[i - 1])) } else { carry.as_ref().map(|(f, r)| (f.as_slice(), r.as_str())) };
                // Only the rules that decide a pack are in its brand (not the smoother's or the player's).
                let pack_rules = motor::DEFAULT_RULES.split("[smoother]").next().unwrap();
                let by_line = guess::BY_LINE.load(std::sync::atomic::Ordering::Relaxed);
                let key = pack_key(pack_rules, &format!("{profile_name}{}", if by_line { " line" } else { "" }), geo, &species, &raw, &names[i]);
                if let Some(text) = bottle::branded("pack", &key) {
                    let mut lines = text.lines();
                    let leans = lines.next() == Some("tile");
                    let hashes: Vec<String> = lines.map(str::to_string).collect();
                    if hashes.len() == species.k && hashes.iter().all(|h| bottle::in_nimbus(h)) {
                        return Ok((raw, hashes, true, leans));
                    }
                }
                let best = pack::choose(geo, frame, before.map(|b| b.0), stacks[i].as_deref(), rules, &profile).remove(0);
                let packed = pack::pack(geo, frame, before.map(|b| b.0), stacks[i].as_deref(), &best.choice, rules);
                let leans = pack::leans(&packed);
                let hashes: Vec<String> = particle::shares(&packed, species.k).into_iter().map(fstrike::put_bytes).collect::<Result<_, _>>()?;
                bottle::brand("pack", &key, &format!("{}\n{}", if leans { "tile" } else { "own" }, hashes.join("\n")));
                Ok((raw, hashes, false, leans))
            })
            .collect::<Result<_, String>>()?;
        carry = frames.pop().zip(raws.last().cloned());
        for (raw, mut hashes, was_struck, leans) in done {
            struck += was_struck as usize;
            if rules.dictionary {
                let parts: Option<Vec<Vec<u8>>> = hashes.iter().map(|h| bottle::from_nimbus(h)).collect();
                let kept = dictionary.close(parts.ok_or("a share is not in Nimbus")?.concat());
                hashes = particle::shares(&kept, species.k).into_iter().map(fstrike::put_bytes).collect::<Result<_, _>>()?;
            }
            let ports = hashes
                .into_iter()
                .map(|h| {
                    *share_ids.entry(h.clone()).or_insert_with(|| {
                        share_hashes.push(h);
                        share_hashes.len() as u32 - 1
                    })
                })
                .collect();
            nodes.push(Node { frame: nodes.len() as u32, col: 0, row: 0, raw, ports, next: None, leans });
        }
    }
    let n = nodes.len();
    if n == 0 {
        return Err(format!("{}: no frames in it", movie_path.display()));
    }
    // GRID: the smallest square-ish grid that has a tile for every frame, filled row by row.
    let cols = (1..=n).find(|c| c * c >= n).unwrap();
    let rows = n.div_ceil(cols);
    for (i, node) in nodes.iter_mut().enumerate() {
        (node.col, node.row) = ((i % cols) as u32, (i / cols) as u32);
        node.next = (i + 1 < n).then_some(i as u32 + 1);
    }
    if rules.dictionary {
        let d = &dictionary;
        let pieces = (d.hits + d.misses).max(1);
        println!(
            "DICT   {} patterns at the end; {} of {} pieces were labels ({:.2} %), covering {:.2} % of the bits; {} of {} frames came out smaller through it; all frames through it would be {} bytes, as they are {}",
            d.patterns(),
            d.hits,
            pieces,
            100.0 * d.hits as f64 / pieces as f64,
            100.0 * d.bytes_by_label as f64 / d.bytes_in.max(1) as f64,
            d.frames_kept,
            d.frames,
            d.bytes_through,
            d.bytes_in
        );
    }
    if !books.is_empty() {
        let (blocks, hits, patterns) = books.iter().fold((0, 0, 0), |n, b| (n.0 + b.blocks, n.1 + b.hits, n.2 + b.patterns()));
        println!("BOOK   {patterns} patterns of block corrections at the end (in {} books); {hits} of {blocks} blocks with corrections were in a book ({:.2} %)", books.len(), 100.0 * hits as f64 / blocks.max(1) as f64);
    }
    if std::env::var_os("FBOTT_PATTERNS").is_some() {
        let mut all: std::collections::HashMap<Vec<(u8, i16)>, u64> = Default::default();
        for b in &books {
            for (w, n) in b.counts() {
                *all.entry(w).or_default() += n as u64;
            }
        }
        let mut v: Vec<_> = all.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let blocks: u64 = books.iter().map(|b| b.blocks).sum();
        let mut so_far = 0;
        for (w, n) in v.iter().take(12) {
            so_far += n;
            println!("pattern {:?}: {n} uses, {:.1} % of blocks with corrections (so far {:.1} %)", w, 100.0 * *n as f64 / blocks as f64, 100.0 * so_far as f64 / blocks as f64);
        }
    }
    println!("GRID   {n} frames as tiles of a {cols} x {rows} grid ({} x {} pixels)", cols * geo.width, rows * geo.height);
    println!("PACK   {} tiles packed, {} struck from Nimbus (already branded); {} lean on the tile before them", n - struck, struck, nodes.iter().filter(|n| n.leans).count());
    let out = out.unwrap_or_else(|| movie_path.with_extension("fbott"));
    let index = Index {
        format: "frame-bottle/2".into(),
        source: movie_path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
        pix_fmt: geo.pix_fmt.clone(),
        source_pix_fmt: movie.source_pix_fmt.clone(),
        width: geo.width,
        height: geo.height,
        fps: movie.fps,
        cols: cols as u32,
        rows: rows as u32,
        species,
        tolerance: rules.tolerance,
        coarse: rules.coarse,
        dictionary: if rules.dictionary { Some(fstrike::put_bytes(&dictionary.bytes())?) } else { None },
        nodes,
        shares: Vec::new(),
    };
    let size = bottle::glue(&out, index, &share_hashes)?;
    println!("GLUE   {} shares struck from Nimbus into {} ({size} bytes)", share_hashes.len(), out.display());
    Ok(Packed { out, frames: n, struck, seconds: began.elapsed().as_secs_f64() })
}

/// Every node unpacked from the bottle itself and compared, byte by byte,
/// with the movie's frames chopped again. Returns (frames alike, nodes, frames in the movie).
/// `far` gathers how far the unpacked samples are from the movie's: the worst, the sum of
/// squares, the samples; `mended` the same for the frames as the player shows them.
fn verify(b: &Bottle, movie_path: &Path, far: &mut (u8, f64, u64), mended: &mut (u8, f64, u64)) -> Result<(usize, usize, usize), String> {
    let distance = |u: &[u8], f: &[u8], far: &mut (u8, f64, u64)| {
        let (worst, squares) = u.par_iter().zip(f.par_iter()).map(|(a, b)| (a.abs_diff(*b), (a.abs_diff(*b) as u64).pow(2))).reduce(|| (0, 0), |x, y| (x.0.max(y.0), x.1 + y.1));
        *far = (far.0.max(worst), far.1 + squares as f64, far.2 + u.len() as u64);
    };
    let mut reader = chop::frames(movie_path, &b.geo)?;
    let batch = rayon::current_num_threads() * 2;
    let (mut same, mut seen) = (0usize, 0usize);
    let mut chain = bottle::Chain::mending(b);
    loop {
        let mut frames = Vec::new();
        while frames.len() < batch {
            match reader.next()? {
                Some(f) => frames.push(f),
                None => break,
            }
        }
        if frames.is_empty() {
            break;
        }
        let nodes = b.index.nodes.len();
        let to = (seen + frames.len()).min(nodes);
        if seen < to {
            let unpacked = b.frames(seen, to, &mut chain)?;
            same += unpacked.iter().zip(&frames).filter(|(u, f)| u == f).count();
            for (k, (u, f)) in unpacked.iter().zip(&frames).enumerate() {
                let before = far.1;
                distance(u, f, far);
                if std::env::var_os("FBOTT_EACH").is_some() {
                    // One line a frame, for looking at where the loss is.
                    let mean = (far.1 - before) / u.len() as f64;
                    println!("frame {} {:.2}", seen + k, if mean == 0.0 { 99.0 } else { 10.0 * (255.0f64 * 255.0 / mean).log10() });
                }
            }
            for (m, f) in chain.mended.drain(..).zip(&frames) {
                distance(&m, f, mended);
            }
        }
        seen += frames.len();
    }
    if let Some(name) = &b.index.dictionary {
        // The player's dictionary must have ended as the packer's did.
        if bottle::sha(&chain.dictionary.bytes()) != *name {
            return Err("the player's dictionary did not end as the packer's did".into());
        }
        println!("dictionary: the player's ended as the packer's did ({} patterns, {} of {} frames came through it)", chain.dictionary.patterns(), chain.dictionary.frames_kept, chain.dictionary.frames);
    }
    Ok((same, b.index.nodes.len(), seen))
}

/// How near the unpacked frames are to the movie's, in words.
fn nearness(far: &(u8, f64, u64), tolerance: u8) -> String {
    let mean = far.1 / far.2.max(1) as f64;
    let psnr = if mean == 0.0 { "no difference at all".to_string() } else { format!("PSNR {:.2} dB", 10.0 * (255.0f64 * 255.0 / mean).log10()) };
    let asked = if tolerance > 0 { format!("tolerance {tolerance}: ") } else { String::new() };
    format!("{asked}the sample furthest from the movie's is {} away; {psnr}", far.0)
}

fn exact(question: &str, places: usize) -> String {
    match golden::ask(question) {
        Ok(a) if a.contains('/') => format!("{} = {a} (Golden Answer, logical)", golden::decimal(&a, places)),
        Ok(a) => format!("{a} (Golden Answer, logical)"),
        Err(e) => format!("not worked out: {e}"),
    }
}

fn show_play(label: &str, s: &player::Stats) {
    println!(
        "{label}: {} frames ({} nodes + {} guessed) in {:.2} s of work, {:.1} ms a frame, enough for {:.0} frames a second",
        s.frames_out(),
        s.nodes,
        s.guessed_frames,
        s.work.as_secs_f64(),
        s.ms_per_frame(),
        1000.0 / s.ms_per_frame().max(1e-9)
    );
    println!(
        "         nodes: {} unpacked from particles, {} struck from Nimbus; guessed: {} made live, {} struck; window: {} frames at most, {} left Nimbus",
        s.unpacked, s.struck, s.guessed, s.guesses_struck, s.window_peak, s.left
    );
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
    println!(
        "         thread time: unpacking {:.0} ms, into Nimbus {:.0} ms, vectors {:.0} ms, guessed frames {:.0} ms",
        ms(s.spent[0]), ms(s.spent[1]), ms(s.spent[2]), ms(s.spent[3])
    );
    if s.blocks > 0 {
        println!("         vectors: {} of {} blocks moved", s.moving_blocks, s.blocks);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut rules = Rules::parse(motor::DEFAULT_RULES)?;
    let mut paths: Vec<PathBuf> = Vec::new();
    let (mut out, mut profile, mut explain, mut smooth, mut bench) = (None, "size".to_string(), false, 2usize, false);
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" => out = Some(PathBuf::from(it.next().ok_or("-o needs a file")?)),
            "--for" => profile = it.next().ok_or("--for needs size or play")?.clone(),
            "--explain" => explain = true,
            "--bench" => bench = true,
            "--dictionary" => rules.dictionary = true,
            "--four" => {
                rules.dictionary = true;
                block::FOUR_ONLY.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            "--line" => guess::BY_LINE.store(true, std::sync::atomic::Ordering::Relaxed),
            "--coarse" => rules.coarse = it.next().and_then(|n| n.parse().ok()).filter(|n| *n >= 1).ok_or("--coarse needs a whole number from 1 to 255")?,
            "--tolerance" => rules.tolerance = it.next().and_then(|n| n.parse().ok()).filter(|n| *n <= 40).ok_or("--tolerance needs a whole number from 0 to 40")?,
            "--smooth" => smooth = it.next().and_then(|n| n.parse().ok()).filter(|n| *n >= 1).ok_or("--smooth needs a whole number, 1 or more")?,
            _ => paths.push(PathBuf::from(a)),
        }
    }
    let path = |i: usize, what: &str| paths.get(i).ok_or(format!("{what} is missing\n{USAGE}"));
    match args.first().map(String::as_str) {
        Some("pack") => {
            let p = pack(path(0, "the movie")?, out, &profile, explain, &rules)?;
            println!("packed {} frames in {:.1} s", p.frames, p.seconds);
        }
        Some("verify") => {
            let b = Bottle::open(path(0, "the bottle")?)?;
            let (mut far, mut mended) = ((0, 0.0, 0), (0, 0.0, 0));
            let (same, nodes, frames) = verify(&b, path(1, "the movie")?, &mut far, &mut mended)?;
            println!("{same} of {frames} frames unpack identically ({nodes} nodes): {}", if same == frames && nodes == frames { "identical" } else { "NOT identical" });
            let close = nodes == frames && (b.index.coarse > 0 || (b.index.tolerance > 0 && far.0 <= b.index.tolerance));
            if b.index.coarse > 0 {
                println!("blocks, coarse {}: {}", b.index.coarse, nearness(&far, 0));
            }
            if b.index.tolerance > 0 {
                println!("as rebuilt  {}", nearness(&far, b.index.tolerance));
                println!("as shown    {} (mended by the player, no byte more)", nearness(&mended, b.index.tolerance));
            }
            if (same != frames || nodes != frames) && !close {
                return Err("the bottle does not hold the movie's frames".into());
            }
        }
        Some("play") => {
            let b = Bottle::open(path(0, "the bottle")?)?;
            let s = if bench {
                player::play(&b, &rules, smooth, &mut |_| true)?
            } else {
                // The frames are made ready on another thread, a window ahead; this one shows each at its time.
                let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(rules.ahead);
                let (bottle, rules) = (&b, &rules);
                std::thread::scope(|scope| {
                    let made = scope.spawn(move || player::play(bottle, rules, smooth, &mut |f| tx.send(f).is_ok()));
                    let mut screen = screen::Screen::open(&bottle.geo, (bottle.index.fps.0 * smooth as u64, bottle.index.fps.1))?;
                    for frame in rx.iter() {
                        if !screen.show(&bottle.geo, &frame) {
                            break;
                        }
                    }
                    drop(rx);
                    made.join().map_err(|_| "the player stopped".to_string())?
                })?
            };
            show_play("played", &s);
        }
        Some("forget") => {
            let b = Bottle::open(path(0, "the bottle")?)?;
            println!("{} frames left Nimbus", player::forget(&b));
        }
        Some("smoother") => {
            // Every second node is hidden and guessed from its two neighbours, then compared
            // with the real frame; the same for a plain mix of the neighbours, with no vectors.
            let b = Bottle::open(path(0, "the bottle")?)?;
            let luma = b.geo.planes[0].len();
            if b.geo.planes[0].bps != 1 {
                return Err("the smoother's check reads 8-bit frames only".into());
            }
            let psnr = |x: &[u8], y: &[u8]| {
                let e: f64 = x[..luma].iter().zip(&y[..luma]).map(|(p, q)| (*p as f64 - *q as f64).powi(2)).sum::<f64>() / luma as f64;
                if e == 0.0 { 99.0 } else { 10.0 * (255.0f64 * 255.0 / e).log10() }
            };
            let all = b.frames(0, b.index.nodes.len().min(50), &mut bottle::Chain::new())?;
            let triples: Vec<usize> = (0..all.len().saturating_sub(2)).step_by(2).take(24).collect();
            let scores: Vec<(f64, f64)> = triples
                .par_iter()
                .map(|i| -> Result<(f64, f64), String> {
                    let (a, real, c) = (&all[*i], &all[i + 1], &all[i + 2]);
                    let v = smooth::vectors(&b.geo, a, c, &rules);
                    let still = smooth::Vectors { block: v.block, bw: v.bw, bh: v.bh, d: vec![(0, 0); v.d.len()] };
                    Ok((psnr(&smooth::between(&b.geo, a, c, &v, 1, 2), real), psnr(&smooth::between(&b.geo, a, c, &still, 1, 2), real)))
                })
                .collect::<Result<_, String>>()?;
            let n = scores.len().max(1) as f64;
            let (with, without) = (scores.iter().map(|s| s.0).sum::<f64>() / n, scores.iter().map(|s| s.1).sum::<f64>() / n);
            let better = scores.iter().filter(|s| s.0 > s.1).count();
            println!("{} hidden frames guessed from their neighbours (two frames apart, so harder than in play):", scores.len());
            println!("  with vectors     {with:.2} dB (PSNR of the light, higher is nearer)");
            println!("  plain mix        {without:.2} dB");
            println!("  vectors nearer in {better} of {}", scores.len());
        }
        Some("info") => {
            let b = Bottle::open(path(0, "the bottle")?)?;
            let i = &b.index;
            println!("{}: {}x{} {} at {}/{} frames a second, {} nodes on a {} x {} grid", i.source, i.width, i.height, i.pix_fmt, i.fps.0, i.fps.1, i.nodes.len(), i.cols, i.rows);
            println!("particle {} ({}), {} ports, bond {} -> {}; {} shares", i.species.id, i.species.kind, i.species.k, i.species.out, i.species.into, i.shares.len());
            for (name, n) in b.choices()? {
                println!("  {n} tiles packed as {name}");
            }
        }
        Some("test") => {
            let movie = path(0, "the movie")?;
            let p = pack(movie, out, &profile, explain, &rules)?;
            let b = Bottle::open(&p.out)?;
            let (mut far, mut mended) = ((0, 0.0, 0), (0, 0.0, 0));
            let (same, nodes, frames) = verify(&b, movie, &mut far, &mut mended)?;
            let identical = same == frames && nodes == frames;
            let close = nodes == frames && (b.index.coarse > 0 || (b.index.tolerance > 0 && far.0 <= b.index.tolerance));
            player::forget(&b);
            let first = player::play(&b, &rules, smooth, &mut |_| true)?;
            let again = player::play(&b, &rules, smooth, &mut |_| true)?;
            let size = |p: &Path| std::fs::metadata(p).map(|m| m.len()).map_err(|e| format!("{}: {e}", p.display()));
            let (original, bottled) = (size(movie)?, size(&p.out)?);
            let raw = b.geo.frame_len() as u64 * nodes as u64;
            let i = &b.index;
            println!();
            println!("REPORT {}", movie.display());
            println!("  frames              {frames} in the movie, {nodes} nodes, grid {} x {} = {} tiles", i.cols, i.rows, exact(&format!("What is {} * {}?", i.cols, i.rows), 0));
            println!("  original size       {original} bytes");
            println!("  Frame Bottle size   {bottled} bytes");
            println!("  compression ratio   original / bottle = {}", exact(&format!("What is {original} / {bottled}?"), 4));
            println!("  bits per pixel      {} (raw RGB is 24, the movie's own YUV 4:2:0 is 12)", exact(&format!("What is {} / {}?", bottled * 8, nodes as u64 * (i.width * i.height) as u64), 4));
            let perfect = b.choices()?.iter().filter(|c| c.0 == "guess, perfect").map(|c| c.1).sum::<usize>();
            println!("  guessed perfectly   {perfect} of {nodes} frames (no correction at all)");
            let (right, tiles) = b.guess_tiles()?;
            let (spoke, planes) = b.golden_planes()?;
            if planes > 0 {
                println!("  Golden's guesser    spoke for {spoke} of {planes} planes (an exact fraction each, kept in the bottle)");
            }
            if tiles > 0 {
                println!("                      {right} of {tiles} tiles of 16 x 16 pixels = {}", exact(&format!("What is {right} / {tiles}?"), 4));
            }
            println!("  frames unpacked     {raw} bytes; unpacked / bottle = {}", exact(&format!("What is {raw} / {bottled}?"), 4));
            println!("  original FPS        {}", exact(&format!("What is {} / {}?", i.fps.0, i.fps.1), 3));
            println!("  playback FPS        {} ({} guessed between each two nodes)", exact(&format!("What is {} * {smooth} / {}?", i.fps.0, i.fps.1), 3), smooth - 1);
            println!("  frames identical    {} ({same} of {frames})", if identical { "yes" } else if close { "no: they were not asked to be" } else { "NO" });
            if b.index.coarse > 0 {
                println!("                      blocks, coarse {}: {}", b.index.coarse, nearness(&far, 0));
            }
            if b.index.tolerance > 0 {
                println!("                      as rebuilt  {}", nearness(&far, b.index.tolerance));
                println!("                      as shown    {} (mended by the player, no byte more)", nearness(&mended, b.index.tolerance));
            }
            println!("  packing             {:.1} s, {} tiles struck from Nimbus", p.seconds, p.struck);
            for (name, n) in b.choices()? {
                println!("                      {n} tiles packed as {name}");
            }
            show_play("  first play", &first);
            show_play("  play again", &again);
            if !identical && !close {
                return Err("the bottle does not hold the movie's frames".into());
            }
        }
        _ => println!("{USAGE}"),
    }
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("fbottle: {e}");
        std::process::exit(1);
    }
}
