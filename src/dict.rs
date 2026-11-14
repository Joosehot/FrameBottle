//! The pattern dictionary: common bit patterns of the corrections, kept by
//! label.
//!
//! After a frame's corrections are made and written, their bits are read
//! again, a byte (8 bits) at a time. The dictionary holds patterns of 2 to
//! 8 bytes (16 to 64 bits). Where the bits ahead are a pattern it holds,
//! only the pattern's label is kept; elsewhere the byte itself.
//!
//! - **It updates itself.** Each time two pieces that were kept one after
//!   the other (a pattern or a byte, then a pattern or a byte) have been
//!   seen together twice, the two joined become a new pattern.
//! - **It is never stored.** The player gets the same pieces back in the
//!   same order and does the same joining, so its dictionary is the same at
//!   every step. Only data both have already seen is used.
//! - **The most common patterns have the shortest labels.** A label is the
//!   pattern's rank by how often it has been used, written so that a low
//!   rank takes few bits (and the coder learns which ranks come up).
//! - **It is capped.** At most `CAP` patterns; when a new one comes and it
//!   is full, the one not used for the longest time is dropped.
//!
//! A frame is kept through the dictionary only when that is smaller than the
//! frame itself; either way both sides run the frame's bytes through it, so
//! it stays the same on both. What a dictionary frame unpacks to is checked
//! against the first 8 bytes of the SHA-256 of the bytes it was made from.
//! At the end of a film the dictionary itself lies in Nimbus, named by its
//! hash, and the bottle's index holds that name: a player whose dictionary
//! ends with another hash did not follow the packer.

use crate::guess::{Decoder, Encoder};
use sha2::Digest;
use std::collections::{BTreeSet, HashMap};

const HALF: u16 = 1 << 11;
/// The most patterns the dictionary holds.
const CAP: usize = 4096;
/// The most pairs waiting to be seen a second time.
const WAITING: usize = 1 << 16;
/// A frame kept through the dictionary starts with this.
pub const KIND: u8 = 9;

/// A pattern: up to 8 bytes in one number, and how many.
type Pattern = (u64, u8);

fn pattern(bytes: &[u8]) -> Pattern {
    (bytes.iter().fold(0u64, |k, b| (k << 8) | *b as u64), bytes.len() as u8)
}

fn join(a: Pattern, b: Pattern) -> Option<Pattern> {
    (a.1 + b.1 <= 8).then(|| ((a.0 << (8 * b.1 as u32)) | b.0, a.1 + b.1))
}

struct Entry {
    pattern: Pattern,
    used: u32,
    /// When it was last used (a count of pieces).
    at: u64,
    rank: usize,
}

/// One way through the pieces for the packer and the player.
enum Coder<'a> {
    Write(Encoder),
    Read(Decoder<'a>),
}

impl Coder<'_> {
    #[inline(always)]
    fn bit(&mut self, p: &mut u16, v: bool) -> bool {
        match self {
            Coder::Write(e) => {
                e.bit(p, v);
                v
            }
            Coder::Read(d) => d.bit(p),
        }
    }
}

pub struct Dictionary {
    entries: Vec<Entry>,
    /// Pattern -> its entry.
    find: HashMap<Pattern, usize>,
    /// The entries by rank: the most used first.
    ranks: Vec<usize>,
    /// (last used, entry): the first is the one not used for the longest.
    oldest: BTreeSet<(u64, usize)>,
    /// Pairs seen together once.
    waiting: HashMap<Pattern, ()>,
    /// The most patterns this dictionary holds.
    cap: usize,
    pieces: u64,
    // What the coder has learned.
    hit: [u16; 2],
    rank_bits: [u16; 32],
    byte: [u16; 256],
    // For the report.
    pub hits: u64,
    pub misses: u64,
    pub bytes_by_label: u64,
    pub bytes_in: u64,
    /// What the frames would take if every one were kept through the dictionary.
    pub bytes_through: u64,
    pub frames_kept: u64,
    pub frames: u64,
}

impl Dictionary {
    pub fn new() -> Dictionary {
        Dictionary::holding(CAP)
    }

    /// A dictionary of at most `cap` patterns (0: the usual 4096).
    pub fn holding(cap: usize) -> Dictionary {
        Dictionary {
            // A label is a rank of at most 15 binary digits.
            cap: if cap == 0 { CAP } else { cap.min(32767) },
            entries: Vec::new(),
            find: HashMap::new(),
            ranks: Vec::new(),
            oldest: BTreeSet::new(),
            waiting: HashMap::new(),
            pieces: 0,
            hit: [HALF; 2],
            rank_bits: [HALF; 32],
            byte: [HALF; 256],
            hits: 0,
            misses: 0,
            bytes_by_label: 0,
            bytes_in: 0,
            bytes_through: 0,
            frames_kept: 0,
            frames: 0,
        }
    }

    /// A pattern was used: it is counted, and moves up past those used less.
    fn used(&mut self, e: usize) {
        self.oldest.remove(&(self.entries[e].at, e));
        self.entries[e].at = self.pieces;
        self.oldest.insert((self.pieces, e));
        self.entries[e].used += 1;
        let mut r = self.entries[e].rank;
        while r > 0 && self.entries[self.ranks[r - 1]].used < self.entries[e].used {
            let other = self.ranks[r - 1];
            self.ranks.swap(r - 1, r);
            self.entries[other].rank = r;
            r -= 1;
        }
        self.entries[e].rank = r;
    }

    /// Two pieces came one after the other: the second time, the two joined become a pattern.
    fn saw(&mut self, before: Pattern, now: Pattern) {
        let Some(both) = join(before, now) else { return };
        if self.find.contains_key(&both) {
            return;
        }
        if self.waiting.remove(&both).is_none() {
            if self.waiting.len() >= WAITING {
                self.waiting.clear();
            }
            self.waiting.insert(both, ());
            return;
        }
        let entry = Entry { pattern: both, used: 1, at: self.pieces, rank: self.ranks.len() };
        let e = if self.entries.len() < self.cap {
            self.entries.push(entry);
            self.ranks.push(self.entries.len() - 1);
            self.entries.len() - 1
        } else {
            // Full: the pattern not used for the longest time gives up its place and its rank.
            let (at, e) = *self.oldest.iter().next().unwrap();
            self.oldest.remove(&(at, e));
            self.find.remove(&self.entries[e].pattern);
            let rank = self.entries[e].rank;
            self.entries[e] = Entry { rank, ..entry };
            e
        };
        self.find.insert(both, e);
        self.oldest.insert((self.entries[e].at, e));
    }

    /// The pieces of `want` (packing or learning) or of the coded bytes (unpacking, `len` bytes out).
    fn run(&mut self, c: &mut Coder, want: &[u8], len: usize) -> Vec<u8> {
        let reading = matches!(c, Coder::Read(_));
        let mut out: Vec<u8> = Vec::with_capacity(len);
        let mut before: Option<Pattern> = None;
        let mut was_hit = 0usize;
        while out.len() < len {
            let at = out.len();
            // The packer looks for the longest pattern the bits ahead are.
            let mut found: Option<usize> = None;
            if !reading {
                for n in (2..=8.min(len - at)).rev() {
                    if let Some(e) = self.find.get(&pattern(&want[at..at + n])) {
                        found = Some(*e);
                        break;
                    }
                }
            }
            let piece = if c.bit(&mut self.hit[was_hit], found.is_some()) {
                // The label: the pattern's rank, low ranks in few bits.
                let rank = self.count(c, found.map_or(0, |e| self.entries[e].rank as u32 + 1)) as usize - 1;
                let e = *self.ranks.get(rank).unwrap_or(&0);
                let p = self.entries.get(e).map_or((0, 2), |x| x.pattern);
                for k in (0..p.1).rev() {
                    out.push((p.0 >> (8 * k as u32)) as u8);
                }
                if !self.entries.is_empty() {
                    self.used(e);
                }
                self.hits += 1;
                self.bytes_by_label += p.1 as u64;
                was_hit = 1;
                p
            } else {
                let b = self.literal(c, if reading { 0 } else { want[at] });
                out.push(b);
                self.misses += 1;
                was_hit = 0;
                (b as u64, 1)
            };
            self.pieces += 1;
            if let Some(b) = before {
                self.saw(b, piece);
            }
            before = Some(piece);
        }
        out.truncate(len);
        out
    }

    /// A whole number from 1: how many binary digits, then the digits under the first.
    fn count(&mut self, c: &mut Coder, n: u32) -> u32 {
        let want = 32 - n.max(1).leading_zeros() as usize;
        let mut digits = 1;
        while digits < 16 && c.bit(&mut self.rank_bits[digits - 1], digits < want) {
            digits += 1;
        }
        let mut value = 1u32;
        for j in (0..digits - 1).rev() {
            value = (value << 1) | c.bit(&mut self.rank_bits[16 + j], (n >> j) & 1 == 1) as u32;
        }
        value
    }

    /// A byte that is in no pattern, bit by bit.
    fn literal(&mut self, c: &mut Coder, b: u8) -> u8 {
        let mut node = 1usize;
        for k in (0..8).rev() {
            let bit = c.bit(&mut self.byte[node], (b >> k) & 1 == 1);
            node = node * 2 + bit as usize;
        }
        node as u8
    }

    /// Packer: the frame's bytes go through the dictionary. Returns what is kept: the
    /// frame through the dictionary (KIND, 8 bytes of its hash, its length, the labels
    /// and bytes) when that is smaller, else the frame as it is.
    pub fn close(&mut self, frame: Vec<u8>) -> Vec<u8> {
        let mut c = Coder::Write(Encoder::new());
        self.run(&mut c, &frame, frame.len());
        let Coder::Write(e) = c else { unreachable!() };
        let coded = e.finish();
        self.frames += 1;
        self.bytes_in += frame.len() as u64;
        self.bytes_through += coded.len() as u64 + 21;
        if coded.len() + 21 >= frame.len() {
            return frame;
        }
        self.frames_kept += 1;
        let mut out = vec![KIND];
        out.extend(&crate::lightning::hex(&sha2::Sha256::digest(&frame)).as_bytes()[..16]);
        out.extend((frame.len() as u32).to_le_bytes());
        out.extend(coded);
        out
    }

    /// Player: the frame's bytes back, and the dictionary moved on exactly as the packer's was.
    pub fn open(&mut self, kept: Vec<u8>) -> Result<Vec<u8>, String> {
        self.frames += 1;
        if kept.first() != Some(&KIND) {
            // The frame was kept as it is: the dictionary still goes through it as the
            // packer's did (what would have been written is thrown away), so it learns the same.
            self.run(&mut Coder::Write(Encoder::new()), &kept, kept.len());
            self.bytes_in += kept.len() as u64;
            return Ok(kept);
        }
        if kept.len() < 21 {
            return Err("a dictionary frame is cut".into());
        }
        let len = u32::from_le_bytes(kept[17..21].try_into().unwrap()) as usize;
        let mut c = Coder::Read(Decoder::new(&kept[21..]));
        let frame = self.run(&mut c, &[], len);
        // The check on the bitstream: the bits rebuilt are the bits the packer had.
        if crate::lightning::hex(&sha2::Sha256::digest(&frame)).as_bytes()[..16] != kept[1..17] {
            return Err("a dictionary frame did not unpack to the bits it was made from".into());
        }
        self.frames_kept += 1;
        self.bytes_in += frame.len() as u64;
        Ok(frame)
    }

    /// The dictionary as bytes: every pattern with its rank, its uses and when it was last used.
    pub fn bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for e in &self.entries {
            out.extend(e.pattern.0.to_le_bytes());
            out.push(e.pattern.1);
            out.extend(e.used.to_le_bytes());
            out.extend(e.at.to_le_bytes());
            out.extend((e.rank as u32).to_le_bytes());
        }
        out
    }

    pub fn patterns(&self) -> usize {
        self.entries.len()
    }
    /// The patterns as bits, the most used first, with how often each was used.
    pub fn commonest(&self, n: usize) -> Vec<(String, u32)> {
        self.ranks.iter().take(n).map(|e| {
            let p = self.entries[*e].pattern;
            ((0..p.1).rev().map(|k| format!("{:08b}", (p.0 >> (8 * k as u32)) as u8)).collect::<Vec<_>>().join(" "), self.entries[*e].used)
        }).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_player_gets_the_same_bits_and_the_same_dictionary() {
        let mut s = 3u32;
        let mut frames: Vec<Vec<u8>> = Vec::new();
        for f in 0..40 {
            // Bytes with patterns in them, and bytes without.
            frames.push((0..3000).map(|i| { s = s.wrapping_mul(1664525).wrapping_add(1013904223); if f % 2 == 0 { ((i / 3) % 7) as u8 * 31 } else { (s >> 24) as u8 } }).collect());
        }
        let (mut packer, mut player) = (Dictionary::new(), Dictionary::new());
        let mut smaller = 0;
        for frame in &frames {
            let kept = packer.close(frame.clone());
            smaller += (kept.len() < frame.len()) as usize;
            assert_eq!(&player.open(kept).unwrap(), frame);
            assert_eq!(packer.bytes(), player.bytes(), "the two dictionaries are the same after every frame");
        }
        assert!(smaller >= 15, "frames with patterns get smaller ({smaller})");
        assert!(packer.patterns() <= CAP);
    }
}
