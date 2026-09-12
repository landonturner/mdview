//! Directory mode: `mdview some/dir` lists the markdown documents under a
//! directory so one can be picked and opened, with `q` returning to the
//! list.

use std::path::{Path, PathBuf};

/// One document in a directory listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    /// Path relative to the listed directory, `/`-separated.
    pub rel: String,
    /// The document's first `#` heading, when it has one near the top.
    pub title: Option<String>,
    pub path: PathBuf,
}

/// The markdown files under a directory, sorted by relative path.
#[derive(Clone, Debug)]
pub struct DirIndex {
    pub root: PathBuf,
    pub entries: Vec<DirEntry>,
}

const MAX_DEPTH: usize = 8;
const MAX_ENTRIES: usize = 5000;
/// Directories never worth descending into for documents.
const SKIP_DIRS: &[&str] = &["node_modules", "target", "vendor", "dist", "build", "__pycache__"];

fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
}

/// Recursively collects markdown files, skipping hidden directories and
/// well-known build/dependency trees.
pub fn scan(root: &Path) -> DirIndex {
    let mut entries = Vec::new();
    walk(root, root, 0, &mut entries);
    entries.sort_by(|a, b| a.rel.to_lowercase().cmp(&b.rel.to_lowercase()).then(a.rel.cmp(&b.rel)));
    DirIndex { root: root.to_path_buf(), entries }
}

fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<DirEntry>) {
    if depth > MAX_DEPTH || out.len() >= MAX_ENTRIES {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut children: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
    children.sort();
    for path in children {
        if out.len() >= MAX_ENTRIES {
            return;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() {
            if name.starts_with('.') || SKIP_DIRS.contains(&name) {
                continue;
            }
            walk(root, &path, depth + 1, out);
        } else if is_markdown(&path) && !name.starts_with('.') {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            out.push(DirEntry { rel, title: first_heading(&path), path });
        }
    }
}

/// The first `# Heading` within the opening lines of the file, if any.
fn first_heading(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = [0u8; 4096];
    let n = f.read(&mut buf).ok()?;
    let head = String::from_utf8_lossy(&buf[..n]);
    head.lines()
        .take(30)
        .map(str::trim)
        .find_map(|l| l.strip_prefix("# ").map(|t| t.trim().to_string()))
        .filter(|t| !t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_markdown_recursively_with_titles() {
        let dir = std::env::temp_dir().join(format!("mdview-index-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub/.hidden")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        std::fs::write(dir.join("b.md"), "# Bravo\n\ntext\n").unwrap();
        std::fs::write(dir.join("A.markdown"), "no heading here\n").unwrap();
        std::fs::write(dir.join("sub/c.md"), "---\nfront: matter\n---\n# Charlie\n").unwrap();
        std::fs::write(dir.join("sub/.hidden/d.md"), "# Hidden\n").unwrap();
        std::fs::write(dir.join("node_modules/pkg/README.md"), "# Dep\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "# Not markdown\n").unwrap();

        let idx = scan(&dir);
        let rels: Vec<&str> = idx.entries.iter().map(|e| e.rel.as_str()).collect();
        assert_eq!(rels, ["A.markdown", "b.md", "sub/c.md"]);
        assert_eq!(idx.entries[0].title, None);
        assert_eq!(idx.entries[1].title.as_deref(), Some("Bravo"));
        assert_eq!(idx.entries[2].title.as_deref(), Some("Charlie"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
