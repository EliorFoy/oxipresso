use std::path::{Path, PathBuf};

use oxipresso_editor_protocol::{EditorCommand, WireProtocol, parse_command};

pub const SIMPLE_TEX: &str =
    "\\documentclass{article}\n\\begin{document}\nHello.\n\\end{document}\n";
pub const MISSING_INPUT_TEX: &str =
    "\\documentclass{article}\n\\begin{document}\n\\input{missing.tex}\n\\end{document}\n";

pub fn parse_sexp(input: &str) -> EditorCommand {
    parse_command(input, WireProtocol::Sexp).expect("test S-expression should parse")
}

pub fn original_texpresso_fixture(name: &str) -> Option<PathBuf> {
    original_texpresso_roots()
        .into_iter()
        .map(|root| root.join("test").join(name))
        .find(|path| path.exists())
}

pub fn read_original_texpresso_fixture(name: &str) -> Option<Vec<u8>> {
    original_texpresso_fixture(name).and_then(|path| std::fs::read(path).ok())
}

fn original_texpresso_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(path) = std::env::var_os("TEXPRESSO_SRC") {
        roots.push(PathBuf::from(path));
    }
    if let Ok(current_dir) = std::env::current_dir() {
        roots.extend(candidate_sibling_roots(&current_dir));
    }
    dedup_paths(roots)
}

fn candidate_sibling_roots(start: &Path) -> Vec<PathBuf> {
    start
        .ancestors()
        .flat_map(|ancestor| {
            [
                ancestor.join("texpresso-src"),
                ancestor.join("texpresso"),
                ancestor.join("let-def").join("texpresso"),
            ]
        })
        .collect()
}

fn dedup_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut deduped = Vec::new();
    for path in paths {
        if !deduped.contains(&path) {
            deduped.push(path);
        }
    }
    deduped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_fixture_constants_parse() {
        assert!(SIMPLE_TEX.contains("\\begin{document}"));
        assert!(MISSING_INPUT_TEX.contains("\\input"));
    }

    #[test]
    fn original_fixture_lookup_is_optional() {
        if let Some(path) = original_texpresso_fixture("simple.tex") {
            assert!(path.ends_with(Path::new("test").join("simple.tex")));
        }
    }
}
