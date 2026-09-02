//! Renders mermaid diagrams and LaTeX math to PNG for the kitty image
//! pipeline. Rendering uses public services (mermaid.ink, codecogs) since a
//! local mermaid/TeX toolchain is rarely installed; results are cached on
//! disk keyed by source hash, and failures are memoized per process so an
//! offline session degrades to highlighted code blocks without re-blocking.

use base64::Engine;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

const FETCH_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_PNG_BYTES: u64 = 8 * 1024 * 1024;

enum Slot {
    /// A background fetch is in flight.
    Pending,
    Ready(Vec<u8>),
    Failed,
}

fn memo() -> &'static Mutex<HashMap<String, Slot>> {
    static MEMO: OnceLock<Mutex<HashMap<String, Slot>>> = OnceLock::new();
    MEMO.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Set when a background fetch finishes; the pager polls this to know a
/// re-render might now swap a code block for its diagram.
static DIRTY: AtomicBool = AtomicBool::new(false);

pub fn take_dirty() -> bool {
    DIRTY.swap(false, Ordering::SeqCst)
}

/// Non-blocking: returns the PNG when it is already available (memo or disk
/// cache), otherwise kicks off a background fetch and returns None — the
/// caller renders its fallback now and re-renders when [`take_dirty`] fires.
pub fn mermaid_png(source: &str, dark: bool) -> Option<Vec<u8>> {
    // Backgrounds chosen to sit naturally on typical terminal themes.
    let (theme, bg, kind) = if dark {
        ("dark", "!2b303b", "mermaid-dark")
    } else {
        ("default", "!ffffff", "mermaid-light")
    };
    let state = serde_json::json!({
        "code": source,
        "mermaid": { "theme": theme },
    });
    let b64 = base64::engine::general_purpose::URL_SAFE.encode(state.to_string());
    let url = format!("https://mermaid.ink/img/base64:{b64}?type=png&bgColor={bg}");
    lookup(kind, source, url)
}

/// See [`mermaid_png`]; same non-blocking contract.
pub fn latex_png(source: &str, dark: bool) -> Option<Vec<u8>> {
    let (color, kind) = if dark { ("white", "latex-dark") } else { ("black", "latex-light") };
    let expr = format!(
        "\\dpi{{200}}\\bg{{transparent}}\\color{{{color}}}{}",
        source.trim()
    );
    let url = format!(
        "https://latex.codecogs.com/png.image?{}",
        percent_encode(&expr)
    );
    lookup(kind, source, url)
}

fn lookup(kind: &str, source: &str, url: String) -> Option<Vec<u8>> {
    let key = cache_key(kind, source);
    {
        let mut m = memo().lock().ok()?;
        match m.get(&key) {
            Some(Slot::Ready(bytes)) => return Some(bytes.clone()),
            Some(Slot::Pending | Slot::Failed) => return None,
            None => {}
        }
        if let Some(bytes) = cache_path(&key)
            .and_then(|p| std::fs::read(p).ok())
            .filter(|b| is_png(b))
        {
            m.insert(key, Slot::Ready(bytes.clone()));
            return Some(bytes);
        }
        m.insert(key.clone(), Slot::Pending);
    }
    std::thread::spawn(move || {
        let result = fetch(&url);
        if let (Some(bytes), Some(path)) = (&result, cache_path(&key)) {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(path, bytes);
        }
        let slot = match result {
            Some(bytes) => Slot::Ready(bytes),
            None => Slot::Failed,
        };
        if let Ok(mut m) = memo().lock() {
            m.insert(key, slot);
        }
        DIRTY.store(true, Ordering::SeqCst);
    });
    None
}

fn cache_key(kind: &str, source: &str) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    kind.hash(&mut h);
    source.hash(&mut h);
    // Two independent hashes to make accidental collisions implausible.
    let a = h.finish();
    source.len().hash(&mut h);
    format!("{kind}-{a:016x}{:016x}", h.finish())
}

fn cache_path(key: &str) -> Option<PathBuf> {
    Some(cache_dir()?.join(format!("{key}.png")))
}

pub fn cache_dir() -> Option<PathBuf> {
    Some(dirs::cache_dir()?.join("mdview"))
}

/// Deletes the on-disk diagram cache. Returns the removed directory, or None
/// if there was nothing to remove.
pub fn clear_cache() -> anyhow::Result<Option<PathBuf>> {
    let Some(dir) = cache_dir() else { return Ok(None) };
    if !dir.exists() {
        return Ok(None);
    }
    std::fs::remove_dir_all(&dir)?;
    Ok(Some(dir))
}

fn fetch(url: &str) -> Option<Vec<u8>> {
    let agent = ureq::AgentBuilder::new()
        .timeout(FETCH_TIMEOUT)
        .build();
    let resp = agent.get(url).call().ok()?;
    let mut bytes = Vec::new();
    resp.into_reader()
        .take(MAX_PNG_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    // Over the cap means the body was truncated mid-stream; caching that
    // would poison the disk cache with a corrupt-but-magic-valid PNG.
    if bytes.len() as u64 > MAX_PNG_BYTES {
        return None;
    }
    is_png(&bytes).then_some(bytes)
}

fn is_png(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0x89, b'P', b'N', b'G'])
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encodes_reserved_bytes() {
        assert_eq!(percent_encode("a b\\c{1}"), "a%20b%5Cc%7B1%7D");
    }

    #[test]
    fn cache_keys_differ_by_kind_and_source() {
        let a = cache_key("mermaid", "graph LR");
        let b = cache_key("latex", "graph LR");
        let c = cache_key("mermaid", "graph TD");
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_eq!(a, cache_key("mermaid", "graph LR"));
    }

    #[test]
    fn png_sniffing() {
        assert!(is_png(&[0x89, b'P', b'N', b'G', 1, 2]));
        assert!(!is_png(b"<html>error</html>"));
    }
}
