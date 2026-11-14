//! NODES and GLUE: the .fbott file.
//!
//! ```text
//! "FBOTTLE1" | every share once, as it lies in Nimbus | the index, deflated | its length (u64) | "1ELTTOBF"
//! ```
//!
//! The index is kept tight: what is said once (the grid, the species, the settings) as
//! text, then a table of the shares (the first 8 bytes of each one's SHA-256 and its
//! length; where it lies follows from the lengths) and a table of the nodes (the
//! frame's SHA-256, whether it leans, and its ports). A node's place on the grid and
//! the node its bond leads to follow from its number, and are not written.
//!
//! The index holds the grid, the particle species and the nodes. A node is
//! one frame: its tile's place on the grid, the hash of the frame as it was
//! chopped, the share each of its particle's ports holds, and the node its
//! bond leads to (the next frame). Shares are Strike's: each is named by
//! the SHA-256 of its content, lies in Nimbus once, and is checked by that
//! name when it is glued and again whenever it is read.

use crate::fstrike as nimbus;
use crate::lightning::{hex, home};
use crate::pack::Geometry;
use crate::particle::Species;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const MAGIC: &[u8; 8] = b"FBOTTLE1";
const MAGIC_END: &[u8; 8] = b"1ELTTOBF";

pub fn sha(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Node {
    pub frame: u32,
    /// The tile's place on the grid.
    pub col: u32,
    pub row: u32,
    /// The SHA-256 of the frame as it was chopped.
    pub raw: String,
    /// The share each port holds (an index into `shares`), in port order.
    pub ports: Vec<u32>,
    /// The node this one's bond leads to: the next frame.
    pub next: Option<u32>,
    /// The tile leans on the tile before it on the grid, and unpacks after it.
    pub leans: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Share {
    pub offset: u64,
    pub len: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Index {
    pub format: String,
    pub source: String,
    pub pix_fmt: String,
    pub source_pix_fmt: String,
    pub width: usize,
    pub height: usize,
    pub fps: (u64, u64),
    /// The grid every frame is a tile of: `cols` x `rows` tiles of `width` x `height`.
    pub cols: u32,
    pub rows: u32,
    pub species: Species,
    /// How far a sample may be from the movie's (0: every frame is exact).
    #[serde(default)]
    pub tolerance: u8,
    /// Blocks, AV1's way: the length of a wave's step (0: not used; then frames are not exact).
    #[serde(default)]
    pub coarse: u8,
    /// The bottle's frames went through the pattern dictionary; this is the name (in
    /// Nimbus) of the dictionary as it stood after the last frame. The dictionary itself
    /// is not in the bottle: the player makes it again as it goes.
    #[serde(default)]
    pub dictionary: Option<String>,
    /// The most patterns the dictionary holds (0: the usual 4096).
    #[serde(default)]
    pub patterns: usize,
    /// Kept in the tables after the text, not in it.
    #[serde(skip)]
    pub nodes: Vec<Node>,
    #[serde(skip)]
    pub shares: Vec<Share>,
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap_or(0)).collect()
}

/// The index as bytes: the text, the table of shares, the table of nodes.
fn index_bytes(index: &Index) -> Result<Vec<u8>, String> {
    let json = serde_json::to_vec(index).map_err(|e| e.to_string())?;
    let mut out = (json.len() as u32).to_le_bytes().to_vec();
    out.extend(json);
    out.extend((index.shares.len() as u32).to_le_bytes());
    for s in &index.shares {
        out.extend(&unhex(&s.sha256)[..8]);
        out.extend((s.len as u32).to_le_bytes());
    }
    out.extend((index.nodes.len() as u32).to_le_bytes());
    for n in &index.nodes {
        out.extend(unhex(&n.raw));
        out.push(n.leans as u8);
        for p in &n.ports {
            out.extend(p.to_le_bytes());
        }
    }
    Ok(out)
}

fn index_from(bytes: &[u8]) -> Result<Index, String> {
    let cut = || "the index is cut".to_string();
    let mut at = 0usize;
    let mut take = |n: usize| -> Result<&[u8], String> {
        let part = bytes.get(at..at + n).ok_or_else(cut)?;
        at += n;
        Ok(part)
    };
    let word = |b: &[u8]| u32::from_le_bytes(b.try_into().unwrap()) as usize;
    let json_len = word(take(4)?);
    let mut index: Index = serde_json::from_slice(take(json_len)?).map_err(|e| e.to_string())?;
    let mut offset = MAGIC.len() as u64;
    for _ in 0..word(take(4)?) {
        let sha256 = hex(take(8)?);
        let len = word(take(4)?) as u64;
        index.shares.push(Share { offset, len, sha256 });
        offset += len;
    }
    let (count, k, cols) = (word(take(4)?), index.species.k, index.cols.max(1));
    for i in 0..count {
        let raw = hex(take(32)?);
        let leans = take(1)?[0] != 0;
        let ports = (0..k).map(|_| take(4).map(|b| word(b) as u32)).collect::<Result<Vec<u32>, String>>()?;
        index.nodes.push(Node { frame: i as u32, col: i as u32 % cols, row: i as u32 / cols, raw, ports, next: (i + 1 < count).then_some(i as u32 + 1), leans });
    }
    Ok(index)
}

/// Where Frame Bottle keeps its brands and its window, in Strike's home.
pub fn dir(name: &str) -> PathBuf {
    home().join("bottle").join(name)
}

/// A brand: `key` (the hash of everything that decides some work) -> what the work gave.
pub fn branded(kind: &str, key: &str) -> Option<String> {
    std::fs::read_to_string(dir(kind).join(key)).ok()
}

pub fn brand(kind: &str, key: &str, value: &str) {
    let d = dir(kind);
    let _ = std::fs::create_dir_all(&d);
    let _ = std::fs::write(d.join(key), value);
}

pub fn in_nimbus(hash: &str) -> bool {
    nimbus::blob(hash).exists()
}

pub fn from_nimbus(hash: &str) -> Option<Vec<u8>> {
    std::fs::read(nimbus::blob(hash)).ok()
}

/// Puts bytes whose SHA-256 is already known (it was just checked) into Nimbus.
pub fn to_nimbus(bytes: &[u8], hash: &str) -> Result<(), String> {
    if in_nimbus(hash) {
        return Ok(());
    }
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = nimbus::store_dir().join("tmp");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = dir.join(format!("fbott-{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    std::fs::write(&tmp, bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
    nimbus::keep(&tmp, hash)
}

/// Takes a frame out of Nimbus (the player's rolling window moved past it).
pub fn leave_nimbus(hash: &str) {
    let p = nimbus::blob(hash);
    nimbus::read_only(&p, false);
    let _ = std::fs::remove_file(p);
}

/// GLUE: every share is struck from Nimbus into the one file, checked by
/// its name on the way, and the index is written after them.
pub fn glue(out: &Path, mut index: Index, share_hashes: &[String]) -> Result<u64, String> {
    let err = |e: std::io::Error| format!("{}: {e}", out.display());
    let mut f = std::io::BufWriter::new(std::fs::File::create(out).map_err(err)?);
    f.write_all(MAGIC).map_err(err)?;
    let mut at = MAGIC.len() as u64;
    index.shares.clear();
    for hash in share_hashes {
        let bytes = from_nimbus(hash).ok_or(format!("the share {hash} is not in Nimbus"))?;
        if sha(&bytes) != *hash {
            return Err(format!("the share {hash} in Nimbus is not what its name says"));
        }
        f.write_all(&bytes).map_err(err)?;
        index.shares.push(Share { offset: at, len: bytes.len() as u64, sha256: hash.clone() });
        at += bytes.len() as u64;
    }
    let json = index_bytes(&index)?;
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    z.write_all(&json).unwrap();
    let z = z.finish().unwrap();
    f.write_all(&z).map_err(err)?;
    f.write_all(&(z.len() as u64).to_le_bytes()).map_err(err)?;
    f.write_all(MAGIC_END).map_err(err)?;
    f.flush().map_err(err)?;
    Ok(at + z.len() as u64 + 16)
}

/// What the frames so far leave for the next one: the frame before it, and the
/// Hammer's stack of all of them.
pub struct Chain {
    /// The node the chain stands before.
    pub at: usize,
    pub before: Option<Vec<u8>>,
    pub hammer: crate::guess::Hammer,
    /// The pattern dictionary as it stands after the frames so far (for a bottle that has one).
    pub dictionary: crate::dict::Dictionary,
    /// The books of block patterns, one for each band (for a bottle of blocks that has them).
    pub books: Vec<crate::block::Book>,
    /// For a bottle with a tolerance: each frame fed is also mended (what the
    /// tolerance lost put right as far as it can be) and waits here to be shown.
    mending: Option<(Geometry, u8)>,
    pub mended: Vec<Vec<u8>>,
}

impl Chain {
    pub fn new() -> Chain {
        Chain { at: 0, before: None, hammer: crate::guess::Hammer::new(), dictionary: crate::dict::Dictionary::new(), books: Vec::new(), mending: None, mended: Vec::new() }
    }
    /// A chain that also mends the frames of this bottle, if it has a tolerance.
    pub fn mending(b: &Bottle) -> Chain {
        let mut c = Chain::new();
        c.mending = (b.index.tolerance > 0).then(|| (b.geo.clone(), b.index.tolerance));
        c.dictionary = crate::dict::Dictionary::holding(b.index.patterns);
        c
    }
    /// The next frame is known: it becomes the frame before, and is laid on the stack.
    pub fn feed(&mut self, frame: &[u8]) {
        self.hammer.strike(frame);
        if let Some((geo, tol)) = &self.mending {
            self.mended.push(crate::guess::mend(geo, frame, *tol));
        }
        match &mut self.before {
            Some(b) if b.len() == frame.len() => b.copy_from_slice(frame),
            b => *b = Some(frame.to_vec()),
        }
        self.at += 1;
    }
}

pub struct Bottle {
    file: Mutex<std::fs::File>,
    pub index: Index,
    pub geo: Geometry,
    /// The hash of the index: the bottle's name for the player's window.
    pub id: String,
}

impl Bottle {
    pub fn open(path: &Path) -> Result<Bottle, String> {
        let err = |e: std::io::Error| format!("{}: {e}", path.display());
        let mut f = std::fs::File::open(path).map_err(err)?;
        let size = f.metadata().map_err(err)?.len();
        let mut head = [0u8; 8];
        f.read_exact(&mut head).map_err(err)?;
        let mut tail = [0u8; 16];
        if size < 24 {
            return Err(format!("{}: not a Frame Bottle", path.display()));
        }
        f.seek(SeekFrom::Start(size - 16)).map_err(err)?;
        f.read_exact(&mut tail).map_err(err)?;
        if &head != MAGIC || &tail[8..] != MAGIC_END {
            return Err(format!("{}: not a Frame Bottle", path.display()));
        }
        let len = u64::from_le_bytes(tail[..8].try_into().unwrap());
        if len > size - 24 {
            return Err(format!("{}: the index is cut", path.display()));
        }
        f.seek(SeekFrom::Start(size - 16 - len)).map_err(err)?;
        let mut z = vec![0u8; len as usize];
        f.read_exact(&mut z).map_err(err)?;
        let mut json = Vec::new();
        flate2::read::ZlibDecoder::new(&z[..]).read_to_end(&mut json).map_err(err)?;
        let index: Index = index_from(&json).map_err(|e| format!("{}: {e}", path.display()))?;
        let geo = Geometry::of(&index.pix_fmt, index.width, index.height).ok_or(format!("unknown pixel format {}", index.pix_fmt))?;
        // The chain of bonds must pass every node once, in frame order.
        for (i, n) in index.nodes.iter().enumerate() {
            let want = (i + 1 < index.nodes.len()).then_some(i as u32 + 1);
            if n.next != want || n.ports.len() != index.species.k || (i == 0 && n.leans) {
                return Err(format!("{}: node {i} is not bonded as a film is", path.display()));
            }
        }
        Ok(Bottle { file: Mutex::new(f), index, geo, id: sha(&json) })
    }

    fn share(&self, i: u32) -> Result<Vec<u8>, String> {
        let s = self.index.shares.get(i as usize).ok_or("a port holds a share the bottle does not have")?;
        let mut bytes = vec![0u8; s.len as usize];
        {
            let mut f = self.file.lock().unwrap();
            f.seek(SeekFrom::Start(s.offset)).map_err(|e| e.to_string())?;
            f.read_exact(&mut bytes).map_err(|e| e.to_string())?;
        }
        // The index holds the first 8 bytes of the share's name.
        if !sha(&bytes).starts_with(&s.sha256) {
            return Err(format!("the share {} in the bottle is not what its name says", s.sha256));
        }
        Ok(bytes)
    }

    /// The packed tile a node's particle holds: its ports' shares, in port order.
    pub fn packed(&self, node: usize) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        for p in &self.index.nodes[node].ports {
            out.extend(self.share(*p)?);
        }
        Ok(out)
    }

    /// UNPACK: the frames of the nodes `from..to`, exactly as they were chopped (each
    /// checked by its hash). `chain` is what the frames before `from` left: the frame
    /// before, and the Hammer's stack. A chain that stands elsewhere is made again
    /// from the top frame. Tiles that stand on their own are unpacked at once; a
    /// leaning or guessed tile then takes what the chain holds, in order.
    pub fn frames(&self, from: usize, to: usize, chain: &mut Chain) -> Result<Vec<Vec<u8>>, String> {
        use rayon::prelude::*;
        if chain.at != from {
            let mending = chain.mending.take();
            *chain = Chain::new();
            chain.mending = mending;
            chain.dictionary = crate::dict::Dictionary::holding(self.index.patterns);
            let mut at = 0;
            while at < from {
                let end = (at + 24).min(from);
                self.frames(at, end, chain)?;
                chain.mended.clear();
                at = end;
            }
        }
        let mut waiting: Vec<Option<Vec<u8>>> = Vec::new();
        let mut tiles: Vec<Vec<u8>> = (from..to).into_par_iter().map(|i| self.packed(i)).collect::<Result<_, String>>()?;
        if self.index.dictionary.is_some() {
            // In order: each frame's bits come back through the dictionary, which moves on.
            for t in tiles.iter_mut() {
                *t = chain.dictionary.open(std::mem::take(t))?;
            }
        }
        let mut out: Vec<Vec<u8>> = tiles
            .into_par_iter()
            .map(|packed| {
                if crate::pack::guessed(&packed) || crate::pack::blocks(&packed) {
                    return Ok((Vec::new(), Some(packed)));
                }
                let res = crate::pack::uncode(&self.geo, &packed)?;
                Ok((crate::pack::restore(&self.geo, &packed, &res), None))
            })
            .collect::<Result<Vec<(Vec<u8>, Option<Vec<u8>>)>, String>>()?
            .into_iter()
            .map(|(frame, guess)| {
                waiting.push(guess);
                frame
            })
            .collect();
        for i in 0..out.len() {
            if let Some(packed) = &waiting[i] {
                // The same guesses as when it was packed, and the corrections added.
                out[i] = if crate::pack::blocks(packed) {
                    crate::block::unpack(&self.geo, &packed[3..], chain.before.as_deref(), chain.hammer.picture(), &mut chain.books)?
                } else {
                    crate::guess::unpack(&self.geo, &packed[3..], chain.before.as_deref(), chain.hammer.guess())?
                };
            } else if self.index.nodes[from + i].leans {
                let prev = chain.before.as_deref().filter(|p| p.len() == out[i].len());
                crate::pack::lean_on(&mut out[i], prev.ok_or("a leaning tile was unpacked without the tile before it")?);
            }
            chain.feed(&out[i]);
        }
        out.par_iter().enumerate().try_for_each(|(i, f)| {
            if sha(f) != self.index.nodes[from + i].raw {
                return Err(format!("node {} did not unpack to its frame", from + i));
            }
            Ok(())
        })?;
        Ok(out)
    }

    /// A frame struck from the window is not unpacked, but its bits still go through
    /// the dictionary, so that it stands where the packer's stood.
    pub fn pass(&self, node: usize, chain: &mut Chain) -> Result<(), String> {
        if self.index.dictionary.is_some() {
            chain.dictionary.open(self.packed(node)?)?;
        }
        Ok(())
    }

    /// Every node's packed tile, in order, out of the dictionary if the bottle has one.
    fn tiles(&self) -> Result<Vec<Vec<u8>>, String> {
        let mut d = crate::dict::Dictionary::holding(self.index.patterns);
        (0..self.index.nodes.len()).map(|i| if self.index.dictionary.is_some() { d.open(self.packed(i)?) } else { self.packed(i) }).collect()
    }

    /// (16 x 16 tiles guessed perfectly, tiles) over all guessed frames.
    pub fn guess_tiles(&self) -> Result<(usize, usize), String> {
        let mut n = (0, 0);
        for packed in self.tiles()? {
            let t = crate::pack::guess_tiles(&packed);
            n = (n.0 + t.0, n.1 + t.1);
        }
        Ok(n)
    }

    /// (planes for which Golden's guesser spoke, planes) over all guessed frames.
    pub fn golden_planes(&self) -> Result<(usize, usize), String> {
        let mut n = (0, 0);
        for packed in self.tiles()? {
            if crate::pack::guessed(&packed) {
                n = (n.0 + crate::pack::golden_planes(&self.geo, &packed), n.1 + self.geo.planes.len());
            }
        }
        Ok(n)
    }

    /// How many nodes chose each way of packing.
    pub fn choices(&self) -> Result<Vec<(String, usize)>, String> {
        let mut n: HashMap<String, usize> = HashMap::new();
        for packed in self.tiles()? {
            if packed.len() >= 3 {
                *n.entry(crate::pack::choice_name(&packed)).or_default() += 1;
            }
        }
        let mut v: Vec<_> = n.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        Ok(v)
    }
}
