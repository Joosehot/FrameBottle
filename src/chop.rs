//! CHOP: a movie into its frames, every one.
//!
//! A movie file (MP4, MOV, ...) is opened by Windows' own Media Foundation,
//! which hands out each decoded frame; a GIF is read by the gif reader and
//! drawn frame over frame as a GIF asks. No outside program is run.

use crate::pack::Geometry;
use std::path::Path;

pub struct Movie {
    pub geo: Geometry,
    /// How the frames were given, when that is not the layout they are kept in.
    pub source_pix_fmt: String,
    pub fps: (u64, u64),
}

pub struct Frames {
    source: Source,
}

enum Source {
    Video(video::Reader),
    Gif(std::vec::IntoIter<Vec<u8>>),
}

fn is_gif(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("gif"))
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

pub fn probe(path: &Path) -> Result<Movie, String> {
    if is_gif(path) {
        let (w, h, frames, fps) = gif_frames(path)?;
        let _ = frames;
        return Ok(Movie { geo: Geometry::of("rgba", w, h).unwrap(), source_pix_fmt: "rgba".into(), fps });
    }
    let r = video::Reader::open(path)?;
    let g = gcd(r.fps.0, r.fps.1).max(1);
    Ok(Movie { geo: Geometry::of("yuv420p", r.width, r.height).unwrap(), source_pix_fmt: "nv12".into(), fps: (r.fps.0 / g, r.fps.1 / g) })
}

/// Every frame of the movie, in order, none dropped and none doubled.
pub fn frames(path: &Path, geo: &Geometry) -> Result<Frames, String> {
    if is_gif(path) {
        let (w, h, frames, _) = gif_frames(path)?;
        if (w, h) != (geo.width, geo.height) || geo.pix_fmt != "rgba" {
            return Err(format!("{}: not the movie this bottle was made from", path.display()));
        }
        return Ok(Frames { source: Source::Gif(frames.into_iter()) });
    }
    let r = video::Reader::open(path)?;
    if (r.width, r.height) != (geo.width, geo.height) || geo.pix_fmt != "yuv420p" {
        return Err(format!("{}: not the movie this bottle was made from", path.display()));
    }
    Ok(Frames { source: Source::Video(r) })
}

impl Frames {
    pub fn next(&mut self) -> Result<Option<Vec<u8>>, String> {
        match &mut self.source {
            Source::Video(r) => r.next(),
            Source::Gif(frames) => Ok(frames.next()),
        }
    }
}

/// A GIF's frames as whole RGBA pictures: each frame is drawn over what the
/// frames before it left, as its disposal says.
fn gif_frames(path: &Path) -> Result<(usize, usize, Vec<Vec<u8>>, (u64, u64)), String> {
    let err = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let file = std::fs::File::open(path).map_err(|e| err(&e))?;
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    let mut decoder = options.read_info(std::io::BufReader::new(file)).map_err(|e| err(&e))?;
    let (w, h) = (decoder.width() as usize, decoder.height() as usize);
    let mut canvas = vec![0u8; w * h * 4];
    let mut out = Vec::new();
    let mut hundredths = 0u64;
    while let Some(frame) = decoder.read_next_frame().map_err(|e| err(&e))? {
        let before = (frame.dispose == gif::DisposalMethod::Previous).then(|| canvas.clone());
        let (fx, fy, fw, fh) = (frame.left as usize, frame.top as usize, frame.width as usize, frame.height as usize);
        for y in 0..fh.min(h.saturating_sub(fy)) {
            for x in 0..fw.min(w.saturating_sub(fx)) {
                let p = &frame.buffer[(y * fw + x) * 4..][..4];
                if p[3] != 0 {
                    canvas[((fy + y) * w + fx + x) * 4..][..4].copy_from_slice(p);
                }
            }
        }
        out.push(canvas.clone());
        hundredths += frame.delay.max(1) as u64;
        match frame.dispose {
            gif::DisposalMethod::Background => {
                for y in fy..(fy + fh).min(h) {
                    canvas[(y * w + fx) * 4..(y * w + (fx + fw).min(w)) * 4].fill(0);
                }
            }
            gif::DisposalMethod::Previous => canvas = before.unwrap(),
            _ => {}
        }
    }
    if out.is_empty() {
        return Err(err(&"no frames in it"));
    }
    // The frame rate is the average: n frames in the sum of their delays (hundredths of a second).
    let (num, den) = (out.len() as u64 * 100, hundredths);
    let g = gcd(num, den).max(1);
    Ok((w, h, out, (num / g, den / g)))
}

#[cfg(windows)]
mod video {
    use std::path::Path;
    use windows::core::HSTRING;
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

    const VIDEO: u32 = 0xFFFF_FFFC; // MF_SOURCE_READER_FIRST_VIDEO_STREAM
    const ALL: u32 = 0xFFFF_FFFE; // MF_SOURCE_READER_ALL_STREAMS
    const END: u32 = 0x2; // MF_SOURCE_READERF_ENDOFSTREAM
    const TYPE_CHANGED: u32 = 0x20; // MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED

    pub struct Reader {
        reader: IMFSourceReader,
        pub width: usize,
        pub height: usize,
        pub fps: (u64, u64),
        /// The decoder's own picture: rows of `stride` bytes, the shown part starting at (left, top).
        stride: usize,
        left: usize,
        top: usize,
        name: String,
    }

    fn started() -> Result<(), String> {
        static ONCE: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
        ONCE.get_or_init(|| unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET).map_err(|e| format!("Media Foundation: {e}"))
        })
        .clone()
    }

    impl Reader {
        pub fn open(path: &Path) -> Result<Reader, String> {
            started()?;
            let name = path.display().to_string();
            let err = |e: windows::core::Error| format!("{name}: {e}");
            let full = std::fs::canonicalize(path).map_err(|e| format!("{name}: {e}"))?;
            let url = full.display().to_string().trim_start_matches(r"\\?\").to_string();
            unsafe {
                let reader = MFCreateSourceReaderFromURL(&HSTRING::from(url), None).map_err(err)?;
                reader.SetStreamSelection(ALL, false).map_err(err)?;
                reader.SetStreamSelection(VIDEO, true).map_err(|_| format!("{name}: no video in it"))?;
                // The frame rate is the file's own.
                let native = reader.GetNativeMediaType(VIDEO, 0).map_err(err)?;
                let rate = native.GetUINT64(&MF_MT_FRAME_RATE).map_err(err)?;
                let fps = (rate >> 32, rate & 0xFFFF_FFFF);
                if fps.0 == 0 || fps.1 == 0 {
                    return Err(format!("{name}: no frame rate"));
                }
                // The decoder's frames as they are: Y, then U and V side by side at half size.
                let want = MFCreateMediaType().map_err(err)?;
                want.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(err)?;
                want.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).map_err(err)?;
                reader.SetCurrentMediaType(VIDEO, None, &want).map_err(|e| format!("{name}: Windows cannot decode this movie's video ({e})"))?;
                let mut r = Reader { reader, width: 0, height: 0, fps, stride: 0, left: 0, top: 0, name: name.clone() };
                r.layout()?;
                Ok(r)
            }
        }

        /// Reads how the decoder lays its picture out, and which part of it is the frame.
        fn layout(&mut self) -> Result<(), String> {
            let err = |e: windows::core::Error| format!("{}: {e}", self.name);
            unsafe {
                let now = self.reader.GetCurrentMediaType(VIDEO).map_err(err)?;
                let size = now.GetUINT64(&MF_MT_FRAME_SIZE).map_err(err)?;
                let (mut w, mut h) = ((size >> 32) as usize, (size & 0xFFFF_FFFF) as usize);
                let stride = now.GetUINT32(&MF_MT_DEFAULT_STRIDE).map(|s| (s as i32).unsigned_abs() as usize).unwrap_or(w).max(w);
                let (mut left, mut top) = (0, 0);
                // The shown part: MFVideoArea { OffsetX, OffsetY (each: fraction u16, value i16), cx, cy }.
                let mut area = [0u8; 16];
                if now.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, &mut area, None).is_ok() {
                    let i16_at = |i: usize| i16::from_le_bytes([area[i], area[i + 1]]).max(0) as usize;
                    let i32_at = |i: usize| i32::from_le_bytes([area[i], area[i + 1], area[i + 2], area[i + 3]]).max(0) as usize;
                    let (cx, cy) = (i32_at(8), i32_at(12));
                    if cx > 0 && cy > 0 && i16_at(2) + cx <= w && i16_at(6) + cy <= h {
                        (left, top, w, h) = (i16_at(2), i16_at(6), cx, cy);
                    }
                }
                if self.width != 0 && (self.width, self.height) != (w, h) {
                    return Err(format!("{}: the frame size changes inside the movie", self.name));
                }
                (self.width, self.height, self.stride, self.left, self.top) = (w, h, stride, left, top);
            }
            Ok(())
        }

        /// The next frame as planar YUV 4:2:0 (Y, U, V), or None at the end.
        pub fn next(&mut self) -> Result<Option<Vec<u8>>, String> {
            loop {
                let (mut flags, mut sample) = (0u32, None);
                unsafe {
                    self.reader.ReadSample(VIDEO, 0, None, Some(&mut flags), None, Some(&mut sample)).map_err(|e| format!("{}: {e}", self.name))?;
                }
                if flags & TYPE_CHANGED != 0 {
                    self.layout()?;
                }
                let Some(sample) = sample else {
                    if flags & END != 0 {
                        return Ok(None);
                    }
                    continue;
                };
                let (w, h, stride) = (self.width, self.height, self.stride);
                let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
                let mut out = Vec::with_capacity(w * h + 2 * cw * ch);
                unsafe {
                    let buffer = sample.ConvertToContiguousBuffer().map_err(|e| format!("{}: {e}", self.name))?;
                    let (mut ptr, mut len) = (std::ptr::null_mut(), 0u32);
                    buffer.Lock(&mut ptr, None, Some(&mut len)).map_err(|e| format!("{}: {e}", self.name))?;
                    let data = std::slice::from_raw_parts(ptr, len as usize);
                    // The decoder's picture may be taller than the frame (1080 rows are decoded as 1088).
                    let rows = data.len() * 2 / (3 * stride);
                    let fits = rows >= self.top + h && data.len() >= stride * rows + (self.top / 2 + ch - 1) * stride + (self.left / 2 + cw) * 2;
                    if fits {
                        for y in 0..h {
                            out.extend_from_slice(&data[(self.top + y) * stride + self.left..][..w]);
                        }
                        for c in 0..2 {
                            for y in 0..ch {
                                let row = &data[stride * rows + (self.top / 2 + y) * stride + (self.left / 2) * 2..][..cw * 2];
                                out.extend(row.iter().skip(c).step_by(2));
                            }
                        }
                    }
                    let _ = buffer.Unlock();
                    if !fits {
                        return Err(format!("{}: the decoder gave a frame of an unexpected size", self.name));
                    }
                }
                return Ok(Some(out));
            }
        }
    }
}

#[cfg(not(windows))]
mod video {
    use std::path::Path;

    pub struct Reader {
        pub width: usize,
        pub height: usize,
        pub fps: (u64, u64),
    }

    impl Reader {
        pub fn open(path: &Path) -> Result<Reader, String> {
            Err(format!("{}: movie files are opened by Windows' Media Foundation; only GIFs can be chopped here", path.display()))
        }
        pub fn next(&mut self) -> Result<Option<Vec<u8>>, String> {
            Ok(None)
        }
    }
}
