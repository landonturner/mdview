mod config;
mod diagram;
mod kitty;
mod pager;
mod render;
mod text;

use anyhow::{bail, Context, Result};
use crossterm::style::Color;
use std::io::{IsTerminal, Read, Write};
use text::{Line, Style};

const USAGE: &str = "\
mdview - a less-style pager for markdown files

Usage: mdview [OPTIONS] [FILE]
       command | mdview

Options:
  -w, --width <N>       Reflow paragraphs to at most N columns (default 120,
                        or `wrap_width` in the config file); 0 or `auto`
                        wraps at the terminal width and re-wraps on resize
  -d, --dump            Print the rendered document to stdout and exit
  -c, --config          Open the config file in $EDITOR (creating it with
                        defaults first if needed)
      --clear-cache     Delete the rendered-diagram cache (mermaid/LaTeX)
  -h, --help            Show this help
  -V, --version         Show version

Config file: ~/.config/mdview/config.toml
  wrap_width = 120
  code_theme = \"base16-ocean.dark\"
  default_view = \"rendered\"   # or \"text\": show diagram blocks as source

Press h inside the pager for key bindings.";

/// `--width` value: a column count, or 0 / `auto` for the terminal width.
fn parse_width(v: &str) -> Result<usize> {
    if v.eq_ignore_ascii_case("auto") {
        return Ok(0);
    }
    v.parse().context("--width must be a number or `auto`")
}

struct Args {
    file: Option<String>,
    width: Option<usize>,
    dump: bool,
    config: bool,
    clear_cache: bool,
}

fn parse_args() -> Result<Args> {
    let mut args =
        Args { file: None, width: None, dump: false, config: false, clear_cache: false };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("mdview {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-d" | "--dump" => args.dump = true,
            "-c" | "--config" => args.config = true,
            "--clear-cache" => args.clear_cache = true,
            "-w" | "--width" => {
                let v = it.next().context("--width requires a value")?;
                args.width = Some(parse_width(&v)?);
            }
            _ if arg.starts_with("--width=") => {
                args.width = Some(parse_width(&arg["--width=".len()..])?);
            }
            _ if arg.starts_with('-') && arg != "-" => bail!("unknown option: {arg}\n\n{USAGE}"),
            _ => {
                if args.file.is_some() {
                    bail!("only one file may be given\n\n{USAGE}");
                }
                args.file = Some(arg);
            }
        }
    }
    Ok(args)
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let mut cfg = config::load();

    if args.clear_cache {
        match diagram::clear_cache()? {
            Some(dir) => println!("removed {}", dir.display()),
            None => println!("diagram cache is already empty"),
        }
        return Ok(());
    }

    if args.config {
        return run_config();
    }

    if let Some(w) = args.width {
        cfg.wrap_width = if w == 0 { 0 } else { w.max(20) };
    }

    let (source, title, base) = match &args.file {
        Some(path) if path != "-" => {
            let contents = std::fs::read_to_string(path).with_context(|| format!("cannot read {path}"))?;
            let p = std::path::Path::new(path);
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.clone());
            let base = p
                .canonicalize()
                .ok()
                .and_then(|c| c.parent().map(|d| d.to_path_buf()));
            (contents, name, base)
        }
        _ => {
            if std::io::stdin().is_terminal() {
                eprintln!("{USAGE}");
                std::process::exit(2);
            }
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf).context("reading stdin")?;
            (buf, "(stdin)".to_string(), std::env::current_dir().ok())
        }
    };
    let base = base.as_deref();

    // Resolve light/dark before building anything color-dependent. The probe
    // needs a controlling tty; without one (cron, CI) fall back to dark. The
    // plain non-TTY path strips colors anyway, so don't pay the probe there.
    let interactive = std::io::stdout().is_terminal();
    let theme = match cfg.theme.as_str() {
        "dark" => render::TermTheme::Dark,
        "light" => render::TermTheme::Light,
        _ if interactive || args.dump => match kitty::probe().bg {
            Some((r, g, b)) => {
                let luma = 0.299 * r as f64 + 0.587 * g as f64 + 0.114 * b as f64;
                if luma > 128.0 { render::TermTheme::Light } else { render::TermTheme::Dark }
            }
            None => render::TermTheme::Dark,
        },
        _ => render::TermTheme::Dark,
    };
    let code_theme = cfg.code_theme.clone().unwrap_or_else(|| {
        match theme {
            render::TermTheme::Dark => "base16-ocean.dark",
            render::TermTheme::Light => "InspiredGitHub",
        }
        .to_string()
    });
    let hl = render::Highlighter::new(&code_theme);

    if args.dump || !std::io::stdout().is_terminal() {
        let doc = render::render(
            &source,
            &hl,
            &render::RenderOpts {
                width: dump_width(&cfg),
                base,
                image_mode: render::ImageMode::None,
                diagrams: true,
                resolve_links: args.file.is_some(),
                theme,
            },
        );
        let mut out = std::io::BufWriter::new(std::io::stdout().lock());
        // --dump mirrors what the pager shows, margin included; the plain
        // fallback stays flush-left so `mdview foo.md | grep ...` output is
        // predictable.
        let margin = " ".repeat(cfg.left_margin.min(16));
        for line in &doc.lines {
            if args.dump {
                let pad = if line.spans.is_empty() { "" } else { margin.as_str() };
                writeln!(out, "{pad}{}", line_ansi(line))?;
            } else {
                writeln!(out, "{}", line.plain())?;
            }
        }
        return Ok(());
    }

    pager::install_panic_hook();
    // stdin's base is only a cwd guess, so relative links stay unresolved
    // there rather than pointing at the wrong files.
    let resolve_links = args.file.is_some();
    pager::run(&source, &title, &cfg, &hl, base, resolve_links, theme)
}

/// Handles `--config`: opens the config file in $EDITOR, seeding it with a
/// commented template first if it doesn't exist yet.
/// Reflow width for `--dump` / piped output: the configured width, or for
/// `wrap_width = 0` the width of the controlling terminal (80 when there is
/// none, e.g. inside a pipeline) less the left margin.
fn dump_width(cfg: &config::Config) -> usize {
    if cfg.wrap_width != 0 {
        return cfg.wrap_width;
    }
    let term_w = crossterm::terminal::size().map(|(w, _)| w as usize).unwrap_or(80);
    term_w.saturating_sub(cfg.left_margin.min(16)).max(20)
}

fn run_config() -> Result<()> {
    let path = config::config_path().context("cannot determine the config directory")?;
    if !path.exists() {
        let dir = path.parent().context("config path has no parent")?;
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        std::fs::write(&path, config::TEMPLATE)
            .with_context(|| format!("cannot write {}", path.display()))?;
    }

    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    // $EDITOR may carry arguments ("code -w"), so split it shell-word-ish.
    let mut words = editor.split_whitespace();
    let program = words.next().context("$EDITOR is empty")?;
    let status = std::process::Command::new(program)
        .args(words)
        .arg(&path)
        .status()
        .with_context(|| format!("failed to launch editor `{editor}`"))?;
    if !status.success() {
        bail!("editor exited with {status}");
    }

    match config::check(&path) {
        Ok(cfg) => {
            println!("wrap_width = {}", cfg.wrap_width);
            match &cfg.code_theme {
                Some(t) => println!("code_theme = \"{t}\""),
                None => println!("code_theme unset (matches the terminal theme)"),
            }
        }
        Err(err) => {
            eprintln!("warning: {}: {err}", path.display());
            eprintln!("mdview will fall back to defaults until it's fixed");
        }
    }
    Ok(())
}

/// Renders a line as an ANSI string for --dump mode.
fn line_ansi(line: &Line) -> String {
    let mut out = String::new();
    for span in &line.spans {
        let codes = style_codes(&span.style);
        // Internal #fragments aren't openable by the terminal; skip OSC 8.
        let osc8 = span.style.link.as_deref().filter(|u| !u.starts_with('#'));
        if let Some(url) = osc8 {
            out.push_str(&format!("\x1b]8;;{url}\x1b\\"));
        }
        if codes.is_empty() {
            out.push_str(&span.text);
        } else {
            out.push_str(&format!("\x1b[{}m{}\x1b[0m", codes.join(";"), span.text));
        }
        if osc8.is_some() {
            out.push_str("\x1b]8;;\x1b\\");
        }
    }
    out
}

fn style_codes(style: &Style) -> Vec<String> {
    let mut codes = Vec::new();
    if style.bold {
        codes.push("1".into());
    }
    if style.dim {
        codes.push("2".into());
    }
    if style.italic {
        codes.push("3".into());
    }
    if style.underline {
        codes.push("4".into());
    }
    if style.reverse {
        codes.push("7".into());
    }
    if style.strikethrough {
        codes.push("9".into());
    }
    if let Some(fg) = style.fg {
        match fg {
            Color::Black => codes.push("30".into()),
            Color::DarkRed => codes.push("31".into()),
            Color::DarkGreen => codes.push("32".into()),
            Color::DarkYellow => codes.push("33".into()),
            Color::DarkBlue => codes.push("34".into()),
            Color::DarkMagenta => codes.push("35".into()),
            Color::DarkCyan => codes.push("36".into()),
            Color::Grey => codes.push("37".into()),
            Color::DarkGrey => codes.push("90".into()),
            Color::Red => codes.push("91".into()),
            Color::Green => codes.push("92".into()),
            Color::Yellow => codes.push("93".into()),
            Color::Blue => codes.push("94".into()),
            Color::Magenta => codes.push("95".into()),
            Color::Cyan => codes.push("96".into()),
            Color::White => codes.push("97".into()),
            Color::Rgb { r, g, b } => codes.push(format!("38;2;{r};{g};{b}")),
            _ => {}
        }
    }
    codes
}
