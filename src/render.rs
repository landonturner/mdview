use crate::text::{Line, Span, Style};
use crossterm::style::Color;
use pulldown_cmark::{
    Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd,
};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const QUOTE_BAR: char = '▎';
const MAX_TABLE_CELL: usize = 40;
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

pub fn render(source: &str, width: usize, hl: &Highlighter) -> Document {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_FOOTNOTES);

    let mut r = Renderer {
        width: width.max(20),
        hl,
        lines: Vec::new(),
        headings: Vec::new(),
        inline: Vec::new(),
        styles: vec![Style::default()],
        prefixes: Vec::new(),
        list_stack: Vec::new(),
        table: None,
        code: None,
        at_item_start: false,
    };
    for event in Parser::new_ext(source, opts) {
        r.event(event);
    }
    r.flush_inline();
    while r.lines.last().is_some_and(|l| l.plain().trim().is_empty()) {
        r.lines.pop();
    }
    Document { lines: r.lines, headings: r.headings }
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

struct Renderer<'a> {
    width: usize,
    hl: &'a Highlighter,
    lines: Vec<Line>,
    headings: Vec<Heading>,
    inline: Vec<Span>,
    styles: Vec<Style>,
    prefixes: Vec<Prefix>,
    list_stack: Vec<Option<u64>>,
    table: Option<TableState>,
    code: Option<CodeState>,
    at_item_start: bool,
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
        if plain.trim().chars().all(|c| c == QUOTE_BAR) {
            return; // already blank (possibly just a quote bar)
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

        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(t) => {
                let text = t.replace('\r', "");
                self.inline.push(Span::new(text, self.cur_style()));
            }
            Event::Code(t) => {
                let mut style = self.cur_style();
                style.fg = Some(Color::Yellow);
                self.inline.push(Span::new(t.into_string(), style));
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
                self.inline.push(Span::new(t.replace('\r', ""), style));
            }
            Event::InlineMath(t) | Event::DisplayMath(t) => {
                let style = self.cur_style().fg(Color::Yellow);
                self.inline.push(Span::new(t.into_string(), style));
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
            Tag::BlockQuote(_) => {
                self.block_start();
                self.prefixes.push(Prefix {
                    first: format!("{QUOTE_BAR} "),
                    rest: format!("{QUOTE_BAR} "),
                    style: Style::default().fg(Color::DarkGreen),
                    first_done: true,
                });
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
                let mut s = self.cur_style();
                s.fg = Some(LINK_COLOR);
                s.underline = true;
                s.link = Some(dest_url.to_string());
                self.styles.push(s.clone());
                self.inline.push(Span::new("🖼 ", s));
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
            TagEnd::Emphasis
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::Link
            | TagEnd::Image => {
                self.styles.pop();
            }
            TagEnd::FootnoteDefinition => {
                self.flush_inline();
                self.prefixes.pop();
                self.at_item_start = false;
            }
            TagEnd::HtmlBlock => self.flush_inline(),
            _ => {}
        }
    }

    fn emit_code(&mut self, code: CodeState) {
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
                        let fg = syn_style.foreground;
                        let mut style = Style::default().fg(Color::Rgb {
                            r: fg.r,
                            g: fg.g,
                            b: fg.b,
                        });
                        style.bold = syn_style.font_style.contains(FontStyle::BOLD);
                        style.italic = syn_style.font_style.contains(FontStyle::ITALIC);
                        style.underline = syn_style.font_style.contains(FontStyle::UNDERLINE);
                        spans.push(Span::new(text, style));
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
        let doc = render("# One\n\ntext\n\n## Two\n", 80, &hl);
        assert_eq!(doc.headings.len(), 2);
        assert_eq!(doc.headings[0].text, "One");
        assert_eq!(doc.headings[1].level, 2);
    }

    #[test]
    fn reflows_paragraph_to_width() {
        let hl = Highlighter::new("base16-ocean.dark");
        let text = "words ".repeat(40);
        let doc = render(&text, 30, &hl);
        for line in &doc.lines {
            assert!(line.width() <= 30, "too wide: {:?}", line.plain());
        }
    }
}
