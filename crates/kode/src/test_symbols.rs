//! Decides whether a graph symbol is a test: by name/path convention, or
//! (Rust) by sitting inside a `#[cfg(test)]` inline module.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Name/path conventions for test symbols across the supported languages.
pub fn is_test_symbol(name: &str, file: &str) -> bool {
    let file_name = file.rsplit('/').next().unwrap_or(file);
    name.starts_with("test_")
        || name.starts_with("Test")
        || file.contains("/tests/")
        || file.starts_with("tests/")
        || file_name == "tests.rs"
        || file_name.ends_with("_test.go")
        || file_name.ends_with("_test.py")
        || (file_name.starts_with("test_") && file_name.ends_with(".py"))
}

/// 1-based inclusive line ranges of `#[cfg(test)]`-gated inline modules.
pub fn rust_test_ranges(text: &str) -> Vec<(u32, u32)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if !lines[i].trim().starts_with("#[cfg(test)]") {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < lines.len() && (lines[j].trim().is_empty() || lines[j].trim().starts_with("#[")) {
            j += 1;
        }
        let Some(decl) = lines.get(j).map(|l| l.trim()) else {
            break;
        };
        let decl = decl
            .strip_prefix("pub(crate) ")
            .or_else(|| decl.strip_prefix("pub "))
            .unwrap_or(decl);
        if !(decl.starts_with("mod ") && decl.contains('{')) {
            i = j.max(i + 1);
            continue;
        }
        // ponytail: braces inside strings and comments are counted as code.
        let mut depth: i64 = 0;
        let mut end = lines.len() - 1;
        for (k, line) in lines.iter().enumerate().skip(j) {
            depth += line.matches('{').count() as i64 - line.matches('}').count() as i64;
            if depth <= 0 {
                end = k;
                break;
            }
        }
        out.push((j as u32 + 1, end as u32 + 1));
        i = end + 1;
    }
    out
}

/// Classifies symbols as tests, reading each Rust file's inline test ranges
/// lazily from disk (unreadable files count as having none).
pub struct TestClassifier {
    root: PathBuf,
    cache: HashMap<String, Vec<(u32, u32)>>,
}

impl TestClassifier {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            cache: HashMap::new(),
        }
    }

    /// Whether `name` at `file:line` is a test.
    pub fn is_test(&mut self, name: &str, file: &str, line: u32) -> bool {
        if is_test_symbol(name, file) {
            return true;
        }
        if !file.ends_with(".rs") || line == 0 {
            return false;
        }
        let root = &self.root;
        let ranges = self.cache.entry(file.to_string()).or_insert_with(|| {
            std::fs::read_to_string(root.join(file))
                .map(|t| rust_test_ranges(&t))
                .unwrap_or_default()
        });
        ranges.iter().any(|&(s, e)| line >= s && line <= e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_symbol_detection() {
        assert!(is_test_symbol("test_parse", "src/lib.rs"));
        assert!(is_test_symbol("parses", "crates/kode/tests/status.rs"));
        assert!(is_test_symbol("parses", "crates/kode/src/tui/tests.rs"));
        assert!(is_test_symbol("TestFoo", "pkg/foo_test.go"));
        assert!(is_test_symbol("check", "tests/test_api.py"));
        assert!(!is_test_symbol(
            "execute_task",
            "crates/kode/src/pipeline.rs"
        ));
    }

    const SAMPLE: &str = "fn top() {}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn inline() {\n        let _x = { 1 };\n    }\n}\n\nfn after() {}\n";

    #[test]
    fn ranges_cover_the_inline_module() {
        assert_eq!(rust_test_ranges(SAMPLE), vec![(4, 11)]);
        assert!(rust_test_ranges("fn a() {}\n").is_empty());
    }

    #[test]
    fn ranges_accept_pub_crate_and_attrs_and_unclosed() {
        let t = "#[cfg(test)]\n#[allow(dead_code)]\npub(crate) mod t {\n    fn a() {}\n}\n";
        assert_eq!(rust_test_ranges(t), vec![(3, 5)]);
        let open = "fn a() {}\n#[cfg(test)]\nmod t {\n    fn b() {}\n";
        assert_eq!(rust_test_ranges(open), vec![(3, 4)]);
    }

    #[test]
    fn classifier_flags_inline_tests_only() {
        let dir = std::env::temp_dir().join(format!("kode-test-symbols-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("lib.rs"), SAMPLE).unwrap();
        let mut c = TestClassifier::new(&dir);
        assert!(c.is_test("inline", "lib.rs", 8));
        assert!(!c.is_test("top", "lib.rs", 1));
        assert!(!c.is_test("after", "lib.rs", 13));
        assert!(!c.is_test("ghost", "missing.rs", 8));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
