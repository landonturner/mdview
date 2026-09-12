//! Background preparation of local images for the kitty graphics path.
//!
//! Decoding a multi-megabyte PNG, downscaling it to what the terminal will
//! actually display, and re-encoding it takes long enough to notice, and the
//! pager used to do all of it — for every image — before drawing a single
//! line of text. Now [`request`] answers from a memo when the prepared image
//! is ready and otherwise kicks the work off on a thread, returning `None`
//! so the caller renders the caption for now. When the thread finishes it
//! raises [`take_dirty`] and the pager re-renders, at which point the image
//! is there.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

/// A PNG sized for display, plus its pixel dimensions.
pub struct Prepared {
    pub png: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
}

enum Slot {
    Pending,
    Ready(Arc<Prepared>),
    Failed,
}

/// Path, mtime (so hot-reloaded images refresh), and the pixel box the
/// result must fit in (so resizes re-prepare at the new size).
type Key = (PathBuf, Option<SystemTime>, u32, u32);

static DIRTY: AtomicBool = AtomicBool::new(false);

/// Set when a background preparation finishes; the pager polls this.
pub fn take_dirty() -> bool {
    DIRTY.swap(false, Ordering::SeqCst)
}

fn memo() -> &'static Mutex<HashMap<Key, Slot>> {
    static M: OnceLock<Mutex<HashMap<Key, Slot>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Non-blocking. Returns the prepared image when it is ready; otherwise
/// starts preparing it (once) and returns `None`.
pub fn request(path: &Path, max_w: u32, max_h: u32) -> Option<Arc<Prepared>> {
    let mtime = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());
    let key: Key = (path.to_path_buf(), mtime, max_w.max(1), max_h.max(1));
    {
        let mut m = memo().lock().ok()?;
        match m.get(&key) {
            Some(Slot::Ready(p)) => return Some(p.clone()),
            Some(Slot::Pending) | Some(Slot::Failed) => return None,
            None => {
                m.insert(key.clone(), Slot::Pending);
            }
        }
    }
    std::thread::spawn(move || {
        let slot = match prepare(&key.0, key.2, key.3) {
            Some(p) => Slot::Ready(Arc::new(p)),
            None => Slot::Failed,
        };
        if let Ok(mut m) = memo().lock() {
            m.insert(key, slot);
        }
        DIRTY.store(true, Ordering::SeqCst);
    });
    None
}

/// Reads, decodes, downscales to fit `max_w` x `max_h` (never upscaling),
/// and encodes as PNG. An original PNG that already fits is passed through
/// untouched.
fn prepare(path: &Path, max_w: u32, max_h: u32) -> Option<Prepared> {
    use image::codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder};
    use image::imageops::FilterType;

    let bytes = std::fs::read(path).ok()?;
    let is_png = image::guess_format(&bytes).ok() == Some(image::ImageFormat::Png);
    let img = image::load_from_memory(&bytes).ok()?;
    let (w, h) = (img.width().max(1), img.height().max(1));
    let scale = (max_w as f64 / w as f64).min(max_h as f64 / h as f64).min(1.0);
    if scale >= 1.0 && is_png {
        return Some(Prepared { png: Arc::new(bytes), width: w, height: h });
    }
    let (tw, th) = if scale < 1.0 {
        (
            ((w as f64 * scale).round() as u32).max(1),
            ((h as f64 * scale).round() as u32).max(1),
        )
    } else {
        (w, h)
    };
    let out = if scale < 1.0 { img.resize_exact(tw, th, FilterType::Triangle) } else { img };
    let mut buf = Vec::new();
    let enc = PngEncoder::new_with_quality(&mut buf, CompressionType::Default, PngFilter::Adaptive);
    out.write_with_encoder(enc).ok()?;
    Some(Prepared { png: Arc::new(buf), width: tw, height: th })
}
