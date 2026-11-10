//! Memory used by `verify_dist` on a large precompressed copy that is not
//! UTF-8. Its own test binary, because it counts every allocation in the
//! process.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use postcms_build::images::{verify_dist, ImagesCfg, Pipeline, Transport};

struct Peak;

static NOW: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Peak {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            let now = NOW.fetch_add(l.size(), Ordering::SeqCst) + l.size();
            PEAK.fetch_max(now, Ordering::SeqCst);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        NOW.fetch_sub(l.size(), Ordering::SeqCst);
    }
}

#[global_allocator]
static A: Peak = Peak;

const SIZE: usize = 64 << 20;

/// A .gz of 64 MiB of bytes that are not UTF-8 is checked a chunk at a time.
/// Decoding it whole and then as lossy UTF-8 (each such byte becomes three)
/// peaked at over 256 MiB.
#[test]
fn a_non_utf8_gz_is_checked_without_a_text_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let dist = tmp.path().join("dist");
    std::fs::create_dir_all(&dist).unwrap();
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut enc, &vec![0xffu8; SIZE]).unwrap();
    std::fs::write(dist.join("data.json.gz"), enc.finish().unwrap()).unwrap();
    let cfg = ImagesCfg::from_site_yaml("images:\n  widths: [32]\n").unwrap().unwrap();
    let t: Box<Transport> = Box::new(|_, _, _| Err("offline".into()));
    let p = Pipeline::new(cfg, tmp.path(), &dist, "https://example.com", true, t);

    let base = NOW.load(Ordering::SeqCst);
    PEAK.store(base, Ordering::SeqCst);
    verify_dist(&dist, Some(&p)).unwrap();
    let used = PEAK.load(Ordering::SeqCst) - base;
    assert!(used < 4 << 20, "verify_dist peaked at {} MiB for {} MiB of text", used >> 20, SIZE >> 20);
}
