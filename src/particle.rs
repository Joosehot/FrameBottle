//! The particle a node is: a species made by the rice bowl's particle maker
//! (Aurelia/particle_maker), read from its vault. A node is one copy of the
//! species. Its ports, in the order the grains' contacts began, hold the
//! packed tile, one share each; the bond from this copy's `out` port to the
//! next copy's `into` port is the link to the next frame. Copies of one
//! species bind port to port where the charges point opposite ways
//! (affinity s = -q_a . q_b > 0), so the bond is the species' strongest
//! such pair: nothing about it is chosen here.
//!
//! A particle carries the data; it does not make it smaller.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Species {
    pub id: String,
    pub kind: String,
    pub definition_hash: String,
    /// The number of ports.
    pub k: usize,
    pub charge: Vec<Vec<f64>>,
    pub mass: f64,
    pub radius: f64,
    /// The bond between two copies: this port of a node ...
    pub out: usize,
    /// ... to this port of the next node ...
    pub into: usize,
    /// ... which attract each other this much.
    pub affinity: f64,
}

pub fn vault_dir() -> PathBuf {
    if let Some(v) = std::env::var_os("AURELIA_VAULT") {
        return PathBuf::from(v);
    }
    let base = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    base.join("Aurelia").join("vault")
}

fn read(path: &std::path::Path) -> Option<Species> {
    let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let d = doc.get("definition")?;
    let charge: Vec<Vec<f64>> = serde_json::from_value(d.get("charge")?.clone()).ok()?;
    let k = charge.len();
    // The strongest attracting pair of two different ports; the first on a tie.
    let mut best: Option<(usize, usize, f64)> = None;
    for a in 0..k {
        for b in 0..k {
            if a == b {
                continue;
            }
            let s = -charge[a].iter().zip(&charge[b]).map(|(x, y)| x * y).sum::<f64>();
            if s > best.map_or(0.0, |x| x.2) {
                best = Some((a, b, s));
            }
        }
    }
    let (out, into, affinity) = best?;
    Some(Species {
        id: doc.get("id")?.as_str()?.to_string(),
        kind: doc.get("kind").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        definition_hash: doc.get("definition_hash")?.as_str()?.to_string(),
        k,
        charge,
        mass: d.get("mass")?.as_f64()?,
        radius: d.get("radius")?.as_f64()?,
        out,
        into,
        affinity,
    })
}

/// The species whose copies bind to each other hardest (a film is a chain
/// of bonds); the first by name on a tie.
pub fn choose() -> Result<Species, String> {
    let dir = vault_dir();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| format!("the particle maker's vault {}: {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    let mut best: Option<Species> = None;
    for f in &files {
        if let Some(s) = read(f) {
            if best.as_ref().is_none_or(|b| s.affinity > b.affinity) {
                best = Some(s);
            }
        }
    }
    best.ok_or(format!("{}: no particle whose copies bind", dir.display()))
}

/// The packed tile as the ports hold it: one share per port, in port order.
pub fn shares(packed: &[u8], k: usize) -> Vec<&[u8]> {
    (0..k).map(|p| &packed[p * packed.len() / k..(p + 1) * packed.len() / k]).collect()
}
