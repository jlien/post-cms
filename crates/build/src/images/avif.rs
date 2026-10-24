//! AVIF sources: the container is read with `avif-parse` and the AV1 data
//! decoded with `rav1d` (the Rust port of dav1d, built without assembly), so
//! decoding needs no system library or C toolchain.
//!
//! rav1d is called through its C API (`extern "C"` functions), where a panic
//! cannot unwind and aborts the process, and malformed input does reach
//! panicking paths in it. So AV1 data is never decoded in the build
//! process: [`decode`] runs the build binary again in a decode-only mode
//! ([`child_main`]), sends the file on stdin and reads 8-bit pixels from
//! stdout. The child has a time limit, no core dumps, a CPU limit and (on
//! Linux) a data-size limit. A child that crashes, times out or answers
//! anything but a well-formed image of the probed size is a decode error
//! for that one image, so the build carries on.
//!
//! The size comes from the AV1 sequence header, so the pixel limit is checked
//! before any pixel is decoded, and the decoder's own frame size limit is set
//! to the same value. [`Probe::memory`] estimates the peak memory of the
//! decode (child and parent) from the bit depth, chroma layout and alpha,
//! for the caller's per-image budget. YUV is converted to 8-bit RGB(A) with
//! the matrix and range the sequence header declares (BT.601 when
//! unspecified), chroma upsampled by nearest sample. The alpha item, when
//! present, becomes the alpha channel (premultiplied alpha is undone).
//! Transforms in the container (`irot`, `imir`, `clap`), grids, and image
//! sequences are not supported: such a file fails to parse or keeps its
//! stored orientation.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use avif_parse::AV1Metadata;
use image::DynamicImage;
use rav1d::include::dav1d::data::Dav1dData;
use rav1d::include::dav1d::dav1d::{Dav1dContext, Dav1dLogger, Dav1dSettings};
use rav1d::include::dav1d::headers::{
    DAV1D_MC_BT2020_NCL, DAV1D_MC_BT709, DAV1D_MC_FCC, DAV1D_MC_IDENTITY, DAV1D_MC_SMPTE240, DAV1D_PIXEL_LAYOUT_I400,
    DAV1D_PIXEL_LAYOUT_I420, DAV1D_PIXEL_LAYOUT_I422,
};
use rav1d::include::dav1d::picture::Dav1dPicture;
use rav1d::src::lib as dav1d;

/// First argument that puts the build binary in decode-only mode.
pub const CHILD_FLAG: &str = "--postcms-decode-avif";
/// Child exit code for a decode error it reported on stderr.
const CHILD_ERR: i32 = 2;
const MAGIC: &[u8; 4] = b"PAV1";
/// Largest input the child reads.
const MAX_INPUT: u64 = 1 << 30;
/// Room for the child's own code, stack and allocator on top of the decode.
const CHILD_OVERHEAD: u64 = 256 << 20;

/// What the header says, before any pixel is decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Probe {
    pub width: u32,
    pub height: u32,
    pub alpha: bool,
    /// Peak bytes the decode takes, child and parent together.
    pub memory: u64,
}

/// Size (from the AV1 sequence header), alpha and decode memory, without
/// decoding a pixel; refuses anything over `max_pixels`. Runs in the build
/// process: `avif-parse` is plain Rust, so a panic in it (a debug assertion
/// on malformed boxes, in debug builds) unwinds and is caught here.
pub(super) fn probe(bytes: &[u8], max_pixels: u64) -> Result<Probe, String> {
    std::panic::catch_unwind(|| probe_inner(bytes, max_pixels)).unwrap_or_else(|_| Err("avif: parser panicked".into()))
}

fn probe_inner(bytes: &[u8], max_pixels: u64) -> Result<Probe, String> {
    let data = avif_parse::read_avif(&mut &bytes[..]).map_err(|e| format!("avif: {e}"))?;
    let meta = data.primary_item_metadata().map_err(|e| format!("avif: {e}"))?;
    let (w, h) = (meta.max_frame_width.get(), meta.max_frame_height.get());
    if w as u64 * h as u64 > max_pixels {
        return Err(format!("{w}x{h} is over max_pixels ({max_pixels})"));
    }
    let alpha = data.alpha_item_metadata().map_err(|e| format!("avif alpha: {e}"))?;
    if let Some(a) = &alpha {
        let (aw, ah) = (a.max_frame_width.get(), a.max_frame_height.get());
        if (aw, ah) != (w, h) {
            return Err(format!("avif: alpha is {aw}x{ah}, image is {w}x{h}"));
        }
    }
    let px = w as u64 * h as u64;
    let memory = decode_memory(px, Format::of(&meta), alpha.as_ref().map(Format::of));
    Ok(Probe { width: w, height: h, alpha: alpha.is_some(), memory })
}

/// Sample format from an AV1 sequence header.
#[derive(Debug, Clone, Copy)]
pub(super) struct Format {
    pub bit_depth: u8,
    /// Horizontal and vertical chroma subsampling.
    pub subsampling: (bool, bool),
    pub monochrome: bool,
}

impl Format {
    fn of(m: &AV1Metadata) -> Format {
        Format { bit_depth: m.bit_depth, subsampling: m.chroma_subsampling, monochrome: m.monochrome }
    }
}

/// Bytes per pixel of one decoded rav1d picture, in quarter bytes: samples
/// of 1 or 2 bytes, luma plus the chroma planes the layout keeps.
fn picture_quarters(m: Format) -> u64 {
    let sample = if m.bit_depth > 8 { 2 } else { 1 };
    let chroma = match (m.monochrome, m.subsampling) {
        (true, _) => 0,
        (false, (true, true)) => 2,  // 4:2:0, two quarter planes
        (false, (true, false)) => 4, // 4:2:2, two half planes
        _ => 8,                      // 4:4:4, two full planes
    };
    sample * (4 + chroma)
}

/// Peak memory of a decode, from what each step holds at once: the alpha
/// plane (1 B/px) is decoded first, then the color picture while the
/// RGB(A) output (3 or 4 B/px) fills; then the output is held by both the
/// child and the parent while it is sent.
pub(super) fn decode_memory(px: u64, color: Format, alpha: Option<Format>) -> u64 {
    let (out, a) = if alpha.is_some() { (4 * 4, 4) } else { (3 * 4, 0) };
    // rav1d holds about PICTURE_FACTOR x the picture while decoding a still
    let pic = |m: Format| PICTURE_FACTOR * picture_quarters(m);
    let alpha_step = alpha.map_or(0, |m| pic(m) + a);
    let color_step = a + pic(color) + out;
    let send_step = a + 2 * out;
    px * alpha_step.max(color_step).max(send_step) / 4
}

/// rav1d's peak while decoding a still, as a multiple of the picture
/// (measured on rav1d 1.1, rounded up).
const PICTURE_FACTOR: u64 = 2;

/// Decode to 8-bit RGB, or RGBA when there is an alpha item, under the same
/// pixel limit as [`probe`], in a child process (`exe` with [`CHILD_FLAG`])
/// that is killed after `timeout`. Any child failure is an error.
pub(super) fn decode(bytes: &[u8], max_pixels: u64, exe: &Path, timeout: Duration) -> Result<DynamicImage, String> {
    let p = probe(bytes, max_pixels)?;
    let channels: u64 = if p.alpha { 4 } else { 3 };
    let want = p.width as u64 * p.height as u64 * channels;
    let mut cmd = Command::new(exe);
    // its own process group, so a timeout kills anything it started too
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = cmd
        .arg(CHILD_FLAG)
        .arg(max_pixels.to_string())
        .arg(p.memory.saturating_add(CHILD_OVERHEAD).to_string())
        .arg(timeout.as_secs().max(1).to_string())
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("avif: cannot start the decoder {}: {e}", exe.display()))?;
    let (stdin, stdout, stderr) = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    let started = Instant::now();
    let (status, out, err) = std::thread::scope(|s| {
        s.spawn(move || {
            if let Some(mut i) = stdin {
                let _ = i.write_all(bytes);
            }
        });
        let out = s.spawn(move || -> Result<Vec<u8>, String> {
            let mut o = stdout.ok_or("no stdout")?;
            let mut head = [0u8; 13];
            o.read_exact(&mut head).map_err(|_| "no image".to_string())?;
            let w = u32::from_le_bytes(head[4..8].try_into().unwrap());
            let h = u32::from_le_bytes(head[8..12].try_into().unwrap());
            if &head[..4] != MAGIC || (w, h) != (p.width, p.height) || head[12] as u64 != channels {
                return Err("unexpected header".into());
            }
            let mut px = Vec::with_capacity(want as usize);
            o.take(want + 1).read_to_end(&mut px).map_err(|e| e.to_string())?;
            if px.len() as u64 != want {
                return Err(format!("{} bytes of pixels, expected {want}", px.len()));
            }
            Ok(px)
        });
        let err = s.spawn(move || {
            let mut e = String::new();
            if let Some(x) = stderr {
                let _ = x.take(4096).read_to_string(&mut e);
            }
            e
        });
        let status = loop {
            match child.try_wait() {
                Ok(Some(st)) => break Ok(st),
                Ok(None) if started.elapsed() >= timeout => {
                    kill_group(&mut child);
                    break Err(format!("avif: decoder timed out after {}s", timeout.as_secs()));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(e) => {
                    kill_group(&mut child);
                    break Err(format!("avif: decoder wait failed: {e}"));
                }
            }
        };
        (status, out.join(), err.join().unwrap_or_default())
    });
    let status = status?;
    if !status.success() {
        if status.code() == Some(CHILD_ERR) {
            let line = err.lines().rev().find(|l| l.starts_with("avif")).unwrap_or("avif: decode failed");
            return Err(line.chars().take(300).collect());
        }
        return Err(format!("avif: decoder crashed ({})", describe(&status)));
    }
    let px = out.unwrap_or_else(|_| Err("reader panicked".into())).map_err(|e| format!("avif: bad decoder output: {e}"))?;
    let img = if p.alpha {
        image::RgbaImage::from_raw(p.width, p.height, px).map(DynamicImage::ImageRgba8)
    } else {
        image::RgbImage::from_raw(p.width, p.height, px).map(DynamicImage::ImageRgb8)
    };
    img.ok_or_else(|| "avif: buffer size".into())
}

/// Kill the child and its process group, and reap it.
fn kill_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(child.id()) {
        // SAFETY: signals the group this child leads (it was started with
        // process_group(0), so its group id is its pid); no memory involved.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn describe(status: &std::process::ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return format!("signal {sig}");
        }
    }
    match status.code() {
        Some(c) => format!("exit {c}"),
        None => "no exit code".into(),
    }
}

/// Decode-only mode: when the first argument is [`CHILD_FLAG`], read an
/// AVIF from stdin, write the header and 8-bit pixels to stdout, and return
/// the exit code; `None` for any other command line. Arguments after the
/// flag: max pixels, memory limit in bytes, CPU seconds, and nothing else
/// (a missing, malformed or extra argument is a usage error).
pub fn child_main() -> Option<i32> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) != Some(CHILD_FLAG) {
        return None;
    }
    let num = |i: usize| args.get(i).and_then(|v| v.parse::<u64>().ok());
    let (4, Some(max_pixels), Some(mem), Some(cpu)) = (args.len(), num(1), num(2), num(3)) else {
        eprintln!("avif: usage: {CHILD_FLAG} <max_pixels> <memory_bytes> <cpu_secs>");
        return Some(CHILD_ERR);
    };
    limit_self(mem, cpu);
    // one short line on stderr for the parent's error message (a panic
    // inside rav1d still aborts: the parent reports that as a crash)
    std::panic::set_hook(Box::new(|info| {
        let at = info.location().map(|l| format!(" at {}:{}", l.file().rsplit('/').next().unwrap_or(""), l.line()));
        eprintln!("avif: decoder panicked{}", at.unwrap_or_default());
    }));
    let mut input = Vec::new();
    if let Err(e) = std::io::stdin().lock().take(MAX_INPUT).read_to_end(&mut input) {
        eprintln!("avif: reading input: {e}");
        return Some(CHILD_ERR);
    }
    match decode_here(&input, max_pixels) {
        Ok((w, h, channels, px)) => {
            drop(input);
            let mut o = std::io::stdout().lock();
            let mut head = MAGIC.to_vec();
            head.extend(w.to_le_bytes());
            head.extend(h.to_le_bytes());
            head.push(channels);
            if o.write_all(&head).and_then(|_| o.write_all(&px)).and_then(|_| o.flush()).is_err() {
                return Some(CHILD_ERR);
            }
            Some(0)
        }
        Err(e) => {
            eprintln!("{e}");
            Some(CHILD_ERR)
        }
    }
}

/// No core dumps, at most `cpu` CPU seconds, and on Linux at most `mem`
/// bytes of data (heap and private mappings).
fn limit_self(mem: u64, cpu: u64) {
    #[cfg(unix)]
    {
        let set = |res, v: u64| {
            let r = libc::rlimit { rlim_cur: v as libc::rlim_t, rlim_max: v as libc::rlim_t };
            // SAFETY: setrlimit only reads the rlimit, for this process.
            unsafe { libc::setrlimit(res, &r) };
        };
        set(libc::RLIMIT_CORE, 0);
        set(libc::RLIMIT_CPU, cpu.saturating_add(1));
        #[cfg(target_os = "linux")]
        set(libc::RLIMIT_DATA, mem);
    }
    let _ = (mem, cpu);
}

/// Decode in this process: (width, height, channels, 8-bit pixels). Only
/// for the child: a rav1d panic aborts the calling process.
fn decode_here(bytes: &[u8], max_pixels: u64) -> Result<(u32, u32, u8, Vec<u8>), String> {
    let p = probe(bytes, max_pixels)?;
    let size = (p.width, p.height);
    let data = avif_parse::read_avif(&mut &bytes[..]).map_err(|e| format!("avif: {e}"))?;
    let color_meta = data.primary_item_metadata().map_err(|e| format!("avif: {e}"))?;
    let alpha_meta = data.alpha_item_metadata().map_err(|e| format!("avif alpha: {e}"))?;
    let n = p.width as usize * p.height as usize;
    // alpha first, so the color pass writes RGBA in one buffer
    let alpha = match (data.alpha_item.as_deref(), alpha_meta) {
        (Some(obu), Some(meta)) => {
            let mut luma = Vec::with_capacity(n);
            decode_av1(obu, max_pixels, &meta, size, Out::Luma(&mut luma))?;
            Some(luma)
        }
        _ => None,
    };
    let channels = if alpha.is_some() { 4 } else { 3 };
    let mut px = Vec::with_capacity(n * channels);
    let out = match &alpha {
        Some(a) => Out::Rgba(&mut px, a, data.premultiplied_alpha),
        None => Out::Rgb(&mut px),
    };
    decode_av1(&data.primary_item, max_pixels, &color_meta, size, out)?;
    Ok((p.width, p.height, channels as u8, px))
}

/// Where a decoded picture goes.
enum Out<'a> {
    /// 8-bit full-range luma (an alpha item).
    Luma(&'a mut Vec<u8>),
    Rgb(&'a mut Vec<u8>),
    /// RGB plus this alpha plane; `true` when color is premultiplied.
    Rgba(&'a mut Vec<u8>, &'a [u8], bool),
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

/// Decode one AV1 still (an AVIF item) on the calling thread into `out`.
/// The picture must have the size, bit depth and layout `meta` declares,
/// which the memory estimate is based on.
fn decode_av1(obu: &[u8], max_pixels: u64, meta: &AV1Metadata, size: (u32, u32), out: Out) -> Result<(), String> {
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
            return convert(&pic.0, meta, size, out);
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

/// Write a decoded picture into `out` as 8-bit samples.
fn convert(p: &Dav1dPicture, meta: &AV1Metadata, size: (u32, u32), out: Out) -> Result<(), String> {
    let (w, h) = (p.p.w, p.p.h);
    if w <= 0 || h <= 0 {
        return Err("avif: empty frame".into());
    }
    if (w as u32, h as u32) != size {
        return Err(format!("avif: frame is {w}x{h}, header says {}x{}", size.0, size.1));
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
    let declared_sub = if meta.monochrome { (0, 0) } else { (meta.chroma_subsampling.0 as usize, meta.chroma_subsampling.1 as usize) };
    let sub = if mono { (0, 0) } else { (ssx, ssy) };
    if bpc != meta.bit_depth as u32 || mono != meta.monochrome || sub != declared_sub {
        return Err("avif: picture format differs from the sequence header".into());
    }
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
    let luma_only = matches!(out, Out::Luma(_));
    let (u_ptr, v_ptr) = if mono || luma_only { (y_ptr, y_ptr) } else { (plane(1)?, plane(2)?) };
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
    let (buf, alpha) = match out {
        Out::Luma(buf) => {
            for y in 0..h {
                for x in 0..w {
                    buf.push(to8(luma(sample(y_ptr, ys, x, y))));
                }
            }
            return Ok(());
        }
        Out::Rgb(buf) => (buf, None),
        Out::Rgba(buf, a, premul) => {
            if a.len() != w * h {
                return Err("avif: alpha plane size differs from the image".into());
            }
            (buf, Some((a, premul)))
        }
    };
    for y in 0..h {
        for x in 0..w {
            let yy = luma(sample(y_ptr, ys, x, y));
            let (r, g, b) = if mono {
                let g = to8(yy);
                (g, g, g)
            } else {
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
                (to8(r), to8(g), to8(b))
            };
            match alpha {
                None => buf.extend_from_slice(&[r, g, b]),
                Some((a, premul)) => {
                    let a = a[y * w + x];
                    let un = |c: u8| {
                        if !premul || a == 255 {
                            c
                        } else if a == 0 {
                            0
                        } else {
                            ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8
                        }
                    };
                    buf.extend_from_slice(&[un(r), un(g), un(b), a]);
                }
            }
        }
    }
    Ok(())
}
