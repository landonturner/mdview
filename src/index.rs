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

/// One row of the directory tree: a folder or a document, in depth-first
/// order with folders before files at each level (the way file explorers
/// lay things out).
#[derive(Clone, Debug)]
pub struct Node {
    pub name: String,
    /// `/`-separated path relative to the root (folders have no trailing slash).
    pub rel: String,
    pub depth: usize,
    pub parent: Option<usize>,
    pub is_dir: bool,
    /// Index into `DirIndex::entries` for documents.
    pub entry: Option<usize>,
    /// Documents anywhere underneath a folder.
    pub doc_count: usize,
}

#[derive(Default)]
struct Folder {
    /// Keyed by lowercase name for case-insensitive ordering.
    subdirs: std::collections::BTreeMap<String, (String, Folder)>,
    files: Vec<(String, usize)>,
}

impl Folder {
    fn insert(&mut self, parts: &[&str], entry: usize) {
        match parts {
            [] => {}
            [file] => self.files.push((file.to_string(), entry)),
            [dir, rest @ ..] => {
                let key = dir.to_lowercase();
                let slot = self.subdirs.entry(key).or_insert_with(|| (dir.to_string(), Folder::default()));
                slot.1.insert(rest, entry);
            }
        }
    }

    fn emit(&self, prefix: &str, depth: usize, parent: Option<usize>, out: &mut Vec<Node>) -> usize {
        let mut count = 0;
        for (name, folder) in self.subdirs.values() {
            let rel = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
            let me = out.len();
            out.push(Node { name: name.clone(), rel: rel.clone(), depth, parent, is_dir: true, entry: None, doc_count: 0 });
            let n = folder.emit(&rel, depth + 1, Some(me), out);
            out[me].doc_count = n;
            count += n;
        }
        let mut files = self.files.clone();
        files.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()).then(a.0.cmp(&b.0)));
        for (name, entry) in files {
            let rel = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
            out.push(Node { name, rel, depth, parent, is_dir: false, entry: Some(entry), doc_count: 1 });
            count += 1;
        }
        count
    }
}

/// Builds the tree rows for a listing.
pub fn tree(entries: &[DirEntry]) -> Vec<Node> {
    let mut root = Folder::default();
    for (i, e) in entries.iter().enumerate() {
        let parts: Vec<&str> = e.rel.split('/').collect();
        root.insert(&parts, i);
    }
    let mut out = Vec::new();
    root.emit("", 0, None, &mut out);
    out
}

#[cfg(test)]
mod tree_tests {
    use super::*;

    fn entry(rel: &str) -> DirEntry {
        DirEntry { rel: rel.to_string(), title: None, path: PathBuf::from(rel) }
    }

    #[test]
    fn folders_first_then_files_with_depth_and_counts() {
        let entries = [entry("zeta.md"), entry("docs/b.md"), entry("docs/api/x.md"), entry("Alpha.md")];
        let t = tree(&entries);
        let shape: Vec<(String, usize, bool)> = t.iter().map(|n| (n.rel.clone(), n.depth, n.is_dir)).collect();
        assert_eq!(
            shape,
            [
                ("docs".to_string(), 0, true),
                ("docs/api".to_string(), 1, true),
                ("docs/api/x.md".to_string(), 2, false),
                ("docs/b.md".to_string(), 1, false),
                ("Alpha.md".to_string(), 0, false),
                ("zeta.md".to_string(), 0, false),
            ]
        );
        assert_eq!(t[0].doc_count, 2);
        assert_eq!(t[1].doc_count, 1);
        assert_eq!(t[2].parent, Some(1));
        assert_eq!(t[3].parent, Some(0));
        assert_eq!(t[4].parent, None);
    }
}
