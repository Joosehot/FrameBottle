//! The Neuras refiner: a small network that makes the motion engine's guess
//! of a tile better before the miss is counted.
//!
//! The network is Neuras's own (`neuras::model::Net`, three 3 x 3
//! convolutions of 8 channels with a ReLU between), and it is trained with
//! Neuras's own training (`neuras::train::net::Model`: its forward and
//! backward passes, squared error, Adam). It is trained beforehand, on
//! clips that are not the test clip, and its weights lie with the player
//! (`refiner.bin`, built into the program), not in any bottle.
//!
//! **What goes in**: four windows of 24 x 24 brightness samples, a tile of
//! 16 x 16 with 4 more on every side: the same place in the last three
//! frames others are guessed from, as the player rebuilt them, and the
//! motion engine's guess of the window. **What comes out**: how the guess
//! of the tile should be changed; the refined guess is the guess plus that.
//!
//! **The same on both sides.** Training is done in floating point, but the
//! refiner the packer and the player run is in whole numbers only: weights
//! in 1024ths, sums in 65 536ths, every rounding a shift. So the two get
//! the same refined tile bit for bit, and only rebuilt frames go in.

use neuras::model::{Layer, Net, Shape};
use neuras::train::net::Model;
use rayon::prelude::*;

/// A window is a tile of 16 x 16 with 4 more samples on every side.
pub const SIDE: usize = 24;
pub const EDGE: usize = 4;
const N: usize = SIDE * SIDE;
/// The windows that go in: three frames and the motion engine's guess.
pub const WINDOWS: usize = 4;
const CHANNELS: usize = 8;
/// One sample for training: the four windows, then the real window.
pub const SAMPLE: usize = (WINDOWS + 1) * N;

/// The network, as Neuras builds and checks it.
pub fn net() -> Net {
    Net::build(
        Shape::Maps { c: WINDOWS, h: SIDE, w: SIDE },
        vec![Layer::Conv { out: CHANNELS, k: 3, stride: 1 }, Layer::Relu, Layer::Conv { out: CHANNELS, k: 3, stride: 1 }, Layer::Relu, Layer::Conv { out: 1, k: 3, stride: 1 }],
    )
    .expect("the refiner's layers fit")
}

/// The refiner in whole numbers: for each of the three convolutions its
/// weights in 1024ths and its biases in 65 536ths.
pub struct Refiner {
    weights: [Vec<i32>; 3],
    biases: [Vec<i32>; 3],
}

/// One 3 x 3 convolution over maps of 24 x 24 in whole numbers: samples and results
/// in 64ths, zero outside the window.
fn conv(a: &[i32], ic: usize, oc: usize, w: &[i32], b: &[i32], relu: bool) -> Vec<i32> {
    let mut out = vec![0i32; oc * N];
    for o in 0..oc {
        for y in 0..SIDE {
            for x in 0..SIDE {
                let mut sum = b[o];
                for c in 0..ic {
                    for ky in 0..3 {
                        let iy = y + ky;
                        if iy < 1 || iy > SIDE {
                            continue;
                        }
                        let row = c * N + (iy - 1) * SIDE;
                        let wrow = ((o * ic + c) * 3 + ky) * 3;
                        for kx in 0..3 {
                            let ix = x + kx;
                            if ix >= 1 && ix <= SIDE {
                                sum += a[row + ix - 1] * w[wrow + kx];
                            }
                        }
                    }
                }
                let v = (sum + 512) >> 10;
                out[o * N + y * SIDE + x] = if relu { v.max(0) } else { v };
            }
        }
    }
    out
}

impl Refiner {
    /// A trained Neuras model's weights, rounded to whole numbers.
    pub fn from_model(m: &Model) -> Refiner {
        let round = |v: &[f32], scale: f32| -> Vec<i32> { v.iter().map(|x| (x * scale).round().clamp(-2_000_000.0, 2_000_000.0) as i32).collect() };
        Refiner {
            weights: [round(&m.params[0], 1024.0), round(&m.params[2], 1024.0), round(&m.params[4], 1024.0)],
            biases: [round(&m.params[1], 65536.0), round(&m.params[3], 65536.0), round(&m.params[5], 65536.0)],
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = b"FBREFINE".to_vec();
        for part in self.weights.iter().chain(self.biases.iter()) {
            out.extend((part.len() as u32).to_le_bytes());
            for v in part {
                out.extend(v.to_le_bytes());
            }
        }
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<Refiner> {
        if bytes.len() < 8 || &bytes[..8] != b"FBREFINE" {
            return None;
        }
        let mut at = 8;
        let mut parts: Vec<Vec<i32>> = Vec::new();
        for _ in 0..6 {
            let n = u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?) as usize;
            at += 4;
            parts.push(bytes.get(at..at + 4 * n)?.chunks_exact(4).map(|c| i32::from_le_bytes(c.try_into().unwrap())).collect());
            at += 4 * n;
        }
        let sizes = [CHANNELS * WINDOWS * 9, CHANNELS * CHANNELS * 9, CHANNELS * 9, CHANNELS, CHANNELS, 1];
        if parts.iter().zip(sizes).any(|(p, n)| p.len() != n) {
            return None;
        }
        let mut it = parts.into_iter();
        Some(Refiner { weights: [it.next()?, it.next()?, it.next()?], biases: [it.next()?, it.next()?, it.next()?] })
    }

    /// How the whole window's guess should be changed, in brightness levels.
    fn change(&self, windows: &[u8]) -> Vec<i32> {
        // A sample as the network was trained on it: (level - 128) / 64, here in 64ths.
        let a: Vec<i32> = windows[..WINDOWS * N].iter().map(|v| *v as i32 - 128).collect();
        let a = conv(&a, WINDOWS, CHANNELS, &self.weights[0], &self.biases[0], true);
        let a = conv(&a, CHANNELS, CHANNELS, &self.weights[1], &self.biases[1], true);
        conv(&a, CHANNELS, 1, &self.weights[2], &self.biases[2], false)
    }

    /// The refined guess of the tile in the middle of the window (16 x 16): the motion
    /// engine's guess (the fourth window) plus the change. `windows` is four windows.
    pub fn refine(&self, windows: &[u8]) -> [i32; 256] {
        let change = self.change(windows);
        let guess = &windows[(WINDOWS - 1) * N..WINDOWS * N];
        let mut out = [0i32; 256];
        for y in 0..16 {
            for x in 0..16 {
                let at = (y + EDGE) * SIDE + x + EDGE;
                out[y * 16 + x] = (guess[at] as i32 + change[at]).clamp(0, 255);
            }
        }
        out
    }
}

/// The refiner that lies with the player: built into the program from `refiner.bin`.
pub fn shipped() -> Option<&'static Refiner> {
    static ONCE: std::sync::OnceLock<Option<Refiner>> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| Refiner::from_bytes(include_bytes!("../refiner.bin"))).as_ref()
}

/// The name of the shipped refiner: the first 16 letters of the SHA-256 of its bytes.
pub fn shipped_name() -> String {
    crate::bottle::sha(include_bytes!("../refiner.bin"))[..16].to_string()
}

/// A sample as the network takes it: the four windows as (level - 128) / 64, and what
/// it should answer: (real - guess) / 64 for the whole window.
fn as_floats(sample: &[u8]) -> (Vec<f32>, Vec<f32>) {
    let x: Vec<f32> = sample[..WINDOWS * N].iter().map(|v| (*v as f32 - 128.0) / 64.0).collect();
    let guess = &sample[(WINDOWS - 1) * N..WINDOWS * N];
    let real = &sample[WINDOWS * N..SAMPLE];
    (x, real.iter().zip(guess).map(|(r, g)| (*r as f32 - *g as f32) / 64.0).collect())
}

/// How wrong the tile's guess is, on average per sample, in squares of levels: the
/// motion engine's own guess, and the refined one.
fn wrong(r: &Refiner, samples: &[u8]) -> (f64, f64) {
    let per: Vec<(f64, f64)> = samples
        .par_chunks(SAMPLE)
        .map(|s| {
            let refined = r.refine(s);
            let (guess, real) = (&s[(WINDOWS - 1) * N..WINDOWS * N], &s[WINDOWS * N..SAMPLE]);
            let (mut plain, mut fine) = (0f64, 0f64);
            for y in 0..16 {
                for x in 0..16 {
                    let at = (y + EDGE) * SIDE + x + EDGE;
                    plain += (real[at] as f64 - guess[at] as f64).powi(2);
                    fine += (real[at] as f64 - refined[y * 16 + x] as f64).powi(2);
                }
            }
            (plain / 256.0, fine / 256.0)
        })
        .collect();
    let n = per.len().max(1) as f64;
    (per.iter().map(|p| p.0).sum::<f64>() / n, per.iter().map(|p| p.1).sum::<f64>() / n)
}

/// Trains the refiner with Neuras on the samples (every tenth is kept out and only
/// measured on) and returns it in whole numbers.
pub fn train(samples: &[u8], epochs: usize, mut say: impl FnMut(String)) -> Refiner {
    let count = samples.len() / SAMPLE;
    let (held, used): (Vec<usize>, Vec<usize>) = (0..count).partition(|i| i % 10 == 9);
    let held_bytes: Vec<u8> = held.iter().flat_map(|i| samples[i * SAMPLE..(i + 1) * SAMPLE].iter().copied()).collect();
    let design = net();
    say(format!("Neuras's network: {} parameters, {} multiply-adds a tile; {} samples to learn from, {} kept out", design.params, design.macs, used.len(), held.len()));
    let mut model = Model::new(&design, 7);
    let (batch, lr) = (32usize, 0.001f64);
    let mut order = used.clone();
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    for epoch in 0..epochs {
        // The same shuffle every time: the same samples give the same refiner.
        for i in (1..order.len()).rev() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            order.swap(i, (seed % (i as u64 + 1)) as usize);
        }
        let began = std::time::Instant::now();
        let mut loss_sum = 0f64;
        for ids in order.chunks(batch) {
            // The batch in a fixed number of parts, summed in order.
            let parts: Vec<(Vec<Vec<f32>>, f32)> = ids
                .par_chunks(ids.len().div_ceil(12).max(1))
                .map(|part| {
                    let mut grads = model.zero_grads();
                    let mut loss = 0f32;
                    for i in part {
                        let (x, y) = as_floats(&samples[i * SAMPLE..(i + 1) * SAMPLE]);
                        let t = model.forward(&x);
                        loss += model.backward_mse(&t, &y, &mut grads);
                    }
                    (grads, loss)
                })
                .collect();
            let mut grads = model.zero_grads();
            for (g, loss) in parts {
                loss_sum += loss as f64;
                for (sum, part) in grads.iter_mut().zip(g) {
                    for (s, p) in sum.iter_mut().zip(part) {
                        *s += p / ids.len() as f32;
                    }
                }
            }
            model.adam(&grads, lr);
        }
        let now = Refiner::from_model(&model);
        let (plain, fine) = wrong(&now, &held_bytes);
        say(format!(
            "epoch {}: loss {:.5}; on the samples kept out, in whole numbers: the motion engine's guess is wrong by {plain:.3} (mean square), the refined guess by {fine:.3} ({:+.1} %); {:.0} s",
            epoch + 1,
            loss_sum / (used.len() * N) as f64,
            100.0 * (fine - plain) / plain.max(1e-9),
            began.elapsed().as_secs_f64()
        ));
    }
    Refiner::from_model(&model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_numbers_give_the_same_every_time_and_follow_the_floats() {
        let model = Model::new(&net(), 7);
        let r = Refiner::from_model(&model);
        let again = Refiner::from_bytes(&r.to_bytes()).unwrap();
        let mut s = 5u32;
        let windows: Vec<u8> = (0..WINDOWS * N).map(|_| { s = s.wrapping_mul(1664525).wrapping_add(1013904223); (s >> 24) as u8 }).collect();
        assert_eq!(r.refine(&windows), again.refine(&windows));
        // The whole-number refiner is the float one to within a level or two.
        let x: Vec<f32> = windows.iter().map(|v| (*v as f32 - 128.0) / 64.0).collect();
        let float = model.outputs(&x);
        let whole = r.change(&windows);
        let far = float.iter().zip(&whole).map(|(f, w)| (f * 64.0 - *w as f32).abs()).fold(0f32, f32::max);
        assert!(far < 2.0, "whole numbers are {far} levels from the floats at most");
    }
}
