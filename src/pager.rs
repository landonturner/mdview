use crate::config::Config;
use crate::render::{render, Document, Highlighter, ImageMode, RenderOpts};
use crate::text::{Line, Style};
use anyhow::Result;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute, queue,
    style::{Attribute, Color, Print, SetAttribute, SetForegroundColor},
    terminal::{
        self, disable_raw_mode, enable_raw_mode, BeginSynchronizedUpdate, Clear, ClearType,
        EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
    },
};
use std::io::{self, Write};
use std::path::Path;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub fn run(
    source: &str,
    title: &str,
    cfg: &Config,
    hl: &Highlighter,
    base: Option<&std::path::Path>,
    resolve_links: bool,
) -> Result<()> {
    let mut out = io::stdout();
    enable_raw_mode()?;
    // From here on, always restore the terminal — even if entering the
    // alternate screen fails, raw mode must not leak to the shell.
    let result = execute!(out, EnterAlternateScreen, cursor::Hide)
        .map_err(Into::into)
        .and_then(|_| {
            Pager::new(source, title, cfg, hl, base, resolve_links).main_loop(&mut out)
        });
    let _ = crate::kitty::delete_all(&mut out);
    let _ = execute!(out, cursor::Show, LeaveAlternateScreen);
    let _ = disable_raw_mode();
    result
}

/// Restores the terminal even if we panic mid-session.
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut out = io::stdout();
        let _ = execute!(out, cursor::Show, LeaveAlternateScreen);
        let _ = disable_raw_mode();
        default(info);
    }));
}

enum Mode {
    Normal,
    Prompt { backward: bool, buf: String },
    Toc { sel: usize },
    Help,
    /// Link-follow: labels overlaid on the visible links; next key picks one.
    Follow { targets: Vec<LinkTarget> },
}

/// One followable link currently on screen.
struct LinkTarget {
    label: char,
    /// Screen row (0-based, within the content area).
    row: u16,
    /// Display column of the link's first cell, before the margin shift.
    col: u16,
    url: String,
}

/// A document the user navigated away from, for the back stack.
struct HistoryEntry {
    source: String,
    title: String,
    base: Option<std::path::PathBuf>,
    top: usize,
}

struct Search {
    query: String,
    /// (line index, byte start, byte end) into each line's plain text.
    matches: Vec<(usize, usize, usize)>,
    cur: usize,
}

struct Pager<'a> {
    source: String,
    title: String,
    cfg: &'a Config,
    hl: &'a Highlighter,
    base: Option<std::path::PathBuf>,
    resolve_links: bool,
    back_stack: Vec<HistoryEntry>,
    image_mode: ImageMode,
    /// Image ids already transmitted to the terminal this session.
    transmitted: std::collections::HashSet<u32>,
    doc: Document,
    /// When false, mermaid/latex blocks show their source instead.
    diagrams: bool,
    /// Set when a re-render may have introduced images the terminal has not
    /// been sent yet; main_loop syncs and clears it.
    images_stale: bool,
    top: usize,
    w: u16,
    h: u16,
    mode: Mode,
    count: String,
    search: Option<Search>,
    message: Option<String>,
    quit: bool,
}

impl<'a> Pager<'a> {
    fn new(
        source: &str,
        title: &str,
        cfg: &'a Config,
        hl: &'a Highlighter,
        base: Option<&std::path::Path>,
        resolve_links: bool,
    ) -> Self {
        let (w, h) = term_size();
        let image_mode = detect_image_mode();
        let diagrams = cfg.default_view != "text";
        let doc = render(
            source,
            hl,
            &RenderOpts {
                width: wrap_width(cfg, w),
                base,
                image_mode,
                diagrams,
                resolve_links,
            },
        );
        Self {
            source: source.to_string(),
            title: title.to_string(),
            cfg,
            hl,
            base: base.map(|p| p.to_path_buf()),
            resolve_links,
            back_stack: Vec::new(),
            image_mode,
            transmitted: std::collections::HashSet::new(),
            doc,
            diagrams,
            images_stale: false,
            top: 0,
            w,
            h,
            mode: Mode::Normal,
            count: String::new(),
            search: None,
            message: None,
            quit: false,
        }
    }

    fn lines(&self) -> &[Line] {
        &self.doc.lines
    }

    fn render_opts(&self) -> RenderOpts<'_> {
        RenderOpts {
            width: wrap_width(self.cfg, self.w),
            base: self.base.as_deref(),
            image_mode: self.image_mode,
            diagrams: self.diagrams,
            resolve_links: self.resolve_links,
        }
    }

    /// Toggles mermaid/latex blocks between rendered diagrams and their
    /// source, keeping the viewport position proportionally.
    fn toggle_diagrams(&mut self) {
        let old_len = self.lines().len().max(1);
        let frac = self.top as f64 / old_len as f64;
        self.diagrams = !self.diagrams;
        self.doc = render(&self.source, self.hl, &self.render_opts());
        self.top = ((frac * self.lines().len() as f64) as usize).min(self.max_top());
        self.images_stale = true;
        self.research();
        self.message = Some(
            if self.diagrams {
                "diagrams: rendered".into()
            } else {
                "diagrams: source".into()
            },
        );
    }

    /// Re-runs the current search against the active view's lines.
    fn research(&mut self) {
        if let Some(s) = &self.search {
            let query = s.query.clone();
            let matches = find_matches(self.lines(), &query);
            self.search = if matches.is_empty() {
                None
            } else {
                Some(Search { query, cur: nearest_match(&matches, self.top), matches })
            };
        }
    }

    fn content_h(&self) -> usize {
        (self.h as usize).saturating_sub(1).max(1)
    }

    fn max_top(&self) -> usize {
        self.lines().len().saturating_sub(self.content_h())
    }

    fn main_loop(&mut self, out: &mut io::Stdout) -> Result<()> {
        self.sync_images(out)?;
        self.draw(out)?;
        loop {
            let mut changed = false;
            // Wake periodically so finished background diagram fetches can be
            // swapped in without waiting for the next keypress.
            if event::poll(std::time::Duration::from_millis(250))? {
                match event::read()? {
                    Event::Key(key)
                        if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                    {
                        self.message = None;
                        match &self.mode {
                            Mode::Normal => self.key_normal(key),
                            Mode::Prompt { .. } => self.key_prompt(key),
                            Mode::Toc { .. } => self.key_toc(key),
                            Mode::Help => self.mode = Mode::Normal,
                            Mode::Follow { .. } => self.key_follow(key),
                        }
                        changed = true;
                    }
                    Event::Resize(w, h) => {
                        self.resize(w, h);
                        self.sync_images(out)?;
                        changed = true;
                    }
                    _ => {}
                }
            }
            if crate::diagram::take_dirty() {
                // A diagram finished rendering: re-render at current size.
                self.resize(self.w, self.h);
                self.images_stale = true;
                changed = true;
            }
            if self.images_stale {
                self.sync_images(out)?;
                self.images_stale = false;
            }
            if self.quit {
                return Ok(());
            }
            if changed {
                self.draw(out)?;
            }
        }
    }

    fn resize(&mut self, w: u16, h: u16) {
        let old_len = self.lines().len().max(1);
        let frac = self.top as f64 / old_len as f64;
        self.w = if w == 0 { 80 } else { w };
        self.h = if h == 0 { 24 } else { h };
        self.image_mode = detect_image_mode();
        self.doc = render(&self.source, self.hl, &self.render_opts());
        self.top = ((frac * self.lines().len() as f64) as usize).min(self.max_top());
        self.research();
    }

    /// Transmits any images the terminal hasn't seen and (re)creates their
    /// virtual placements. Placeholder cells in the document do the rest.
    fn sync_images(&mut self, out: &mut io::Stdout) -> Result<()> {
        for img in &self.doc.images {
            if self.transmitted.insert(img.id) {
                crate::kitty::transmit(out, img.id, &img.png)?;
            }
            crate::kitty::place(out, img.id, img.cols, img.rows)?;
        }
        out.flush()?;
        Ok(())
    }

    fn take_count(&mut self) -> Option<usize> {
        if self.count.is_empty() {
            return None;
        }
        let n = self.count.parse().ok();
        self.count.clear();
        n
    }

    fn scroll(&mut self, delta: i64) {
        let top = self.top as i64 + delta;
        self.top = top.clamp(0, self.max_top() as i64) as usize;
    }

    fn key_normal(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.content_h() as i64;
        match key.code {
            KeyCode::Char(c @ '0'..='9') if !ctrl => {
                self.count.push(c);
                return;
            }
            KeyCode::Char('q') | KeyCode::Char('Q') if !ctrl => self.quit = true,
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Char('j') | KeyCode::Down | KeyCode::Enter => {
                let n = self.take_count().unwrap_or(1);
                self.scroll(n as i64);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let n = self.take_count().unwrap_or(1);
                self.scroll(-(n as i64));
            }
            KeyCode::Char(' ') | KeyCode::PageDown => self.scroll(page),
            KeyCode::Char('f') | KeyCode::Char('v') if ctrl => self.scroll(page),
            KeyCode::Char('f') => self.scroll(page),
            KeyCode::Char('b') | KeyCode::PageUp => self.scroll(-page),
            KeyCode::Char('d') => self.scroll(page / 2),
            KeyCode::Char('u') => self.scroll(-page / 2),
            KeyCode::Char('g') | KeyCode::Home => {
                let n = self.take_count();
                self.top = n.map_or(0, |n| n.saturating_sub(1)).min(self.max_top());
            }
            KeyCode::Char('G') | KeyCode::End => {
                let n = self.take_count();
                self.top = n.map_or(self.max_top(), |n| n.saturating_sub(1).min(self.max_top()));
            }
            KeyCode::Char('p') | KeyCode::Char('%') => {
                if let Some(n) = self.take_count() {
                    let n = n.min(100);
                    self.top = (self.lines().len() * n / 100).min(self.max_top());
                }
            }
            KeyCode::Char('v') => self.toggle_diagrams(),
            KeyCode::Char('o') if ctrl => self.go_back(),
            KeyCode::Char('o') => self.enter_follow_mode(),
            KeyCode::Backspace => self.go_back(),
            KeyCode::Char('/') => self.mode = Mode::Prompt { backward: false, buf: String::new() },
            KeyCode::Char('?') => self.mode = Mode::Prompt { backward: true, buf: String::new() },
            KeyCode::Char('n') => self.next_match(1),
            KeyCode::Char('N') => self.next_match(-1),
            KeyCode::Char(']') | KeyCode::Char('}') => self.jump_heading(true),
            KeyCode::Char('[') | KeyCode::Char('{') => self.jump_heading(false),
            KeyCode::Char('t') => {
                if self.doc.headings.is_empty() {
                    self.message = Some("No headings in this document".into());
                } else {
                    let sel = self
                        .doc
                        .headings
                        .iter()
                        .rposition(|h| h.line <= self.top)
                        .unwrap_or(0);
                    self.mode = Mode::Toc { sel };
                }
            }
            KeyCode::Char('h') | KeyCode::F(1) => self.mode = Mode::Help,
            _ => {}
        }
        self.count.clear();
    }

    fn key_prompt(&mut self, key: KeyEvent) {
        let Mode::Prompt { backward, buf } = &mut self.mode else { return };
        let backward = *backward;
        match key.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.mode = Mode::Normal;
            }
            KeyCode::Backspace => {
                if buf.pop().is_none() {
                    self.mode = Mode::Normal;
                }
            }
            KeyCode::Enter => {
                let query = std::mem::take(buf);
                self.mode = Mode::Normal;
                if !query.is_empty() {
                    self.do_search(query, backward);
                }
            }
            KeyCode::Char(c) => buf.push(c),
            _ => {}
        }
    }

    fn key_toc(&mut self, key: KeyEvent) {
        let Mode::Toc { sel, .. } = &mut self.mode else { return };
        let last = self.doc.headings.len().saturating_sub(1);
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => *sel = (*sel + 1).min(last),
            KeyCode::Char('k') | KeyCode::Up => *sel = sel.saturating_sub(1),
            KeyCode::Char('g') | KeyCode::Home => *sel = 0,
            KeyCode::Char('G') | KeyCode::End => *sel = last,
            KeyCode::Enter => {
                let line = self.doc.headings[*sel].line;
                self.top = line.min(self.max_top());
                self.mode = Mode::Normal;
            }
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('t') => self.mode = Mode::Normal,
            _ => {}
        }
    }

    /// Collects the links visible on screen and overlays selection labels.
    fn enter_follow_mode(&mut self) {
        const LABELS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
        let visible_w = (self.w - effective_margin(self.cfg, self.w)) as usize;
        let mut targets: Vec<LinkTarget> = Vec::new();
        'rows: for row in 0..self.content_h() {
            let Some(line) = self.lines().get(self.top + row) else { break };
            let mut col = 0usize;
            let mut prev: Option<&str> = None;
            for span in &line.spans {
                if col >= visible_w {
                    break;
                }
                if let Some(url) = &span.style.link {
                    if prev != Some(url.as_str()) {
                        if targets.len() >= LABELS.len() {
                            break 'rows;
                        }
                        targets.push(LinkTarget {
                            label: LABELS[targets.len()] as char,
                            row: row as u16,
                            col: col as u16,
                            url: url.clone(),
                        });
                    }
                }
                prev = span.style.link.as_deref();
                col += span.width();
            }
        }
        if targets.is_empty() {
            self.message = Some("No links on screen".into());
        } else {
            self.mode = Mode::Follow { targets };
        }
    }

    fn key_follow(&mut self, key: KeyEvent) {
        let Mode::Follow { targets } = std::mem::replace(&mut self.mode, Mode::Normal) else {
            return;
        };
        if let KeyCode::Char(c) = key.code {
            if let Some(t) = targets.into_iter().find(|t| t.label == c) {
                self.follow_link(&t.url);
            }
        }
    }

    /// Dispatches a followed link by target kind.
    fn follow_link(&mut self, url: &str) {
        if let Some(fragment) = url.strip_prefix('#') {
            self.jump_fragment(fragment);
            return;
        }
        if let Some(path) = url.strip_prefix("file://") {
            let (path, fragment) = match path.split_once('#') {
                Some((p, f)) => (p, Some(f.to_string())),
                None => (path, None),
            };
            let is_md = Path::new(path)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"));
            if is_md {
                self.navigate_to(Path::new(path), fragment.as_deref());
                return;
            }
            if !Path::new(path).exists() {
                self.message = Some(format!("Not found: {path}"));
                return;
            }
        }
        open_external(url, &mut self.message);
    }

    /// Loads another markdown file into the pager, pushing the current
    /// document onto the back stack.
    fn navigate_to(&mut self, path: &Path, fragment: Option<&str>) {
        let source = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(err) => {
                self.message = Some(format!("{}: {err}", path.display()));
                return;
            }
        };
        let title = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let base = path.canonicalize().ok().and_then(|c| c.parent().map(|d| d.to_path_buf()));
        self.back_stack.push(HistoryEntry {
            source: std::mem::take(&mut self.source),
            title: std::mem::replace(&mut self.title, title),
            base: std::mem::replace(&mut self.base, base),
            top: self.top,
        });
        self.source = source;
        self.resolve_links = true; // navigated docs always have a real base
        self.doc = render(&self.source, self.hl, &self.render_opts());
        self.top = 0;
        self.search = None;
        self.images_stale = true;
        if let Some(f) = fragment {
            self.jump_fragment(f);
        }
    }

    /// Pops the back stack, restoring the previous document and position.
    fn go_back(&mut self) {
        let Some(prev) = self.back_stack.pop() else {
            self.message = Some("No previous document".into());
            return;
        };
        self.source = prev.source;
        self.title = prev.title;
        self.base = prev.base;
        self.doc = render(&self.source, self.hl, &self.render_opts());
        self.top = prev.top.min(self.max_top());
        self.search = None;
        self.images_stale = true;
    }

    /// Jumps to the heading whose GitHub-style slug (or text) matches.
    fn jump_fragment(&mut self, fragment: &str) {
        let want = fragment.to_ascii_lowercase();
        let found = self
            .doc
            .headings
            .iter()
            .find(|h| slugify(&h.text) == want || h.text.eq_ignore_ascii_case(fragment))
            .map(|h| h.line);
        match found {
            Some(line) => self.top = line.min(self.max_top()),
            None => self.message = Some(format!("No heading #{fragment}")),
        }
    }

    fn jump_heading(&mut self, forward: bool) {
        let target = if forward {
            self.doc.headings.iter().find(|h| h.line > self.top).map(|h| h.line)
        } else {
            self.doc.headings.iter().rev().find(|h| h.line < self.top).map(|h| h.line)
        };
        match target {
            Some(line) => self.top = line.min(self.max_top()),
            None => self.message = Some("No more headings".into()),
        }
    }

    fn do_search(&mut self, query: String, backward: bool) {
        let matches = find_matches(self.lines(), &query);
        if matches.is_empty() {
            self.message = Some(format!("Pattern not found: {query}"));
            self.search = None;
            return;
        }
        let cur = if backward {
            matches
                .iter()
                .rposition(|m| m.0 < self.top)
                .unwrap_or(matches.len() - 1)
        } else {
            matches.iter().position(|m| m.0 >= self.top).unwrap_or(0)
        };
        self.search = Some(Search { query, matches, cur });
        self.goto_match();
    }

    fn next_match(&mut self, dir: i64) {
        let Some(s) = &mut self.search else {
            self.message = Some("No previous search".into());
            return;
        };
        let len = s.matches.len() as i64;
        let next = s.cur as i64 + dir;
        if next < 0 || next >= len {
            s.cur = next.rem_euclid(len) as usize;
            self.message = Some("(search wrapped)".into());
        } else {
            s.cur = next as usize;
        }
        self.goto_match();
    }

    fn goto_match(&mut self) {
        if let Some(s) = &self.search {
            let line = s.matches[s.cur].0;
            // Keep the match on screen; put it at the top like less does,
            // unless we're near the end of the document.
            self.top = line.min(self.max_top());
        }
    }

    // ---- drawing ----

    fn draw(&mut self, out: &mut io::Stdout) -> Result<()> {
        queue!(out, BeginSynchronizedUpdate)?;
        let rows = self.content_h();
        let margin = effective_margin(self.cfg, self.w);
        for row in 0..rows {
            queue!(out, cursor::MoveTo(0, row as u16), Clear(ClearType::UntilNewLine))?;
            // Content (including kitty image placeholder cells) starts at the
            // margin; the status bar and overlays keep absolute coordinates.
            queue!(out, cursor::MoveTo(margin, row as u16))?;
            let idx = self.top + row;
            if idx < self.lines().len() {
                self.draw_line(out, idx)?;
            } else {
                queue!(
                    out,
                    SetForegroundColor(Color::DarkGrey),
                    Print("~"),
                    SetAttribute(Attribute::Reset)
                )?;
            }
        }
        self.draw_status(out)?;
        match &self.mode {
            Mode::Toc { .. } => self.draw_toc(out)?,
            Mode::Help => self.draw_help(out)?,
            Mode::Follow { targets } => {
                // Labels overwrite the link's first cell only. Kitty image
                // placeholder lines carry no link styles, so placeholder
                // cells are never touched.
                for t in targets {
                    queue!(
                        out,
                        cursor::MoveTo(margin + t.col, t.row),
                        SetAttribute(Attribute::Reverse),
                        SetAttribute(Attribute::Bold),
                        Print(t.label),
                        SetAttribute(Attribute::Reset)
                    )?;
                }
            }
            _ => {}
        }
        queue!(out, EndSynchronizedUpdate)?;
        out.flush()?;
        Ok(())
    }

    fn draw_line(&self, out: &mut io::Stdout, idx: usize) -> Result<()> {
        let line = &self.lines()[idx];
        // Matches are sorted by line, so binary-search the slice for this row
        // instead of scanning every match per visible line per frame.
        let ranges: Vec<(usize, usize)> = match &self.search {
            Some(s) => {
                let start = s.matches.partition_point(|m| m.0 < idx);
                let end = s.matches.partition_point(|m| m.0 <= idx);
                s.matches[start..end].iter().map(|m| (m.1, m.2)).collect()
            }
            None => Vec::new(),
        };

        let max = (self.w - effective_margin(self.cfg, self.w)) as usize;
        let mut col = 0usize;
        let mut offset = 0usize; // byte offset into the line's plain text
        'spans: for span in &line.spans {
            let len = span.text.len();
            for (seg, highlighted) in segment(&span.text, offset, &ranges) {
                let mut style = span.style.clone();
                if highlighted {
                    style.reverse = true;
                }
                let (used, complete) = emit_span(out, seg, &style, max - col)?;
                col += used;
                // Stop at the first truncation: emitting later segments after
                // skipping content here would garble the right edge (e.g. a
                // wide CJK char that didn't fit followed by narrow chars).
                if !complete {
                    break 'spans;
                }
            }
            offset += len;
        }
        Ok(())
    }

    fn draw_status(&self, out: &mut io::Stdout) -> Result<()> {
        let row = self.h.saturating_sub(1);
        queue!(out, cursor::MoveTo(0, row), Clear(ClearType::UntilNewLine))?;

        if let Mode::Prompt { backward, buf } = &self.mode {
            let prompt = format!("{}{buf}", if *backward { '?' } else { '/' });
            queue!(out, Print(fit(&prompt, self.w as usize)))?;
            return Ok(());
        }

        let total = self.lines().len();
        let bottom = (self.top + self.content_h()).min(total);
        let right = if total == 0 || bottom >= total {
            format!(" {}-{}/{} END ", self.top.min(total) + 1, bottom, total)
        } else {
            format!(" {}-{}/{} {}% ", self.top + 1, bottom, total, bottom * 100 / total)
        };
        let left = match &self.message {
            Some(m) => format!(" {m}"),
            None => format!(" {} ", self.title),
        };
        let left = if self.count.is_empty() {
            left
        } else {
            format!("{left} [{}]", self.count)
        };

        let w = self.w as usize;
        let rw = right.as_str().width();
        let mut bar = fit(&left, w.saturating_sub(rw));
        let used = bar.as_str().width();
        bar.push_str(&" ".repeat(w.saturating_sub(used + rw)));
        bar.push_str(&right);
        queue!(
            out,
            SetAttribute(Attribute::Reverse),
            Print(fit(&bar, w)),
            SetAttribute(Attribute::Reset)
        )?;
        Ok(())
    }

    fn draw_toc(&self, out: &mut io::Stdout) -> Result<()> {
        let Mode::Toc { sel, .. } = self.mode else { return Ok(()) };
        let headings = &self.doc.headings;
        // min-then-cap: on very narrow terminals the cap wins (a plain
        // `clamp(16, w-6)` would panic when w < 22).
        let inner_w = headings
            .iter()
            .map(|h| h.text.as_str().width() + (h.level as usize - 1) * 2)
            .max()
            .unwrap_or(10)
            .max(16)
            .min((self.w as usize).saturating_sub(6).max(4));
        let inner_h = headings.len().min(self.content_h().saturating_sub(2)).max(1);
        let scroll = sel.saturating_sub(inner_h.saturating_sub(1));

        let x0 = (self.w as usize).saturating_sub(inner_w + 4) / 2;
        let y0 = self.content_h().saturating_sub(inner_h + 2) / 2;
        self.draw_box(out, x0, y0, inner_w, inner_h, " Contents ")?;
        for row in 0..inner_h {
            let i = scroll + row;
            queue!(out, cursor::MoveTo((x0 + 2) as u16, (y0 + 1 + row) as u16))?;
            let Some(h) = headings.get(i) else { continue };
            let indent = "  ".repeat(h.level as usize - 1);
            let text = fit(&format!("{indent}{}", h.text), inner_w);
            let pad = inner_w.saturating_sub(text.as_str().width());
            if i == sel {
                queue!(out, SetAttribute(Attribute::Reverse))?;
            }
            queue!(out, Print(text), Print(" ".repeat(pad)), SetAttribute(Attribute::Reset))?;
        }
        Ok(())
    }

    fn draw_help(&self, out: &mut io::Stdout) -> Result<()> {
        let entries: &[(&str, &str)] = &[
            ("q", "quit"),
            ("j / k, ↓ / ↑", "scroll one line"),
            ("SPACE / b", "page down / up"),
            ("d / u", "half page down / up"),
            ("g / G", "go to top / bottom (Ng: line N)"),
            ("Np", "go to N percent"),
            ("/ ?", "search forward / backward"),
            ("n / N", "next / previous match"),
            ("] / [", "next / previous heading"),
            ("t", "table of contents"),
            ("v", "show diagrams rendered / as source"),
            ("o", "follow a link (labels appear)"),
            ("BKSP / ^o", "back to previous document"),
            ("h", "this help"),
        ];
        let key_w = 14;
        let desc_w = entries.iter().map(|(_, d)| d.width()).max().unwrap_or(20);
        let inner_w = (key_w + desc_w).min((self.w as usize).saturating_sub(6)).max(20);
        let inner_h = entries.len().min(self.content_h().saturating_sub(2));
        let x0 = (self.w as usize).saturating_sub(inner_w + 4) / 2;
        let y0 = self.content_h().saturating_sub(inner_h + 2) / 2;
        self.draw_box(out, x0, y0, inner_w, inner_h, " Keys ")?;
        for (row, (keys, what)) in entries.iter().take(inner_h).enumerate() {
            queue!(
                out,
                cursor::MoveTo((x0 + 2) as u16, (y0 + 1 + row) as u16),
                SetAttribute(Attribute::Bold),
                Print(format!("{keys:<key_w$}")),
                SetAttribute(Attribute::Reset),
                Print(fit(what, inner_w.saturating_sub(key_w)))
            )?;
        }
        Ok(())
    }

    fn draw_box(
        &self,
        out: &mut io::Stdout,
        x0: usize,
        y0: usize,
        inner_w: usize,
        inner_h: usize,
        title: &str,
    ) -> Result<()> {
        let top = {
            let t = fit(title, inner_w);
            let fill = inner_w + 2 - t.as_str().width();
            format!("┌{}{}┐", t, "─".repeat(fill))
        };
        queue!(out, cursor::MoveTo(x0 as u16, y0 as u16), Print(&top))?;
        for row in 0..inner_h {
            queue!(
                out,
                cursor::MoveTo(x0 as u16, (y0 + 1 + row) as u16),
                Print(format!("│ {} │", " ".repeat(inner_w)))
            )?;
        }
        queue!(
            out,
            cursor::MoveTo(x0 as u16, (y0 + 1 + inner_h) as u16),
            Print(format!("└{}┘", "─".repeat(inner_w + 2)))
        )?;
        Ok(())
    }
}

/// GitHub-style heading slug: lowercase, spaces to hyphens, punctuation
/// dropped.
fn slugify(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if c == ' ' || c == '-' || c == '_' {
            out.push(if c == '_' { '_' } else { '-' });
        }
    }
    out
}

/// Hands a URL to the OS opener, detached so the pager keeps its screen.
fn open_external(url: &str, message: &mut Option<String>) {
    let opener = if cfg!(target_os = "macos") {
        "open".to_string()
    } else {
        std::env::var("BROWSER").unwrap_or_else(|_| "xdg-open".to_string())
    };
    match std::process::Command::new(&opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            // Reap in the background so no zombie outlives the click.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            *message = Some(format!("Opened {url}"));
        }
        Err(err) => *message = Some(format!("{opener}: {err}")),
    }
}

fn term_size() -> (u16, u16) {
    let (w, h) = terminal::size().unwrap_or((80, 24));
    (if w == 0 { 80 } else { w }, if h == 0 { 24 } else { h })
}

/// Answers "does this terminal do kitty graphics with Unicode placeholders",
/// probing the terminal once per session (any protocol-capable terminal —
/// kitty, Ghostty, Konsole, … — answers the query; env vars are only the
/// fallback when the probe times out). WezTerm answers the query but does not
/// implement Unicode placeholders, so it is explicitly excluded.
fn graphics_probe() -> &'static crate::kitty::Probe {
    static PROBE: std::sync::OnceLock<crate::kitty::Probe> = std::sync::OnceLock::new();
    PROBE.get_or_init(|| crate::kitty::probe_terminal(std::time::Duration::from_millis(500)))
}

/// Kitty graphics need protocol support plus the terminal's cell pixel size
/// (window-size ioctl, or the probe's CSI 16t reply) to scale image grids.
fn detect_image_mode() -> ImageMode {
    let term = std::env::var("TERM").unwrap_or_default();
    let prog = std::env::var("TERM_PROGRAM").unwrap_or_default();
    if prog.eq_ignore_ascii_case("wezterm") || term.contains("wezterm") {
        return ImageMode::None;
    }
    let probe = graphics_probe();
    if !probe.graphics && !crate::kitty::env_hint() {
        return ImageMode::None;
    }
    let ioctl_cell = match terminal::window_size() {
        Ok(ws) if ws.columns > 0 && ws.rows > 0 && ws.width > 0 && ws.height > 0 => {
            Some(((ws.width / ws.columns).max(1), (ws.height / ws.rows).max(1)))
        }
        _ => None,
    };
    match ioctl_cell.or(probe.cell) {
        Some((cell_w, cell_h)) => ImageMode::Kitty { cell_w, cell_h },
        None => ImageMode::None,
    }
}

/// The configured left margin, shrunk toward 0 on terminals too narrow to
/// afford it while keeping at least 20 columns of text.
fn effective_margin(cfg: &Config, term_w: u16) -> u16 {
    (cfg.left_margin as u16).min(term_w.saturating_sub(20))
}

fn wrap_width(cfg: &Config, term_w: u16) -> usize {
    let usable = term_w.saturating_sub(effective_margin(cfg, term_w));
    cfg.wrap_width.min(usable as usize).max(20)
}

/// Truncates a string to at most `max` display columns.
fn fit(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cw > max {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out
}

/// Splits `text` (starting at `offset` bytes into the line) into segments that
/// are inside/outside the highlight ranges.
fn segment<'t>(
    text: &'t str,
    offset: usize,
    ranges: &[(usize, usize)],
) -> Vec<(&'t str, bool)> {
    if ranges.is_empty() {
        return vec![(text, false)];
    }
    let end = offset + text.len();
    let mut cuts = vec![0usize];
    for &(s, e) in ranges {
        for b in [s, e] {
            if b > offset && b < end {
                cuts.push(b - offset);
            }
        }
    }
    cuts.sort_unstable();
    cuts.dedup();
    cuts.push(text.len());

    let mut out = Vec::new();
    for pair in cuts.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if a == b {
            continue;
        }
        let abs = offset + a;
        let hl = ranges.iter().any(|&(s, e)| abs >= s && abs < e);
        out.push((&text[a..b], hl));
    }
    out
}

/// Writes `text` in `style`, using at most `budget` columns. Returns the
/// columns used and whether the whole text fit.
fn emit_span(
    out: &mut io::Stdout,
    text: &str,
    style: &Style,
    budget: usize,
) -> Result<(usize, bool)> {
    let mut used = 0;
    let mut buf = String::new();
    let mut complete = true;
    for ch in text.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cw > budget {
            complete = false;
            break;
        }
        buf.push(ch);
        used += cw;
    }
    if buf.is_empty() {
        return Ok((0, complete));
    }
    let osc8 = style.link.as_deref().filter(|u| !u.starts_with('#'));
    if let Some(url) = osc8 {
        queue!(out, Print(format!("\x1b]8;;{url}\x1b\\")))?;
    }
    if let Some(fg) = style.fg {
        queue!(out, SetForegroundColor(fg))?;
    }
    if style.bold {
        queue!(out, SetAttribute(Attribute::Bold))?;
    }
    if style.dim {
        queue!(out, SetAttribute(Attribute::Dim))?;
    }
    if style.italic {
        queue!(out, SetAttribute(Attribute::Italic))?;
    }
    if style.underline {
        queue!(out, SetAttribute(Attribute::Underlined))?;
    }
    if style.strikethrough {
        queue!(out, SetAttribute(Attribute::CrossedOut))?;
    }
    if style.reverse {
        queue!(out, SetAttribute(Attribute::Reverse))?;
    }
    queue!(out, Print(&buf), SetAttribute(Attribute::Reset))?;
    if osc8.is_some() {
        queue!(out, Print("\x1b]8;;\x1b\\"))?;
    }
    Ok((used, complete))
}

fn fold(ch: char) -> char {
    ch.to_lowercase().next().unwrap_or(ch)
}

/// Substring search over the rendered lines. Case-insensitive unless the query
/// contains an uppercase character (smartcase). Returns byte ranges.
pub fn find_matches(lines: &[Line], query: &str) -> Vec<(usize, usize, usize)> {
    let smart_ci = !query.chars().any(|c| c.is_uppercase());
    let needle: Vec<char> = if smart_ci {
        query.chars().map(fold).collect()
    } else {
        query.chars().collect()
    };
    if needle.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let hay = line.plain();
        let chars: Vec<(usize, usize, char)> = hay
            .char_indices()
            .map(|(b, c)| (b, b + c.len_utf8(), if smart_ci { fold(c) } else { c }))
            .collect();
        if chars.len() < needle.len() {
            continue;
        }
        let mut start = 0;
        while start + needle.len() <= chars.len() {
            let window = &chars[start..start + needle.len()];
            if window.iter().map(|t| t.2).eq(needle.iter().copied()) {
                out.push((i, window[0].0, window[window.len() - 1].1));
                start += needle.len();
            } else {
                start += 1;
            }
        }
    }
    out
}

fn nearest_match(matches: &[(usize, usize, usize)], top: usize) -> usize {
    matches.iter().position(|m| m.0 >= top).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::Span;

    fn line(s: &str) -> Line {
        let mut l = Line::default();
        l.push(Span::plain(s));
        l
    }

    #[test]
    fn smartcase_search() {
        let lines = vec![line("Hello World"), line("hello again")];
        let m = find_matches(&lines, "hello");
        assert_eq!(m.len(), 2);
        let m = find_matches(&lines, "Hello");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].0, 0);
    }

    #[test]
    fn match_offsets_are_byte_ranges() {
        let lines = vec![line("héllo héllo")];
        let m = find_matches(&lines, "héllo");
        assert_eq!(m.len(), 2);
        assert_eq!(&lines[0].plain()[m[0].1..m[0].2], "héllo");
    }

    #[test]
    fn segment_splits_on_ranges() {
        let segs = segment("abcdef", 0, &[(2, 4)]);
        assert_eq!(segs, vec![("ab", false), ("cd", true), ("ef", false)]);
    }

    #[test]
    fn segment_with_offset() {
        // Span starts at byte 10; range covers bytes 8..12 -> first 2 bytes lit.
        let segs = segment("abcdef", 10, &[(8, 12)]);
        assert_eq!(segs, vec![("ab", true), ("cdef", false)]);
    }
}
