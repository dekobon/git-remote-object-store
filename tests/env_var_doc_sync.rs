//! Enforces that every `pub` / `pub(crate)` `const ENV_*` constant
//! declared in `src/**/*.rs` or `cli/src/**/*.rs` has a row in
//! `docs/environment-variables.md`, AND that the documented default
//! values for the env vars match the live `DEFAULT_*` constants the
//! code actually reads.
//!
//! Both crate roots are scanned because the helper binaries' shared
//! entrypoint lives in the `cli` crate (`cli/src/lib.rs`) and reads
//! env vars too (e.g. `GIT_DIR`); a single-root scan would miss them.
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
//! When the env-row check fails, either add the missing row to
//! `docs/environment-variables.md` or remove the dead constant.
//!
//! The default-value check (issue #184) scans `pub const DEFAULT_*: u64`
//! declarations and asserts that every documented mention of the default
//! (across `docs/`, `cli/src/management.rs`, and `man/*.1`) names the
//! current numeric value. The patterns matched are explicit anchors
//! (e.g. `(falling back to 60s)`, `Default is 24 hours.`) so unrelated
//! occurrences of `60` or `24` in the same files do not produce false
//! positives. When this check fails, update either the constant or every
//! documented mention listed in the failure message.

use std::fs;
use std::path::{Path, PathBuf};

/// Project root (the directory containing this test's `Cargo.toml`).
fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Source-tree roots scanned for `ENV_*` and `DEFAULT_*` declarations.
///
/// Both the library crate (`src/`) and the CLI / helper-binaries crate
/// (`cli/src/`) read env vars, so a single-root scan would silently miss
/// the helper-binaries' reads (see issue #186 — `GIT_DIR` slipped past the
/// scan because it lives in `cli/src/lib.rs`).
const RUST_SCAN_ROOTS: &[&str] = &["src", "cli/src"];

/// Collect every Rust source file under each configured scan root.
fn collect_all_rust_sources(root: &Path) -> Vec<PathBuf> {
    let mut sources = Vec::new();
    for rel in RUST_SCAN_ROOTS {
        let dir = root.join(rel);
        assert!(
            dir.is_dir(),
            "expected scan root `{}` to exist; update RUST_SCAN_ROOTS \
             if the workspace layout changed",
            dir.display(),
        );
        collect_rust_files(&dir, &mut sources);
    }
    sources
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

    let sources = collect_all_rust_sources(&root);
    assert!(
        !sources.is_empty(),
        "no Rust files found under any of {RUST_SCAN_ROOTS:?}",
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
        "scan found zero ENV_ constants under {RUST_SCAN_ROOTS:?}; the regex shape \
         probably drifted — update `extract_env_constants` to match the project's \
         current declaration style",
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
        "the following env-var constants are declared in the workspace but not \
         mentioned in docs/environment-variables.md (the single index, per \
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

/// Extract every `pub` / `pub(crate)` `const DEFAULT_<IDENT>: u64 = <N>;`
/// declaration from a Rust source file. Returns `(name, value)` pairs
/// where `name` is the Rust identifier (e.g. `DEFAULT_LOCK_TTL_SECONDS`)
/// and `value` is the integer literal. Underscores in the literal are
/// stripped before parsing so `1_024` parses as `1024`.
///
/// Only `u64` constants are matched because the project's tunable
/// defaults (lock TTL, grace hours) are all unsigned counts; widening
/// the scan to every numeric type would invite false positives from
/// unrelated constants. If a new default needs a different type, extend
/// the matcher explicitly.
fn extract_default_constants(source: &str) -> Vec<(String, u64)> {
    let mut found = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim_start();
        let after_pub = trimmed
            .strip_prefix("pub(crate) const DEFAULT_")
            .or_else(|| trimmed.strip_prefix("pub const DEFAULT_"));
        let Some(rest) = after_pub else { continue };

        let Some(name_end) = rest.find(':') else {
            continue;
        };
        let name = format!("DEFAULT_{}", rest[..name_end].trim());

        let after_colon = rest[name_end + 1..].trim_start();
        let Some(rest_after_type) = after_colon.strip_prefix("u64") else {
            continue;
        };
        let after_eq = rest_after_type.trim_start();
        let Some(after_eq) = after_eq.strip_prefix('=') else {
            continue;
        };
        let value_str: String = after_eq
            .chars()
            .skip_while(|c| c.is_whitespace())
            .take_while(|c| c.is_ascii_digit() || *c == '_')
            .filter(|c| *c != '_')
            .collect();
        let Ok(value) = value_str.parse::<u64>() else {
            continue;
        };

        found.push((name, value));
    }
    found
}

/// One documented mention of a default value, found by anchored scan.
#[derive(Debug, Clone)]
struct DocumentedMention {
    /// Source-file path (relative to the project root) where the match was found.
    path: PathBuf,
    /// 1-based line number for human-readable diagnostics.
    line: usize,
    /// Text of the surrounding pattern, e.g. `(falling back to 60s)`.
    snippet: String,
    /// Numeric value documented at this site.
    documented: u64,
}

/// Normalize a source file so an anchored scan can match patterns
/// even when they wrap across lines (the common case in Rust
/// doc-comments such as `/// foo (falling\n/// back to 60s)`). Returns
/// the normalized byte buffer plus a `normalized-byte-offset →
/// 1-based-original-line-number` map keyed at every output byte.
///
/// The normalization rules:
///
/// * Each newline is rewritten as a single space.
/// * Immediately after a newline, any run of whitespace plus an
///   optional `///`, `//!`, or `//` prefix plus any further
///   whitespace is collapsed away (the space written for the newline
///   becomes the only separator between joined lines).
///
/// Operating on bytes (rather than `String`) sidesteps UTF-8
/// re-encoding: the input may contain multi-byte characters (e.g.
/// em-dashes in docs/getting-started.md), and a `b as char` push
/// would silently expand each non-ASCII byte to two output bytes,
/// breaking position-to-line lookup.
fn normalize_with_line_map(haystack: &str) -> (Vec<u8>, Vec<usize>) {
    let bytes = haystack.as_bytes();
    let mut normalized: Vec<u8> = Vec::with_capacity(bytes.len());
    // line_for[i] = 1-based original line containing original byte i.
    let mut line_for: Vec<usize> = Vec::with_capacity(bytes.len() + 1);
    // For every byte written to `normalized`, the original byte offset
    // that produced it. Lets us translate a hit position back to an
    // original line via `line_for`.
    let mut norm_to_orig: Vec<usize> = Vec::with_capacity(bytes.len() + 1);

    let mut line = 1usize;
    for &b in bytes {
        line_for.push(line);
        if b == b'\n' {
            line += 1;
        }
    }
    // Sentinel so an index equal to bytes.len() is valid.
    line_for.push(line);

    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\n' {
            // Collapse newline + continuation prefix into a single space.
            normalized.push(b' ');
            norm_to_orig.push(i);
            i += 1;
            // Skip leading whitespace on the continuation line.
            while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
                i += 1;
            }
            // Skip a comment prefix, if present, so `(falling\n   /// back
            // to 60s)` matches the anchor `(falling back to 60s)`. Both
            // `///` and `//!` consume three bytes; `//` consumes two.
            if i + 2 < bytes.len() && (&bytes[i..i + 3] == b"///" || &bytes[i..i + 3] == b"//!") {
                i += 3;
            } else if i + 1 < bytes.len() && &bytes[i..i + 2] == b"//" {
                i += 2;
            }
            // Skip whitespace after the comment prefix.
            while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
                i += 1;
            }
        } else {
            normalized.push(b);
            norm_to_orig.push(i);
            i += 1;
        }
    }
    // Allow a hit position equal to normalized.len() to map back.
    norm_to_orig.push(bytes.len());

    // Compose: hit byte offset in normalized → line in original.
    let mut hit_to_line: Vec<usize> = Vec::with_capacity(norm_to_orig.len());
    for &orig in &norm_to_orig {
        let l = *line_for.get(orig).unwrap_or(&line);
        hit_to_line.push(l);
    }
    (normalized, hit_to_line)
}

/// Search for `needle` (a byte slice) in `haystack` starting at `from`.
/// Returns the absolute byte offset of the first match or `None`.
fn find_bytes(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// Locate every occurrence of `<prefix><digits><suffix>` in `haystack`
/// and return the parsed digits paired with their 1-based line number
/// and a short snippet showing the matched anchor. Both anchors must be
/// present to count as a match, which is what keeps unrelated digits in
/// the same file from being treated as documented defaults.
///
/// The haystack is first normalized so anchors that wrap across a Rust
/// doc-comment or roff line break still match. Reported line numbers
/// point at the line where the prefix anchor began in the original
/// input, which is what a maintainer needs to jump to.
///
/// Underscores in the digit run are tolerated (matching Rust literal
/// style) but the project's documented defaults don't use them.
fn find_anchored_values(haystack: &str, prefix: &str, suffix: &str) -> Vec<(usize, String, u64)> {
    let (normalized, hit_to_line) = normalize_with_line_map(haystack);
    let prefix_bytes = prefix.as_bytes();
    let suffix_bytes = suffix.as_bytes();
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while let Some(p) = find_bytes(&normalized, prefix_bytes, cursor) {
        let after_start = p + prefix_bytes.len();
        let mut end = after_start;
        while end < normalized.len() {
            let b = normalized[end];
            if b.is_ascii_digit() || b == b'_' {
                end += 1;
            } else {
                break;
            }
        }
        let digits_slice = &normalized[after_start..end];
        if digits_slice.is_empty() {
            // No digits after the prefix; advance past this prefix
            // occurrence so we keep scanning the rest of the input.
            cursor = after_start;
            continue;
        }
        if normalized[end..].starts_with(suffix_bytes) {
            // `digits_slice` is ASCII (only `[0-9_]`) so the unchecked
            // conversion is sound; we still use the checked form to
            // satisfy the no-`unsafe` rule.
            let digits = std::str::from_utf8(digits_slice).expect("ascii digits are valid UTF-8");
            let value: u64 = digits.replace('_', "").parse().unwrap_or(0);
            let snippet = format!("{prefix}{digits}{suffix}");
            let line = hit_to_line.get(p).copied().unwrap_or(1);
            out.push((line, snippet, value));
        }
        cursor = after_start;
    }
    out
}

/// Anchored-pattern set tied to a single `DEFAULT_*` constant. Each
/// `(prefix, suffix)` pair must surround the documented digit run for
/// the scan to count it. Listing the anchors explicitly (rather than
/// matching every bare `60` in the docs) keeps unrelated occurrences
/// like "60-day retention" out of the comparison.
struct DefaultPatterns {
    /// Live constant name (e.g. `DEFAULT_LOCK_TTL_SECONDS`).
    constant: &'static str,
    /// `(prefix, suffix)` anchors. Both must be present.
    anchors: &'static [(&'static str, &'static str)],
    /// Files to scan, relative to the project root. The set is
    /// hand-curated rather than a recursive walk so adding new doc
    /// surfaces is an explicit decision (and so the scan doesn't drift
    /// into `target/`, `vendor/`, or generated artefacts).
    doc_paths: &'static [&'static str],
}

/// Doc surfaces and anchor patterns that bind a digit run to "the
/// default for this constant". When a new doc location starts referring
/// to a default, add it here so drift is caught the next time the
/// constant changes. Anchors lifted from the actual prose:
///
/// * `(falling back to 60s)` — CLI doc-comments, man pages, README/docs prose
/// * `falling back to 60 seconds` — getting-started narrative
/// * `(60s default)` — getting-started troubleshooting
/// * `Default is 24 hours.` — getting-started GC section
/// * `default 24 hours` — storage-engines.md
/// * `**24h**` — getting-started bullet list
/// * `` | `<ENV_NAME>` | `<VALUE>` `` — env-vars.md table row
///
/// The env-vars table needs a different shape because the digit is the
/// second `` ` ``-delimited cell on the row, not adjacent prose; it is
/// handled by [`check_env_table_default`] rather than the anchor list.
const DEFAULT_PATTERNS: &[DefaultPatterns] = &[
    DefaultPatterns {
        constant: "DEFAULT_LOCK_TTL_SECONDS",
        anchors: &[
            ("(falling back to ", "s)"),
            ("falling back to ", " seconds"),
            ("(", "s default)"),
        ],
        doc_paths: &[
            "docs/getting-started.md",
            "cli/src/management.rs",
            "man/git-remote-object-store-doctor.1",
            "man/git-remote-object-store-compact.1",
            "man/git-remote-object-store-delete-branch.1",
            "man/git-remote-object-store.1",
        ],
    },
    DefaultPatterns {
        constant: "DEFAULT_GRACE_HOURS",
        anchors: &[
            ("(falling back to ", ")"),
            // Bare "falling back to N);" in narrative prose.
            ("falling back to ", ");"),
            ("Default is ", " hours."),
            ("default ", " hours"),
            ("**", "h**"),
        ],
        doc_paths: &[
            "docs/getting-started.md",
            "docs/storage-engines.md",
            "cli/src/management.rs",
            "man/git-remote-object-store-gc.1",
            "man/git-remote-object-store-compact.1",
        ],
    },
];

/// Env-vars-table check: locate the row whose first cell matches the
/// env-var name `ENV_<TAIL>` (where `TAIL` is derived from the
/// constant name by stripping `DEFAULT_`) and return the digit value
/// in the second `` ` ``-delimited cell.
///
/// The mapping from `DEFAULT_*` to `ENV_*` is hand-curated because the
/// env-var name does not always match the `DEFAULT_` suffix verbatim
/// (e.g. `DEFAULT_GRACE_HOURS` is read via
/// `GIT_REMOTE_OBJECT_STORE_GC_GRACE_HOURS`).
const ENV_TABLE_BINDINGS: &[(&str, &str)] = &[
    (
        "DEFAULT_LOCK_TTL_SECONDS",
        "GIT_REMOTE_OBJECT_STORE_LOCK_TTL_SECONDS",
    ),
    (
        "DEFAULT_GRACE_HOURS",
        "GIT_REMOTE_OBJECT_STORE_GC_GRACE_HOURS",
    ),
];

/// Parse the second `` ` ``-delimited cell of the env-vars table row
/// for `env_var_name` and return its digit value, or `None` if the row
/// is missing or unparseable. The doc rules require the row to exist
/// (enforced by `every_env_constant_has_a_documentation_row`), but a
/// missing default cell is reported as a separate diagnostic so the
/// author sees both failures rather than only the first.
fn parse_env_table_default(doc: &str, env_var_name: &str) -> Option<(usize, String, u64)> {
    for (idx, line) in doc.lines().enumerate() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("| `") {
            continue;
        }
        // Confirm the row's first cell names the env var we're looking
        // for; otherwise unrelated rows would match the second-cell
        // backtick pair.
        let after_first_tick = trimmed.trim_start_matches("| `");
        let Some(close) = after_first_tick.find('`') else {
            continue;
        };
        if &after_first_tick[..close] != env_var_name {
            continue;
        }
        let rest = &after_first_tick[close + 1..];
        // The next `` ` `` pair delimits the default value.
        let Some(open) = rest.find('`') else { continue };
        let after_open = &rest[open + 1..];
        let Some(close2) = after_open.find('`') else {
            continue;
        };
        let cell = &after_open[..close2];
        let digits: String = cell.chars().filter(char::is_ascii_digit).collect();
        if digits.is_empty() {
            // Cell exists but doesn't carry a digit (e.g., "unset" for
            // env vars without a numeric default). Treat as no binding.
            return None;
        }
        let value: u64 = digits.parse().ok()?;
        return Some((idx + 1, cell.to_owned(), value));
    }
    None
}

/// Collect every `DEFAULT_*` constant declared under the configured scan
/// roots (see [`RUST_SCAN_ROOTS`]).
fn collect_defaults(root: &Path) -> Vec<(String, u64, PathBuf)> {
    let sources = collect_all_rust_sources(root);

    let mut defaults: Vec<(String, u64, PathBuf)> = Vec::new();
    for path in &sources {
        let body =
            fs::read_to_string(path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
        for (name, value) in extract_default_constants(&body) {
            defaults.push((name, value, path.clone()));
        }
    }
    defaults
}

/// Scan the documented mentions of `patterns.constant` and append any
/// divergences (documented != live) to `divergences`.
fn check_anchored_patterns(
    root: &Path,
    patterns: &DefaultPatterns,
    live: u64,
    src_path: &Path,
    divergences: &mut Vec<String>,
) {
    let mut mentions: Vec<DocumentedMention> = Vec::new();
    for rel in patterns.doc_paths {
        let path = root.join(rel);
        let body = fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
        for (prefix, suffix) in patterns.anchors {
            for (line_no, snippet, value) in find_anchored_values(&body, prefix, suffix) {
                mentions.push(DocumentedMention {
                    path: PathBuf::from(rel),
                    line: line_no,
                    snippet,
                    documented: value,
                });
            }
        }
    }

    // Fail loudly if a constant claims doc coverage but no documented
    // mention is found anywhere — that means either the anchors drifted
    // or the docs lost their mention of the default. Either way the next
    // constant change would silently break the docs.
    assert!(
        !mentions.is_empty(),
        "no documented mention of `{}` was located via the configured anchors. \
         Either the docs no longer mention the default (re-add it) or the anchor \
         patterns in DEFAULT_PATTERNS drifted from the prose (update them). \
         Constant is declared in {}.",
        patterns.constant,
        src_path.strip_prefix(root).unwrap_or(src_path).display(),
    );

    for mention in &mentions {
        if mention.documented != live {
            let constant = patterns.constant;
            let path = mention.path.display();
            let line_no = mention.line;
            let snippet = &mention.snippet;
            let documented = mention.documented;
            divergences.push(format!(
                "  - {path}:{line_no} — `{snippet}` documents `{documented}` but \
                 `{constant}` is currently `{live}`",
            ));
        }
    }
}

/// Check the env-vars-table row for `(constant, env_name)` and append
/// a divergence if the documented default does not match `live`.
fn check_env_table_row(
    env_doc: &str,
    constant: &str,
    env_name: &str,
    live: u64,
    divergences: &mut Vec<String>,
) {
    match parse_env_table_default(env_doc, env_name) {
        Some((row_line, cell, documented)) if documented != live => {
            divergences.push(format!(
                "  - docs/environment-variables.md:{row_line} — row for `{env_name}` \
                 has default cell `{cell}` ({documented}) but `{constant}` is \
                 currently `{live}`",
            ));
        }
        Some(_) => {}
        None => {
            divergences.push(format!(
                "  - docs/environment-variables.md — row for `{env_name}` is missing \
                 or has no numeric default cell; expected to match `{constant}` \
                 (= `{live}`)",
            ));
        }
    }
}

#[test]
fn documented_defaults_match_live_constants() {
    let root = project_root();
    let defaults = collect_defaults(&root);
    assert!(
        !defaults.is_empty(),
        "scan found zero DEFAULT_ constants under {RUST_SCAN_ROOTS:?}; \
         the matcher probably drifted",
    );

    // Every entry in `DEFAULT_PATTERNS` and `ENV_TABLE_BINDINGS` must
    // refer to a constant that actually exists. A typo here would
    // silently skip the check.
    for patterns in DEFAULT_PATTERNS {
        assert!(
            defaults
                .iter()
                .any(|(name, _, _)| name == patterns.constant),
            "DEFAULT_PATTERNS names `{}` but no such constant exists under \
             {RUST_SCAN_ROOTS:?}; remove the entry or fix the spelling",
            patterns.constant,
        );
    }
    for (constant, _) in ENV_TABLE_BINDINGS {
        assert!(
            defaults.iter().any(|(name, _, _)| name == constant),
            "ENV_TABLE_BINDINGS names `{constant}` but no such constant exists \
             under {RUST_SCAN_ROOTS:?}",
        );
    }

    let env_doc_path = root.join("docs/environment-variables.md");
    let env_doc = fs::read_to_string(&env_doc_path)
        .unwrap_or_else(|err| panic!("read {}: {err}", env_doc_path.display()));

    let mut divergences: Vec<String> = Vec::new();
    for patterns in DEFAULT_PATTERNS {
        let Some((_, live, src_path)) = defaults
            .iter()
            .find(|(name, _, _)| name == patterns.constant)
        else {
            continue;
        };
        check_anchored_patterns(&root, patterns, *live, src_path, &mut divergences);
    }
    for (constant, env_name) in ENV_TABLE_BINDINGS {
        let Some((_, live, _)) = defaults.iter().find(|(name, _, _)| name == constant) else {
            continue;
        };
        check_env_table_row(&env_doc, constant, env_name, *live, &mut divergences);
    }

    assert!(
        divergences.is_empty(),
        "documented default values diverge from the live `DEFAULT_*` constants \
         (see issue #184 — defaults are duplicated across docs, man pages, and \
         CLI doc-comments without mechanical sync). Update either the constant \
         or each documented mention below:\n{}",
        divergences.join("\n"),
    );
}

/// Regression guard for issue #186: the env-var doc-sync scan must
/// reach `cli/src/` so helper-binary env reads (such as `GIT_DIR`)
/// can't quietly skip the docs-coverage check.
///
/// We assert two things:
///   1. The scan visits at least one file under each configured root.
///   2. The literal `"GIT_DIR"` is one of the values discovered.
///
/// Either failure means the scan no longer covers the CLI crate, which
/// is exactly the regression the issue fixed.
#[test]
fn scan_covers_cli_src_root() {
    let root = project_root();
    let sources = collect_all_rust_sources(&root);

    for rel in RUST_SCAN_ROOTS {
        let expected_prefix = root.join(rel);
        assert!(
            sources.iter().any(|p| p.starts_with(&expected_prefix)),
            "scan visited zero files under `{}`; RUST_SCAN_ROOTS or \
             collect_rust_files drifted",
            expected_prefix.display(),
        );
    }

    let mut declared_values: Vec<String> = Vec::new();
    for path in &sources {
        let body =
            fs::read_to_string(path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
        for (_, value) in extract_env_constants(&body) {
            declared_values.push(value);
        }
    }

    assert!(
        declared_values.iter().any(|v| v == "GIT_DIR"),
        "scan did not pick up `GIT_DIR` from cli/src/lib.rs; either the \
         constant was renamed or the scan stopped covering cli/src/. \
         Declared values found: {declared_values:?}",
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

    #[test]
    fn extract_default_picks_up_pub_u64() {
        let src = "pub const DEFAULT_LOCK_TTL_SECONDS: u64 = 60;\n\
                   pub(crate) const DEFAULT_GRACE_HOURS: u64 = 24;\n\
                   pub const DEFAULT_BIG: u64 = 1_024;\n";
        assert_eq!(
            extract_default_constants(src),
            vec![
                ("DEFAULT_LOCK_TTL_SECONDS".to_owned(), 60),
                ("DEFAULT_GRACE_HOURS".to_owned(), 24),
                ("DEFAULT_BIG".to_owned(), 1_024),
            ]
        );
    }

    #[test]
    fn extract_default_ignores_other_types() {
        // Non-u64 numeric defaults are out of scope; the scan would
        // need different anchors to validate their documented form.
        let src = "pub const DEFAULT_RATIO: f64 = 0.5;\n\
                   pub const DEFAULT_NAME: &str = \"hi\";\n";
        assert!(extract_default_constants(src).is_empty());
    }

    #[test]
    fn extract_default_ignores_private() {
        let src = "const DEFAULT_INTERNAL: u64 = 5;";
        assert!(extract_default_constants(src).is_empty());
    }

    #[test]
    fn anchored_scan_extracts_matching_digits() {
        let body = "see (falling back to 60s) for details\n\
                    unrelated 60-something line\n\
                    and (falling back to 120s) elsewhere";
        let hits = find_anchored_values(body, "(falling back to ", "s)");
        let values: Vec<u64> = hits.iter().map(|(_, _, v)| *v).collect();
        assert_eq!(values, vec![60, 120]);
        // The bare "60-something" must not match — both anchors are required.
        assert!(hits.iter().all(|(_, snippet, _)| snippet.contains("s)")));
    }

    #[test]
    fn anchored_scan_handles_multiple_matches_on_one_line() {
        let body = "first (falling back to 60s) and second (falling back to 30s) on the same line";
        let hits = find_anchored_values(body, "(falling back to ", "s)");
        let values: Vec<u64> = hits.iter().map(|(_, _, v)| *v).collect();
        assert_eq!(values, vec![60, 30]);
    }

    #[test]
    fn anchored_scan_crosses_rust_doc_comment_wrap() {
        // Rust doc-comments often wrap mid-anchor; the scan must
        // collapse `\n        /// ` into a single space so the
        // prefix still matches.
        let body =
            "        /// Default reads `ENV_X` (falling\n        /// back to 60s) — go on.\n";
        let hits = find_anchored_values(body, "(falling back to ", "s)");
        let values: Vec<u64> = hits.iter().map(|(_, _, v)| *v).collect();
        assert_eq!(values, vec![60]);
        // Line number must point to where the prefix anchor began
        // (line 1 here, where `(falling` lives).
        assert_eq!(hits[0].0, 1);
    }

    #[test]
    fn anchored_scan_handles_non_ascii_after_match() {
        // docs/getting-started.md ends the documented-default line
        // with an em-dash. UTF-8 multi-byte sequences after the match
        // must not corrupt the position-to-line mapping for matches
        // earlier in the file.
        let body = "line 1\nlock (falling back to 60s) — em-dash\nline 3";
        let hits = find_anchored_values(body, "(falling back to ", "s)");
        let values: Vec<u64> = hits.iter().map(|(_, _, v)| *v).collect();
        assert_eq!(values, vec![60]);
        assert_eq!(hits[0].0, 2);
    }

    #[test]
    fn env_table_default_extracts_value() {
        let doc = "intro\n\
                   | Variable | Default | Effect | Read at |\n\
                   |---|---|---|---|\n\
                   | `GIT_REMOTE_OBJECT_STORE_FOO` | `42` | something | `src/foo.rs` |\n\
                   trailing";
        let parsed = parse_env_table_default(doc, "GIT_REMOTE_OBJECT_STORE_FOO");
        let (_, cell, value) = parsed.expect("row present");
        assert_eq!(value, 42);
        assert_eq!(cell, "42");
    }

    #[test]
    fn env_table_default_returns_none_for_non_numeric() {
        let doc = "| `GIT_REMOTE_OBJECT_STORE_BAR` | `unset` | x | `src/bar.rs` |";
        assert!(parse_env_table_default(doc, "GIT_REMOTE_OBJECT_STORE_BAR").is_none());
    }

    #[test]
    fn env_table_default_returns_none_for_missing_row() {
        let doc = "no rows here";
        assert!(parse_env_table_default(doc, "GIT_REMOTE_OBJECT_STORE_BAR").is_none());
    }
}
