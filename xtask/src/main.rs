//! Workspace automation entry point.
//!
//! Run via the workspace alias defined in `.cargo/config.toml`:
//!
//! ```text
//! cargo xtask install [--bin-dir <DIR>] [--no-install] [--dry-run]
//! cargo xtask man     [--check]
//! ```
//!
//! New automations belong here as sibling modules (e.g. `bench`).

// xtask is an ordinary CLI, not a wire-protocol binary, so it follows the
// standard Unix convention: `--help` to stdout, errors and progress to
// stderr. The workspace-wide ban on `print!`/`println!` (see
// `clippy.toml`) exists to protect the helper-protocol binaries; this
// file opts out the same way `cli/src/bin/git-remote-object-store.rs`
// does.
#![allow(clippy::disallowed_macros)]

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, bail};

mod install;
mod man;

const USAGE: &str = "\
cargo xtask <subcommand> [options]

Subcommands:
  install     Install helper binaries and create `+`-form symlinks
  man         Render manpages for every shipped binary into `man/`

Run `cargo xtask <subcommand> --help` for subcommand-specific options.
";

const INSTALL_USAGE: &str = "\
cargo xtask install [options]

Runs `cargo install --path cli --force`, then creates the four `+`-form
symlinks git invokes by scheme name (git-remote-s3+https, …) alongside
the cargo-installed hyphenated binaries. Re-runs are idempotent.

Options:
  --bin-dir <PATH>   Directory holding the cargo-installed binaries.
                     Defaults to $CARGO_INSTALL_ROOT/bin, then
                     $CARGO_HOME/bin, then $HOME/.cargo/bin.
  --no-install       Skip `cargo install`; just refresh the symlinks.
  --dry-run          Print the planned actions without writing.
  -h, --help         Show this message.
";

const MAN_USAGE: &str = "\
cargo xtask man [options]

Renders the workspace's manpages into the top-level `man/` directory:
the management CLI (`git-remote-object-store`) and its subcommands are
generated from the clap definition; the four `git-remote-{s3,az}-{http,
https}` helper binaries and `git-lfs-object-store` ship hand-authored
stubs.

Options:
  --check     Regenerate into a scratch dir and exit non-zero if the
              result differs from the checked-in `man/` tree.
  -h, --help  Show this message.
";

/// Outcome of parsing a subcommand's arguments. The `HelpRequested` arm
/// keeps the help path free of `process::exit` so flow control stays
/// inside `main()` and the function remains unit-testable.
#[derive(Debug)]
pub(crate) enum ParseOutcome<T> {
    Run(T),
    HelpRequested,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("xtask: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(subcommand) = args.next() else {
        eprint!("{USAGE}");
        bail!("missing subcommand");
    };

    match subcommand.as_str() {
        "install" => match parse_install(args)? {
            ParseOutcome::Run(options) => install::run(&options),
            ParseOutcome::HelpRequested => {
                print!("{INSTALL_USAGE}");
                Ok(())
            }
        },
        "man" => match man::parse(args)? {
            ParseOutcome::Run(options) => man::run(&options),
            ParseOutcome::HelpRequested => {
                print!("{MAN_USAGE}");
                Ok(())
            }
        },
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(())
        }
        other => {
            eprint!("{USAGE}");
            bail!("unknown subcommand: {other}")
        }
    }
}

fn parse_install(args: impl Iterator<Item = String>) -> Result<ParseOutcome<install::Options>> {
    let mut options = install::Options::default();
    let mut args = args;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bin-dir" => {
                let Some(value) = args.next() else {
                    bail!("--bin-dir requires a path argument");
                };
                options.bin_dir = Some(PathBuf::from(value));
            }
            "--no-install" => options.no_install = true,
            "--dry-run" => options.dry_run = true,
            "-h" | "--help" => return Ok(ParseOutcome::HelpRequested),
            other => bail!("unknown option for install: {other}"),
        }
    }
    Ok(ParseOutcome::Run(options))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<ParseOutcome<install::Options>> {
        parse_install(args.iter().map(|s| (*s).to_owned()))
    }

    #[test]
    fn parse_install_defaults_when_no_args() {
        let outcome = parse(&[]).expect("parse");
        let ParseOutcome::Run(options) = outcome else {
            panic!("expected Run, got HelpRequested");
        };
        assert!(options.bin_dir.is_none());
        assert!(!options.no_install);
        assert!(!options.dry_run);
    }

    #[test]
    fn parse_install_bin_dir_consumes_following_arg() {
        let outcome = parse(&["--bin-dir", "/custom/bin"]).expect("parse");
        let ParseOutcome::Run(options) = outcome else {
            panic!("expected Run");
        };
        assert_eq!(options.bin_dir, Some(PathBuf::from("/custom/bin")));
    }

    #[test]
    fn parse_install_bin_dir_without_value_errors() {
        let err = parse(&["--bin-dir"]).expect_err("must require value");
        assert!(
            err.to_string()
                .contains("--bin-dir requires a path argument"),
            "error should explain the missing value, got: {err}"
        );
    }

    #[test]
    fn parse_install_sets_no_install_flag() {
        let outcome = parse(&["--no-install"]).expect("parse");
        let ParseOutcome::Run(options) = outcome else {
            panic!("expected Run");
        };
        assert!(options.no_install);
        assert!(!options.dry_run);
    }

    #[test]
    fn parse_install_sets_dry_run_flag() {
        let outcome = parse(&["--dry-run"]).expect("parse");
        let ParseOutcome::Run(options) = outcome else {
            panic!("expected Run");
        };
        assert!(options.dry_run);
        assert!(!options.no_install);
    }

    #[test]
    fn parse_install_combines_flags_with_bin_dir() {
        let outcome = parse(&["--no-install", "--bin-dir", "/x", "--dry-run"]).expect("parse");
        let ParseOutcome::Run(options) = outcome else {
            panic!("expected Run");
        };
        assert_eq!(options.bin_dir, Some(PathBuf::from("/x")));
        assert!(options.no_install);
        assert!(options.dry_run);
    }

    #[test]
    fn parse_install_help_short_returns_help_requested() {
        assert!(matches!(
            parse(&["-h"]).expect("parse"),
            ParseOutcome::HelpRequested
        ));
    }

    #[test]
    fn parse_install_help_long_returns_help_requested() {
        assert!(matches!(
            parse(&["--help"]).expect("parse"),
            ParseOutcome::HelpRequested
        ));
    }

    #[test]
    fn parse_install_unknown_option_errors() {
        let err = parse(&["--what"]).expect_err("must reject");
        assert!(
            err.to_string().contains("unknown option"),
            "error should mention 'unknown option', got: {err}"
        );
    }
}
