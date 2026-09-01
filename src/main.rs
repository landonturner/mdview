mod config;
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
  -w, --width <N>       Reflow paragraphs to at most N columns (default 80,
                        or `wrap_width` in the config file)
  -d, --dump            Print the rendered document to stdout and exit
  -c, --config          Open the config file in $EDITOR (creating it with
                        defaults first if needed)
  -h, --help            Show this help
  -V, --version         Show version

Config file: ~/.config/mdview/config.toml
  wrap_width = 80
  code_theme = \"base16-ocean.dark\"

Press h inside the pager for key bindings.";

struct Args {
    file: Option<String>,
    width: Option<usize>,
    dump: bool,
    config: bool,
}

fn parse_args() -> Result<Args> {
    let mut args = Args { file: None, width: None, dump: false, config: false };
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
            "-w" | "--width" => {
                let v = it.next().context("--width requires a value")?;
                args.width = Some(v.parse().context("--width must be a number")?);
            }
            _ if arg.starts_with("--width=") => {
                args.width = Some(arg["--width=".len()..].parse().context("--width must be a number")?);
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

    if args.config {
        return run_config();
    }

    if let Some(w) = args.width {
        cfg.wrap_width = w.max(20);
    }

    let (source, title) = match &args.file {
        Some(path) if path != "-" => {
            let contents = std::fs::read_to_string(path).with_context(|| format!("cannot read {path}"))?;
            let name = std::path::Path::new(path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.clone());
            (contents, name)
        }
        _ => {
            if std::io::stdin().is_terminal() {
                eprintln!("{USAGE}");
                std::process::exit(2);
            }
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf).context("reading stdin")?;
            (buf, "(stdin)".to_string())
        }
    };

    let hl = render::Highlighter::new(&cfg.code_theme);

    if args.dump || !std::io::stdout().is_terminal() {
        let doc = render::render(&source, cfg.wrap_width, &hl);
        let mut out = std::io::BufWriter::new(std::io::stdout().lock());
        for line in &doc.lines {
            if args.dump {
                writeln!(out, "{}", line_ansi(line))?;
            } else {
                writeln!(out, "{}", line.plain())?;
            }
        }
        return Ok(());
    }

    pager::install_panic_hook();
    pager::run(&source, &title, &cfg, &hl)
}

/// Handles `--config`: opens the config file in $EDITOR, seeding it with a
/// commented template first if it doesn't exist yet.
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
            println!("code_theme = \"{}\"", cfg.code_theme);
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
        if let Some(url) = &span.style.link {
            out.push_str(&format!("\x1b]8;;{url}\x1b\\"));
        }
        if codes.is_empty() {
            out.push_str(&span.text);
        } else {
            out.push_str(&format!("\x1b[{}m{}\x1b[0m", codes.join(";"), span.text));
        }
        if span.style.link.is_some() {
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
