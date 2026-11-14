//! NODES and GLUE: the .fbott file.
//!
//! ```text
//! "FBOTTLE1" | every share once, as it lies in Nimbus | the index, deflated | its length (u64) | "1ELTTOBF"
//! ```
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
    pub nodes: Vec<Node>,
    pub shares: Vec<Share>,
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
    let json = serde_json::to_vec(&index).map_err(|e| e.to_string())?;
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    z.write_all(&json).unwrap();
    let z = z.finish().unwrap();
    f.write_all(&z).map_err(err)?;
    f.write_all(&(z.len() as u64).to_le_bytes()).map_err(err)?;
    f.write_all(MAGIC_END).map_err(err)?;
    f.flush().map_err(err)?;
    Ok(at + z.len() as u64 + 16)
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
        let index: Index = serde_json::from_slice(&json).map_err(|e| format!("{}: {e}", path.display()))?;
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
        if sha(&bytes) != s.sha256 {
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
    /// checked by its hash). `before` is the frame of node `from - 1` if the caller has
    /// it; without it the run starts further back, at a tile that stands on its own.
    /// All nodes are unpacked at once; a leaning tile is then put on the frame before
    /// it, in order.
    pub fn frames(&self, from: usize, to: usize, before: Option<&[u8]>) -> Result<Vec<Vec<u8>>, String> {
        use rayon::prelude::*;
        let mut start = from;
        if before.is_none() {
            while self.index.nodes[start].leans {
                start = start.checked_sub(1).ok_or("the first tile leans on nothing")?;
            }
        }
        // A guessed tile waits for the frame before it; the others are unpacked at once.
        let mut waiting: Vec<Option<Vec<u8>>> = Vec::new();
        let mut out: Vec<Vec<u8>> = (start..to)
            .into_par_iter()
            .map(|i| {
                let packed = self.packed(i)?;
                if crate::pack::guessed(&packed) {
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
            let (done, rest) = out.split_at_mut(i);
            let prev = done.last().map(|f| f.as_slice()).or(before);
            if let Some(packed) = &waiting[i] {
                // The same guesses as when it was packed, and the corrections added.
                rest[0] = crate::guess::unpack(&self.geo, &packed[3..], prev)?;
            } else if self.index.nodes[start + i].leans {
                let prev = prev.filter(|p| p.len() == rest[0].len());
                crate::pack::lean_on(&mut rest[0], prev.ok_or("a leaning tile was unpacked without the tile before it")?);
            }
        }
        out.par_iter().enumerate().try_for_each(|(i, f)| {
            if sha(f) != self.index.nodes[start + i].raw {
                return Err(format!("node {} did not unpack to its frame", start + i));
            }
            Ok(())
        })?;
        Ok(out.split_off(from - start))
    }

    /// (16 x 16 tiles guessed perfectly, tiles) over all guessed frames.
    pub fn guess_tiles(&self) -> Result<(usize, usize), String> {
        let mut n = (0, 0);
        for i in 0..self.index.nodes.len() {
            let t = crate::pack::guess_tiles(&self.packed(i)?);
            n = (n.0 + t.0, n.1 + t.1);
        }
        Ok(n)
    }

    /// (planes for which Golden's guesser spoke, planes) over all guessed frames.
    pub fn golden_planes(&self) -> Result<(usize, usize), String> {
        let mut n = (0, 0);
        for i in 0..self.index.nodes.len() {
            let packed = self.packed(i)?;
            if crate::pack::guessed(&packed) {
                n = (n.0 + crate::pack::golden_planes(&self.geo, &packed), n.1 + self.geo.planes.len());
            }
        }
        Ok(n)
    }

    /// How many nodes chose each way of packing.
    pub fn choices(&self) -> Result<Vec<(String, usize)>, String> {
        let mut n: HashMap<String, usize> = HashMap::new();
        for i in 0..self.index.nodes.len() {
            let packed = self.packed(i)?;
            if packed.len() >= 3 {
                *n.entry(crate::pack::choice_name(&packed)).or_default() += 1;
            }
        }
        let mut v: Vec<_> = n.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        Ok(v)
    }
}
