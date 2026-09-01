use crate::text::{Line, Span, Style};
use crossterm::style::Color;
use pulldown_cmark::{
    Alignment, BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd,
};
use std::path::Path;
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const QUOTE_BAR: char = '▎';
const MAX_TABLE_CELL: usize = 40;
/// Tallest an inline image renders, in terminal rows.
const MAX_IMAGE_ROWS: u16 = 24;

/// How images are drawn.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ImageMode {
    /// No graphics: images render as "🖼 alt" hyperlinks.
    None,
    /// Kitty graphics protocol (Unicode placeholders), with the terminal's
    /// cell size in pixels for scaling.
    Kitty { cell_w: u16, cell_h: u16 },
}

/// An image queued for kitty-protocol transmission by the pager.
pub struct KittyImage {
    pub id: u32,
    pub png: Vec<u8>,
    pub cols: u16,
    pub rows: u16,
}
/// Soft blue for links; explicit RGB so it reads as blue regardless of how
/// the terminal theme tints ANSI bright-blue.
const LINK_COLOR: Color = Color::Rgb { r: 0x5f, g: 0xaf, b: 0xff };

pub struct Heading {
    pub line: usize,
    pub level: u8,
    pub text: String,
}

pub struct Document {
    pub lines: Vec<Line>,
    pub headings: Vec<Heading>,
    pub images: Vec<KittyImage>,
}

/// Owns the (expensive to load) syntect data so it survives re-renders.
pub struct Highlighter {
    syntaxes: SyntaxSet,
    theme: Theme,
}

impl Highlighter {
    pub fn new(theme_name: &str) -> Self {
        let syntaxes = SyntaxSet::load_defaults_newlines();
        let mut themes = ThemeSet::load_defaults();
        let theme = themes
            .themes
            .remove(theme_name)
            .or_else(|| themes.themes.remove("base16-ocean.dark"))
            .expect("syntect default themes missing");
        Self { syntaxes, theme }
    }
}

pub fn render(
    source: &str,
    width: usize,
    hl: &Highlighter,
    base: Option<&Path>,
    image_mode: ImageMode,
    diagrams: bool,
) -> Document {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_FOOTNOTES);
    opts.insert(Options::ENABLE_GFM); // admonitions: > [!NOTE] etc.

    let mut r = Renderer {
        width: width.max(20),
        hl,
        base,
        image_mode,
        diagrams,
        images: Vec::new(),
        lines: Vec::new(),
        headings: Vec::new(),
        inline: Vec::new(),
        styles: vec![Style::default()],
        prefixes: Vec::new(),
        list_stack: Vec::new(),
        table: None,
        code: None,
        image: None,
        at_item_start: false,
    };
    for event in Parser::new_ext(source, opts) {
        r.event(event);
    }
    r.flush_inline();
    while r.lines.last().is_some_and(|l| l.plain().trim().is_empty()) {
        r.lines.pop();
    }
    Document { lines: r.lines, headings: r.headings, images: r.images }
}

struct Prefix {
    first: String,
    rest: String,
    style: Style,
    first_done: bool,
}

struct TableState {
    aligns: Vec<Alignment>,
    head: Vec<Vec<Span>>,
    rows: Vec<Vec<Vec<Span>>>,
    cur_row: Vec<Vec<Span>>,
    in_head: bool,
}

struct CodeState {
    lang: String,
    buf: String,
}

/// Alt text and target collected between Start(Image) and End(Image).
struct ImageCapture {
    url: String,
    alt: String,
}

struct Renderer<'a> {
    width: usize,
    hl: &'a Highlighter,
    /// Directory the markdown file lives in; resolves relative image paths.
    base: Option<&'a Path>,
    image_mode: ImageMode,
    /// When false, mermaid/latex blocks stay syntax-highlighted code.
    diagrams: bool,
    images: Vec<KittyImage>,
    lines: Vec<Line>,
    headings: Vec<Heading>,
    inline: Vec<Span>,
    styles: Vec<Style>,
    prefixes: Vec<Prefix>,
    list_stack: Vec<Option<u64>>,
    table: Option<TableState>,
    code: Option<CodeState>,
    image: Option<ImageCapture>,
    at_item_start: bool,
}

/// Strips carriage returns and expands tabs; a raw `\t` reaching the terminal
/// advances to the next tab stop and desyncs all column accounting.
fn clean_inline(text: &str) -> String {
    text.replace('\r', "").replace('\t', "    ")
}

fn syn_to_style(syn: syntect::highlighting::Style) -> Style {
    let fg = syn.foreground;
    let mut style = Style::default().fg(Color::Rgb { r: fg.r, g: fg.g, b: fg.b });
    style.bold = syn.font_style.contains(FontStyle::BOLD);
    style.italic = syn.font_style.contains(FontStyle::ITALIC);
    style.underline = syn.font_style.contains(FontStyle::UNDERLINE);
    style
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn heading_style(level: u8) -> Style {
    let color = match level {
        1 => Color::Magenta,
        2 => Color::Cyan,
        3 => Color::Blue,
        4 => Color::Green,
        5 => Color::Yellow,
        _ => Color::White,
    };
    Style::default().fg(color).bold()
}

impl<'a> Renderer<'a> {
    fn cur_style(&self) -> Style {
        self.styles.last().cloned().unwrap_or_default()
    }

    fn avail(&self) -> usize {
        let used: usize = self.prefixes.iter().map(|p| p.rest.as_str().width()).sum();
        self.width.saturating_sub(used).max(10)
    }

    /// Marks the start of a block element: flushes pending inline content and
    /// inserts a separating blank line unless we just opened a list item.
    fn block_start(&mut self) {
        self.flush_inline();
        if self.at_item_start {
            self.at_item_start = false;
        } else {
            self.ensure_blank();
        }
    }

    fn ensure_blank(&mut self) {
        let Some(last) = self.lines.last() else { return };
        let plain = last.plain();
        // Already blank: empty, or only (possibly nested) quote bars.
        if plain.chars().all(|c| c.is_whitespace() || c == QUOTE_BAR) {
            return;
        }
        let mut line = Line::default();
        for p in &self.prefixes {
            if p.first_done {
                line.push(Span::new(p.rest.clone(), p.style.clone()));
            }
        }
        self.lines.push(line);
    }

    /// Emits one physical line, prepending the active prefixes.
    fn push_line(&mut self, content: Vec<Span>) {
        let mut line = Line::default();
        for p in &mut self.prefixes {
            if p.first_done {
                line.push(Span::new(p.rest.clone(), p.style.clone()));
            } else {
                line.push(Span::new(p.first.clone(), p.style.clone()));
                p.first_done = true;
            }
        }
        for span in content {
            line.push(span);
        }
        self.lines.push(line);
    }

    fn flush_inline(&mut self) {
        if self.inline.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.inline);
        let avail = self.avail();
        for wrapped in wrap_spans(&spans, avail) {
            self.push_line(wrapped);
        }
    }

    fn event(&mut self, event: Event) {
        // A code block swallows raw text until it closes.
        if let Some(code) = &mut self.code {
            match event {
                Event::Text(t) => {
                    code.buf.push_str(&t);
                    return;
                }
                Event::End(TagEnd::CodeBlock) => {
                    let code = self.code.take().unwrap();
                    self.emit_code(code);
                    return;
                }
                _ => return,
            }
        }

        // Between Start(Image) and End(Image), inline events feed the alt text.
        if self.image.is_some() {
            match event {
                Event::Text(t) | Event::Code(t) | Event::InlineHtml(t) => {
                    self.image.as_mut().unwrap().alt.push_str(&t);
                    return;
                }
                Event::SoftBreak | Event::HardBreak => {
                    self.image.as_mut().unwrap().alt.push(' ');
                    return;
                }
                Event::End(TagEnd::Image) => {
                    let cap = self.image.take().unwrap();
                    self.emit_image(cap);
                    return;
                }
                // Nested emphasis/links inside alt text: keep only the text.
                Event::Start(_) | Event::End(_) => return,
                _ => return,
            }
        }

        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(t) => {
                let text = clean_inline(&t);
                self.inline.push(Span::new(text, self.cur_style()));
            }
            Event::Code(t) => {
                let mut style = self.cur_style();
                style.fg = Some(Color::Yellow);
                self.inline.push(Span::new(clean_inline(&t), style));
            }
            Event::SoftBreak => self.inline.push(Span::new(" ", self.cur_style())),
            Event::HardBreak => self.inline.push(Span::new("\n", self.cur_style())),
            Event::Rule => {
                self.block_start();
                let rule = "─".repeat(self.avail());
                self.push_line(vec![Span::new(rule, Style::default().fg(Color::DarkGrey))]);
            }
            Event::TaskListMarker(done) => {
                let (mark, style) = if done {
                    ("☑ ", Style::default().fg(Color::Green))
                } else {
                    ("☐ ", Style::default().fg(Color::DarkGrey))
                };
                self.inline.push(Span::new(mark, style));
            }
            Event::FootnoteReference(name) => {
                let style = self.cur_style().fg(Color::DarkCyan);
                self.inline.push(Span::new(format!("[^{name}]"), style));
            }
            Event::Html(t) | Event::InlineHtml(t) => {
                let style = self.cur_style().dim();
                self.inline.push(Span::new(clean_inline(&t), style));
            }
            Event::InlineMath(t) | Event::DisplayMath(t) => {
                let style = self.cur_style().fg(Color::Yellow);
                self.inline.push(Span::new(clean_inline(&t), style));
            }
        }
    }

    fn start_tag(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => self.block_start(),
            Tag::Heading { level, .. } => {
                self.block_start();
                self.styles.push(heading_style(heading_level(level)));
            }
            Tag::BlockQuote(kind) => {
                self.block_start();
                // Emoji chosen from U+1F3xx+ (unambiguously wide) so column
                // accounting matches what terminals draw.
                let (color, title) = match kind {
                    Some(BlockQuoteKind::Note) => (Color::Blue, Some("📝 Note")),
                    Some(BlockQuoteKind::Tip) => (Color::Green, Some("💡 Tip")),
                    Some(BlockQuoteKind::Important) => (Color::Magenta, Some("📢 Important")),
                    Some(BlockQuoteKind::Warning) => (Color::Yellow, Some("🚨 Warning")),
                    Some(BlockQuoteKind::Caution) => (Color::Red, Some("🛑 Caution")),
                    None => (Color::DarkGreen, None),
                };
                self.prefixes.push(Prefix {
                    first: format!("{QUOTE_BAR} "),
                    rest: format!("{QUOTE_BAR} "),
                    style: Style::default().fg(color),
                    first_done: true,
                });
                if let Some(title) = title {
                    self.push_line(vec![Span::new(title, Style::default().fg(color).bold())]);
                    // Content follows directly under the title, no blank line.
                    self.at_item_start = true;
                }
            }
            Tag::CodeBlock(kind) => {
                self.block_start();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().unwrap_or("").to_string()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some(CodeState { lang, buf: String::new() });
            }
            Tag::List(start) => {
                self.flush_inline();
                if self.list_stack.is_empty() {
                    if self.at_item_start {
                        self.at_item_start = false;
                    } else {
                        self.ensure_blank();
                    }
                } else {
                    self.at_item_start = false;
                }
                self.list_stack.push(start);
            }
            Tag::Item => {
                self.flush_inline();
                let depth = self.list_stack.len();
                let marker = match self.list_stack.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        m
                    }
                    _ => {
                        let bullets = ["•", "◦", "▪"];
                        format!("{} ", bullets[depth.saturating_sub(1) % bullets.len()])
                    }
                };
                let rest = " ".repeat(marker.as_str().width());
                self.prefixes.push(Prefix {
                    first: marker,
                    rest,
                    style: Style::default().fg(Color::DarkCyan),
                    first_done: false,
                });
                self.at_item_start = true;
            }
            Tag::Table(aligns) => {
                self.block_start();
                self.table = Some(TableState {
                    aligns,
                    head: Vec::new(),
                    rows: Vec::new(),
                    cur_row: Vec::new(),
                    in_head: false,
                });
            }
            Tag::TableHead => {
                if let Some(t) = &mut self.table {
                    t.in_head = true;
                    t.cur_row.clear();
                }
            }
            Tag::TableRow => {
                if let Some(t) = &mut self.table {
                    t.cur_row.clear();
                }
            }
            Tag::TableCell => self.inline.clear(),
            Tag::Emphasis => {
                let mut s = self.cur_style();
                s.italic = true;
                self.styles.push(s);
            }
            Tag::Strong => {
                let mut s = self.cur_style();
                s.bold = true;
                self.styles.push(s);
            }
            Tag::Strikethrough => {
                let mut s = self.cur_style();
                s.strikethrough = true;
                self.styles.push(s);
            }
            Tag::Link { dest_url, .. } => {
                let mut s = self.cur_style();
                s.fg = Some(LINK_COLOR);
                s.underline = true;
                s.link = Some(dest_url.to_string());
                self.styles.push(s);
            }
            Tag::Image { dest_url, .. } => {
                self.image = Some(ImageCapture { url: dest_url.to_string(), alt: String::new() });
            }
            Tag::FootnoteDefinition(name) => {
                self.block_start();
                let marker = format!("[^{name}] ");
                let rest = " ".repeat(marker.as_str().width());
                self.prefixes.push(Prefix {
                    first: marker,
                    rest,
                    style: Style::default().fg(Color::DarkCyan),
                    first_done: false,
                });
                self.at_item_start = true;
            }
            Tag::HtmlBlock => self.block_start(),
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush_inline(),
            TagEnd::Heading(level) => {
                let lv = heading_level(level);
                let text: String = self.inline.iter().map(|s| s.text.as_str()).collect();
                let text_width = text.as_str().width();
                let start = self.lines.len();
                self.flush_inline();
                self.styles.pop();
                self.headings.push(Heading { line: start, level: lv, text });
                if lv <= 2 {
                    let ch = if lv == 1 { "━" } else { "─" };
                    let w = text_width.clamp(1, self.avail());
                    let mut style = heading_style(lv);
                    style.bold = false;
                    style.dim = lv == 2;
                    self.push_line(vec![Span::new(ch.repeat(w), style)]);
                }
            }
            TagEnd::BlockQuote(_) => {
                self.flush_inline();
                self.prefixes.pop();
            }
            TagEnd::List(_) => {
                self.list_stack.pop();
            }
            TagEnd::Item => {
                self.flush_inline();
                self.prefixes.pop();
                self.at_item_start = false;
            }
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    self.emit_table(t);
                }
            }
            TagEnd::TableHead => {
                if let Some(t) = &mut self.table {
                    t.head = std::mem::take(&mut t.cur_row);
                    t.in_head = false;
                }
            }
            TagEnd::TableRow => {
                if let Some(t) = &mut self.table {
                    let row = std::mem::take(&mut t.cur_row);
                    t.rows.push(row);
                }
            }
            TagEnd::TableCell => {
                let spans = std::mem::take(&mut self.inline);
                if let Some(t) = &mut self.table {
                    t.cur_row.push(spans);
                }
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                self.styles.pop();
            }
            TagEnd::Image => {} // handled by the image intercept in event()
            TagEnd::FootnoteDefinition => {
                self.flush_inline();
                self.prefixes.pop();
                self.at_item_start = false;
            }
            TagEnd::HtmlBlock => self.flush_inline(),
            _ => {}
        }
    }

    /// Emits an image as kitty Unicode-placeholder cells when the terminal
    /// supports the graphics protocol; otherwise as a "🖼 alt" hyperlink.
    fn emit_image(&mut self, cap: ImageCapture) {
        match self.prepare_kitty_image(&cap.url) {
            Some((id, cols, rows)) => {
                self.flush_inline();
                self.push_placeholder_block(id, cols, rows);
                let mut caption = Style::default().dim();
                caption.link = Some(cap.url);
                let alt = if cap.alt.is_empty() { "image" } else { &cap.alt };
                self.push_line(vec![Span::new(format!("🖼 {alt}"), caption)]);
            }
            None => {
                let mut s = self.cur_style();
                s.fg = Some(LINK_COLOR);
                s.underline = true;
                s.link = Some(cap.url);
                self.inline.push(Span::new(format!("🖼 {}", cap.alt), s));
            }
        }
    }

    /// Emits the placeholder cell grid for a queued image.
    fn push_placeholder_block(&mut self, id: u32, cols: u16, rows: u16) {
        // Image id travels in the 24-bit foreground color.
        let id_color = Color::Rgb {
            r: ((id >> 16) & 0xff) as u8,
            g: ((id >> 8) & 0xff) as u8,
            b: (id & 0xff) as u8,
        };
        for row in 0..rows as usize {
            let mut text = String::new();
            for col in 0..cols as usize {
                match crate::kitty::placeholder_cell(row, col) {
                    Some(cell) => text.push_str(&cell),
                    None => break,
                }
            }
            self.push_line(vec![Span::new(text, Style::default().fg(id_color))]);
        }
    }

    /// Loads a local image, sizes its cell grid, and queues it for protocol
    /// transmission. Returns (id, cols, rows), or None to fall back to a link.
    fn prepare_kitty_image(&mut self, url: &str) -> Option<(u32, u16, u16)> {
        if !matches!(self.image_mode, ImageMode::Kitty { .. }) {
            return None;
        }
        if url.contains("://") {
            return None; // remote images fall back to a link
        }
        let path = Path::new(url);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.base?.join(path)
        };
        let bytes = std::fs::read(&path).ok()?;
        // kitty renders PNG (f=100); re-encode other formats.
        let png = if image::guess_format(&bytes).ok() == Some(image::ImageFormat::Png) {
            bytes
        } else {
            let img = image::load_from_memory(&bytes).ok()?;
            let mut buf = std::io::Cursor::new(Vec::new());
            img.write_to(&mut buf, image::ImageFormat::Png).ok()?;
            buf.into_inner()
        };
        self.queue_kitty_image(png)
    }

    /// Sizes a PNG's cell grid and queues it for transmission.
    fn queue_kitty_image(&mut self, png: Vec<u8>) -> Option<(u32, u16, u16)> {
        let ImageMode::Kitty { cell_w, cell_h } = self.image_mode else {
            return None;
        };
        let (px_w, px_h) = image::load_from_memory(&png)
            .ok()
            .map(|i| (i.width().max(1) as f64, i.height().max(1) as f64))?;

        // Fit the cell grid to the wrap width and a height cap, never
        // upscaling beyond the image's native pixel size.
        let (cell_w, cell_h) = (cell_w.max(1) as f64, cell_h.max(1) as f64);
        let scale = (self.avail() as f64 * cell_w / px_w)
            .min(MAX_IMAGE_ROWS as f64 * cell_h / px_h)
            .min(1.0);
        let cols = ((px_w * scale / cell_w).ceil() as u16).max(1);
        let rows = ((px_h * scale / cell_h).ceil() as u16).max(1);

        // Content-derived id (24-bit, nonzero): re-renders that renumber or
        // add images keep ids stable per content, so the pager's
        // already-transmitted bookkeeping never pairs an old transmit with
        // different pixels.
        let id = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            png.hash(&mut h);
            ((h.finish() % 0xFF_FFFE) + 1) as u32
        };
        if let Some(existing) = self.images.iter().find(|i| i.id == id) {
            // Same content appears twice; one virtual placement serves both.
            return Some((id, existing.cols, existing.rows));
        }
        self.images.push(KittyImage { id, png, cols, rows });
        Some((id, cols, rows))
    }

    /// Renders a ```mermaid / ```latex block as an inline image. Returns
    /// false (leaving the block untouched) when the terminal lacks graphics
    /// support or the diagram service is unreachable.
    fn try_emit_diagram(&mut self, code: &CodeState) -> bool {
        if !self.diagrams || !matches!(self.image_mode, ImageMode::Kitty { .. }) {
            return false;
        }
        let png = match code.lang.as_str() {
            "mermaid" => crate::diagram::mermaid_png(&code.buf),
            "latex" | "tex" | "math" | "katex" => crate::diagram::latex_png(&code.buf),
            _ => None,
        };
        let Some(png) = png else { return false };
        let Some((id, cols, rows)) = self.queue_kitty_image(png) else {
            return false;
        };
        self.push_placeholder_block(id, cols, rows);
        true
    }

    fn emit_code(&mut self, code: CodeState) {
        if self.try_emit_diagram(&code) {
            return;
        }
        let syntax = self
            .hl
            .syntaxes
            .find_syntax_by_token(&code.lang)
            .unwrap_or_else(|| self.hl.syntaxes.find_syntax_plain_text());
        let mut highlighter = HighlightLines::new(syntax, &self.hl.theme);
        for raw in code.buf.lines() {
            let line = raw.replace('\t', "    ");
            let mut spans = vec![Span::plain("  ")];
            let with_nl = format!("{line}\n");
            match highlighter.highlight_line(&with_nl, &self.hl.syntaxes) {
                Ok(ranges) => {
                    for (syn_style, text) in ranges {
                        let text = text.trim_end_matches('\n');
                        if text.is_empty() {
                            continue;
                        }
                        spans.push(Span::new(text, syn_to_style(syn_style)));
                    }
                }
                Err(_) => spans.push(Span::plain(line.clone())),
            }
            self.push_line(spans);
        }
    }

    fn emit_table(&mut self, t: TableState) {
        let border = Style::default().fg(Color::DarkGrey);
        let ncols = t
            .aligns
            .len()
            .max(t.head.len())
            .max(t.rows.iter().map(|r| r.len()).max().unwrap_or(0));
        if ncols == 0 {
            return;
        }

        let mut widths = vec![1usize; ncols];
        for row in std::iter::once(&t.head).chain(t.rows.iter()) {
            for (i, cell) in row.iter().enumerate() {
                let w: usize = cell.iter().map(|s| s.width()).sum();
                widths[i] = widths[i].max(w.min(MAX_TABLE_CELL));
            }
        }
        // Shrink the widest columns until the table fits (or columns bottom out).
        let avail = self.avail();
        loop {
            let total: usize = widths.iter().sum::<usize>() + 3 * ncols + 1;
            if total <= avail {
                break;
            }
            let Some(widest) = widths
                .iter()
                .enumerate()
                .max_by_key(|(_, w)| **w)
                .filter(|(_, w)| **w > 5)
                .map(|(i, _)| i)
            else {
                break;
            };
            widths[widest] -= 1;
        }

        let hborder = |l: &str, m: &str, r: &str| -> String {
            let mut s = String::from(l);
            for (i, w) in widths.iter().enumerate() {
                s.push_str(&"─".repeat(w + 2));
                s.push_str(if i + 1 == ncols { r } else { m });
            }
            s
        };

        self.push_line(vec![Span::new(hborder("┌", "┬", "┐"), border.clone())]);
        if !t.head.is_empty() {
            let row = self.table_row(&t.head, &t.aligns, &widths, true, &border);
            self.push_line(row);
            self.push_line(vec![Span::new(hborder("├", "┼", "┤"), border.clone())]);
        }
        for row in &t.rows {
            let row = self.table_row(row, &t.aligns, &widths, false, &border);
            self.push_line(row);
        }
        self.push_line(vec![Span::new(hborder("└", "┴", "┘"), border)]);
    }

    fn table_row(
        &self,
        cells: &[Vec<Span>],
        aligns: &[Alignment],
        widths: &[usize],
        bold: bool,
        border: &Style,
    ) -> Vec<Span> {
        let mut out = Vec::new();
        for (i, width) in widths.iter().enumerate() {
            out.push(Span::new(if i == 0 { "│ " } else { " │ " }, border.clone()));
            let empty = Vec::new();
            let cell = cells.get(i).unwrap_or(&empty);
            let (mut spans, used) = truncate_spans(cell, *width);
            if bold {
                for s in &mut spans {
                    s.style.bold = true;
                }
            }
            let pad = width.saturating_sub(used);
            let (left, right) = match aligns.get(i) {
                Some(Alignment::Right) => (pad, 0),
                Some(Alignment::Center) => (pad / 2, pad - pad / 2),
                _ => (0, pad),
            };
            out.push(Span::plain(" ".repeat(left)));
            out.extend(spans);
            out.push(Span::plain(" ".repeat(right)));
        }
        out.push(Span::new(" │", border.clone()));
        out
    }
}

/// Cuts a styled cell down to `max` columns, appending `…` when truncated.
fn truncate_spans(spans: &[Span], max: usize) -> (Vec<Span>, usize) {
    let total: usize = spans.iter().map(|s| s.width()).sum();
    if total <= max {
        let cleaned: Vec<Span> = spans
            .iter()
            .map(|s| Span::new(s.text.replace('\n', " "), s.style.clone()))
            .collect();
        return (cleaned, total);
    }
    let budget = max.saturating_sub(1); // room for the ellipsis
    let mut out = Vec::new();
    let mut used = 0;
    'outer: for span in spans {
        let mut buf = String::new();
        for ch in span.text.chars() {
            let ch = if ch == '\n' { ' ' } else { ch };
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + cw > budget {
                if !buf.is_empty() {
                    out.push(Span::new(buf, span.style.clone()));
                }
                out.push(Span::new("…", Style::default().dim()));
                used += 1;
                break 'outer;
            }
            buf.push(ch);
            used += cw;
        }
        if !buf.is_empty() {
            out.push(Span::new(buf, span.style.clone()));
        }
    }
    (out, used)
}

enum Tok {
    Word(Vec<Span>),
    Space(Style),
    Break,
}

fn tokenize(spans: &[Span]) -> Vec<Tok> {
    let mut toks: Vec<Tok> = Vec::new();
    for span in spans {
        let mut buf = String::new();
        for ch in span.text.chars() {
            if ch == '\n' {
                flush_frag(&mut toks, &mut buf, &span.style);
                toks.push(Tok::Break);
            } else if ch.is_whitespace() {
                flush_frag(&mut toks, &mut buf, &span.style);
                if !matches!(toks.last(), Some(Tok::Space(_) | Tok::Break) | None) {
                    toks.push(Tok::Space(span.style.clone()));
                }
            } else {
                buf.push(ch);
            }
        }
        flush_frag(&mut toks, &mut buf, &span.style);
    }
    toks
}

fn flush_frag(toks: &mut Vec<Tok>, buf: &mut String, style: &Style) {
    if buf.is_empty() {
        return;
    }
    let frag = Span::new(std::mem::take(buf), style.clone());
    if let Some(Tok::Word(frags)) = toks.last_mut() {
        frags.push(frag);
    } else {
        toks.push(Tok::Word(vec![frag]));
    }
}

/// Greedy word-wrap over styled spans. Words split across style boundaries
/// stay glued together; words longer than the width are hard-broken.
pub fn wrap_spans(spans: &[Span], width: usize) -> Vec<Vec<Span>> {
    let width = width.max(1);
    let mut out: Vec<Vec<Span>> = Vec::new();
    let mut cur: Vec<Span> = Vec::new();
    let mut curw = 0usize;

    let finish = |out: &mut Vec<Vec<Span>>, cur: &mut Vec<Span>, curw: &mut usize| {
        while cur.last().is_some_and(|s| s.text == " ") {
            cur.pop();
            *curw -= 1;
        }
        out.push(std::mem::take(cur));
        *curw = 0;
    };

    for tok in tokenize(spans) {
        match tok {
            Tok::Break => finish(&mut out, &mut cur, &mut curw),
            Tok::Space(style) => {
                if curw > 0 && curw < width {
                    cur.push(Span::new(" ", style));
                    curw += 1;
                }
            }
            Tok::Word(frags) => {
                let w: usize = frags.iter().map(|f| f.width()).sum();
                if curw > 0 && curw + w > width {
                    finish(&mut out, &mut cur, &mut curw);
                }
                if w > width {
                    for frag in frags {
                        for ch in frag.text.chars() {
                            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
                            if curw + cw > width && curw > 0 {
                                finish(&mut out, &mut cur, &mut curw);
                            }
                            match cur.last_mut() {
                                Some(last) if last.style == frag.style => last.text.push(ch),
                                _ => cur.push(Span::new(ch.to_string(), frag.style.clone())),
                            }
                            curw += cw;
                        }
                    }
                } else {
                    cur.extend(frags);
                    curw += w;
                }
            }
        }
    }
    if !cur.is_empty() {
        finish(&mut out, &mut cur, &mut curw);
    }
    if out.is_empty() {
        out.push(Vec::new());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Vec<Span>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.iter().map(|s| s.text.as_str()).collect())
            .collect()
    }

    #[test]
    fn wraps_at_width() {
        let spans = [Span::plain("the quick brown fox jumps over the lazy dog")];
        let wrapped = wrap_spans(&spans, 10);
        for line in plain(&wrapped) {
            assert!(line.len() <= 10, "line too long: {line:?}");
        }
        assert_eq!(plain(&wrapped).join(" "), "the quick brown fox jumps over the lazy dog");
    }

    #[test]
    fn keeps_styled_word_together() {
        let bold = Style::default().bold();
        let spans = [
            Span::plain("aaa bb"),
            Span::new("old", bold),
            Span::plain(" cc"),
        ];
        let wrapped = wrap_spans(&spans, 6);
        let lines = plain(&wrapped);
        assert_eq!(lines, vec!["aaa", "bbold", "cc"]);
    }

    #[test]
    fn hard_breaks_long_words() {
        let spans = [Span::plain("abcdefghij")];
        let wrapped = wrap_spans(&spans, 4);
        assert_eq!(plain(&wrapped), vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn renders_headings_into_index() {
        let hl = Highlighter::new("base16-ocean.dark");
        let doc = render("# One\n\ntext\n\n## Two\n", 80, &hl, None, ImageMode::None, true);
        assert_eq!(doc.headings.len(), 2);
        assert_eq!(doc.headings[0].text, "One");
        assert_eq!(doc.headings[1].level, 2);
    }

    #[test]
    fn expands_tabs_in_inline_text() {
        let hl = Highlighter::new("base16-ocean.dark");
        let doc = render("a\tb and `c\td`\n", 80, &hl, None, ImageMode::None, true);
        for line in &doc.lines {
            assert!(!line.plain().contains('\t'), "raw tab in {:?}", line.plain());
        }
    }

    #[test]
    fn no_double_blank_between_nested_quote_paragraphs() {
        let hl = Highlighter::new("base16-ocean.dark");
        let doc = render("> > one\n> >\n> > two\n", 80, &hl, None, ImageMode::None, true);
        let blanks = doc
            .lines
            .iter()
            .filter(|l| l.plain().chars().all(|c| c.is_whitespace() || c == QUOTE_BAR))
            .count();
        assert_eq!(blanks, 1, "{:?}", doc.lines.iter().map(|l| l.plain()).collect::<Vec<_>>());
    }

    #[test]
    fn renders_admonition_title() {
        let hl = Highlighter::new("base16-ocean.dark");
        let doc = render("> [!WARNING]\n> Careful here.\n", 80, &hl, None, ImageMode::None, true);
        let text: Vec<String> = doc.lines.iter().map(|l| l.plain()).collect();
        assert!(text.iter().any(|l| l.contains("Warning")), "{text:?}");
        assert!(text.iter().any(|l| l.contains("Careful here.")), "{text:?}");
        // Title line and body both carry the quote bar.
        assert!(text.iter().filter(|l| l.contains(QUOTE_BAR)).count() >= 2);
    }

    #[test]
    fn kitty_mode_emits_placeholders_and_queues_image() {
        let dir = std::env::temp_dir().join("mdview-test-img");
        std::fs::create_dir_all(&dir).unwrap();
        // 32x64 px at 8x16 cells -> a 4x4 placeholder grid.
        let img = image::RgbaImage::from_pixel(32, 64, image::Rgba([255, 0, 0, 255]));
        img.save(dir.join("red.png")).unwrap();

        let hl = Highlighter::new("base16-ocean.dark");
        let kitty = ImageMode::Kitty { cell_w: 8, cell_h: 16 };
        let doc = render("![a red square](red.png)\n", 80, &hl, Some(&dir), kitty, true);
        assert_eq!(doc.images.len(), 1);
        let img = &doc.images[0];
        assert_eq!((img.cols, img.rows), (4, 4));
        // Content-derived id: 24-bit, nonzero, stable across renders.
        assert!(img.id > 0 && img.id <= 0xFF_FFFF);
        let again = render("![a red square](red.png)\n", 80, &hl, Some(&dir), kitty, true);
        assert_eq!(again.images[0].id, img.id);
        assert!(!img.png.is_empty());
        let placeholder_rows = doc
            .lines
            .iter()
            .filter(|l| l.plain().contains(crate::kitty::PLACEHOLDER))
            .count();
        assert_eq!(placeholder_rows, 4);
        let text: Vec<String> = doc.lines.iter().map(|l| l.plain()).collect();
        assert!(text.iter().any(|l| l.contains("a red square")), "{text:?}");

        // Remote URLs and ImageMode::None fall back to a link.
        let doc = render("![remote](https://example.com/x.png)\n", 80, &hl, Some(&dir), kitty, true);
        assert!(doc.images.is_empty());
        assert!(doc.lines.iter().any(|l| l.plain().contains("🖼 remote")));
        let doc = render("![local](red.png)\n", 80, &hl, Some(&dir), ImageMode::None, true);
        assert!(doc.images.is_empty());
        assert!(doc.lines.iter().any(|l| l.plain().contains("🖼 local")));
    }

    #[test]
    fn mermaid_block_stays_code_without_graphics() {
        let hl = Highlighter::new("base16-ocean.dark");
        let doc = render(
            "```mermaid\nflowchart LR\n  A --> B\n```\n",
            80,
            &hl,
            None,
            ImageMode::None,
            true,
        );
        assert!(doc.images.is_empty());
        assert!(doc.lines.iter().any(|l| l.plain().contains("A --> B")));
    }

    #[test]
    fn kitty_escape_sequences_are_well_formed() {
        let mut buf = Vec::new();
        crate::kitty::transmit(&mut buf, 3, b"12345").unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.starts_with("\x1b_Ga=t,i=3,f=100,t=d,q=2,m=0;"), "{s:?}");
        assert!(s.ends_with("\x1b\\"));

        let mut buf = Vec::new();
        crate::kitty::place(&mut buf, 3, 10, 5).unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "\x1b_Ga=p,U=1,i=3,p=1,c=10,r=5,q=2\x1b\\"
        );

        let cell = crate::kitty::placeholder_cell(0, 1).unwrap();
        let chars: Vec<char> = cell.chars().collect();
        assert_eq!(chars[0], crate::kitty::PLACEHOLDER);
        assert_eq!(chars[1], '\u{0305}'); // row 0
        assert_eq!(chars[2], '\u{030D}'); // col 1
    }

    #[test]
    fn reflows_paragraph_to_width() {
        let hl = Highlighter::new("base16-ocean.dark");
        let text = "words ".repeat(40);
        let doc = render(&text, 30, &hl, None, ImageMode::None, true);
        for line in &doc.lines {
            assert!(line.width() <= 30, "too wide: {:?}", line.plain());
        }
    }
}
