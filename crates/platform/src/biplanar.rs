//! Biplanar 4:2:0 pictures (NV12, and P010 with its 10-bit samples in the high bits of 16-bit
//! words) as read back from a decoder surface, into the planar [`PixelData::Yuv8`] /
//! [`PixelData::Yuv16`] frames the rest of EpsiCut uses. Safe code, tested on every target.
//!
//! This is the one place the Windows backend copies pixels (surface to staging texture to these
//! planes). A zero-copy path would replace it, not the decoder around it.

use std::sync::Arc;

use epsicut_frame::{Chroma, PixelData, VideoFrame, pool};

/// A mapped biplanar surface: a luma plane followed by interleaved chroma, `stride` bytes per row.
pub struct Biplanar<'a> {
    pub luma: &'a [u8],
    pub chroma: &'a [u8],
    pub stride: usize,
    /// Rows in the luma plane (the surface height, which may exceed the picture's).
    pub rows: usize,
}

/// Where the picture is in the surface and how its samples are stored.
#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    /// Output rectangle in luma samples: x, y, width, height.
    pub crop: (u32, u32, u32, u32),
    /// Significant bits per sample: 8 (NV12) or 10 (P010).
    pub bits: u32,
    pub color: epsicut_color::ColorInfo,
    pub par: (u32, u32),
}

fn row(plane: &[u8], stride: usize, y: usize, x: usize, len: usize) -> Result<&[u8], String> {
    let start = y.checked_mul(stride).and_then(|s| s.checked_add(x)).ok_or("plane offset overflows")?;
    plane.get(start..start.checked_add(len).ok_or("plane offset overflows")?).ok_or_else(|| "plane row out of range".to_string())
}

/// Copy the picture at `g.crop` out of `src` into a planar 4:2:0 frame (deinterleaving chroma,
/// shifting 10-bit samples down from the high bits). Errors when the surface is smaller than the
/// rectangle needs.
pub fn to_frame(src: &Biplanar, g: &Geometry) -> Result<VideoFrame, String> {
    let (cx, cy, w, h) = g.crop;
    let (ox, oy, w, h) = (cx as usize, cy as usize, w as usize, h as usize);
    if w == 0 || h == 0 || !matches!(g.bits, 8 | 10) {
        return Err(format!("cannot read a {w}x{h} picture of {} bits", g.bits));
    }
    let bps = if g.bits > 8 { 2 } else { 1 };
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let (cox, coy) = (ox / 2, oy / 2);
    let needs_rows = oy.checked_add(h).ok_or("picture size overflows")?;
    let needs_cols = ox.checked_add(w).and_then(|c| c.checked_mul(bps)).ok_or("picture size overflows")?;
    if src.rows < needs_rows || src.stride < needs_cols || src.stride < (cox + cw) * 2 * bps {
        return Err(format!("decoded surface ({} rows, {} byte rows) is smaller than the {w}x{h} picture at {ox},{oy}", src.rows, src.stride));
    }
    let data = if bps == 1 {
        let mut yp = pool::take_u8(w * h);
        for y in 0..h {
            yp.extend_from_slice(row(src.luma, src.stride, oy + y, ox, w)?);
        }
        let (mut u, mut v) = (pool::take_u8(cw * ch), pool::take_u8(cw * ch));
        for y in 0..ch {
            // two passes over the row: each is a simple strided copy the compiler vectorises
            let pairs = row(src.chroma, src.stride, coy + y, cox * 2, cw * 2)?.as_chunks::<2>().0;
            u.extend(pairs.iter().map(|p| p[0]));
            v.extend(pairs.iter().map(|p| p[1]));
        }
        PixelData::Yuv8 { planes: [Arc::new(yp), Arc::new(u), Arc::new(v)], chroma: Chroma::C420, alpha: None }
    } else {
        let shift = 16 - g.bits;
        let mut yp = pool::take_u16(w * h);
        for y in 0..h {
            yp.extend(row(src.luma, src.stride, oy + y, ox * 2, w * 2)?.as_chunks::<2>().0.iter().map(|b| u16::from_le_bytes([b[0], b[1]]) >> shift));
        }
        let (mut u, mut v) = (pool::take_u16(cw * ch), pool::take_u16(cw * ch));
        for y in 0..ch {
            let quads = row(src.chroma, src.stride, coy + y, cox * 4, cw * 4)?.as_chunks::<4>().0;
            u.extend(quads.iter().map(|q| u16::from_le_bytes([q[0], q[1]]) >> shift));
            v.extend(quads.iter().map(|q| u16::from_le_bytes([q[2], q[3]]) >> shift));
        }
        PixelData::Yuv16 { planes: [Arc::new(yp), Arc::new(u), Arc::new(v)], chroma: Chroma::C420, bits: g.bits, alpha: None }
    };
    Ok(VideoFrame { width: w as u32, height: h as u32, data, color: g.color, par: g.par, pts: epsicut_time::Tick::ZERO })
}

/// Rounded mean of a 2 x 2 block of 8-bit samples.
fn mean4(a: u8, b: u8, c: u8, d: u8) -> u8 {
    ((u16::from(a) + u16::from(b) + u16::from(c) + u16::from(d) + 2) >> 2) as u8
}

/// Rounded mean of a 2 x 2 block of samples of at most 14 bits (the sum fits 16 bits).
fn mean4_16(a: u16, b: u16, c: u16, d: u16) -> u16 {
    (a.saturating_add(b).saturating_add(c).saturating_add(d).saturating_add(2)) >> 2
}

/// A little-endian 16-bit sample at byte `i` of `b`, shifted down from the high bits (0 past the end).
fn word(b: &[u8], i: usize, shift: u32) -> u16 {
    match b.get(i..i + 2) {
        Some(&[lo, hi]) => u16::from_le_bytes([lo, hi]) >> shift,
        _ => 0,
    }
}

/// [`to_frame`] at half the size on each axis, for draft playback at reduced resolution: every
/// sample is the mean of the 2 x 2 block it covers (as [`VideoFrame::box_decimated`] takes it,
/// over Y'CbCr codes). A quarter of the samples are written, so the conversion, everything done
/// to the frame afterwards (rotation, upload) and the memory it takes cost a quarter too.
/// Pictures under 4 samples on a side come out whole.
pub fn to_frame_half(src: &Biplanar, g: &Geometry) -> Result<VideoFrame, String> {
    let (cx, cy, w, h) = g.crop;
    let (ox, oy, w, h) = (cx as usize, cy as usize, w as usize, h as usize);
    if w < 4 || h < 4 || !matches!(g.bits, 8 | 10) {
        return to_frame(src, g);
    }
    let bps = if g.bits > 8 { 2 } else { 1 };
    // the source picture's chroma plane, and where it starts
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let (cox, coy) = (ox / 2, oy / 2);
    let needs_rows = oy.checked_add(h).ok_or("picture size overflows")?;
    let needs_cols = ox.checked_add(w).and_then(|c| c.checked_mul(bps)).ok_or("picture size overflows")?;
    if src.rows < needs_rows || src.stride < needs_cols || src.stride < (cox + cw) * 2 * bps {
        return Err(format!("decoded surface ({} rows, {} byte rows) is smaller than the {w}x{h} picture at {ox},{oy}", src.rows, src.stride));
    }
    // the half-size picture and its chroma plane
    let (hw, hh) = (w / 2, h / 2);
    let (hcw, hch) = (hw.div_ceil(2), hh.div_ceil(2));
    // chroma rows 2y and 2y + 1 of the source (the last one twice where the plane ends)
    let chroma_rows = |y: usize| (coy + (2 * y).min(ch - 1), coy + (2 * y + 1).min(ch - 1));
    let data = if bps == 1 {
        let mut yp = pool::take_u8(hw * hh);
        for y in 0..hh {
            let a = row(src.luma, src.stride, oy + 2 * y, ox, hw * 2)?.as_chunks::<2>().0;
            let b = row(src.luma, src.stride, oy + 2 * y + 1, ox, hw * 2)?.as_chunks::<2>().0;
            yp.extend(a.iter().zip(b).map(|(p, q)| mean4(p[0], p[1], q[0], q[1])));
        }
        let (mut u, mut v) = (pool::take_u8(hcw * hch), pool::take_u8(hcw * hch));
        for y in 0..hch {
            let (y0, y1) = chroma_rows(y);
            // pairs of chroma samples: Cb Cr Cb Cr
            let (a, a_last) = row(src.chroma, src.stride, y0, cox * 2, cw * 2)?.as_chunks::<4>();
            let (b, b_last) = row(src.chroma, src.stride, y1, cox * 2, cw * 2)?.as_chunks::<4>();
            let whole = a.len().min(hcw);
            u.extend(a.iter().zip(b).take(whole).map(|(p, q)| mean4(p[0], p[2], q[0], q[2])));
            v.extend(a.iter().zip(b).take(whole).map(|(p, q)| mean4(p[1], p[3], q[1], q[3])));
            if whole < hcw
                && let (Some(p), Some(q)) = (a_last.first_chunk::<2>(), b_last.first_chunk::<2>())
            {
                // an odd chroma width: the last column stands for its missing neighbour too
                u.push(mean4(p[0], p[0], q[0], q[0]));
                v.push(mean4(p[1], p[1], q[1], q[1]));
            }
            u.resize((y + 1) * hcw, 128);
            v.resize((y + 1) * hcw, 128);
        }
        PixelData::Yuv8 { planes: [Arc::new(yp), Arc::new(u), Arc::new(v)], chroma: Chroma::C420, alpha: None }
    } else {
        let shift = 16 - g.bits;
        let mut yp = pool::take_u16(hw * hh);
        for y in 0..hh {
            // two 16-bit samples per output sample
            let a = row(src.luma, src.stride, oy + 2 * y, ox * 2, hw * 4)?.as_chunks::<4>().0;
            let b = row(src.luma, src.stride, oy + 2 * y + 1, ox * 2, hw * 4)?.as_chunks::<4>().0;
            yp.extend(a.iter().zip(b).map(|(p, q)| mean4_16(word(p, 0, shift), word(p, 2, shift), word(q, 0, shift), word(q, 2, shift))));
        }
        let mid = 1u16 << (g.bits - 1);
        let (mut u, mut v) = (pool::take_u16(hcw * hch), pool::take_u16(hcw * hch));
        for y in 0..hch {
            let (y0, y1) = chroma_rows(y);
            // pairs of chroma samples, 16 bits each: Cb Cr Cb Cr
            let (a, a_last) = row(src.chroma, src.stride, y0, cox * 4, cw * 4)?.as_chunks::<8>();
            let (b, b_last) = row(src.chroma, src.stride, y1, cox * 4, cw * 4)?.as_chunks::<8>();
            let whole = a.len().min(hcw);
            u.extend(a.iter().zip(b).take(whole).map(|(p, q)| mean4_16(word(p, 0, shift), word(p, 4, shift), word(q, 0, shift), word(q, 4, shift))));
            v.extend(a.iter().zip(b).take(whole).map(|(p, q)| mean4_16(word(p, 2, shift), word(p, 6, shift), word(q, 2, shift), word(q, 6, shift))));
            if whole < hcw && a_last.len() >= 4 && b_last.len() >= 4 {
                // an odd chroma width: the last column stands for its missing neighbour too
                let (pu, qu) = (word(a_last, 0, shift), word(b_last, 0, shift));
                let (pv, qv) = (word(a_last, 2, shift), word(b_last, 2, shift));
                u.push(mean4_16(pu, pu, qu, qu));
                v.push(mean4_16(pv, pv, qv, qv));
            }
            u.resize((y + 1) * hcw, mid);
            v.resize((y + 1) * hcw, mid);
        }
        PixelData::Yuv16 { planes: [Arc::new(yp), Arc::new(u), Arc::new(v)], chroma: Chroma::C420, bits: g.bits, alpha: None }
    };
    Ok(VideoFrame { width: hw as u32, height: hh as u32, data, color: g.color, par: g.par, pts: epsicut_time::Tick::ZERO })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What [`to_frame_half`] must equal: the whole picture, box-filtered by two.
    fn halved(f: &VideoFrame) -> VideoFrame {
        f.box_decimated(2).expect("a picture large enough to halve")
    }

    #[test]
    fn half_size_nv12_is_the_box_filtered_picture() {
        // odd and even sizes, a crop off the origin, a stride wider than the picture
        for (w, h, ox, oy) in [(8u32, 8u32, 0u32, 0u32), (16, 12, 2, 2), (10, 6, 0, 2), (6, 10, 4, 0), (12, 8, 0, 0)] {
            let (stride, rows) = ((ox + w + 6) as usize, (oy + h + 2) as usize);
            let luma: Vec<u8> = (0..stride * rows).map(|i| (i * 37 % 251) as u8).collect();
            let chroma: Vec<u8> = (0..stride * rows.div_ceil(2)).map(|i| (i * 29 % 239) as u8).collect();
            let src = Biplanar { luma: &luma, chroma: &chroma, stride, rows };
            let g = geometry((ox, oy, w, h), 8);
            let (whole, half) = (to_frame(&src, &g).unwrap(), to_frame_half(&src, &g).unwrap());
            let want = halved(&whole);
            assert_eq!((half.width, half.height), (w / 2, h / 2), "{w}x{h}");
            let (PixelData::Yuv8 { planes: got, .. }, PixelData::Yuv8 { planes: exp, .. }) = (&half.data, &want.data) else { panic!("8-bit") };
            assert_eq!(got[0], exp[0], "luma of {w}x{h} at {ox},{oy}");
            assert_eq!((got[1].len(), got[2].len()), (exp[1].len(), exp[2].len()), "chroma size of {w}x{h}");
            if w % 4 == 0 && h % 4 == 0 {
                assert_eq!((&got[1], &got[2]), (&exp[1], &exp[2]), "chroma of {w}x{h} at {ox},{oy}");
            }
        }
    }

    #[test]
    fn half_size_p010_is_the_box_filtered_picture() {
        let (w, h, stride, rows) = (8u32, 8u32, 24usize, 8usize);
        let words = |n: usize, k: usize| -> Vec<u8> { (0..n).flat_map(|i| ((((i * k) % 1021) as u16) << 6).to_le_bytes()).collect() };
        let (luma, chroma) = (words(stride / 2 * rows, 37), words(stride / 2 * rows / 2, 29));
        let src = Biplanar { luma: &luma, chroma: &chroma, stride, rows };
        let g = geometry((0, 0, w, h), 10);
        let (whole, half) = (to_frame(&src, &g).unwrap(), to_frame_half(&src, &g).unwrap());
        let want = halved(&whole);
        let (PixelData::Yuv16 { planes: got, bits, .. }, PixelData::Yuv16 { planes: exp, .. }) = (&half.data, &want.data) else { panic!("16-bit") };
        assert_eq!(*bits, 10);
        assert_eq!((half.width, half.height), (4, 4));
        assert_eq!((&got[0], &got[1], &got[2]), (&exp[0], &exp[1], &exp[2]));
    }

    #[test]
    fn half_size_of_a_tiny_or_short_surface_does_not_panic() {
        let luma = vec![7u8; 64];
        let chroma = vec![9u8; 32];
        let src = Biplanar { luma: &luma, chroma: &chroma, stride: 8, rows: 8 };
        // under 4 samples on a side: the whole picture
        let tiny = to_frame_half(&src, &geometry((0, 0, 2, 2), 8)).unwrap();
        assert_eq!((tiny.width, tiny.height), (2, 2));
        for crop in [(0, 0, 9, 8), (0, 0, 8, 9), (4, 0, 8, 8), (u32::MAX, 0, 8, 8)] {
            assert!(to_frame_half(&src, &geometry(crop, 8)).is_err(), "{crop:?}");
        }
        let short = Biplanar { luma: &luma, chroma: &chroma[..8], stride: 8, rows: 8 };
        assert!(to_frame_half(&short, &geometry((0, 0, 8, 8), 8)).is_err());
        assert_eq!(to_frame_half(&src, &geometry((0, 0, 8, 8), 8)).unwrap().width, 4);
    }

    fn geometry(crop: (u32, u32, u32, u32), bits: u32) -> Geometry {
        Geometry { crop, bits, color: epsicut_color::ColorInfo::REC709, par: (1, 1) }
    }

    #[test]
    fn nv12_is_deinterleaved_and_cropped() {
        // 8x6 surface, stride 10; the picture is the 4x4 at (2, 2)
        let (stride, rows) = (10usize, 6usize);
        let luma: Vec<u8> = (0..stride * rows).map(|i| i as u8).collect();
        let chroma: Vec<u8> = (0..stride * rows / 2).map(|i| 100 + i as u8).collect();
        let f = to_frame(&Biplanar { luma: &luma, chroma: &chroma, stride, rows }, &geometry((2, 2, 4, 4), 8)).unwrap();
        let PixelData::Yuv8 { planes, chroma: c, .. } = &f.data else { panic!("8-bit") };
        assert_eq!(*c, Chroma::C420);
        assert_eq!(&planes[0][..], &[22, 23, 24, 25, 32, 33, 34, 35, 42, 43, 44, 45, 52, 53, 54, 55]);
        // chroma rows 1..3, pairs from byte 2: row 1 starts at 10, row 2 at 20
        assert_eq!(&planes[1][..], &[112, 114, 122, 124]);
        assert_eq!(&planes[2][..], &[113, 115, 123, 125]);
    }

    #[test]
    fn p010_shifts_down_from_the_high_bits() {
        let (stride, rows) = (8usize, 2usize);
        let mut luma = vec![0u8; stride * rows];
        for (i, s) in luma.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            *s = (((i as u16) + 1) << 6).to_le_bytes();
        }
        let mut chroma = vec![0u8; stride];
        for (i, s) in chroma.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            *s = ((500 + i as u16) << 6).to_le_bytes();
        }
        let f = to_frame(&Biplanar { luma: &luma, chroma: &chroma, stride, rows }, &geometry((0, 0, 4, 2), 10)).unwrap();
        let PixelData::Yuv16 { planes, bits, .. } = &f.data else { panic!("16-bit") };
        assert_eq!(*bits, 10);
        assert_eq!(&planes[0][..], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!((&planes[1][..], &planes[2][..]), (&[500, 502][..], &[501, 503][..]));
    }

    #[test]
    fn surfaces_smaller_than_the_picture_are_errors() {
        let luma = vec![0u8; 64];
        let chroma = vec![0u8; 32];
        let src = Biplanar { luma: &luma, chroma: &chroma, stride: 8, rows: 8 };
        for crop in [(0, 0, 9, 8), (0, 0, 8, 9), (4, 0, 8, 8), (0, 0, 0, 8), (u32::MAX, 0, 8, 8), (0, 0, 8, 8)] {
            let r = to_frame(&src, &geometry(crop, 8));
            assert_eq!(r.is_ok(), crop == (0, 0, 8, 8), "{crop:?}");
        }
        assert!(to_frame(&src, &geometry((0, 0, 4, 4), 12)).is_err());
        // a short chroma plane is an error, not a panic
        let short = Biplanar { luma: &luma, chroma: &chroma[..8], stride: 8, rows: 8 };
        assert!(to_frame(&short, &geometry((0, 0, 8, 8), 8)).is_err());
    }
}
