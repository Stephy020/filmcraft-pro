//! Large stills, decoded when they are first shown instead of when they are imported.
//!
//! A phone photo is 12 to 48 megapixels: decoding one takes most of a second and its RGBA8 pixels
//! take 50 to 200 MB. [`super::StillSource`] decodes at import, on whichever thread imports (the
//! UI's), and keeps the pixels for as long as the source is open, so a handful of photos froze the
//! window for seconds and took a gigabyte. A [`LazyStill`] reads the header at import (size,
//! alpha, orientation), keeps the file's bytes, and decodes on the first frame request, which
//! comes from a frame worker. It then answers reduced-scale requests ([`FrameRequest::scale`])
//! with a half, quarter or eighth size picture, so a monitor that draws a 24 megapixel photo in a
//! thousand pixels uploads one or six megabytes instead of ninety-eight.
//!
//! Decoded pixels of all lazy stills count against [`BUDGET`]: over it, the stills used least
//! recently give theirs up (and decode again when asked). Stills asked for among the last
//! [`RECENT`] requests keep theirs whatever the total, so a frame that composites many photos, or
//! an export walking through one, never decodes the same photo twice in a row.
//!
//! The reduced pictures are 2 x 2 box means in linear light over premultiplied colour, each made
//! from the next larger one. Stills under [`LAZY_MIN_PIXELS`], and the formats our own decoders
//! read (Targa, DPX, PSD, DNG, HDR, EXR, YCbCr TIFF), stay with [`super::StillSource`]: a file
//! that cannot be decoded is then still refused at import.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use filmcraft_color::{linear_to_srgb_u8_in, linear_to_srgb_u8_table, srgb_u8_to_linear_table};
use filmcraft_frame::{PixelData, VideoFrame};
use image::ImageDecoder;
use image::metadata::Orientation;
use rayon::prelude::*;

use crate::{FrameRequest, MediaError, MediaInfo, MediaSource, Result};

/// Stills with fewer pixels are decoded at import: that takes a few milliseconds, and an
/// unreadable file is refused there.
pub const LAZY_MIN_PIXELS: u64 = 2_000_000;

/// Decoded pixels all lazy stills may hold before the least recently used give theirs up.
const BUDGET: usize = 384 << 20;

/// A still asked for among this many of the latest requests keeps its pixels (see the module doc).
const RECENT: u64 = 64;

/// Sizes kept: whole, half, quarter, eighth.
const LEVELS: usize = 4;

/// A reduced picture is not made smaller than this on its short side.
const MIN_SIDE: u32 = 64;

static CLOCK: AtomicU64 = AtomicU64::new(0);
static HELD: AtomicUsize = AtomicUsize::new(0);
static ALL: Mutex<Vec<Weak<Inner>>> = Mutex::new(Vec::new());

#[derive(Default)]
struct Pixels {
    levels: [Option<Arc<VideoFrame>>; LEVELS],
    /// [`CLOCK`] reading of the last request.
    used: u64,
    /// Bytes of `levels`, as counted in [`HELD`].
    bytes: usize,
}

impl Pixels {
    /// Recount `bytes` after `levels` changed and bring [`HELD`] in line.
    fn recount(&mut self) {
        let now: usize = self.levels.iter().flatten().map(|f| f.byte_size()).sum();
        if now >= self.bytes {
            HELD.fetch_add(now - self.bytes, Ordering::Relaxed);
        } else {
            sub_held(self.bytes - now);
        }
        self.bytes = now;
    }
}

fn sub_held(n: usize) {
    let _ = HELD.try_update(Ordering::Relaxed, Ordering::Relaxed, |h| Some(h.saturating_sub(n)));
}

struct Inner {
    name: String,
    bytes: Arc<[u8]>,
    pixels: Mutex<Pixels>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        sub_held(self.pixels.get_mut().unwrap_or_else(PoisonError::into_inner).bytes);
    }
}

/// A still whose pixels are decoded on demand (see the module doc).
pub struct LazyStill {
    info: MediaInfo,
    size: (u32, u32),
    inner: Arc<Inner>,
}

/// Decoded pixel bytes all lazy stills hold right now (diagnostics and tests).
pub fn held_bytes() -> usize {
    HELD.load(Ordering::Relaxed)
}

/// `bytes` (the file `name`) as a lazy still, or `None` when it should be decoded at import: a
/// format of our own decoders, a header the image decoder does not read, or a small picture.
pub fn open(name: &str, bytes: &Arc<[u8]>) -> Option<LazyStill> {
    if super::dng::sniff(bytes)
        || super::hdr::sniff(bytes)
        || crate::dpx::sniff(bytes)
        || crate::tga::sniff(name, bytes)
        || super::psd::sniff(bytes)
        || super::ycbcr_tiff::sniff(bytes)
    {
        return None;
    }
    let mut decoder = image::ImageReader::new(std::io::Cursor::new(&bytes[..])).with_guessed_format().ok()?.into_decoder().ok()?;
    let (w, h) = decoder.dimensions();
    let pixels = u64::from(w) * u64::from(h);
    if w == 0 || h == 0 || !(LAZY_MIN_PIXELS..=super::MAX_PIXELS).contains(&pixels) {
        return None;
    }
    let has_alpha = decoder.color_type().has_alpha();
    // as `decode_raster` turns the picture: a quarter turn swaps its sides
    let turned = matches!(
        decoder.orientation().unwrap_or(Orientation::NoTransforms),
        Orientation::Rotate90 | Orientation::Rotate270 | Orientation::Rotate90FlipH | Orientation::Rotate270FlipH
    );
    let (w, h) = if turned { (h, w) } else { (w, h) };
    let inner = Arc::new(Inner { name: name.to_string(), bytes: bytes.clone(), pixels: Mutex::new(Pixels::default()) });
    let mut all = ALL.lock().unwrap_or_else(PoisonError::into_inner);
    all.retain(|w| w.strong_count() > 0);
    all.push(Arc::downgrade(&inner));
    Some(LazyStill { info: super::still_info(name, w, h, has_alpha, bytes.len() as u64), size: (w, h), inner })
}

/// The smallest kept size that is at least `scale` of a `w` x `h` picture: 0 whole, 1 half…
fn level_for(scale: f32, (w, h): (u32, u32)) -> usize {
    let mut level = 0;
    // (a NaN scale compares false: the whole picture)
    while level + 1 < LEVELS && scale <= 0.5f32.powi(level as i32 + 1) && (w.min(h) >> (level + 1)) >= MIN_SIDE {
        level += 1;
    }
    level
}

/// `f` (straight-alpha sRGB RGBA8) at half the size on each axis: the 2 x 2 box mean of
/// premultiplied colour in linear light. Other pixel formats, and pictures under two pixels on a
/// side, come back as they are.
fn halved(f: &Arc<VideoFrame>) -> Arc<VideoFrame> {
    let PixelData::Rgba8(px) = &f.data else { return f.clone() };
    let (w, h) = (f.width as usize, f.height as usize);
    if w < 2 || h < 2 || px.len() < w * h * 4 {
        return f.clone();
    }
    let (ow, oh) = (w / 2, h / 2);
    let (lin, enc) = (srgb_u8_to_linear_table(), linear_to_srgb_u8_table());
    let mut out = vec![0u8; ow * oh * 4];
    out.par_chunks_mut(ow * 4).enumerate().for_each(|(y, row)| {
        let (Some(a), Some(b)) = (px.get(2 * y * w * 4..(2 * y + 1) * w * 4), px.get((2 * y + 1) * w * 4..(2 * y + 2) * w * 4)) else { return };
        for ((o, p), q) in row.as_chunks_mut::<4>().0.iter_mut().zip(a.as_chunks::<8>().0).zip(b.as_chunks::<8>().0) {
            // the four pixels: p[0..4], p[4..8], q[0..4], q[4..8]
            let alpha = u32::from(p[3]) + u32::from(p[7]) + u32::from(q[3]) + u32::from(q[7]);
            if alpha == 4 * 255 {
                for c in 0..3 {
                    let v = lin[p[c] as usize] + lin[p[c + 4] as usize] + lin[q[c] as usize] + lin[q[c + 4] as usize];
                    o[c] = linear_to_srgb_u8_in(enc, v * 0.25);
                }
                o[3] = 255;
            } else if alpha > 0 {
                for c in 0..3 {
                    let v = lin[p[c] as usize] * f32::from(p[3])
                        + lin[p[c + 4] as usize] * f32::from(p[7])
                        + lin[q[c] as usize] * f32::from(q[3])
                        + lin[q[c + 4] as usize] * f32::from(q[7]);
                    o[c] = linear_to_srgb_u8_in(enc, v / alpha as f32);
                }
                o[3] = ((alpha + 2) / 4) as u8;
            }
        }
    });
    Arc::new(VideoFrame { width: ow as u32, height: oh as u32, data: PixelData::Rgba8(Arc::new(out)), color: f.color, par: f.par, pts: f.pts })
}

impl LazyStill {
    /// The picture at `level`, decoding and reducing as needed. Reduced sizes made on the way are
    /// kept; the whole picture is kept only when it is what was asked for.
    fn frame(&self, level: usize, now: u64) -> Result<Arc<VideoFrame>> {
        let mut px = self.inner.pixels.lock().unwrap_or_else(PoisonError::into_inner);
        px.used = now;
        if let Some(f) = px.levels.get(level).and_then(Option::as_ref) {
            return Ok(f.clone());
        }
        // the nearest larger size held, else the file
        let held = (0..level).rev().find_map(|l| px.levels.get(l).and_then(Option::as_ref).map(|f| (l, f.clone())));
        let (mut at, mut cur) = match held {
            Some(found) => found,
            None => {
                let r = super::decode_raster(&self.inner.name, &self.inner.bytes)?;
                if (r.width, r.height) != self.size {
                    return Err(MediaError::Decode(format!(
                        "{}: the picture is {} x {}, its header says {} x {}",
                        self.inner.name, r.width, r.height, self.size.0, self.size.1
                    )));
                }
                (0, Arc::new(VideoFrame::rgba8(r.width, r.height, r.rgba)))
            }
        };
        if level == 0
            && let Some(slot) = px.levels.first_mut()
        {
            *slot = Some(cur.clone());
        }
        while at < level {
            cur = halved(&cur);
            at += 1;
            if let Some(slot) = px.levels.get_mut(at) {
                *slot = Some(cur.clone());
            }
        }
        px.recount();
        Ok(cur)
    }

    /// Over [`BUDGET`]: the other stills give up their pixels, least recently used first, except
    /// those asked for among the last [`RECENT`] requests and those decoding right now.
    fn trim(&self, now: u64) {
        if HELD.load(Ordering::Relaxed) <= BUDGET {
            return;
        }
        let mut others: Vec<Arc<Inner>> =
            ALL.lock().unwrap_or_else(PoisonError::into_inner).iter().filter_map(Weak::upgrade).filter(|o| !Arc::ptr_eq(o, &self.inner)).collect();
        let used = |o: &Arc<Inner>| o.pixels.try_lock().map(|p| p.used).unwrap_or(u64::MAX);
        others.sort_by_key(used);
        for o in others {
            if HELD.load(Ordering::Relaxed) <= BUDGET {
                break;
            }
            // (a still that is locked is decoding: it is in use)
            if let Ok(mut p) = o.pixels.try_lock()
                && p.used.saturating_add(RECENT) < now
            {
                p.levels = Default::default();
                p.recount();
            }
        }
    }
}

impl MediaSource for LazyStill {
    fn info(&self) -> &MediaInfo {
        &self.info
    }

    fn video_frame(&self, req: FrameRequest) -> Result<Arc<VideoFrame>> {
        let now = CLOCK.fetch_add(1, Ordering::Relaxed).saturating_add(1);
        let f = self.frame(level_for(req.scale, self.size), now)?;
        self.trim(now);
        Ok(f)
    }

    fn audio(&self, _start: i64, _frames: usize, _sample_rate: u32) -> Result<filmcraft_frame::AudioBuffer> {
        Err(MediaError::NoStream("audio"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PNG of `w` x `h` pixels: a gradient, opaque unless `alpha`.
    fn png(w: u32, h: u32, alpha: bool) -> Arc<[u8]> {
        let img = image::RgbaImage::from_fn(w, h, |x, y| {
            image::Rgba([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8, if alpha { (x % 255) as u8 } else { 255 }])
        });
        let mut out = std::io::Cursor::new(Vec::new());
        if alpha {
            image::DynamicImage::ImageRgba8(img).write_to(&mut out, image::ImageFormat::Png).unwrap();
        } else {
            image::DynamicImage::ImageRgba8(img).to_rgb8().write_to(&mut out, image::ImageFormat::Png).unwrap();
        }
        out.into_inner().into()
    }

    fn at(s: &LazyStill, scale: f32) -> Arc<VideoFrame> {
        s.video_frame(FrameRequest { time: filmcraft_time::Tick::ZERO, scale }).unwrap()
    }

    #[test]
    fn small_stills_and_unreadable_files_are_left_to_the_eager_decoder() {
        assert!(open("small.png", &png(64, 64, false)).is_none());
        assert!(open("junk.png", &Arc::from(&b"not a picture at all"[..])).is_none());
        assert!(open("empty.png", &Arc::from(&b""[..])).is_none());
    }

    #[test]
    fn a_large_still_reports_its_picture_without_decoding_it() {
        let before = held_bytes();
        let s = open("big.png", &png(2000, 1200, false)).expect("2.4 megapixels");
        let v = s.info().video.as_ref().unwrap();
        assert_eq!((v.width, v.height, v.has_alpha), (2000, 1200, false));
        assert_eq!(s.info().kind, crate::MediaKind::Still);
        // nothing of this still is decoded yet (other tests' stills may come and go meanwhile)
        assert_eq!(s.inner.pixels.lock().unwrap().bytes, 0);
        let _ = before;
        assert!(open("alpha.png", &png(2000, 1200, true)).unwrap().info().video.as_ref().unwrap().has_alpha);
    }

    #[test]
    fn the_whole_picture_is_what_the_eager_decoder_makes() {
        let bytes = png(2000, 1200, true);
        let s = open("big.png", &bytes).unwrap();
        let eager = crate::still::StillSource::decode("big.png", &bytes).unwrap();
        let (a, b) = (at(&s, 1.0), eager.video_frame(FrameRequest::full(filmcraft_time::Tick::ZERO)).unwrap());
        assert_eq!((a.width, a.height), (b.width, b.height));
        assert_eq!(a.to_rgba8(), b.to_rgba8());
        // above half scale the whole picture is still what comes back
        assert_eq!(at(&s, 0.6).width, 2000);
    }

    #[test]
    fn reduced_scales_get_reduced_pictures() {
        let s = open("big.png", &png(2000, 1200, false)).unwrap();
        for (scale, w, h) in [(0.5, 1000, 600), (0.3, 1000, 600), (0.25, 500, 300), (0.2, 500, 300), (0.125, 250, 150), (0.01, 250, 150)] {
            let f = at(&s, scale);
            assert_eq!((f.width, f.height), (w, h), "scale {scale}");
        }
        // asked for at a reduced scale only: the whole picture was not kept
        assert!(s.inner.pixels.lock().unwrap().levels[0].is_none());
        assert_eq!(at(&s, f32::NAN).width, 2000);
        assert!(s.inner.pixels.lock().unwrap().levels[0].is_some());
    }

    #[test]
    fn a_half_size_picture_is_the_linear_mean_of_its_blocks() {
        // a black / white checkerboard halves to linear 50 % grey (sRGB 188), not code 128
        let board: Vec<u8> = (0..4 * 4).flat_map(|i| if (i % 4 + i / 4) % 2 == 0 { [255, 255, 255, 255] } else { [0, 0, 0, 255] }).collect();
        let h = halved(&Arc::new(VideoFrame::rgba8(4, 4, board)));
        assert_eq!((h.width, h.height), (2, 2));
        assert!(
            h.to_rgba8().as_chunks::<4>().0.iter().all(|p| p[3] == 255 && (186..=189).contains(&p[0]) && p[0] == p[1] && p[1] == p[2]),
            "{:?}",
            h.to_rgba8()
        );
        // transparent pixels do not darken their neighbours: red beside nothing stays red
        let mixed: Vec<u8> = [[255, 0, 0, 255], [9, 9, 9, 0], [9, 9, 9, 0], [255, 0, 0, 255]].concat();
        let m = halved(&Arc::new(VideoFrame::rgba8(2, 2, mixed)));
        assert_eq!(m.to_rgba8(), vec![255, 0, 0, 128]);
        // too small to halve, and a short buffer: handed back, never a panic
        assert_eq!(halved(&Arc::new(VideoFrame::rgba8(1, 5, vec![7; 20]))).height, 5);
        let short = VideoFrame { data: PixelData::Rgba8(Arc::new(vec![7; 20])), ..VideoFrame::rgba8(1, 1, vec![0; 4]) };
        let short = VideoFrame { width: 8, height: 8, ..short };
        assert_eq!(halved(&Arc::new(short)).width, 8);
    }

    #[test]
    fn levels_follow_the_scale_and_stop_at_small_pictures() {
        assert_eq!(level_for(1.0, (4000, 3000)), 0);
        assert_eq!(level_for(0.51, (4000, 3000)), 0);
        assert_eq!(level_for(0.5, (4000, 3000)), 1);
        assert_eq!(level_for(0.25, (4000, 3000)), 2);
        assert_eq!(level_for(0.0, (4000, 3000)), 3);
        assert_eq!(level_for(-1.0, (4000, 3000)), 3);
        assert_eq!(level_for(f32::NAN, (4000, 3000)), 0);
        // a 200-pixel side is not reduced under 64
        assert_eq!(level_for(0.0, (20_000, 200)), 1);
    }

    #[test]
    fn a_dropped_still_gives_its_bytes_back() {
        let s = open("big.png", &png(2000, 1200, false)).unwrap();
        let _ = at(&s, 1.0);
        let mine = s.inner.pixels.lock().unwrap().bytes;
        assert_eq!(mine, 2000 * 1200 * 4);
        assert!(held_bytes() >= mine);
        // giving the pixels up by hand is what `trim` does to a still that was not used lately
        let mut p = s.inner.pixels.lock().unwrap();
        p.levels = Default::default();
        p.recount();
        assert_eq!(p.bytes, 0);
    }
}
