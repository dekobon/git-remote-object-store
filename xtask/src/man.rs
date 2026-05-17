//! `xtask man` — render manpages for every binary the workspace ships.
//!
//! The management CLI (`git-remote-object-store`) is clap-derived, so
//! its pages and the pages for each subcommand are generated from
//! [`clap_mangen`]. The four `git-remote-{s3,az}-{http,https}` helper
//! binaries and `git-lfs-object-store` have no clap surface (they speak
//! the git remote-helper protocol or the LFS custom-transfer protocol
//! directly) and so ship hand-authored troff stubs that point at the
//! management page for option detail.
//!
//! Output goes to the top-level `man/` directory. `--check` regenerates
//! into a scratch dir and exits non-zero if the result would differ from
//! what is checked in — CI uses this to enforce that the tree never
//! drifts from the clap definition.
//!
//! The hand-authored stub set is the source of truth for the helper
//! binaries' presentation: `git-remote-s3+http`, `git-remote-s3+https`,
//! `git-remote-az+http`, `git-remote-az+https`, `git-lfs-object-store`.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::CommandFactory;

use git_remote_object_store_cli::management::Cli;

use crate::ParseOutcome;

/// Parsed `xtask man` invocation.
#[derive(Debug, Default)]
pub(crate) struct Options {
    /// Render into a scratch directory and fail if the result differs
    /// from the checked-in `man/` tree.
    pub check: bool,
}

/// Parse argv for `xtask man`.
pub(crate) fn parse(args: impl Iterator<Item = String>) -> Result<ParseOutcome<Options>> {
    let mut options = Options::default();
    for arg in args {
        match arg.as_str() {
            "--check" => options.check = true,
            "-h" | "--help" => return Ok(ParseOutcome::HelpRequested),
            other => anyhow::bail!("unknown option for man: {other}"),
        }
    }
    Ok(ParseOutcome::Run(options))
}

/// Entry point for `xtask man`.
pub(crate) fn run(options: &Options) -> Result<()> {
    let workspace_root = workspace_root()?;
    let target_dir = workspace_root.join("man");

    if options.check {
        let scratch = tempfile::tempdir().context("create scratch dir for `xtask man --check`")?;
        write_all_pages(scratch.path())?;
        compare_trees(&target_dir, scratch.path())?;
        eprintln!("man: up to date");
        return Ok(());
    }

    fs::create_dir_all(&target_dir)
        .with_context(|| format!("create output directory {}", target_dir.display()))?;
    write_all_pages(&target_dir)?;
    eprintln!("man: wrote pages to {}", target_dir.display());
    Ok(())
}

/// Write every manpage this workspace ships into `out_dir`.
fn write_all_pages(out_dir: &Path) -> Result<()> {
    // 1. Clap-derived pages for the management CLI and every subcommand.
    let root = Cli::command();
    render_clap(&root, out_dir)?;
    render_subcommands(&root, root.get_name(), out_dir)?;

    // 2. Hand-authored stubs for the helper-protocol binaries and the
    //    LFS custom-transfer agent. These point at the management
    //    binary for option detail because they have no clap CLI of
    //    their own.
    for (name, body) in HAND_AUTHORED_PAGES {
        let path = out_dir.join(format!("{name}.1"));
        fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
    }

    Ok(())
}

/// Walk every clap subcommand and render `<parent>-<sub>.1`.
fn render_subcommands(parent: &clap::Command, prefix: &str, out_dir: &Path) -> Result<()> {
    for sub in parent.get_subcommands() {
        if sub.get_name() == "help" {
            continue;
        }
        let full_name = format!("{prefix}-{}", sub.get_name());
        // Recurse first so we can hand ownership of `full_name` to clap
        // on the last line — avoids cloning it for the recursion.
        render_subcommands(sub, &full_name, out_dir)?;
        let sub_cmd = sub.clone().name(full_name);
        render_clap(&sub_cmd, out_dir)?;
    }
    Ok(())
}

/// Render a single clap-derived page.
///
/// We compose the page section-by-section instead of calling
/// [`clap_mangen::Man::render`] so we can omit the auto-generated
/// VERSION section — its body would otherwise be a literal
/// `vX.Y.Z` that drifts every release, forcing a man-page
/// regeneration commit on each bump. `--version` on the CLI is
/// the runtime source of truth; the man page does not need to
/// repeat it. For the same reason `.source(...)` carries only
/// the project name, not `project VERSION`.
fn render_clap(cmd: &clap::Command, out_dir: &Path) -> Result<()> {
    let name = cmd.get_name().to_string();
    let man = clap_mangen::Man::new(cmd.clone())
        .title(name.to_uppercase())
        .section("1")
        .source("git-remote-object-store".to_string())
        .manual("git-remote-object-store Manual".to_string());

    let mut buffer = Vec::<u8>::new();
    let ctx = || format!("render manpage for `{name}`");
    man.render_title(&mut buffer).with_context(ctx)?;
    man.render_name_section(&mut buffer).with_context(ctx)?;
    man.render_synopsis_section(&mut buffer).with_context(ctx)?;
    man.render_description_section(&mut buffer)
        .with_context(ctx)?;
    man.render_options_section(&mut buffer).with_context(ctx)?;
    man.render_subcommands_section(&mut buffer)
        .with_context(ctx)?;
    man.render_extra_section(&mut buffer).with_context(ctx)?;
    // VERSION section deliberately omitted — see fn docstring.
    man.render_authors_section(&mut buffer).with_context(ctx)?;

    let path = out_dir.join(format!("{name}.1"));
    fs::write(&path, buffer).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Compare the checked-in tree to the freshly-rendered scratch tree.
///
/// Exits non-zero if any file differs or is missing on either side. The
/// error message is a unified summary of every divergence — CI logs the
/// whole list so the operator does not need to re-run `--check` after
/// each fix.
fn compare_trees(checked_in: &Path, fresh: &Path) -> Result<()> {
    let mut errors = Vec::new();

    let fresh_files = list_files(fresh)?;
    let checked_files = list_files(checked_in).unwrap_or_default();

    for name in &fresh_files {
        let want = fresh.join(name);
        let have = checked_in.join(name);
        if !have.is_file() {
            errors.push(format!("missing from man/: {name}"));
            continue;
        }
        // `want` / `have` are joined from caller-supplied directories and
        // the `.1` filenames the same xtask just rendered. xtask is a
        // developer-only build tool; no untrusted input. The actix-web
        // path-traversal rule does not apply.
        // nosemgrep: rust.actix.path-traversal.tainted-path.tainted-path
        let want_bytes = fs::read(&want).with_context(|| format!("read {}", want.display()))?;
        // nosemgrep: rust.actix.path-traversal.tainted-path.tainted-path
        let have_bytes = fs::read(&have).with_context(|| format!("read {}", have.display()))?;
        if want_bytes != have_bytes {
            errors.push(format!("drift: man/{name}"));
        }
    }
    for name in &checked_files {
        if !fresh_files.contains(name) {
            errors.push(format!("stale page in man/: {name} (no longer produced)"));
        }
    }

    if errors.is_empty() {
        return Ok(());
    }

    // Render the error list to stderr verbatim, then bail with a hint.
    let stderr = io::stderr();
    let mut h = stderr.lock();
    for e in &errors {
        let _ = writeln!(h, "man drift: {e}");
    }
    bail!("man/ is out of date; rerun `cargo xtask man` and commit the result");
}

fn list_files(dir: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    // `dir` is a caller-supplied man-page directory inside the project;
    // xtask is a developer-only build tool with no untrusted input. The
    // actix-web path-traversal rule does not apply.
    // nosemgrep: rust.actix.path-traversal.tainted-path.tainted-path
    let read = fs::read_dir(dir).with_context(|| format!("read directory {}", dir.display()))?;
    for entry in read {
        let entry = entry.with_context(|| format!("read entry under {}", dir.display()))?;
        if let Some(name) = entry.file_name().to_str()
            && name.ends_with(".1")
        {
            out.push(name.to_owned());
        }
    }
    out.sort();
    Ok(out)
}

/// Resolve the workspace root (the parent of `xtask/`).
fn workspace_root() -> Result<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .map(Path::to_path_buf)
        .context("xtask must be a workspace member with a parent directory")
}

/// `(filename without `.1`, troff body)` for each hand-authored page.
///
/// Bodies use literal troff because the helper binaries have no clap
/// CLI to autogenerate from. Keep them short — they point at the
/// management binary's page for everything beyond URL grammar and the
/// few environment variables that gate behaviour.
const HAND_AUTHORED_PAGES: &[(&str, &str)] = &[
    (
        "git-remote-s3+http",
        include_str!("../../man/git-remote-s3+http.1"),
    ),
    (
        "git-remote-s3+https",
        include_str!("../../man/git-remote-s3+https.1"),
    ),
    (
        "git-remote-az+http",
        include_str!("../../man/git-remote-az+http.1"),
    ),
    (
        "git-remote-az+https",
        include_str!("../../man/git-remote-az+https.1"),
    ),
    (
        "git-lfs-object-store",
        include_str!("../../man/git-lfs-object-store.1"),
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&str]) -> Result<ParseOutcome<Options>> {
        parse(args.iter().map(|s| (*s).to_owned()))
    }

    #[test]
    fn parse_man_no_args_runs_default() {
        let outcome = parse_args(&[]).expect("parse");
        let ParseOutcome::Run(options) = outcome else {
            panic!("expected Run, got HelpRequested");
        };
        assert!(!options.check);
    }

    #[test]
    fn parse_man_check_flag_sets_check() {
        let outcome = parse_args(&["--check"]).expect("parse");
        let ParseOutcome::Run(options) = outcome else {
            panic!("expected Run");
        };
        assert!(options.check);
    }

    #[test]
    fn parse_man_help_short_returns_help_requested() {
        assert!(matches!(
            parse_args(&["-h"]).expect("parse"),
            ParseOutcome::HelpRequested
        ));
    }

    #[test]
    fn parse_man_help_long_returns_help_requested() {
        assert!(matches!(
            parse_args(&["--help"]).expect("parse"),
            ParseOutcome::HelpRequested
        ));
    }

    #[test]
    fn parse_man_unknown_option_errors() {
        let err = parse_args(&["--what"]).expect_err("must reject");
        assert!(
            err.to_string().contains("unknown option"),
            "error should mention 'unknown option', got: {err}"
        );
    }
}
