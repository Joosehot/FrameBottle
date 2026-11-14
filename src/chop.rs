//! CHOP: a movie into its frames, every one, by ffmpeg.

use crate::pack::Geometry;
use std::io::Read;
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};

pub struct Movie {
    pub geo: Geometry,
    /// The movie's own pixel format; differs from geo.pix_fmt when the frames are converted.
    pub source_pix_fmt: String,
    pub fps: (u64, u64),
}

fn ratio(s: &str) -> Option<(u64, u64)> {
    let (a, b) = s.split_once('/')?;
    let (a, b) = (a.parse().ok()?, b.parse().ok()?);
    (a > 0 && b > 0).then_some((a, b))
}

pub fn probe(path: &Path) -> Result<Movie, String> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries"])
        .arg("stream=width,height,pix_fmt,avg_frame_rate,r_frame_rate:stream_side_data=rotation")
        .args(["-of", "json"])
        .arg(path)
        .output()
        .map_err(|e| format!("ffprobe: {e}"))?;
    if !out.status.success() {
        return Err(format!("ffprobe: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| format!("ffprobe: {e}"))?;
    let s = doc.pointer("/streams/0").ok_or(format!("{}: no video in it", path.display()))?;
    let n = |k: &str| s.get(k).and_then(|v| v.as_u64()).ok_or(format!("ffprobe: no {k}"));
    let (mut w, mut h) = (n("width")? as usize, n("height")? as usize);
    // ffmpeg turns a frame that is stored on its side upright.
    let turn = s
        .pointer("/side_data_list")
        .and_then(|l| l.as_array())
        .and_then(|l| l.iter().find_map(|d| d.get("rotation")?.as_f64()))
        .unwrap_or(0.0);
    if (turn.abs() as i64) % 180 == 90 {
        std::mem::swap(&mut w, &mut h);
    }
    let source = s.get("pix_fmt").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let text = |k: &str| s.get(k).and_then(|v| v.as_str()).and_then(ratio);
    let fps = text("avg_frame_rate").or(text("r_frame_rate")).ok_or("ffprobe: no frame rate")?;
    let geo = match Geometry::of(&source, w, h) {
        Some(g) => g,
        // A format Frame Bottle has no layout for is taken as RGB (with alpha if it has one).
        None => {
            let alpha = ["rgba", "bgra", "argb", "abgr", "yuva", "gbrap", "ya"].iter().any(|a| source.starts_with(a));
            Geometry::of(if alpha { "rgba" } else { "rgb24" }, w, h).unwrap()
        }
    };
    Ok(Movie { geo, source_pix_fmt: source, fps })
}

pub struct Frames {
    child: Child,
    out: ChildStdout,
    len: usize,
}

/// Every frame of the movie, in order, none dropped and none doubled.
pub fn frames(path: &Path, geo: &Geometry) -> Result<Frames, String> {
    let mut child = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-map", "0:v:0", "-fps_mode", "passthrough", "-f", "rawvideo", "-pix_fmt", &geo.pix_fmt, "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("ffmpeg: {e}"))?;
    let out = child.stdout.take().unwrap();
    Ok(Frames { child, out, len: geo.frame_len() })
}

impl Frames {
    pub fn next(&mut self) -> Result<Option<Vec<u8>>, String> {
        let mut frame = vec![0u8; self.len];
        let mut got = 0;
        while got < self.len {
            match self.out.read(&mut frame[got..]).map_err(|e| format!("ffmpeg: {e}"))? {
                0 => break,
                n => got += n,
            }
        }
        if got == 0 {
            let status = self.child.wait().map_err(|e| e.to_string())?;
            return if status.success() { Ok(None) } else { Err("ffmpeg failed while chopping".into()) };
        }
        if got < self.len {
            return Err("ffmpeg gave a part of a frame".into());
        }
        Ok(Some(frame))
    }
}
