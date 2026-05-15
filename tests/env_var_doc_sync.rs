//! Enforces that every `pub` / `pub(crate)` `const ENV_*` constant
//! declared in `src/**/*.rs` has a row in `docs/environment-variables.md`.
//!
//! Per `.claude/rules/environment-variables.md`, that page is the single
//! index for every env var the project reads; the audit and fix-issue
//! skills cite it as authoritative. This test makes the sync rule
//! mechanical instead of relying on a human remembering to add a row
//! every time `pub const ENV_<NAME>: &str = "..."` lands.
//!
//! The scan is intentionally narrow: it matches the declaration shape
//! the project actually uses (`const ENV_<IDENT>: &str = "..."`) and
//! ignores everything else. Variables read indirectly through the AWS
//! or Azure SDKs (e.g. `AWS_ACCESS_KEY_ID`) are not declared as
//! constants in this crate and so are out of scope for this check —
//! the doc covers them separately.
//!
//! When this test fails, either add the missing row to
//! `docs/environment-variables.md` or remove the dead constant.

use std::fs;
use std::path::{Path, PathBuf};

/// Project root (the directory containing this test's `Cargo.toml`).
fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Recursively collect every `.rs` file under `dir`.
fn collect_rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read src dir") {
        let entry = entry.expect("read src entry");
        let path = entry.path();
        let file_type = entry.file_type().expect("file type");
        if file_type.is_dir() {
            collect_rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// Extract every `ENV_<IDENT>` declaration value from a Rust source file.
///
/// Matches both `pub const ENV_FOO: &str = "BAR";` and
/// `pub(crate) const ENV_FOO: &str = "BAR";`. Returns `(name, value)`
/// pairs, where `name` is the Rust identifier (e.g. `ENV_FOO`) and
/// `value` is the string literal between the double quotes.
fn extract_env_constants(source: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim_start();
        let after_pub = trimmed
            .strip_prefix("pub(crate) const ENV_")
            .or_else(|| trimmed.strip_prefix("pub const ENV_"));
        let Some(rest) = after_pub else { continue };

        let Some(name_end) = rest.find(':') else {
            continue;
        };
        let name = format!("ENV_{}", rest[..name_end].trim());

        let Some(first_quote) = rest.find('"') else {
            continue;
        };
        let after_quote = &rest[first_quote + 1..];
        let Some(closing) = after_quote.find('"') else {
            continue;
        };
        let value = after_quote[..closing].to_owned();

        found.push((name, value));
    }
    found
}

#[test]
fn every_env_constant_has_a_documentation_row() {
    let root = project_root();
    let src = root.join("src");

    let mut sources = Vec::new();
    collect_rust_files(&src, &mut sources);
    assert!(
        !sources.is_empty(),
        "no Rust files found under {}",
        src.display()
    );

    let mut declared = Vec::new();
    for path in &sources {
        let body =
            fs::read_to_string(path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
        for (name, value) in extract_env_constants(&body) {
            declared.push((name, value, path.clone()));
        }
    }
    assert!(
        !declared.is_empty(),
        "scan found zero ENV_ constants under {}; the regex shape probably drifted — \
         update `extract_env_constants` to match the project's current declaration style",
        src.display()
    );

    let doc_path = root.join("docs/environment-variables.md");
    let doc = fs::read_to_string(&doc_path)
        .unwrap_or_else(|err| panic!("read {}: {err}", doc_path.display()));

    let missing: Vec<_> = declared
        .iter()
        .filter(|(_, value, _)| !doc.contains(value))
        .collect();

    assert!(
        missing.is_empty(),
        "the following env-var constants are declared in src/ but not mentioned \
         in docs/environment-variables.md (the single index, per \
         .claude/rules/environment-variables.md):\n{}",
        missing
            .iter()
            .map(|(name, value, path)| format!(
                "  - `{value}` (constant `{name}` in {})",
                path.strip_prefix(&root).unwrap_or(path).display()
            ))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn extract_picks_up_pub_const() {
        let src = r#"
            pub const ENV_FOO: &str = "GIT_REMOTE_FOO";
            other line
            pub(crate) const ENV_BAR: &str = "GIT_REMOTE_BAR";
        "#;
        assert_eq!(
            extract_env_constants(src),
            vec![
                ("ENV_FOO".to_owned(), "GIT_REMOTE_FOO".to_owned()),
                ("ENV_BAR".to_owned(), "GIT_REMOTE_BAR".to_owned()),
            ]
        );
    }

    #[test]
    fn extract_ignores_non_env_constants() {
        let src = "pub const TIMEOUT: u64 = 30;";
        assert!(extract_env_constants(src).is_empty());
    }

    #[test]
    fn extract_ignores_private_constants() {
        // Private constants are local helpers — they don't need a doc row.
        let src = r#"const ENV_PRIVATE: &str = "PRIVATE";"#;
        assert!(extract_env_constants(src).is_empty());
    }
}
