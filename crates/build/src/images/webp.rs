//! Still WebP encoding through libwebp (`libwebp-sys`, statically linked).
//!
//! The one unsafe block calls libwebp's advanced API the same way
//! `WebPEncodeRGB`/`WebPEncodeRGBA` do: default preset, lossy at the given
//! quality, method 4, alpha (when present) compressed losslessly. Every
//! input is checked before it reaches C, and the picture and the output
//! buffer are freed on every path. No Rust code runs inside a libwebp call
//! (the writer callback is libwebp's own `WebPMemoryWrite`), so nothing can
//! unwind across the FFI boundary.

use libwebp_sys as sys;

/// libwebp's version as `major.minor.patch` (for example `1.6.0`).
pub fn libwebp_version() -> String {
    // SAFETY: takes no arguments and returns a packed integer.
    let v = unsafe { sys::WebPGetEncoderVersion() };
    format!("{}.{}.{}", (v >> 16) & 0xff, (v >> 8) & 0xff, v & 0xff)
}

/// Pixel layout of the buffer handed to [`encode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    Rgb,
    Rgba,
}

impl Layout {
    fn bytes_per_pixel(self) -> usize {
        match self {
            Layout::Rgb => 3,
            Layout::Rgba => 4,
        }
    }
}

/// Frees the picture and the memory writer when dropped, so every return
/// path (including an early `?`) releases what libwebp allocated.
struct Guard {
    pic: sys::WebPPicture,
    out: sys::WebPMemoryWriter,
}

impl Drop for Guard {
    fn drop(&mut self) {
        // SAFETY: both were initialized by their libwebp init functions
        // before the guard was built; each free is safe on an empty struct
        // and is called exactly once.
        unsafe {
            sys::WebPPictureFree(&mut self.pic);
            sys::WebPMemoryWriterClear(&mut self.out);
        }
    }
}

/// Encode tightly packed 8-bit `pixels` (`width * height` pixels in
/// `layout`, rows top to bottom) as a lossy still WebP at `quality`
/// (0 to 100).
pub fn encode(pixels: &[u8], layout: Layout, width: u32, height: u32, quality: f32) -> Result<Vec<u8>, String> {
    if width == 0 || height == 0 || width > sys::WEBP_MAX_DIMENSION || height > sys::WEBP_MAX_DIMENSION {
        return Err(format!("webp: {width}x{height} is outside 1..={} px per side", sys::WEBP_MAX_DIMENSION));
    }
    if !quality.is_finite() || !(0.0..=100.0).contains(&quality) {
        return Err(format!("webp: quality {quality} is outside 0..=100"));
    }
    // Both sides are at most 16383, so these products fit in i32 and usize.
    let stride = width as usize * layout.bytes_per_pixel();
    let expected = stride * height as usize;
    if pixels.len() != expected {
        return Err(format!("webp: {} bytes for a {width}x{height} {layout:?} image (expected {expected})", pixels.len()));
    }
    let stride = i32::try_from(stride).map_err(|_| "webp: row too long".to_string())?;

    // SAFETY: each struct is filled by its libwebp init function before
    // use and the init results are checked; the import reads exactly
    // `height` rows of `stride` bytes from `pixels`, whose length was
    // checked above; the encoder writes only through `WebPMemoryWrite` into
    // `out`, which the guard frees; the output is copied into a Vec before
    // the guard drops, and only when libwebp reported success and a
    // non-null buffer.
    unsafe {
        let mut config = std::mem::MaybeUninit::<sys::WebPConfig>::uninit();
        if sys::WebPConfigInitInternal(
            config.as_mut_ptr(),
            sys::WebPPreset::WEBP_PRESET_DEFAULT,
            quality,
            sys::WEBP_ENCODER_ABI_VERSION as _,
        ) == 0
        {
            return Err("webp: config (library version mismatch)".into());
        }
        let mut config = config.assume_init();
        config.lossless = 0;
        config.alpha_compression = 1;
        if sys::WebPValidateConfig(&config) == 0 {
            return Err("webp: invalid config".into());
        }

        let mut pic = std::mem::MaybeUninit::<sys::WebPPicture>::uninit();
        if sys::WebPPictureInitInternal(pic.as_mut_ptr(), sys::WEBP_ENCODER_ABI_VERSION as _) == 0 {
            return Err("webp: picture (library version mismatch)".into());
        }
        let mut out = std::mem::MaybeUninit::<sys::WebPMemoryWriter>::uninit();
        sys::WebPMemoryWriterInit(out.as_mut_ptr());
        let mut g = Guard { pic: pic.assume_init(), out: out.assume_init() };

        g.pic.use_argb = 1;
        g.pic.width = width as i32;
        g.pic.height = height as i32;
        let imported = match layout {
            Layout::Rgb => sys::WebPPictureImportRGB(&mut g.pic, pixels.as_ptr(), stride),
            Layout::Rgba => sys::WebPPictureImportRGBA(&mut g.pic, pixels.as_ptr(), stride),
        };
        if imported == 0 {
            return Err("webp: out of memory importing the image".into());
        }

        // The writer pointer is taken after the guard is in place, so it
        // points at the memory the guard frees.
        g.pic.writer = Some(sys::WebPMemoryWrite);
        g.pic.custom_ptr = std::ptr::addr_of_mut!(g.out).cast();
        if sys::WebPEncode(&config, &mut g.pic) == 0 {
            return Err(format!("webp: encoding failed ({:?})", g.pic.error_code));
        }
        if g.out.mem.is_null() || g.out.size == 0 {
            return Err("webp: encoder returned no data".into());
        }
        Ok(std::slice::from_raw_parts(g.out.mem, g.out.size).to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(bytes: &[u8]) -> image::RgbaImage {
        image::load_from_memory_with_format(bytes, image::ImageFormat::WebP).expect("decodes").to_rgba8()
    }

    /// Small deterministic generator (xorshift), so failures reproduce.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    #[test]
    fn links_libwebp_1_6_or_newer() {
        let v = libwebp_version();
        let parts: Vec<u32> = v.split('.').map(|p| p.parse().unwrap()).collect();
        assert!((parts[0], parts[1]) >= (1, 6), "linked libwebp {v}");
    }

    #[test]
    fn rgb_round_trips_at_size() {
        let (w, h) = (37, 21);
        let px: Vec<u8> = (0..w * h * 3).map(|i| (i * 7 % 251) as u8).collect();
        let out = encode(&px, Layout::Rgb, w, h, 80.0).unwrap();
        assert_eq!(&out[..4], b"RIFF");
        assert_eq!(&out[8..12], b"WEBP");
        let img = decode(&out);
        assert_eq!(img.dimensions(), (w, h));
        assert!(img.pixels().all(|p| p.0[3] == 255), "no alpha channel for RGB input");
    }

    #[test]
    fn rgba_keeps_alpha_exactly() {
        // left half transparent, right half opaque, a ramp in the middle row
        let (w, h) = (16u32, 9u32);
        let mut px = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let a = if y == 4 { (x * 17) as u8 } else if x < w / 2 { 0 } else { 255 };
                px.extend_from_slice(&[200, 40, 90, a]);
            }
        }
        let img = decode(&encode(&px, Layout::Rgba, w, h, 75.0).unwrap());
        for (i, p) in img.pixels().enumerate() {
            assert_eq!(p.0[3], px[i * 4 + 3], "alpha at pixel {i}");
        }
    }

    #[test]
    fn quality_changes_size() {
        let (w, h) = (64, 64);
        let mut r = Rng(7);
        let px: Vec<u8> = (0..w * h * 3).map(|_| r.next() as u8).collect();
        let low = encode(&px, Layout::Rgb, w, h, 10.0).unwrap();
        let high = encode(&px, Layout::Rgb, w, h, 95.0).unwrap();
        assert!(low.len() < high.len(), "{} vs {}", low.len(), high.len());
    }

    #[test]
    fn same_input_same_bytes() {
        let px: Vec<u8> = (0..30 * 10 * 4).map(|i| (i % 256) as u8).collect();
        assert_eq!(encode(&px, Layout::Rgba, 30, 10, 75.0).unwrap(), encode(&px, Layout::Rgba, 30, 10, 75.0).unwrap());
    }

    #[test]
    fn edge_dimensions_encode() {
        for (w, h) in [(1, 1), (1, 2), (2, 1), (3, 5), (17, 1), (1, 17), (WEBP_MAX, 1), (1, WEBP_MAX)] {
            for layout in [Layout::Rgb, Layout::Rgba] {
                let px = vec![128u8; w as usize * h as usize * layout.bytes_per_pixel()];
                let out = encode(&px, layout, w, h, 75.0).unwrap_or_else(|e| panic!("{w}x{h} {layout:?}: {e}"));
                assert_eq!(decode(&out).dimensions(), (w, h));
            }
        }
    }

    const WEBP_MAX: u32 = sys::WEBP_MAX_DIMENSION;

    #[test]
    fn rejects_bad_dimensions() {
        for (w, h) in [(0, 1), (1, 0), (0, 0), (WEBP_MAX + 1, 1), (1, WEBP_MAX + 1), (u32::MAX, u32::MAX)] {
            let e = encode(&[0; 4], Layout::Rgba, w, h, 75.0).unwrap_err();
            assert!(e.contains("outside"), "{w}x{h}: {e}");
        }
    }

    #[test]
    fn rejects_buffer_length_mismatch() {
        for len in [0, 11, 13, 4 * 4 * 3 - 1] {
            assert!(encode(&vec![0; len], Layout::Rgb, 2, 2, 75.0).is_err(), "len {len}");
        }
        assert!(encode(&[0; 16], Layout::Rgb, 2, 2, 75.0).is_err(), "longer than needed");
        assert!(encode(&[0; 12], Layout::Rgba, 2, 2, 75.0).is_err(), "RGB-sized buffer for RGBA");
        // the largest picture with a 1-byte buffer must fail before any read
        assert!(encode(&[0], Layout::Rgba, WEBP_MAX, WEBP_MAX, 75.0).is_err());
    }

    #[test]
    fn rejects_bad_quality() {
        for q in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.5, 100.5] {
            assert!(encode(&[0; 3], Layout::Rgb, 1, 1, q).is_err(), "quality {q}");
        }
        for q in [0.0, 100.0] {
            assert!(encode(&[0; 3], Layout::Rgb, 1, 1, q).is_ok(), "quality {q}");
        }
    }

    /// Random sizes, layouts, alpha patterns and qualities; every output
    /// must decode to the input size and keep fully transparent and fully
    /// opaque pixels as they were. `WEBP_FUZZ_ITERS` raises the count.
    #[test]
    fn fuzz_random_inputs() {
        let iters: u64 = std::env::var("WEBP_FUZZ_ITERS").ok().and_then(|v| v.parse().ok()).unwrap_or(200);
        let mut r = Rng(0x9e37_79b9_7f4a_7c15);
        for i in 0..iters {
            let (wmax, hmax) = (if r.below(8) == 0 { 300 } else { 40 }, if r.below(8) == 0 { 300 } else { 40 });
            let w = 1 + r.below(wmax) as u32;
            let h = 1 + r.below(hmax) as u32;
            let layout = if r.below(2) == 0 { Layout::Rgb } else { Layout::Rgba };
            let q = r.below(101) as f32;
            let alpha_mode = r.below(4); // 0 random, 1 all 0, 2 all 255, 3 binary
            let n = w as usize * h as usize;
            let mut px = Vec::with_capacity(n * layout.bytes_per_pixel());
            for _ in 0..n {
                let v = r.next();
                px.extend_from_slice(&[v as u8, (v >> 8) as u8, (v >> 16) as u8]);
                if layout == Layout::Rgba {
                    px.push(match alpha_mode {
                        0 => (v >> 24) as u8,
                        1 => 0,
                        2 => 255,
                        _ => if (v >> 24) & 1 == 0 { 0 } else { 255 },
                    });
                }
            }
            // a wrong length must be refused, never read past
            let short = &px[..px.len() - 1];
            assert!(encode(short, layout, w, h, q).is_err(), "iter {i}: short buffer accepted");
            let out = encode(&px, layout, w, h, q).unwrap_or_else(|e| panic!("iter {i} {w}x{h} {layout:?} q{q}: {e}"));
            let img = decode(&out);
            assert_eq!(img.dimensions(), (w, h), "iter {i}");
            if layout == Layout::Rgba {
                for (j, p) in img.pixels().enumerate() {
                    let a = px[j * 4 + 3];
                    if a == 0 || a == 255 {
                        assert_eq!(p.0[3], a, "iter {i} {w}x{h} q{q}: alpha at {j}");
                    }
                }
            }
        }
    }
}
