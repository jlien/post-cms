//! AVIF sources: the container is read with `avif-parse` and the AV1 data
//! decoded with `rav1d` (the Rust port of dav1d, built without assembly), so
//! decoding needs no system library or C toolchain.
//!
//! The size comes from the AV1 sequence header, so the pixel limit is checked
//! before any pixel is decoded, and the decoder's own frame size limit is set
//! to the same value. Decoding runs on the calling thread. YUV is converted
//! to 8-bit RGB(A) with the matrix and range the sequence header declares
//! (BT.601 when unspecified), chroma upsampled by nearest sample. The alpha
//! item, when present, becomes the alpha channel (premultiplied alpha is
//! undone). Transforms in the container (`irot`, `imir`, `clap`), grids,
//! and image sequences are not supported: such a file fails to parse or
//! keeps its stored orientation.

use std::ptr::NonNull;

use image::DynamicImage;
use rav1d::include::dav1d::data::Dav1dData;
use rav1d::include::dav1d::dav1d::{Dav1dContext, Dav1dLogger, Dav1dSettings};
use rav1d::include::dav1d::headers::{
    DAV1D_MC_BT2020_NCL, DAV1D_MC_BT709, DAV1D_MC_FCC, DAV1D_MC_IDENTITY, DAV1D_MC_SMPTE240, DAV1D_PIXEL_LAYOUT_I400,
    DAV1D_PIXEL_LAYOUT_I420, DAV1D_PIXEL_LAYOUT_I422,
};
use rav1d::include::dav1d::picture::Dav1dPicture;
use rav1d::src::lib as dav1d;

/// Size (from the AV1 sequence header) and whether there is an alpha item,
/// without decoding a pixel; refuses anything over `max_pixels`.
pub(super) fn probe(bytes: &[u8], max_pixels: u64) -> Result<(u32, u32, bool), String> {
    let data = avif_parse::read_avif(&mut &bytes[..]).map_err(|e| format!("avif: {e}"))?;
    let meta = data.primary_item_metadata().map_err(|e| format!("avif: {e}"))?;
    let (w, h) = (meta.max_frame_width.get(), meta.max_frame_height.get());
    if w as u64 * h as u64 > max_pixels {
        return Err(format!("{w}x{h} is over max_pixels ({max_pixels})"));
    }
    if let Some(a) = data.alpha_item_metadata().map_err(|e| format!("avif alpha: {e}"))? {
        let (aw, ah) = (a.max_frame_width.get(), a.max_frame_height.get());
        if aw as u64 * ah as u64 > max_pixels {
            return Err(format!("alpha {aw}x{ah} is over max_pixels ({max_pixels})"));
        }
    }
    Ok((w, h, data.alpha_item.is_some()))
}

/// Decode to 8-bit RGB, or RGBA when there is an alpha item, under the same
/// pixel limit as [`probe`]. A decoder panic is an error, not a crash.
pub(super) fn decode(bytes: &[u8], max_pixels: u64) -> Result<DynamicImage, String> {
    std::panic::catch_unwind(|| decode_inner(bytes, max_pixels)).unwrap_or_else(|_| Err("avif: decoder panicked".into()))
}

fn decode_inner(bytes: &[u8], max_pixels: u64) -> Result<DynamicImage, String> {
    let (w, h, _) = probe(bytes, max_pixels)?;
    let data = avif_parse::read_avif(&mut &bytes[..]).map_err(|e| format!("avif: {e}"))?;
    let color = decode_av1(&data.primary_item, max_pixels)?;
    if (color.width, color.height) != (w, h) {
        return Err(format!("avif: frame is {}x{}, header says {w}x{h}", color.width, color.height));
    }
    let Some(alpha_obu) = data.alpha_item.as_deref() else {
        let img = image::RgbImage::from_raw(w, h, color.rgb).ok_or("avif: buffer size")?;
        return Ok(DynamicImage::ImageRgb8(img));
    };
    let alpha = decode_av1(alpha_obu, max_pixels)?;
    if (alpha.width, alpha.height) != (w, h) {
        return Err("avif: alpha plane size differs from the image".into());
    }
    let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
    for (px, a) in color.rgb.chunks_exact(3).zip(alpha.luma.iter()) {
        let a = *a;
        if data.premultiplied_alpha && a < 255 {
            for c in px {
                rgba.push(if a == 0 { 0 } else { ((*c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8 });
            }
        } else {
            rgba.extend_from_slice(px);
        }
        rgba.push(a);
    }
    let img = image::RgbaImage::from_raw(w, h, rgba).ok_or("avif: buffer size")?;
    Ok(DynamicImage::ImageRgba8(img))
}

struct Frame {
    width: u32,
    height: u32,
    /// 8-bit RGB, row-major.
    rgb: Vec<u8>,
    /// 8-bit full-range luma (the alpha plane, for an alpha item).
    luma: Vec<u8>,
}

/// An open rav1d decoder, closed on drop.
struct Decoder(Option<Dav1dContext>);

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: the context came from dav1d_open and is closed once here.
        unsafe { dav1d::dav1d_close(Some(NonNull::from(&mut self.0))) }
    }
}

/// A decoded picture, released on drop.
struct Picture(Dav1dPicture);

impl Drop for Picture {
    fn drop(&mut self) {
        // SAFETY: the picture was filled by dav1d_get_picture.
        unsafe { dav1d::dav1d_picture_unref(Some(NonNull::from(&mut self.0))) }
    }
}

/// Input data, released on drop (a no-op once the decoder consumed it).
struct Data(Dav1dData);

impl Drop for Data {
    fn drop(&mut self) {
        // SAFETY: created by dav1d_data_create; unref is safe after consumption.
        unsafe { dav1d::dav1d_data_unref(Some(NonNull::from(&mut self.0))) }
    }
}

fn eagain(r: i32) -> bool {
    r == -libc::EAGAIN
}

/// Decode one AV1 still (an AVIF item) on the calling thread.
fn decode_av1(obu: &[u8], max_pixels: u64) -> Result<Frame, String> {
    if obu.is_empty() {
        return Err("avif: empty item".into());
    }
    let mut s = std::mem::MaybeUninit::<Dav1dSettings>::uninit();
    // SAFETY: dav1d_default_settings writes a complete Dav1dSettings.
    let mut s = unsafe {
        dav1d::dav1d_default_settings(NonNull::new(s.as_mut_ptr()).unwrap());
        s.assume_init()
    };
    s.n_threads = 1;
    s.max_frame_delay = 1;
    s.all_layers = 0;
    s.frame_size_limit = max_pixels.min(u32::MAX as u64) as u32;
    // SAFETY: no callback, so nothing is ever called.
    s.logger = unsafe { Dav1dLogger::new(None, None) };
    let mut dec = Decoder(None);
    // SAFETY: both pointers are valid for the call.
    let r = unsafe { dav1d::dav1d_open(Some(NonNull::from(&mut dec.0)), Some(NonNull::from(&mut s))) };
    if r.0 < 0 || dec.0.is_none() {
        return Err(format!("avif: decoder open failed ({})", r.0));
    }
    let mut data = Data(Dav1dData::default());
    // SAFETY: dav1d_data_create allocates `obu.len()` bytes and returns them.
    let buf = unsafe { dav1d::dav1d_data_create(Some(NonNull::from(&mut data.0)), obu.len()) };
    if buf.is_null() {
        return Err("avif: out of memory".into());
    }
    // SAFETY: buf points to obu.len() writable bytes owned by `data`.
    unsafe { std::ptr::copy_nonoverlapping(obu.as_ptr(), buf, obu.len()) };
    let mut pic = Picture(Dav1dPicture::default());
    // A still is one frame; bound the loop in case a stream never yields one.
    for _ in 0..64 {
        if data.0.sz > 0 {
            // SAFETY: the context is open; data is a valid Dav1dData.
            let r = unsafe { dav1d::dav1d_send_data(dec.0, Some(NonNull::from(&mut data.0))) };
            if r.0 < 0 && !eagain(r.0) {
                return Err(format!("avif: decode error ({})", r.0));
            }
        }
        // SAFETY: the context is open; pic is a valid out pointer.
        let r = unsafe { dav1d::dav1d_get_picture(dec.0, Some(NonNull::from(&mut pic.0))) };
        if r.0 == 0 {
            return to_frame(&pic.0);
        }
        if !eagain(r.0) {
            return Err(format!("avif: decode error ({})", r.0));
        }
    }
    Err("avif: no frame decoded".into())
}

/// (Kr, Kb) for a matrix-coefficients code; `None` for identity (GBR).
fn matrix(mc: u32) -> Option<(f32, f32)> {
    Some(match mc {
        DAV1D_MC_IDENTITY => return None,
        DAV1D_MC_BT709 => (0.2126, 0.0722),
        DAV1D_MC_FCC => (0.30, 0.11),
        DAV1D_MC_SMPTE240 => (0.212, 0.087),
        DAV1D_MC_BT2020_NCL => (0.2627, 0.0593),
        // BT.470BG, BT.601, unspecified and anything else
        _ => (0.299, 0.114),
    })
}

fn to_frame(p: &Dav1dPicture) -> Result<Frame, String> {
    let (w, h) = (p.p.w, p.p.h);
    if w <= 0 || h <= 0 {
        return Err("avif: empty frame".into());
    }
    let (w, h) = (w as usize, h as usize);
    let bpc = p.p.bpc as u32;
    if !(8..=12).contains(&bpc) {
        return Err(format!("avif: unsupported bit depth {bpc}"));
    }
    let layout = p.p.layout;
    let (ssx, ssy) = match layout {
        DAV1D_PIXEL_LAYOUT_I420 => (1, 1),
        DAV1D_PIXEL_LAYOUT_I422 => (1, 0),
        _ => (0, 0),
    };
    let mono = layout == DAV1D_PIXEL_LAYOUT_I400;
    // SAFETY: a decoded picture carries its sequence header.
    let seq = p.seq_hdr.map(|s| unsafe { s.as_ref() }).ok_or("avif: no sequence header")?;
    let full = seq.color_range != 0;
    let mtx = matrix(seq.mtrx);
    let hbd = bpc > 8;
    let max = ((1u32 << bpc) - 1) as f32;
    let plane = |i: usize| -> Result<*const u8, String> {
        p.data[i].map(|d| d.as_ptr() as *const u8).ok_or_else(|| "avif: missing plane".to_string())
    };
    let y_ptr = plane(0)?;
    let (u_ptr, v_ptr) = if mono { (y_ptr, y_ptr) } else { (plane(1)?, plane(2)?) };
    let (ys, cs) = (p.stride[0], p.stride[1]);
    // SAFETY: (x, y) are inside the plane whose stride and size rav1d reported.
    let sample = |base: *const u8, stride: isize, x: usize, y: usize| -> f32 {
        unsafe {
            let row = base.offset(stride * y as isize);
            if hbd { *(row as *const u16).add(x) as f32 } else { *row.add(x) as f32 }
        }
    };
    // to 0..1 (luma) and -0.5..0.5 (chroma)
    let scale = (1u32 << (bpc - 8)) as f32;
    let luma = |v: f32| if full { v / max } else { (v - 16.0 * scale) / (219.0 * scale) };
    let chroma = |v: f32| {
        let mid = (1u32 << (bpc - 1)) as f32;
        if full { (v - mid) / max } else { (v - mid) / (224.0 * scale) }
    };
    let to8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    let mut rgb = Vec::with_capacity(w * h * 3);
    let mut out_luma = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            let yy = luma(sample(y_ptr, ys, x, y));
            out_luma.push(to8(yy));
            if mono {
                let g = to8(yy);
                rgb.extend_from_slice(&[g, g, g]);
                continue;
            }
            let (cx, cy) = (x >> ssx, y >> ssy);
            let (u, v) = (sample(u_ptr, cs, cx, cy), sample(v_ptr, cs, cx, cy));
            let (r, g, b) = match mtx {
                None => {
                    // GBR: Y is G, U is B, V is R, all full samples
                    let f = |s: f32| if full { s / max } else { (s - 16.0 * scale) / (219.0 * scale) };
                    (f(v), yy, f(u))
                }
                Some((kr, kb)) => {
                    let (cb, cr) = (chroma(u), chroma(v));
                    let r = yy + 2.0 * (1.0 - kr) * cr;
                    let b = yy + 2.0 * (1.0 - kb) * cb;
                    let g = (yy - kr * r - kb * b) / (1.0 - kr - kb);
                    (r, g, b)
                }
            };
            rgb.extend_from_slice(&[to8(r), to8(g), to8(b)]);
        }
    }
    Ok(Frame { width: w as u32, height: h as u32, rgb, luma: out_luma })
}
