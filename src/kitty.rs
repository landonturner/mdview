//! Kitty graphics protocol, Unicode placeholder mode.
//!
//! Images are transmitted once (chunked base64 PNG), then a *virtual
//! placement* (`U=1`) defines the cell grid. The image is shown by printing
//! ordinary text cells: U+10EEEE with a row and a column diacritic, with the
//! image id encoded in the 24-bit foreground color. Because those are plain
//! cells, scrolling, clearing, and overwriting need no special handling.
//! Support is detected by probing the terminal (see `probe_terminal`), so any
//! terminal implementing the protocol qualifies; unsupported terminals render
//! images as plain hyperlinks instead.

use base64::Engine;
use std::io::{self, Write};

/// The placeholder character the terminal replaces with image fragments.
pub const PLACEHOLDER: char = '\u{10EEEE}';

/// Row/column index encoding, from kitty's rowcolumn-diacritics.txt.
pub const ROW_COLUMN_DIACRITICS: [char; 297] = [
    '\u{0305}', '\u{030D}', '\u{030E}', '\u{0310}', '\u{0312}', '\u{033D}', '\u{033E}',
    '\u{033F}', '\u{0346}', '\u{034A}', '\u{034B}', '\u{034C}', '\u{0350}', '\u{0351}',
    '\u{0352}', '\u{0357}', '\u{035B}', '\u{0363}', '\u{0364}', '\u{0365}', '\u{0366}',
    '\u{0367}', '\u{0368}', '\u{0369}', '\u{036A}', '\u{036B}', '\u{036C}', '\u{036D}',
    '\u{036E}', '\u{036F}', '\u{0483}', '\u{0484}', '\u{0485}', '\u{0486}', '\u{0487}',
    '\u{0592}', '\u{0593}', '\u{0594}', '\u{0595}', '\u{0597}', '\u{0598}', '\u{0599}',
    '\u{059C}', '\u{059D}', '\u{059E}', '\u{059F}', '\u{05A0}', '\u{05A1}', '\u{05A8}',
    '\u{05A9}', '\u{05AB}', '\u{05AC}', '\u{05AF}', '\u{05C4}', '\u{0610}', '\u{0611}',
    '\u{0612}', '\u{0613}', '\u{0614}', '\u{0615}', '\u{0616}', '\u{0617}', '\u{0657}',
    '\u{0658}', '\u{0659}', '\u{065A}', '\u{065B}', '\u{065D}', '\u{065E}', '\u{06D6}',
    '\u{06D7}', '\u{06D8}', '\u{06D9}', '\u{06DA}', '\u{06DB}', '\u{06DC}', '\u{06DF}',
    '\u{06E0}', '\u{06E1}', '\u{06E2}', '\u{06E4}', '\u{06E7}', '\u{06E8}', '\u{06EB}',
    '\u{06EC}', '\u{0730}', '\u{0732}', '\u{0733}', '\u{0735}', '\u{0736}', '\u{073A}',
    '\u{073D}', '\u{073F}', '\u{0740}', '\u{0741}', '\u{0743}', '\u{0745}', '\u{0747}',
    '\u{0749}', '\u{074A}', '\u{07EB}', '\u{07EC}', '\u{07ED}', '\u{07EE}', '\u{07EF}',
    '\u{07F0}', '\u{07F1}', '\u{07F3}', '\u{0816}', '\u{0817}', '\u{0818}', '\u{0819}',
    '\u{081B}', '\u{081C}', '\u{081D}', '\u{081E}', '\u{081F}', '\u{0820}', '\u{0821}',
    '\u{0822}', '\u{0823}', '\u{0825}', '\u{0826}', '\u{0827}', '\u{0829}', '\u{082A}',
    '\u{082B}', '\u{082C}', '\u{082D}', '\u{0951}', '\u{0953}', '\u{0954}', '\u{0F82}',
    '\u{0F83}', '\u{0F86}', '\u{0F87}', '\u{135D}', '\u{135E}', '\u{135F}', '\u{17DD}',
    '\u{193A}', '\u{1A17}', '\u{1A75}', '\u{1A76}', '\u{1A77}', '\u{1A78}', '\u{1A79}',
    '\u{1A7A}', '\u{1A7B}', '\u{1A7C}', '\u{1B6B}', '\u{1B6D}', '\u{1B6E}', '\u{1B6F}',
    '\u{1B70}', '\u{1B71}', '\u{1B72}', '\u{1B73}', '\u{1CD0}', '\u{1CD1}', '\u{1CD2}',
    '\u{1CDA}', '\u{1CDB}', '\u{1CE0}', '\u{1DC0}', '\u{1DC1}', '\u{1DC3}', '\u{1DC4}',
    '\u{1DC5}', '\u{1DC6}', '\u{1DC7}', '\u{1DC8}', '\u{1DC9}', '\u{1DCB}', '\u{1DCC}',
    '\u{1DD1}', '\u{1DD2}', '\u{1DD3}', '\u{1DD4}', '\u{1DD5}', '\u{1DD6}', '\u{1DD7}',
    '\u{1DD8}', '\u{1DD9}', '\u{1DDA}', '\u{1DDB}', '\u{1DDC}', '\u{1DDD}', '\u{1DDE}',
    '\u{1DDF}', '\u{1DE0}', '\u{1DE1}', '\u{1DE2}', '\u{1DE3}', '\u{1DE4}', '\u{1DE5}',
    '\u{1DE6}', '\u{1DFE}', '\u{20D0}', '\u{20D1}', '\u{20D4}', '\u{20D5}', '\u{20D6}',
    '\u{20D7}', '\u{20DB}', '\u{20DC}', '\u{20E1}', '\u{20E7}', '\u{20E9}', '\u{20F0}',
    '\u{2CEF}', '\u{2CF0}', '\u{2CF1}', '\u{2DE0}', '\u{2DE1}', '\u{2DE2}', '\u{2DE3}',
    '\u{2DE4}', '\u{2DE5}', '\u{2DE6}', '\u{2DE7}', '\u{2DE8}', '\u{2DE9}', '\u{2DEA}',
    '\u{2DEB}', '\u{2DEC}', '\u{2DED}', '\u{2DEE}', '\u{2DEF}', '\u{2DF0}', '\u{2DF1}',
    '\u{2DF2}', '\u{2DF3}', '\u{2DF4}', '\u{2DF5}', '\u{2DF6}', '\u{2DF7}', '\u{2DF8}',
    '\u{2DF9}', '\u{2DFA}', '\u{2DFB}', '\u{2DFC}', '\u{2DFD}', '\u{2DFE}', '\u{2DFF}',
    '\u{A66F}', '\u{A67C}', '\u{A67D}', '\u{A6F0}', '\u{A6F1}', '\u{A8E0}', '\u{A8E1}',
    '\u{A8E2}', '\u{A8E3}', '\u{A8E4}', '\u{A8E5}', '\u{A8E6}', '\u{A8E7}', '\u{A8E8}',
    '\u{A8E9}', '\u{A8EA}', '\u{A8EB}', '\u{A8EC}', '\u{A8ED}', '\u{A8EE}', '\u{A8EF}',
    '\u{A8F0}', '\u{A8F1}', '\u{AAB0}', '\u{AAB2}', '\u{AAB3}', '\u{AAB7}', '\u{AAB8}',
    '\u{AABE}', '\u{AABF}', '\u{AAC1}', '\u{FE20}', '\u{FE21}', '\u{FE22}', '\u{FE23}',
    '\u{FE24}', '\u{FE25}', '\u{FE26}', '\u{10A0F}', '\u{10A38}', '\u{1D185}', '\u{1D186}',
    '\u{1D187}', '\u{1D188}', '\u{1D189}', '\u{1D1AA}', '\u{1D1AB}', '\u{1D1AC}', '\u{1D1AD}',
    '\u{1D242}', '\u{1D243}', '\u{1D244}'
];

/// Environment hint that the terminal speaks the protocol; used when the
/// runtime probe gets no answer (e.g. very slow links).
pub fn env_hint() -> bool {
    if std::env::var_os("KITTY_WINDOW_ID").is_some() {
        return true;
    }
    let term = std::env::var("TERM").unwrap_or_default();
    let prog = std::env::var("TERM_PROGRAM").unwrap_or_default();
    term.contains("kitty") || term.contains("ghostty") || prog.eq_ignore_ascii_case("ghostty")
}

/// What the terminal told us when probed.
#[derive(Clone, Copy, Debug, Default)]
pub struct Probe {
    /// It answered the graphics query, so it implements the protocol.
    pub graphics: bool,
    /// Cell pixel size from a CSI 16t reply, if given.
    pub cell: Option<(u16, u16)>,
    /// Terminal background color from an OSC 11 reply, if given.
    pub bg: Option<(u8, u8, u8)>,
}

/// The probe, run once per process. Self-contained (it manages its own
/// termios state), so it is safe to call before or after raw mode.
pub fn probe() -> &'static Probe {
    static PROBE: std::sync::OnceLock<Probe> = std::sync::OnceLock::new();
    PROBE.get_or_init(|| probe_terminal(std::time::Duration::from_millis(500)))
}

/// Probes the terminal directly (requires raw mode): a graphics query and a
/// cell-size query, terminated by DA1 — every terminal answers DA1, so its
/// reply tells us the others aren't coming. This detects any terminal that
/// implements the protocol, not just ones we know by name.
pub fn probe_terminal(timeout: std::time::Duration) -> Probe {
    let mut probe = Probe::default();
    unsafe {
        let fd = libc::open(
            c"/dev/tty".as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK,
        );
        if fd < 0 {
            return probe;
        }
        // Responses only arrive unbuffered in raw mode; manage it on this fd
        // alone so the probe works before crossterm sets up the terminal.
        let mut saved: libc::termios = std::mem::zeroed();
        let restore = libc::tcgetattr(fd, &mut saved) == 0;
        if restore {
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            libc::tcsetattr(fd, libc::TCSANOW, &raw);
        }
        // 1x1 RGB query image (id 31), OSC 11 (background color), CSI 16t
        // (cell size), then DA1 as the universal terminator.
        let query =
            b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b]11;?\x1b\\\x1b[16t\x1b[c";
        if libc::write(fd, query.as_ptr().cast(), query.len()) < 0 {
            if restore {
                libc::tcsetattr(fd, libc::TCSANOW, &saved);
            }
            libc::close(fd);
            return probe;
        }
        let deadline = std::time::Instant::now() + timeout;
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let remain = deadline.saturating_duration_since(std::time::Instant::now());
            if remain.is_zero() {
                break;
            }
            let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
            let ready = libc::poll(&mut pfd, 1, remain.as_millis() as libc::c_int);
            if ready <= 0 {
                break;
            }
            // poll also wakes on POLLHUP/POLLERR with no data; reading then
            // would block (or spin). Only read when input is actually there.
            if pfd.revents & libc::POLLIN == 0 {
                break;
            }
            let mut chunk = [0u8; 256];
            let n = libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len());
            if n <= 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n as usize]);
            if da1_answered(&buf) {
                break;
            }
        }
        // Keystrokes typed during the probe window land in `buf` alongside
        // the responses; push them back into the tty input queue so the
        // pager's event loop still sees them (best effort — TIOCSTI may be
        // disabled on hardened Linux, in which case they are dropped).
        for b in non_response_bytes(&buf) {
            let _ = libc::ioctl(fd, libc::TIOCSTI, &b as *const u8);
        }
        if restore {
            libc::tcsetattr(fd, libc::TCSANOW, &saved);
        }
        libc::close(fd);
        parse_probe(&buf, &mut probe);
    }
    probe
}

/// Everything in the probe buffer that is not part of an escape-sequence
/// response: APC (`ESC _ ... ESC \`) and CSI (`ESC [ ... final`) are skipped.
fn non_response_bytes(buf: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        if buf[i] == 0x1b && i + 1 < buf.len() {
            match buf[i + 1] {
                b'_' => {
                    // APC: runs until ESC \
                    let end = find(&buf[i + 2..], b"\x1b\\").map(|e| i + 2 + e + 2);
                    i = end.unwrap_or(buf.len());
                    continue;
                }
                b']' => {
                    // OSC (our OSC 11 reply): runs until BEL or ESC \
                    let tail = &buf[i + 2..];
                    let end = tail
                        .iter()
                        .position(|&b| b == 0x07)
                        .map(|e| i + 2 + e + 1)
                        .or_else(|| find(tail, b"\x1b\\").map(|e| i + 2 + e + 2));
                    i = end.unwrap_or(buf.len());
                    continue;
                }
                b'[' => {
                    // CSI: parameter/intermediate bytes, then a final 0x40-0x7e.
                    // Only OUR responses are dropped (DA1 `ESC[?..c`, cell size
                    // `ESC[6;..t`); anything else — arrow keys, PgUp/PgDn —
                    // is a keystroke and must be re-injected intact.
                    let mut j = i + 2;
                    while j < buf.len() && !(0x40..=0x7e).contains(&buf[j]) {
                        j += 1;
                    }
                    let end = (j + 1).min(buf.len());
                    let seq = &buf[i..end];
                    let is_da1 = seq.get(2) == Some(&b'?') && seq.last() == Some(&b'c');
                    let is_cell = seq.starts_with(b"\x1b[6;") && seq.last() == Some(&b't');
                    if !is_da1 && !is_cell {
                        out.extend_from_slice(seq);
                    }
                    i = end;
                    continue;
                }
                _ => {}
            }
        }
        out.push(buf[i]);
        i += 1;
    }
    out
}

/// The DA1 reply (`ESC [ ? ... c`) marks the end of the probe conversation.
fn da1_answered(buf: &[u8]) -> bool {
    if let Some(i) = find(buf, b"\x1b[?") {
        return buf[i + 3..].contains(&b'c');
    }
    false
}

fn parse_probe(buf: &[u8], probe: &mut Probe) {
    // Graphics reply: ESC _ G i=31 ... ; OK ESC \
    if let Some(i) = find(buf, b"\x1b_G") {
        let tail = &buf[i..];
        let end = find(tail, b"\x1b\\").unwrap_or(tail.len());
        probe.graphics = find(&tail[..end], b"OK").is_some();
    }
    // Background reply: ESC ] 11 ; rgb:RRRR/GGGG/BBBB (BEL or ST terminated)
    if let Some(i) = find(buf, b"\x1b]11;") {
        let tail = &buf[i + 5..];
        let end = tail
            .iter()
            .position(|&b| b == 0x07 || b == 0x1b)
            .unwrap_or(tail.len());
        probe.bg = parse_x_color(&String::from_utf8_lossy(&tail[..end]));
    }
    // Cell size reply: ESC [ 6 ; <height> ; <width> t
    if let Some(i) = find(buf, b"\x1b[6;") {
        let tail = &buf[i + 4..];
        if let Some(t) = tail.iter().position(|&b| b == b't') {
            let body = String::from_utf8_lossy(&tail[..t]);
            let mut parts = body.split(';');
            if let (Some(h), Some(w)) = (
                parts.next().and_then(|v| v.parse::<u16>().ok()),
                parts.next().and_then(|v| v.parse::<u16>().ok()),
            ) {
                if h > 0 && w > 0 {
                    probe.cell = Some((w, h));
                }
            }
        }
    }
}

/// Parses X11 color spec replies: `rgb:2b2b/3030/3b3b` (1-4 hex digits per
/// channel, scaled to 8 bits).
fn parse_x_color(s: &str) -> Option<(u8, u8, u8)> {
    let body = s.trim().strip_prefix("rgb:")?;
    let mut out = [0u8; 3];
    let mut parts = body.split('/');
    for slot in &mut out {
        let p = parts.next()?;
        if p.is_empty() || p.len() > 4 {
            return None;
        }
        let v = u16::from_str_radix(p, 16).ok()? as u32;
        let max = (1u32 << (4 * p.len() as u32)) - 1;
        *slot = ((v * 255 + max / 2) / max) as u8;
    }
    Some((out[0], out[1], out[2]))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Transmits PNG data for `id` (chunked; q=2 suppresses responses).
pub fn transmit(out: &mut impl Write, id: u32, png: &[u8]) -> io::Result<()> {
    let data = base64::engine::general_purpose::STANDARD.encode(png);
    let mut chunks = data.as_bytes().chunks(4096).peekable();
    let mut first = true;
    while let Some(chunk) = chunks.next() {
        let more = if chunks.peek().is_some() { 1 } else { 0 };
        if first {
            write!(out, "\x1b_Ga=t,i={id},f=100,t=d,q=2,m={more};")?;
            first = false;
        } else {
            write!(out, "\x1b_Gm={more};")?;
        }
        out.write_all(chunk)?;
        write!(out, "\x1b\\")?;
    }
    Ok(())
}

/// Creates (or replaces) the virtual placement sizing `id` to cols x rows.
pub fn place(out: &mut impl Write, id: u32, cols: u16, rows: u16) -> io::Result<()> {
    write!(out, "\x1b_Ga=p,U=1,i={id},p=1,c={cols},r={rows},q=2\x1b\\")
}

/// Deletes all transmitted images and placements (used on exit).
pub fn delete_all(out: &mut impl Write) -> io::Result<()> {
    write!(out, "\x1b_Ga=d,d=A,q=2\x1b\\")
}

/// One placeholder cell: base char + row diacritic + column diacritic.
pub fn placeholder_cell(row: usize, col: usize) -> Option<String> {
    let r = ROW_COLUMN_DIACRITICS.get(row)?;
    let c = ROW_COLUMN_DIACRITICS.get(col)?;
    let mut s = String::with_capacity(8);
    s.push(PLACEHOLDER);
    s.push(*r);
    s.push(*c);
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_x_color_replies() {
        assert_eq!(parse_x_color("rgb:2b2b/3030/3b3b"), Some((0x2b, 0x30, 0x3b)));
        assert_eq!(parse_x_color("rgb:ff/ff/ff"), Some((255, 255, 255)));
        assert_eq!(parse_x_color("rgb:f/0/8"), Some((255, 0, 0x88)));
        assert_eq!(parse_x_color("nonsense"), None);
    }

    #[test]
    fn probe_replies_are_never_reinjected() {
        let buf = b"\x1b_Gi=31;OK\x1b\\\x1b]11;rgb:ffff/ffff/ffff\x07\x1b[6;20;10t\x1b[?64cq";
        assert_eq!(non_response_bytes(buf), b"q");
    }
}
