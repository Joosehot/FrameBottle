//! Golden Answer (Nuome), in its logical kind, works out the report's
//! numbers exactly. A question it has no logical answer to is said so; the
//! number is then not shown as Golden's.

use std::path::PathBuf;
use std::process::Command;

fn nuome() -> PathBuf {
    if let Some(v) = std::env::var_os("NUOME") {
        return PathBuf::from(v);
    }
    let base = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    base.join("Nuome").join("target").join("release").join(if cfg!(windows) { "nuome.exe" } else { "nuome" })
}

/// The short answer to `question`, proved (logical kind), or why there is none.
pub fn ask(question: &str) -> Result<String, String> {
    let out = Command::new(nuome()).args(["--answer", "logical", question]).output().map_err(|e| format!("Golden Answer could not be run ({}): {e}", nuome().display()))?;
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        if let Some(a) = line.strip_prefix("answer: ") {
            return Ok(a.trim().to_string());
        }
    }
    Err(format!("Golden Answer has no logical answer to \"{question}\""))
}

/// "a/b" or "a" as a decimal with `places` places, for reading only.
pub fn decimal(exact: &str, places: usize) -> String {
    let (a, b) = exact.split_once('/').unwrap_or((exact, "1"));
    match (a.trim().parse::<f64>(), b.trim().parse::<f64>()) {
        (Ok(a), Ok(b)) if b != 0.0 => format!("{:.places$}", a / b),
        _ => exact.to_string(),
    }
}
