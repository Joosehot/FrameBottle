//! HotMotor for frames: the rules offer variants, the beam keeps the best
//! partial packs, the judges score the choices together on the tile itself.
//! Every number comes from rules.toml. (The same motor as Strike's
//! src/motor.rs, whose rules are rustc's decisions; here they are a tile's.)

use std::collections::BTreeMap;

pub const AXES: [&str; 3] = ["pack", "play", "size"];

#[derive(Clone, Debug)]
pub struct Variant {
    pub value: String,
    pub axes: [f64; 3],
}

#[derive(Clone, Debug)]
pub struct Rule {
    pub name: String,
    pub weight: f64,
    pub variants: Vec<Variant>,
}

#[derive(Debug)]
pub struct Rules {
    pub beam: usize,
    pub profiles: BTreeMap<String, [f64; 3]>,
    pub rules: Vec<Rule>,
    pub judge_size: f64,
    pub sample_bands: usize,
    pub band_rows: usize,
    pub block: usize,
    pub range: usize,
    pub zero_bias: f64,
    pub poor_match: f64,
    pub ahead: usize,
    pub behind: usize,
    pub bands: usize,
    /// How wrong a guess may be and still be left uncorrected (0: every frame comes back
    /// exactly). Not in rules.toml: the command's --tolerance sets it.
    pub tolerance: u8,
    /// Blocks, AV1's way: how long a wave's step is (0: not used). The command's --coarse sets it.
    pub coarse: u8,
    /// The pattern dictionary is used (the command's --dictionary).
    pub dictionary: bool,
    /// The most patterns the bit dictionary holds (0: the usual 4096).
    pub patterns: usize,
    pub blocks: crate::block::How,
}

pub const DEFAULT_RULES: &str = include_str!("../rules.toml");

fn num(t: &toml::Table, key: &str, at: &str) -> Result<f64, String> {
    match t.get(key) {
        Some(toml::Value::Float(f)) => Ok(*f),
        Some(toml::Value::Integer(i)) => Ok(*i as f64),
        _ => Err(format!("rules.toml: {at} needs the number {key}")),
    }
}

fn axes(t: &toml::Table, at: &str) -> Result<[f64; 3], String> {
    let mut a = [0.0; 3];
    for (i, name) in AXES.iter().enumerate() {
        a[i] = num(t, name, at)?;
    }
    Ok(a)
}

impl Rules {
    /// Read and check rules.toml: a typo fails here, not in a pack.
    pub fn parse(text: &str) -> Result<Rules, String> {
        let doc: toml::Table = text.parse().map_err(|e| format!("rules.toml: {e}"))?;
        let beam = num(&doc, "beam", "the top")? as usize;
        if beam == 0 {
            return Err("rules.toml: beam must be at least 1".into());
        }
        let mut profiles = BTreeMap::new();
        for (name, p) in doc.get("profile").and_then(|p| p.as_table()).ok_or("rules.toml: no [profile]")? {
            let p = p.as_table().ok_or("rules.toml: a profile is a table")?;
            profiles.insert(name.clone(), axes(p, &format!("profile.{name}"))?);
        }
        // The decisions Frame Bottle knows how to turn into a packed tile, in order.
        let known = ["from", "predict", "coder"];
        let mut rules = Vec::new();
        for name in known {
            let at = format!("rule.{name}");
            let r = doc.get("rule").and_then(|r| r.get(name)).and_then(|r| r.as_table()).ok_or(format!("rules.toml: no [{at}]"))?;
            let mut variants = Vec::new();
            for v in r.get("variants").and_then(|v| v.as_array()).ok_or(format!("rules.toml: {at} has no variants"))? {
                let v = v.as_table().ok_or(format!("rules.toml: a variant of {at} is not a table"))?;
                let value = v.get("value").and_then(|x| x.as_str()).ok_or(format!("rules.toml: a variant of {at} has no value"))?;
                let known_values: &[&str] = match name {
                    "from" => &crate::pack::FROMS,
                    "predict" => &crate::pack::PREDICTS,
                    _ => &crate::pack::CODERS,
                };
                if !known_values.contains(&value) {
                    return Err(format!("rules.toml: {at} has the unknown value {value} (known: {})", known_values.join(", ")));
                }
                variants.push(Variant { value: value.to_string(), axes: axes(v, &at)? });
            }
            if variants.is_empty() {
                return Err(format!("rules.toml: {at} has no variants"));
            }
            rules.push(Rule { name: name.to_string(), weight: num(r, "weight", &at)?, variants });
        }
        let table = |name: &str| doc.get(name).and_then(|j| j.as_table()).ok_or(format!("rules.toml: no [{name}]"));
        let (judge, smoother, player) = (table("judge")?, table("smoother")?, table("player")?);
        let block = num(smoother, "block", "smoother")? as usize;
        if block < 4 || block % 4 != 0 {
            return Err("rules.toml: smoother.block must be a multiple of 4".into());
        }
        Ok(Rules {
            beam,
            profiles,
            rules,
            judge_size: num(judge, "size", "judge")?,
            sample_bands: (num(judge, "sample_bands", "judge")? as usize).max(1),
            band_rows: (num(judge, "band_rows", "judge")? as usize).max(1),
            block,
            range: num(smoother, "range", "smoother")? as usize,
            zero_bias: num(smoother, "zero_bias", "smoother")?,
            poor_match: num(smoother, "poor_match", "smoother")?,
            ahead: (num(player, "ahead", "player")? as usize).max(1),
            behind: num(player, "behind", "player")? as usize,
            bands: (num(table("guess")?, "bands", "guess")? as usize).clamp(1, 4096),
            tolerance: 0,
            coarse: 0,
            dictionary: false,
            patterns: 0,
            blocks: crate::block::How {
                range: num(table("block")?, "range", "block")? as usize,
                slope: (num(table("block")?, "slope", "block")? as usize).clamp(1, 255) as u8,
                dead: (num(table("block")?, "dead", "block")? as usize).clamp(1, 255) as u8,
                still: num(table("block")?, "still", "block")? as u32,
            },
        })
    }
}

/// A choice of one variant per rule (so far): rule name -> value.
pub type Choice = Vec<(String, String)>;

pub fn get<'a>(choice: &'a Choice, rule: &str) -> Option<&'a str> {
    choice.iter().find(|(r, _)| r == rule).map(|(_, v)| v.as_str())
}

#[derive(Clone, Debug)]
pub struct Candidate {
    pub choice: Choice,
    pub local: f64,
    pub judged: f64,
    pub notes: Vec<String>,
}

impl Candidate {
    pub fn score(&self) -> f64 {
        self.local + self.judged
    }
}

/// The beam search: rule by rule, every partial pack grows by every variant
/// and the best `beam` are kept. Ties break on the variants' order in
/// rules.toml, so the result is deterministic. Returns the finalists, best
/// first.
pub fn search(rules: &Rules, profile: &[f64; 3], judge: &mut dyn FnMut(&Choice) -> (f64, Vec<String>)) -> Vec<Candidate> {
    let mut beam = vec![Candidate { choice: Vec::new(), local: 0.0, judged: 0.0, notes: Vec::new() }];
    for rule in &rules.rules {
        let mut grown = Vec::new();
        for c in &beam {
            for v in &rule.variants {
                let mut choice = c.choice.clone();
                choice.push((rule.name.clone(), v.value.clone()));
                let dot: f64 = v.axes.iter().zip(profile).map(|(a, p)| a * p).sum();
                let local = c.local + rule.weight * dot;
                let (judged, notes) = judge(&choice);
                grown.push(Candidate { choice, local, judged, notes });
            }
        }
        // A stable sort keeps the earlier variant first on a tie.
        grown.sort_by(|a, b| b.score().partial_cmp(&a.score()).unwrap_or(std::cmp::Ordering::Equal));
        grown.truncate(rules.beam);
        beam = grown;
    }
    beam
}
