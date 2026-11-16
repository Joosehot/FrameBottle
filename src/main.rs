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
//! Fractal mode (src/fractal.rs) keeps a frame as rules that make it out of itself and
//! out of the frames before it; HotMotor searches the rules and chooses how they are
//! written, the lightning brands each frame's rules and Nimbus keeps them.
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
mod fractal;
mod golden;
mod guess;
mod light;
mod mix;
mod far;
mod sheets;
mod tones;
mod trial;
mod zeros;
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
             [--mix]                     exact, 8-bit YUV 4:2:0: four guessers mixed, the misses written by three learners mixed;
                                         every band of every frame is a job, mapped first and struck on all cores
             [--alone]                   with --mix: every frame stands alone (nothing is read from the frame before)
             [--relight]                 with --mix: a light map (an offset and a gain for each tile of 32 x 32) laid on the frame before;
                                         --relight-offset: offsets only
             [--golden]                  with --mix: fixed weights for each stretch of film in place of learning (faster to unpack, larger);
                                         --chunk N: the frames of a stretch (60); each stretch starts with a frame that stands alone
             [--quarter]                 with --mix: vectors to a quarter of a pixel
             [--plates N]                with --mix: pressure plates, fixed weights chosen by where a sample stands (worked out for each stretch, as --golden's);
                                         1: the plates are the guess, 2: the plates are a fifth guesser; with --golden: the plates in place of its one set of weights
             [--two]                     with --mix: only the two learning guessers are weighed for the guess
             [--bands N]                 with --mix: the bands a frame is cut into (16)
             [--window N]                with --mix, and for play: the frames the strike lets in at once (32)
             [--line]                    exact, but each sample first says whether it stayed what it was
             [--light N]                 exact packing of dimmed light: N brightness levels on a curve (64, 96, 128)
             [--grid N]                  the same with a grid of N levels made for each frame
             [--grain]                   with --coarse or --tolerance: fake film grain on what is shown
             [--breakdown]               with --coarse: what every frame's bytes are spent on
             [--predict]                 with --coarse: the Predictive Motion Engine (vectors predicted from the frames so far, cuts found)
             [--between]                 with --coarse: every second frame is guessed from the frames on both sides
             [--fractal N]               fractal mode: frames as rules HotMotor searches (N: what a bit is worth in squares of error);
                                         the frames that stand alone are blocks (--coarse, 8 if not given)
             [--wave N]                  with --fractal: the player runs a wave out from the seams of the ranges; a step of N or more is left alone
             [--fractal-keys]            with --fractal: the frames that stand alone are rules too, made from flat grey
             [--dictionary]              common bit patterns of the corrections are kept by label
             [--four]                    the dictionary with only four ready-made patterns of block corrections
             [--dictionary-first]        with --coarse: the corrections as plain bits, the dictionary does the final packing
             [--patterns N]              the most patterns the bit dictionary holds (4096)
fbottle verify <bottle> <movie>          every node against the movie's own frames
fbottle play <bottle> [--smooth N] [--bench]   N times the movie's frame rate (2); --bench: no window, timed
fbottle test <movie> [--smooth N] [--for size|play] [--tolerance N]   pack, verify, play twice without a window, report
fbottle info <bottle>
fbottle forget <bottle>                  empty the bottle's window in Nimbus
fbottle png <bottle> <frame> <file>      one frame as the player rebuilds it, as a PNG picture
fbottle raw <bottle> <file>              the frames as the player shows them, one after the other, as plain bytes
fbottle smoother <bottle>                how near a guessed frame comes to a real one";

struct Packed {
    out: PathBuf,
    frames: usize,
    struck: usize,
    seconds: f64,
}

/// A packed frame becomes a node: through the dictionary if there is one, into the
/// particle's ports, into Nimbus.
fn keep(mut packed: Vec<u8>, rebuilt: &[u8], dictionary: Option<&mut dict::Dictionary>, ports: usize, share_ids: &mut HashMap<String, u32>, share_hashes: &mut Vec<String>, nodes: &mut Vec<Node>) -> Result<(), String> {
    let leans = pack::leans(&packed);
    if let Some(d) = dictionary {
        packed = d.close(packed);
    }
    let hashes: Vec<String> = particle::shares(&packed, ports).into_iter().map(fstrike::put_bytes).collect::<Result<_, _>>()?;
    let ports = hashes
        .into_iter()
        .map(|h| {
            *share_ids.entry(h.clone()).or_insert_with(|| {
                share_hashes.push(h);
                share_hashes.len() as u32 - 1
            })
        })
        .collect();
    nodes.push(Node { frame: nodes.len() as u32, col: 0, row: 0, raw: bottle::sha(rebuilt), ports, next: None, leans });
    Ok(())
}

/// A group of frames packed as blocks. The last frame of the group is guessed from the
/// last frame kept before it (`carry`); then the middle one from those two, and so on
/// inward, each between the two nearest frames already packed. Returns each frame's
/// bytes and the frame as the player will rebuild it, in the frames' own order.
fn pack_group(geo: &pack::Geometry, rules: &Rules, frames: &[Vec<u8>], carry: &mut Option<Vec<u8>>, hammer: &mut guess::Hammer, books: &mut Vec<block::Book>, motion: &mut block::Motion, sizes: &mut [usize; 3], spent: &mut Vec<[u64; 8]>) -> Vec<(Vec<u8>, Vec<u8>)> {
    let m = frames.len();
    let mut done: Vec<Option<(Vec<u8>, Vec<u8>)>> = vec![None; m];
    let mut spent_of = vec![[0u64; 8]; m];
    block::spent();
    let last = block::pack(geo, &frames[m - 1], carry.as_deref(), None, hammer.picture(), rules.bands, rules.coarse, &rules.blocks, rules.dictionary.then_some(&mut *books), (m as u8, 0), rules.predict.then_some(&mut *motion));
    spent_of[m - 1] = block::spent();
    hammer.strike(&last.1);
    sizes[if carry.is_none() { 0 } else { 1 }] += last.0.len();
    done[m - 1] = Some(last);
    // Spans between two packed frames (-1 is the frame kept before the group); the left half first.
    let mut spans: Vec<(isize, usize)> = vec![(-1, m - 1)];
    while let Some((lo, hi)) = spans.pop() {
        if hi as isize - lo < 2 {
            continue;
        }
        let k = ((lo + hi as isize) / 2) as usize;
        // Nothing is guessed from a frame with no frame left between it and its two: its steps are longest.
        let leaf = k as isize - lo < 2 && hi - k < 2;
        let longer = if leaf { rules.blocks.between } else { rules.blocks.between.sqrt() };
        let coarse = ((rules.coarse as f64 * longer).round() as u8).max(rules.coarse);
        let packed = {
            let before: &[u8] = if lo < 0 { carry.as_deref().expect("a group after the first frame") } else { &done[lo as usize].as_ref().unwrap().1 };
            let after = &done[hi].as_ref().unwrap().1;
            block::pack(geo, &frames[k], Some(before), Some(after), hammer.picture(), rules.bands, coarse, &rules.blocks, rules.dictionary.then_some(&mut *books), ((k as isize - lo) as u8, (hi - k) as u8), rules.predict.then_some(&mut *motion))
        };
        spent_of[k] = block::spent();
        sizes[2] += packed.0.len();
        done[k] = Some(packed);
        spans.push((k as isize, hi));
        spans.push((lo, k));
    }
    *carry = Some(done[m - 1].as_ref().unwrap().1.clone());
    spent.extend(spent_of);
    done.into_iter().map(Option::unwrap).collect()
}

fn pack_key(rules_text: &str, profile: &str, geo: &pack::Geometry, species: &particle::Species, raw: &str, before: &str) -> String {
    bottle::sha(format!("fbottle pack 3 {}\n{rules_text}\n{profile}\n{} {}x{}\n{} {}\n{raw}\n{before}", guess::VERSION, geo.pix_fmt, geo.width, geo.height, species.definition_hash, species.k).as_bytes())
}

fn pack(movie_path: &Path, out: Option<PathBuf>, profile_name: &str, explain: bool, rules: &Rules) -> Result<Packed, String> {
    let began = Instant::now();
    let profile = *rules.profiles.get(profile_name).ok_or(format!("--for {profile_name}: rules.toml has no such profile"))?;
    let movie = chop::probe(movie_path)?;
    let geo = &movie.geo;
    let mut light = match (rules.light, rules.grid) {
        (0, 0) => None,
        (n, 0) => Some(light::Light::curve(n)),
        (_, n) => Some(light::Light::grid(n)),
    };
    if light.is_some() && (rules.coarse > 0 || rules.tolerance > 0 || !block::fits(geo)) {
        return Err("--light and --grid go with the exact packing of 8-bit YUV 4:2:0 only".into());
    }
    // For fake grain: the squares of what the rebuilt brightness differs from the movie's by.
    let (mut grain_squares, mut grain_samples) = (0f64, 0f64);
    let light_len = geo.planes[0].len();
    let far_light = |a: &[u8], b: &[u8]| a[..light_len].iter().zip(&b[..light_len]).map(|(x, y)| (*x as f64 - *y as f64).powi(2)).sum::<f64>();
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
    let batch = if rules.mix && (rules.golden || rules.plates > 0) { rules.chunk } else if rules.mix { rules.window } else { rayon::current_num_threads() * 2 };
    let mut nodes: Vec<Node> = Vec::new();
    let mut share_ids: HashMap<String, u32> = HashMap::new();
    let mut share_hashes: Vec<String> = Vec::new();
    let (mut struck, mut explained) = (0usize, !explain);
    // The tile before the next one on the grid: the last frame of the batch before.
    let mut carry_exact: Option<(Vec<u8>, String)> = None;
    // For the packings that are not exact: the last frame as the player will have it, the
    // frame that waits to be guessed from both sides, and how many frames have come.
    let mut carry: Option<Vec<u8>> = None;
    let mut group: Vec<Vec<u8>> = Vec::new();
    // The Predictive Motion Engine's memory, the light of the frame before (for finding
    // cuts), and the cuts found.
    let mut motion = block::Motion::new(rules.history);
    let mut light_before: Option<Vec<u8>> = None;
    let mut cuts = 0usize;
    // For --breakdown: what each frame's bits were spent on, and each frame's bytes
    // (with whether it stores levels to put right, and whether it lies between two).
    let mut spent: Vec<[u64; 8]> = Vec::new();
    let mut payload_bytes: Vec<(usize, bool, bool)> = Vec::new();
    block::COUNTING.store(rules.breakdown, std::sync::atomic::Ordering::Relaxed);
    // Bytes of the first frame, of the frames guessed from the one before, of the frames between two.
    let mut sizes = [0usize; 3];
    // The Hammer's stack of every frame so far, and one name for all of them (for the brand).
    let mut hammer = guess::Hammer::new();
    let mut so_far = String::new();
    // The pattern dictionary: every frame's bits go through it, in order.
    let mut dictionary = dict::Dictionary::holding(rules.patterns);
    let mut books: Vec<block::Book> = Vec::new();
    // Fractal mode: the frame before the last as the player will have it, the names of
    // the last two, and for each frame of rules its bytes, what its bits went to (the
    // quadtree and kinds, places, turns, contrast and brightness) and its ranges by rule.
    let mut carry2: Option<Vec<u8>> = None;
    let (mut carry_name, mut carry2_name) = (String::from("-"), String::from("-"));
    let mut ruled: Vec<(usize, [u64; 8])> = Vec::new();
    let mut mix_bytes = 0usize;
    let mut mix_strike = mix::Struck::default();
    let mut done_fresh: Vec<(usize, Vec<u8>)> = Vec::new();
    // What the mix has learned so far, and the frames struck while it was not needed yet.
    let mut mix_memory = mix::Memory::new();
    let mut mix_struck: Option<Vec<Vec<u8>>> = Some(Vec::new());
    let fractal_how = fractal::How { profile, ..rules.fractal.clone() };
    // A group of frames is packed as blocks and its frames become nodes, in their own order.
    macro_rules! finish_group {
        () => {{
            let frames_of_group = std::mem::take(&mut group);
            let packed = pack_group(geo, rules, &frames_of_group, &mut carry, &mut hammer, &mut books, &mut motion, &mut sizes, &mut spent);
            for ((payload, rebuilt), frame) in packed.into_iter().zip(&frames_of_group) {
                payload_bytes.push((payload.len(), payload[0] & 64 != 0, payload[0] & 32 != 0));
                if rules.grain {
                    grain_squares += far_light(frame, &rebuilt);
                    grain_samples += light_len as f64;
                }
                keep([vec![3, 0, 0], payload].concat(), &rebuilt, rules.dictionary.then_some(&mut dictionary), species.k, &mut share_ids, &mut share_hashes, &mut nodes)?;
            }
        }};
    }
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
        // Dimmed light: each frame's brightness becomes level numbers, in order.
        let mut grids: Vec<Option<Vec<u8>>> = Vec::new();
        if let Some(l) = light.as_mut() {
            for f in frames.iter_mut() {
                grids.push(l.dim(geo, f));
            }
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
        if rules.mix {
            // The mix: exact, each frame from the real frame before it (or alone), so a
            // batch is packed side by side. The lightning brands each frame's pack.
            if !block::fits(geo) || rules.coarse > 0 || rules.tolerance > 0 || light.is_some() || rules.dictionary {
                return Err("--mix packs 8-bit YUV 4:2:0 exactly, with --alone or without, and nothing else".into());
            }
            if rules.golden || rules.plates > 0 {
                // A stretch of fixed weights stands on its own: nothing goes on from the stretch before.
                carry_exact = None;
                so_far.clear();
                mix_struck = Some(Vec::new());
                mix_memory = mix::Memory::new();
            }
            let raws: Vec<String> = frames.par_iter().map(|f| bottle::sha(f)).collect();
            let brand_of = |raw: &str, before: &str| bottle::sha(format!("fbottle {} {} {}x{} {} {} {:?} {raw} {before}", mix::VERSION, geo.pix_fmt, geo.width, geo.height, rules.bands, rules.relight, (rules.two, rules.quarter, rules.golden, rules.chunk, rules.plates)).as_bytes());
            // The map: each frame's brand (alone: its own; else it names every frame so far),
            // and what Nimbus already holds under it.
            let keys: Vec<String> = raws
                .iter()
                .map(|raw| {
                    if rules.alone {
                        return brand_of(raw, "alone");
                    }
                    let key = brand_of(raw, &so_far);
                    so_far = bottle::sha(format!("{so_far}{raw}").as_bytes());
                    key
                })
                .collect();
            let mut known: Vec<Option<Vec<u8>>> = keys.par_iter().map(|key| bottle::branded("mix", key).and_then(|hash| bottle::from_nimbus(hash.trim()))).collect();
            // The strike: the frames not known, all their bands at once.
            let fresh: Vec<usize> = if rules.alone {
                (0..frames.len()).filter(|i| known[*i].is_none()).collect()
            } else {
                // What a frame is packed with comes from every frame before it: from the first
                // frame not known on, all are packed.
                let from = if mix_struck.is_some() { known.iter().position(|k| k.is_none()).unwrap_or(frames.len()) } else { 0 };
                (from..frames.len()).collect()
            };
            if let (Some(first), false) = (fresh.first(), rules.alone) {
                if let Some(mut struck_so_far) = mix_struck.take() {
                    // The first frame to be packed after struck ones: the memory learns them first, as the player would.
                    struck_so_far.extend(known[..*first].iter().flatten().cloned());
                    let caught = mix::strike_unpack(geo, struck_so_far.len(), rules.window, None, &mut mix_memory, &mut |f| Ok(std::mem::take(&mut struck_so_far[f])), &|_, _| Ok(()), &mut |_, _| Ok(true))?;
                    mix_strike.add(&caught);
                }
            } else if let (Some(all), false) = (mix_struck.as_mut(), rules.alone) {
                all.extend(known.iter().flatten().cloned());
            }
            if !fresh.is_empty() {
                let how = mix::How { light: rules.relight, two: rules.two, quarter: rules.quarter, gold: rules.golden, tones: false, pyramid: false, plates: rules.plates };
                let these: Vec<&[u8]> = fresh.iter().map(|i| &frames[*i][..]).collect();
                let before: Option<&[u8]> = if fresh[0] > 0 { Some(&frames[fresh[0] - 1]) } else { carry_exact.as_ref().map(|(f, _)| f.as_slice()) };
                let (packed, how) = if rules.alone { mix::strike_pack(geo, &these, None, true, rules.bands, &mut mix::Memory::new(), how)? } else { mix::strike_pack(geo, &these, before, false, rules.bands, &mut mix_memory, how)? };
                mix_strike.add(&how);
                let hashes: Vec<String> = packed.par_iter().map(|p| fstrike::put_bytes(p)).collect::<Result<_, String>>()?;
                for ((i, payload), hash) in fresh.iter().zip(packed).zip(hashes) {
                    bottle::brand("mix", &keys[*i], &hash);
                    known[*i] = None;
                    done_fresh.push((*i, payload));
                }
            }
            let mut fresh_now = std::mem::take(&mut done_fresh).into_iter().peekable();
            let done: Vec<(Vec<u8>, bool)> = known
                .into_iter()
                .enumerate()
                .map(|(i, k)| match fresh_now.next_if(|f| f.0 == i) {
                    Some((_, payload)) => (payload, false),
                    None => (k.expect("a frame is either known or packed"), true),
                })
                .collect();
            for ((payload, was_struck), frame) in done.into_iter().zip(&frames) {
                struck += was_struck as usize;
                mix_bytes += payload.len();
                keep([vec![5, 0, 0], payload].concat(), frame, None, species.k, &mut share_ids, &mut share_hashes, &mut nodes)?;
            }
            carry_exact = frames.pop().zip(raws.last().cloned());
            continue;
        }
        if rules.fractal_on {
            // Fractal mode: each frame is rules over the last two frames as the player
            // will have them, so one at a time. A frame after a cut stands alone.
            for frame in std::mem::take(&mut frames) {
                let light: Vec<u8> = frame[..light_len].iter().step_by(7).copied().collect();
                let unlike = light_before.as_ref().map_or(0.0, |b| b.iter().zip(&light).map(|(x, y)| x.abs_diff(*y) as f64).sum::<f64>() / light.len() as f64);
                light_before = Some(light);
                if unlike > rules.cut {
                    cuts += 1;
                    (carry, carry2) = (None, None);
                }
                if carry.is_none() && !rules.fractal_keys {
                    // The frame that stands alone is kept as blocks.
                    group.push(frame);
                    finish_group!();
                    carry2 = None;
                    carry_name = bottle::sha(carry.as_ref().unwrap());
                    continue;
                }
                if carry.is_none() {
                    (carry_name, carry2_name) = ("-".into(), "-".into());
                }
                // Strike: the lightning brands the frame's rules by everything that decides
                // them; a frame searched once is struck from Nimbus.
                let key = bottle::sha(
                    format!("fbottle fractal 1
{fractal_how:?}
{:?}
{}
{} {}x{} {}
{}
{carry_name}
{}", rules.fractal_writing, rules.beam, geo.pix_fmt, geo.width, geo.height, rules.bands, bottle::sha(&frame), if carry2.is_some() { &carry2_name } else { "-" })
                        .as_bytes(),
                );
                let branded = bottle::branded("fractal", &key).and_then(|text| {
                    let mut lines = text.lines();
                    let payload = bottle::from_nimbus(lines.next()?)?;
                    let counts: Vec<u64> = lines.next()?.split(' ').filter_map(|n| n.parse().ok()).collect();
                    Some((payload, <[u64; 8]>::try_from(counts).ok()?))
                });
                let (payload, counts) = match branded {
                    Some(found) => {
                        struck += 1;
                        found
                    }
                    None => {
                        let payload = fractal::pack(geo, &frame, carry.as_deref(), carry2.as_deref(), rules.bands, &fractal_how, rules);
                        let counts: [u64; 8] = std::array::from_fn(|i| if i < 4 { &fractal::SPENT[i] } else { &fractal::RANGES[i - 4] }.swap(0, std::sync::atomic::Ordering::Relaxed));
                        let hash = fstrike::put_bytes(&payload)?;
                        bottle::brand("fractal", &key, &format!("{hash}
{}", counts.map(|c| c.to_string()).join(" ")));
                        (payload, counts)
                    }
                };
                let rebuilt = fractal::unpack(geo, &payload, carry.as_deref(), carry2.as_deref())?;
                ruled.push((payload.len(), counts));
                keep([vec![4, 0, 0], payload].concat(), &rebuilt, None, species.k, &mut share_ids, &mut share_hashes, &mut nodes)?;
                hammer.strike(&rebuilt);
                carry2 = carry.take();
                carry2_name = std::mem::replace(&mut carry_name, bottle::sha(&rebuilt));
                carry = Some(rebuilt);
            }
            continue;
        }
        if rules.coarse > 0 {
            // Blocks: frames are packed in groups (one frame a group without --between, and
            // the first frame always alone), each from the frames as the player will have them.
            for frame in std::mem::take(&mut frames) {
                if rules.predict {
                    // A cut: the picture is another one altogether. What is waiting is
                    // finished, and this frame stands alone, with a new history.
                    let light: Vec<u8> = frame[..light_len].iter().step_by(7).copied().collect();
                    let unlike = light_before.as_ref().map_or(0.0, |b| b.iter().zip(&light).map(|(x, y)| x.abs_diff(*y) as f64).sum::<f64>() / light.len() as f64);
                    light_before = Some(light);
                    if unlike > rules.cut {
                        if !group.is_empty() {
                            finish_group!();
                        }
                        cuts += 1;
                        carry = None;
                        hammer = guess::Hammer::new();
                    }
                }
                group.push(frame);
                let full = if carry.is_none() || !rules.between { 1 } else { rules.blocks.group.max(1) };
                if group.len() >= full {
                    finish_group!();
                }
            }
            continue;
        }
        if rules.tolerance > 0 {
            // A tolerance: guesses wrong by no more than it are left as they are. Each frame
            // is guessed from the frames as the player will have them, so one at a time.
            for frame in std::mem::take(&mut frames) {
                let (payload, rebuilt) = guess::pack_within(geo, &frame, carry.as_deref(), hammer.guess(), rules.bands, rules.tolerance);
                hammer.strike(&rebuilt);
                if rules.grain {
                    grain_squares += far_light(&frame, &rebuilt);
                    grain_samples += light_len as f64;
                }
                keep([vec![2, 0, 0], payload].concat(), &rebuilt, rules.dictionary.then_some(&mut dictionary), species.k, &mut share_ids, &mut share_hashes, &mut nodes)?;
                carry = Some(rebuilt);
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
                let before: Option<(&[u8], &str)> = if i > 0 { Some((&frames[i - 1], &raws[i - 1])) } else { carry_exact.as_ref().map(|(f, r)| (f.as_slice(), r.as_str())) };
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
        carry_exact = frames.pop().zip(raws.last().cloned());
        for (i, (raw, mut hashes, was_struck, leans)) in done.into_iter().enumerate() {
            struck += was_struck as usize;
            if rules.dictionary || light.is_some() {
                let parts: Option<Vec<Vec<u8>>> = hashes.iter().map(|h| bottle::from_nimbus(h)).collect();
                let mut kept = parts.ok_or("a share is not in Nimbus")?.concat();
                if light.is_some() {
                    // The frame's grid goes in front of it (or "the one before").
                    kept = light::wrap(grids[i].as_deref(), kept);
                }
                if rules.dictionary {
                    kept = dictionary.close(kept);
                }
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
    // The frames left over at the end are a last, shorter group.
    if !group.is_empty() {
        finish_group!();
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
    if rules.predict {
        println!("PREDICT  the motion engine remembers {} frames; cuts found: {cuts}; check of its predictions {:016x}", rules.history, motion.check);
    }
    if sizes[0] > 0 {
        println!("SIZES  the first frame {} bytes; the frames guessed from the one before {} bytes; the frames between two {} bytes", sizes[0], sizes[1], sizes[2]);
    }
    if let Some(l) = &light {
        println!("LIGHT  brightness kept as level numbers: {} of {} frames store a grid, the others use the one before", l.stored, l.frames);
    }
    if rules.grain {
        println!("GRAIN  the brightness the packing left out is {:.2} levels strong (root mean square); the player lays three quarters of it back as fake grain", light::grain_of(grain_squares, grain_samples) as f64 / 16.0);
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
    if rules.dictionary && std::env::var_os("FBOTT_PATTERNS").is_some() {
        for (bits, used) in dictionary.commonest(8) {
            println!("bit pattern {bits}: used {used} times");
        }
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
        format: "frame-bottle/3".into(),
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
        between: rules.coarse > 0 && rules.between,
        light: light.is_some(),
        predict: if rules.predict { rules.history } else { 0 },
        predict_check: motion.check,
        grain: if rules.grain { light::grain_of(grain_squares, grain_samples) } else { 0 },
        dictionary: if rules.dictionary { Some(fstrike::put_bytes(&dictionary.bytes())?) } else { None },
        patterns: rules.patterns,
        nodes,
        shares: Vec::new(),
    };
    let size = bottle::glue(&out, index, &share_hashes)?;
    println!("GLUE   {} shares struck from Nimbus into {} ({size} bytes)", share_hashes.len(), out.display());
    if rules.mix {
        // Where the bytes are, as the mix counts them (of the frames packed now, not struck).
        let s: Vec<f64> = mix::SPENT.iter().map(|a| a.load(std::sync::atomic::Ordering::Relaxed) as f64 / 8000.0).collect();
        let samples = (n * geo.frame_len()) as f64;
        println!("MIX    {mix_bytes} bytes of frames, {:.3} bits a sample{}", 8.0 * mix_bytes as f64 / samples, if rules.alone { "; every frame stands alone" } else { "" });
        let lit: Vec<u64> = mix::LIT.iter().map(|a| a.load(std::sync::atomic::Ordering::Relaxed)).collect();
        if lit[1] > 0 {
            println!("LIGHT  {} of {} tiles put right by the light map ({:.1} %)", lit[0], lit[1], 100.0 * lit[0] as f64 / lit[1] as f64);
        }
        if mix_strike.map > 0.0 {
            println!("GOLD   stretches of {} frames; every vector found first in {:.1} s, the fixed weights worked out in {:.1} s (here, by least squares: Golden Answer solves up to three letters today, these are 14)", rules.chunk, mix_strike.map, mix_strike.solve);
        }
        if mix_strike.jobs > 0 {
            println!("STRIKE {} jobs struck in windows of {} frames; {} workers, in a job {:.0} % of the strikes' {:.1} s", mix_strike.jobs, rules.window, mix_strike.workers, 100.0 * mix_strike.busy_share(), mix_strike.wall);
        }
        if struck < n {
            let all = s[0] + s[1] + s[2] + s[3];
            let share = |v: f64| format!("{:10.0} bytes {:5.1} %", v, 100.0 * v / all.max(1.0));
            println!("BREAKDOWN of the {} frames packed now:", n - struck);
            println!("  light                 {}", share(s[0]));
            println!("  the first colour      {}", share(s[1]));
            println!("  the second colour     {}", share(s[2]));
            println!("  vectors               {}", share(s[3]));
            println!("  by what is said: is there a miss {}; its sign {}; its size {}", share(s[4]), share(s[5]), share(s[6]));
        }
    }
    if rules.fractal_on {
        // Where the bytes are, as the fractal mode counts them.
        let bytes = |millibits: u64| millibits as f64 / 8000.0;
        let file = size as f64;
        let share = |v: f64| format!("{:9.0} bytes {:5.1} %", v, 100.0 * v / file);
        let total: usize = ruled.iter().map(|r| r.0).sum();
        let sum = |i: usize| ruled.iter().map(|r| r.1[i]).sum::<u64>();
        let ranges: u64 = (4..8).map(sum).sum();
        println!("FRACTAL  {} frames of rules ({} bytes, {:.0} a frame), {} frames that stand alone ({} bytes); cuts found: {cuts}", ruled.len(), total, total as f64 / ruled.len().max(1) as f64, n - ruled.len(), sizes[0]);
        println!("BREAKDOWN, the whole bottle ({size} bytes):");
        println!("  the frames that stand alone      {}", share(sizes[0] as f64));
        println!("  quadtree flags and kinds of rule {}", share(bytes(sum(0))));
        println!("  domain places and vectors        {}", share(bytes(sum(1))));
        println!("  turns (isometries)               {}", share(bytes(sum(2))));
        println!("  contrast and brightness (s, o)   {}", share(bytes(sum(3))));
        println!("  lengths, headers, coder rounding {}", share(total as f64 - bytes(sum(0) + sum(1) + sum(2) + sum(3))));
        println!("  the index and the bottle's ends  {}", share(file - total as f64 - sizes[0] as f64));
        println!("  ranges by rule: skip {:.1} %, moved {:.1} %, own (a domain of the frame itself) {:.1} %, flat {:.1} % of {ranges}", 100.0 * sum(4) as f64 / ranges.max(1) as f64, 100.0 * sum(5) as f64 / ranges.max(1) as f64, 100.0 * sum(6) as f64 / ranges.max(1) as f64, 100.0 * sum(7) as f64 / ranges.max(1) as f64);
    } else if rules.breakdown && !spent.is_empty() {
        // Where the bytes are: per frame, then in all.
        let bytes = |millibits: u64| millibits as f64 / 8000.0;
        let mut all = [0f64; 6];
        let (mut blocks, mut empty, mut tiles, mut skipped) = (0u64, 0u64, 0u64, 0u64);
        println!("BREAKDOWN, bytes per frame: frame, kind, in all | vectors | waves | tile kinds and flags | book labels | levels put right | lengths, header and the coder's rounding | blocks with no wave | tiles skipped");
        for (i, (s, (len, levels, between))) in spent.iter().zip(&payload_bytes).enumerate() {
            let (vectors, waves, kinds, labels) = (bytes(s[1]), bytes(s[2]), bytes(s[0]), bytes(s[3]));
            let levels = if *levels { 96.0 } else { 0.0 };
            let rest = *len as f64 - vectors - waves - kinds - labels - levels;
            println!(
                "  {i:4} {} {len:7} | {vectors:7.0} | {waves:7.0} | {kinds:6.0} | {labels:5.0} | {levels:3.0} | {rest:5.0} | {:5.1} % | {:5.1} %",
                if i == 0 { "first  " } else if *between { "between" } else { "before " },
                100.0 * s[5] as f64 / s[4].max(1) as f64,
                100.0 * s[7] as f64 / s[6].max(1) as f64
            );
            for (a, v) in all.iter_mut().zip([*len as f64, vectors, waves, kinds, labels, levels]) {
                *a += v;
            }
            (blocks, empty, tiles, skipped) = (blocks + s[4], empty + s[5], tiles + s[6], skipped + s[7]);
        }
        let file = size as f64;
        let share = |v: f64| format!("{:9.0} bytes {:5.1} %", v, 100.0 * v / file);
        println!("BREAKDOWN, the whole bottle ({size} bytes):");
        println!("  waves (the corrections)          {}", share(all[2]));
        println!("  vectors                          {}", share(all[1]));
        println!("  tile kinds and flags             {}", share(all[3]));
        println!("  labels of the book               {}", share(all[4]));
        println!("  levels put right                 {}", share(all[5]));
        println!("  light grid                       {}  (the blocks keep no light grid)", share(0.0));
        println!("  lengths, headers, coder rounding {}", share(all[0] - all[1] - all[2] - all[3] - all[4] - all[5]));
        println!("  the index and the bottle's ends  {}", share(file - all[0]));
        println!("  blocks with no wave left: {:.1} % of {blocks}; tiles skipped (the same picture from another frame, nothing put right): {:.1} % of {tiles}", 100.0 * empty as f64 / blocks.max(1) as f64, 100.0 * skipped as f64 / tiles.max(1) as f64);
    }
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
    if b.index.predict > 0 {
        // The player's engine must have predicted every vector the packer's did.
        if chain.motion.check != b.index.predict_check {
            return Err("the player's motion engine did not predict what the packer's did".into());
        }
        println!("motion engine: the player predicted exactly what the packer did (check {:016x})", chain.motion.check);
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
    if let Some(k) = &s.strike {
        println!("         the strike: {} jobs mapped and struck in a window of {} frames; {} workers, in a job {:.0} % of the time", k.jobs, s.window_peak, k.workers, 100.0 * k.busy_share());
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut rules = Rules::parse(motor::DEFAULT_RULES)?;
    let mut paths: Vec<PathBuf> = Vec::new();
    let (mut out, mut profile, mut explain, mut smooth, mut bench) = (None, "size".to_string(), false, 2usize, false);
    let mut plan = far::Plan { past: 1, near: 30, period: 1, sparse: false };
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" => out = Some(PathBuf::from(it.next().ok_or("-o needs a file")?)),
            "--for" => profile = it.next().ok_or("--for needs size or play")?.clone(),
            "--explain" => explain = true,
            "--bench" => bench = true,
            "--dictionary" => rules.dictionary = true,
            "--between" => rules.between = true,
            "--predict" => rules.predict = true,
            "--breakdown" => rules.breakdown = true,
            "--grain" => rules.grain = true,
            "--light" => rules.light = it.next().and_then(|n| n.parse().ok()).filter(|n| (2..=256).contains(n)).ok_or("--light needs a number of levels from 2 to 256")?,
            "--grid" => rules.grid = it.next().and_then(|n| n.parse().ok()).filter(|n| (2..=256).contains(n)).ok_or("--grid needs a number of levels from 2 to 256")?,
            "--dictionary-first" => {
                rules.dictionary = true;
                block::PLAIN_BITS.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            "--patterns" => rules.patterns = it.next().and_then(|n| n.parse().ok()).filter(|n| *n >= 1).ok_or("--patterns needs a whole number, 1 or more")?,
            "--four" => {
                rules.dictionary = true;
                block::FOUR_ONLY.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            "--line" => guess::BY_LINE.store(true, std::sync::atomic::Ordering::Relaxed),
            "--fractal" => {
                rules.fractal_on = true;
                rules.fractal.lambda = it.next().and_then(|n| n.parse().ok()).filter(|n| *n > 0.0).ok_or("--fractal needs a number above 0")?;
            }
            "--fractal-keys" => rules.fractal_keys = true,
            "--mix" => rules.mix = true,
            "--relight" => rules.relight = 2,
            "--relight-offset" => rules.relight = 1,
            "--two" => rules.two = true,
            "--quarter" => rules.quarter = true,
            "--golden" => rules.golden = true,
            "--plates" => rules.plates = it.next().and_then(|n| n.parse().ok()).filter(|n| (1..=2).contains(n)).ok_or("--plates needs 1 (the plates are the guess) or 2 (the plates are a fifth guesser)")?,
            "--past" => plan.past = it.next().and_then(|n| n.parse().ok()).filter(|n| *n >= 1).ok_or("--past needs a number of frames, 1 or more")?,
            "--near" => plan.near = it.next().and_then(|n| n.parse().ok()).filter(|n| *n >= 1).ok_or("--near needs a number of frames, 1 or more")?,
            "--ahead" => plan.period = it.next().and_then(|n| n.parse().ok()).filter(|n| *n >= 1).ok_or("--ahead needs a number of frames, 1 or more")?,
            "--sparse" => plan.sparse = true,
            "--chunk" => rules.chunk = it.next().and_then(|n| n.parse().ok()).filter(|n| *n >= 1).ok_or("--chunk needs a whole number, 1 or more")?,
            "--bands" => rules.bands = it.next().and_then(|n| n.parse().ok()).filter(|n| (1..=4096).contains(n)).ok_or("--bands needs a whole number from 1 to 4096")?,
            "--window" => rules.window = it.next().and_then(|n| n.parse().ok()).filter(|n| *n >= 1).ok_or("--window needs a whole number, 1 or more")?,
            "--alone" => rules.alone = true,
            "--wave" => {
                rules.fractal.wave_edge = it.next().and_then(|n| n.parse().ok()).filter(|n| *n >= 1).ok_or("--wave needs a whole number from 1 to 255")?;
                rules.fractal.wave = rules.wave_ticks;
            }
            "--coarse" => rules.coarse = it.next().and_then(|n| n.parse().ok()).filter(|n| *n >= 1).ok_or("--coarse needs a whole number from 1 to 255")?,
            "--tolerance" => rules.tolerance = it.next().and_then(|n| n.parse().ok()).filter(|n| *n <= 40).ok_or("--tolerance needs a whole number from 0 to 40")?,
            "--smooth" => smooth = it.next().and_then(|n| n.parse().ok()).filter(|n| *n >= 1).ok_or("--smooth needs a whole number, 1 or more")?,
            _ => paths.push(PathBuf::from(a)),
        }
    }
    if rules.fractal_on {
        if rules.between || rules.predict || rules.dictionary || rules.tolerance > 0 || rules.light > 0 || rules.grid > 0 {
            return Err("--fractal goes alone (with --coarse for the frames that stand alone, and --fractal-keys)".into());
        }
        if rules.coarse == 0 {
            rules.coarse = 8;
        }
    }
    let path = |i: usize, what: &str| paths.get(i).ok_or(format!("{what} is missing\n{USAGE}"));
    match args.first().map(String::as_str) {
        Some("pack") => {
            let p = pack(path(0, "the movie")?, out, &profile, explain, &rules)?;
            println!("packed {} frames in {:.1} s", p.frames, p.seconds);
        }
        Some("trial") => {
            // fbottle trial <raw yuv420p> <width> <height> <frames> [bands] [alone]
            let n = |i: usize| -> Result<usize, String> { path(i, "a number")?.to_string_lossy().parse().map_err(|_| "trial: <yuv> <width> <height> <frames> [bands]".to_string()) };
            trial::run(path(0, "the raw frames")?, n(1)?, n(2)?, n(3)?, n(4).unwrap_or(rules.bands), rules.alone)?;
        }
        Some("sheets") => {
            // fbottle sheets <raw yuv420p> <width> <height> <frames> [bands]
            let n = |i: usize| -> Result<usize, String> { path(i, "a number")?.to_string_lossy().parse().map_err(|_| "sheets: <yuv> <width> <height> <frames> [bands]".to_string()) };
            sheets::run(path(0, "the raw frames")?, n(1)?, n(2)?, n(3)?, n(4).unwrap_or(rules.bands))?;
        }
        Some("far") => {
            // fbottle far pack <raw yuv420p> <width> <height> <frames> -o <file> [--past N] [--near N] [--ahead N] [--sparse]
            // fbottle far unpack <file> <raw yuv420p to check against>
            match paths.first().and_then(|p| p.to_str()) {
                Some("pack") => {
                    let n = |i: usize| -> Result<usize, String> { path(i, "a number")?.to_string_lossy().parse().map_err(|_| "far pack: <yuv> <width> <height> <frames> -o <file>".to_string()) };
                    far::pack(path(1, "the raw frames")?, n(2)?, n(3)?, n(4)?, rules.bands, plan, &out.ok_or("far pack needs -o <file>")?)?;
                }
                Some("unpack") => far::unpack(path(1, "the packed file")?, path(2, "the raw frames")?)?,
                _ => return Err("far: pack or unpack".into()),
            }
        }
        Some("tones") => {
            // fbottle tones <raw yuv420p> <width> <height> <frames> [bands]
            let n = |i: usize| -> Result<usize, String> { path(i, "a number")?.to_string_lossy().parse().map_err(|_| "tones: <yuv> <width> <height> <frames> [bands]".to_string()) };
            tones::run(path(0, "the raw frames")?, n(1)?, n(2)?, n(3)?, n(4).unwrap_or(rules.bands))?;
        }
        Some("zeros") => {
            // fbottle zeros <raw yuv420p> <width> <height> <frames> [bands]
            let n = |i: usize| -> Result<usize, String> { path(i, "a number")?.to_string_lossy().parse().map_err(|_| "zeros: <yuv> <width> <height> <frames> [bands]".to_string()) };
            zeros::run(path(0, "the raw frames")?, n(1)?, n(2)?, n(3)?, n(4).unwrap_or(rules.bands))?;
        }
        Some("verify") => {
            let b = Bottle::open(path(0, "the bottle")?)?;
            let (mut far, mut mended) = ((0, 0.0, 0), (0, 0.0, 0));
            let (same, nodes, frames) = verify(&b, path(1, "the movie")?, &mut far, &mut mended)?;
            println!("{same} of {frames} frames unpack identically ({nodes} nodes): {}", if same == frames && nodes == frames { "identical" } else { "NOT identical" });
            let close = nodes == frames && (b.index.coarse > 0 || b.index.light || (b.index.tolerance > 0 && far.0 <= b.index.tolerance));
            if b.index.coarse > 0 {
                println!("blocks, coarse {}: {}", b.index.coarse, nearness(&far, 0));
            }
            if b.index.light || b.index.grain > 0 {
                println!("as shown    {}", nearness(&mended, 0));
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
            if let Some(p) = mix::profile() {
                println!("{p}");
            }
        }
        Some("png") => {
            use std::io::Write;
            let b = Bottle::open(path(0, "the bottle")?)?;
            let k: usize = path(1, "the frame's number")?.to_string_lossy().parse().map_err(|_| "the frame's number is not a number")?;
            let out = path(2, "the file to write")?;
            if k >= b.index.nodes.len() || !block::fits(&b.geo) {
                return Err(format!("the bottle has {} frames of {}; png shows 8-bit YUV 4:2:0", b.index.nodes.len(), b.index.pix_fmt));
            }
            let mut chain = bottle::Chain::mending(&b);
            let mut frame = Vec::new();
            for from in (0..=k).step_by(24) {
                let frames = b.frames(from, (from + 24).min(k + 1), &mut chain)?;
                let shown: Vec<Vec<u8>> = chain.mended.drain(..).collect();
                frame = if shown.is_empty() { frames } else { shown }.pop().unwrap();
            }
            // The picture in red, green and blue (BT.709, video range), row by row.
            let (w, h) = (b.geo.width, b.geo.height);
            let (cw, at) = (b.geo.planes[1].w, b.geo.offsets());
            let mut rows = Vec::with_capacity((3 * w + 1) * h);
            for y in 0..h {
                rows.push(0u8);
                for x in 0..w {
                    let l = 1.164 * (frame[at[0] + y * w + x] as f64 - 16.0);
                    let (u, v) = (frame[at[1] + (y / 2) * cw + x / 2] as f64 - 128.0, frame[at[2] + (y / 2) * cw + x / 2] as f64 - 128.0);
                    for c in [l + 1.793 * v, l - 0.213 * u - 0.533 * v, l + 2.112 * u] {
                        rows.push(c.round().clamp(0.0, 255.0) as u8);
                    }
                }
            }
            let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            z.write_all(&rows).map_err(|e| e.to_string())?;
            let z = z.finish().map_err(|e| e.to_string())?;
            let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
            let mut chunk = |name: &[u8; 4], data: &[u8]| {
                png.extend((data.len() as u32).to_be_bytes());
                let mut crc = flate2::Crc::new();
                crc.update(name);
                crc.update(data);
                png.extend(name);
                png.extend(data);
                png.extend(crc.sum().to_be_bytes());
            };
            chunk(b"IHDR", &[&(w as u32).to_be_bytes()[..], &(h as u32).to_be_bytes()[..], &[8, 2, 0, 0, 0]].concat());
            chunk(b"IDAT", &z);
            chunk(b"IEND", &[]);
            std::fs::write(out, &png).map_err(|e| format!("{}: {e}", out.display()))?;
            println!("frame {k} of {} written to {} ({} bytes)", path(0, "the bottle")?.display(), out.display(), png.len());
        }
        Some("raw") => {
            // The frames as the player shows them, for measuring from outside.
            use std::io::Write;
            let b = Bottle::open(path(0, "the bottle")?)?;
            let out = path(1, "the file to write")?;
            let mut file = std::io::BufWriter::new(std::fs::File::create(out).map_err(|e| format!("{}: {e}", out.display()))?);
            let mut chain = bottle::Chain::mending(&b);
            let n = b.index.nodes.len();
            for from in (0..n).step_by(24) {
                let frames = b.frames(from, (from + 24).min(n), &mut chain)?;
                let shown: Vec<Vec<u8>> = chain.mended.drain(..).collect();
                for f in if shown.is_empty() { &frames } else { &shown } {
                    file.write_all(f).map_err(|e| e.to_string())?;
                }
            }
            println!("{n} frames of {}x{} {} written to {}", b.index.width, b.index.height, b.index.pix_fmt, out.display());
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
            let all = b.frames(0, b.index.nodes.len().min(50), &mut bottle::Chain::mending(&b))?;
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
            let close = nodes == frames && (b.index.coarse > 0 || b.index.light || (b.index.tolerance > 0 && far.0 <= b.index.tolerance));
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
