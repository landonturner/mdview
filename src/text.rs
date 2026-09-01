use crossterm::style::Color;
use unicode_width::UnicodeWidthStr;

/// Visual styling for a run of text, including an optional OSC 8 hyperlink target.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Style {
    pub fg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub reverse: bool,
    pub link: Option<String>,
}

impl Style {
    pub fn fg(mut self, color: Color) -> Self {
        self.fg = Some(color);
        self
    }

    pub fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    pub fn dim(mut self) -> Self {
        self.dim = true;
        self
    }
}

#[derive(Clone, Debug)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

impl Span {
    pub fn new(text: impl Into<String>, style: Style) -> Self {
        Self { text: text.into(), style }
    }

    pub fn plain(text: impl Into<String>) -> Self {
        Self::new(text, Style::default())
    }

    pub fn width(&self) -> usize {
        self.text.as_str().width()
    }
}

/// One physical (already wrapped) line of rendered output.
#[derive(Clone, Debug, Default)]
pub struct Line {
    pub spans: Vec<Span>,
}

impl Line {
    pub fn push(&mut self, span: Span) {
        if span.text.is_empty() {
            return;
        }
        match self.spans.last_mut() {
            Some(last) if last.style == span.style => last.text.push_str(&span.text),
            _ => self.spans.push(span),
        }
    }

    pub fn plain(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }

    #[allow(dead_code)] // used by render tests
    pub fn width(&self) -> usize {
        self.spans.iter().map(|s| s.width()).sum()
    }
}
