//! Every Rust file stays within the length AGENTS.md sets; blank and `//` lines do not count.

use std::fs;
use std::path::{Path, PathBuf};

const MAX_CODE_LINES: usize = 300;

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn code_lines(text: &str) -> usize {
    text.lines()
        .map(str::trim_start)
        .filter(|line| !line.is_empty() && !line.starts_with("//"))
        .count()
}

#[test]
fn no_rust_file_exceeds_the_length_limit() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    for dir in ["src", "tests"] {
        rust_files(&root.join(dir), &mut files);
    }
    let over: Vec<String> = files
        .iter()
        .filter_map(|path| {
            let lines = code_lines(&fs::read_to_string(path).unwrap());
            (lines > MAX_CODE_LINES).then(|| {
                format!(
                    "{} ({lines} lines)",
                    path.strip_prefix(root).unwrap().display()
                )
            })
        })
        .collect();
    assert!(
        over.is_empty(),
        "files over {MAX_CODE_LINES} code lines:\n{}",
        over.join("\n")
    );
}
