//! `xtask install` — install the helper binaries and create the `+`-form
//! symlinks git invokes by scheme name.
//!
//! Cargo refuses `+` in `[[bin]] name`, so the four helper binaries ship
//! hyphenated (`git-remote-s3-https`, …). Git looks them up by scheme
//! (`git-remote-s3+https` for an `s3+https://...` URL), so each hyphenated
//! binary needs a `+`-named symlink alongside it. This module automates the
//! symlink step that the README used to ask users to script by hand.

use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// The four `(cargo-name, plus-name)` pairs that need a symlink. The
/// hyphenated names are what `cargo install` produces; the `+`-form names
/// are what git resolves from the URL scheme.
///
/// Keep this list in sync with `cli/Cargo.toml` `[[bin]]` entries and with
/// the packaging post-install scripts (Debian / RPM / Alpine / Homebrew).
pub(crate) const HELPER_PAIRS: &[(&str, &str)] = &[
    ("git-remote-s3-https", "git-remote-s3+https"),
    ("git-remote-s3-http", "git-remote-s3+http"),
    ("git-remote-az-https", "git-remote-az+https"),
    ("git-remote-az-http", "git-remote-az+http"),
];

/// Parsed `install` invocation.
#[derive(Debug, Default)]
pub(crate) struct Options {
    /// Explicit override for the directory holding cargo-installed
    /// binaries. When `None`, [`resolve_bin_dir`] consults the environment.
    pub bin_dir: Option<PathBuf>,
    /// Skip the `cargo install --path cli` step (symlinks only).
    pub no_install: bool,
    /// Print the planned actions without writing the filesystem or
    /// spawning cargo.
    pub dry_run: bool,
}

/// Abstraction over `std::env::var_os` so the resolver is unit-testable
/// without mutating process-wide environment state (which is `unsafe` in
/// Rust 2024 under threaded test runners).
pub(crate) trait EnvSource {
    fn get(&self, key: &str) -> Option<OsString>;
}

/// Production [`EnvSource`] backed by the real process environment.
pub(crate) struct StdEnv;

impl EnvSource for StdEnv {
    fn get(&self, key: &str) -> Option<OsString> {
        std::env::var_os(key)
    }
}

/// Resolve the directory cargo installs binaries into.
///
/// Precedence (matches cargo's own rules):
///
/// 1. `--bin-dir <PATH>` (the `bin_dir` field on [`Options`]).
/// 2. `$CARGO_INSTALL_ROOT/bin`.
/// 3. `$CARGO_HOME/bin`.
/// 4. `$HOME/.cargo/bin`.
///
/// Errors if none of the above is set.
pub(crate) fn resolve_bin_dir(options: &Options, env: &impl EnvSource) -> Result<PathBuf> {
    if let Some(explicit) = &options.bin_dir {
        return Ok(explicit.clone());
    }
    if let Some(install_root) = env.get("CARGO_INSTALL_ROOT") {
        return Ok(PathBuf::from(install_root).join("bin"));
    }
    if let Some(cargo_home) = env.get("CARGO_HOME") {
        return Ok(PathBuf::from(cargo_home).join("bin"));
    }
    if let Some(home) = env.get("HOME") {
        return Ok(PathBuf::from(home).join(".cargo").join("bin"));
    }
    bail!(
        "cannot determine cargo install bin directory: pass --bin-dir, \
         or set CARGO_INSTALL_ROOT, CARGO_HOME, or HOME"
    )
}

/// Outcome of a single [`refresh_symlink`] call. Returned so callers can
/// summarise what happened without inspecting the filesystem twice. The
/// `Would*` variants are only returned in dry-run mode; the side-effecting
/// variants are only returned outside dry-run mode.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LinkOutcome {
    /// No prior entry; symlink created.
    Created,
    /// Prior symlink found and replaced.
    Replaced,
    /// `dry_run` set, no prior entry; the link would have been created.
    WouldCreate,
    /// `dry_run` set, prior symlink found; the link would have been replaced.
    WouldReplace,
}

impl LinkOutcome {
    /// Human-readable verb for status output.
    fn verb(self) -> &'static str {
        match self {
            LinkOutcome::Created => "created",
            LinkOutcome::Replaced => "replaced",
            LinkOutcome::WouldCreate => "would create",
            LinkOutcome::WouldReplace => "would replace",
        }
    }
}

/// Create (or refresh) a single `<bin_dir>/<plus_name>` symlink pointing
/// at the sibling `<cargo_name>`.
///
/// Behaviour:
///
/// - If the cargo-named target does not exist, returns an error (the user
///   skipped `cargo install` or pointed `--bin-dir` at the wrong place).
/// - If the link path already holds a symlink, it is removed and recreated
///   — this is what makes re-runs idempotent.
/// - If the link path holds a regular file or directory, refuses to
///   clobber it. The user has something we did not create at that path.
/// - The created link is **relative** (just the cargo-name), so the link
///   keeps working if the bin directory is moved as a whole.
#[cfg(unix)]
pub(crate) fn refresh_symlink(
    bin_dir: &Path,
    cargo_name: &str,
    plus_name: &str,
    dry_run: bool,
) -> Result<LinkOutcome> {
    let target = bin_dir.join(cargo_name);
    let link = bin_dir.join(plus_name);

    if !target.exists() {
        bail!(
            "missing cargo-installed helper at {} \
             (run `cargo install --path cli` first, or pass --bin-dir)",
            target.display()
        );
    }

    let prior = match fs::symlink_metadata(&link) {
        Ok(meta) => Some(meta),
        Err(err) if err.kind() == ErrorKind::NotFound => None,
        Err(err) => {
            return Err(err)
                .with_context(|| format!("inspect existing entry at {}", link.display()));
        }
    };

    let replacing = match &prior {
        Some(meta) if meta.is_symlink() => true,
        Some(_) => {
            bail!(
                "{} exists and is not a symlink; refusing to overwrite",
                link.display()
            );
        }
        None => false,
    };

    if dry_run {
        return Ok(if replacing {
            LinkOutcome::WouldReplace
        } else {
            LinkOutcome::WouldCreate
        });
    }

    if replacing {
        fs::remove_file(&link)
            .with_context(|| format!("remove existing symlink {}", link.display()))?;
    }

    std::os::unix::fs::symlink(cargo_name, &link)
        .with_context(|| format!("create symlink {} -> {}", link.display(), cargo_name))?;

    Ok(if replacing {
        LinkOutcome::Replaced
    } else {
        LinkOutcome::Created
    })
}

#[cfg(not(unix))]
pub(crate) fn refresh_symlink(
    _bin_dir: &Path,
    _cargo_name: &str,
    _plus_name: &str,
    _dry_run: bool,
) -> Result<LinkOutcome> {
    bail!(
        "xtask install is Unix-only today; on Windows install with `cargo \
         install --path cli` and configure the `+`-form helper names via \
         your shell or a PATH wrapper"
    )
}

/// Run `cargo install --path cli --force`. `--force` makes the install
/// step idempotent when re-running the xtask after a code change.
fn run_cargo_install(workspace_root: &Path) -> Result<()> {
    let status = Command::new(env!("CARGO"))
        .args(["install", "--path", "cli", "--force"])
        .current_dir(workspace_root)
        .status()
        .context("spawn `cargo install --path cli --force`")?;
    if !status.success() {
        bail!("`cargo install --path cli --force` failed with status {status}");
    }
    Ok(())
}

/// Workspace root (the directory holding the top-level `Cargo.toml`).
/// `env!("CARGO_MANIFEST_DIR")` points at `xtask/`; the workspace root
/// is its parent.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

/// Entry point for `cargo xtask install`. Resolves the install bin dir,
/// optionally runs `cargo install`, then refreshes every `+`-form symlink.
pub(crate) fn run(options: &Options) -> Result<()> {
    let bin_dir = resolve_bin_dir(options, &StdEnv)?;

    if options.no_install {
        eprintln!("xtask install: skipping cargo install (--no-install)");
    } else if options.dry_run {
        eprintln!(
            "xtask install: would run `cargo install --path cli --force` \
             in {}",
            workspace_root().display()
        );
    } else {
        eprintln!("xtask install: running `cargo install --path cli --force`");
        run_cargo_install(&workspace_root())?;
    }

    eprintln!("xtask install: bin directory: {}", bin_dir.display());

    for (cargo_name, plus_name) in HELPER_PAIRS {
        let outcome = refresh_symlink(&bin_dir, cargo_name, plus_name, options.dry_run)?;
        eprintln!("  {} {plus_name} -> {cargo_name}", outcome.verb());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct MapEnv(HashMap<String, OsString>);

    impl MapEnv {
        fn new() -> Self {
            Self(HashMap::new())
        }
        fn with(mut self, key: &str, value: &str) -> Self {
            self.0.insert(key.to_owned(), OsString::from(value));
            self
        }
    }

    impl EnvSource for MapEnv {
        fn get(&self, key: &str) -> Option<OsString> {
            self.0.get(key).cloned()
        }
    }

    #[test]
    fn resolve_bin_dir_prefers_explicit_flag() {
        let opts = Options {
            bin_dir: Some(PathBuf::from("/explicit/bin")),
            ..Options::default()
        };
        let env = MapEnv::new()
            .with("CARGO_INSTALL_ROOT", "/should/not/win")
            .with("CARGO_HOME", "/should/not/win-either")
            .with("HOME", "/home/x");
        assert_eq!(
            resolve_bin_dir(&opts, &env).expect("resolve"),
            PathBuf::from("/explicit/bin")
        );
    }

    #[test]
    fn resolve_bin_dir_uses_cargo_install_root_over_cargo_home() {
        let env = MapEnv::new()
            .with("CARGO_INSTALL_ROOT", "/cir")
            .with("CARGO_HOME", "/ch")
            .with("HOME", "/h");
        assert_eq!(
            resolve_bin_dir(&Options::default(), &env).expect("resolve"),
            PathBuf::from("/cir/bin")
        );
    }

    #[test]
    fn resolve_bin_dir_uses_cargo_home_over_home() {
        let env = MapEnv::new().with("CARGO_HOME", "/ch").with("HOME", "/h");
        assert_eq!(
            resolve_bin_dir(&Options::default(), &env).expect("resolve"),
            PathBuf::from("/ch/bin")
        );
    }

    #[test]
    fn resolve_bin_dir_falls_back_to_home_dot_cargo_bin() {
        let env = MapEnv::new().with("HOME", "/home/user");
        assert_eq!(
            resolve_bin_dir(&Options::default(), &env).expect("resolve"),
            PathBuf::from("/home/user/.cargo/bin")
        );
    }

    #[test]
    fn resolve_bin_dir_errors_when_no_signal() {
        let env = MapEnv::new();
        assert!(resolve_bin_dir(&Options::default(), &env).is_err());
    }

    // The symlink behaviour is Unix-only; the tests gate on the same cfg
    // so a Windows build of the xtask still compiles cleanly.
    #[cfg(unix)]
    mod symlink {
        use super::*;
        use std::fs::File;
        use tempfile::TempDir;

        fn make_target(bin_dir: &Path, name: &str) {
            File::create(bin_dir.join(name)).expect("create target");
        }

        #[test]
        fn creates_when_absent() {
            let dir = TempDir::new().expect("tempdir");
            make_target(dir.path(), "git-remote-s3-https");
            let outcome = refresh_symlink(
                dir.path(),
                "git-remote-s3-https",
                "git-remote-s3+https",
                false,
            )
            .expect("refresh");
            assert_eq!(outcome, LinkOutcome::Created);

            let link = dir.path().join("git-remote-s3+https");
            let read = fs::read_link(&link).expect("read link");
            assert_eq!(read, PathBuf::from("git-remote-s3-https"));
        }

        #[test]
        fn replaces_existing_symlink() {
            let dir = TempDir::new().expect("tempdir");
            make_target(dir.path(), "git-remote-s3-https");
            // Pre-existing symlink to a stale target. We do not require
            // the stale target to exist — broken symlinks must still be
            // refreshed cleanly.
            std::os::unix::fs::symlink("stale-target", dir.path().join("git-remote-s3+https"))
                .expect("seed stale symlink");

            let outcome = refresh_symlink(
                dir.path(),
                "git-remote-s3-https",
                "git-remote-s3+https",
                false,
            )
            .expect("refresh");
            assert_eq!(outcome, LinkOutcome::Replaced);

            let read = fs::read_link(dir.path().join("git-remote-s3+https")).expect("read link");
            assert_eq!(read, PathBuf::from("git-remote-s3-https"));
        }

        #[test]
        fn refuses_to_clobber_regular_file() {
            let dir = TempDir::new().expect("tempdir");
            make_target(dir.path(), "git-remote-s3-https");
            File::create(dir.path().join("git-remote-s3+https")).expect("seed regular file");

            let err = refresh_symlink(
                dir.path(),
                "git-remote-s3-https",
                "git-remote-s3+https",
                false,
            )
            .expect_err("must refuse to clobber");
            assert!(
                err.to_string().contains("not a symlink"),
                "error should mention 'not a symlink', got: {err}"
            );
            // The regular file must still be there, unmodified.
            assert!(
                dir.path()
                    .join("git-remote-s3+https")
                    .symlink_metadata()
                    .expect("metadata")
                    .is_file(),
                "regular file should not have been touched"
            );
        }

        #[test]
        fn refuses_to_clobber_directory() {
            let dir = TempDir::new().expect("tempdir");
            make_target(dir.path(), "git-remote-s3-https");
            fs::create_dir(dir.path().join("git-remote-s3+https")).expect("seed dir");

            let err = refresh_symlink(
                dir.path(),
                "git-remote-s3-https",
                "git-remote-s3+https",
                false,
            )
            .expect_err("must refuse to clobber a directory");
            assert!(err.to_string().contains("not a symlink"));
        }

        #[test]
        fn missing_target_errors() {
            let dir = TempDir::new().expect("tempdir");
            // Note: no target file created.
            let err = refresh_symlink(
                dir.path(),
                "git-remote-s3-https",
                "git-remote-s3+https",
                false,
            )
            .expect_err("missing target must error");
            assert!(
                err.to_string().contains("missing cargo-installed helper"),
                "error should explain the missing helper, got: {err}"
            );
        }

        #[test]
        fn dry_run_reports_would_create_when_link_absent() {
            let dir = TempDir::new().expect("tempdir");
            make_target(dir.path(), "git-remote-s3-https");

            let outcome = refresh_symlink(
                dir.path(),
                "git-remote-s3-https",
                "git-remote-s3+https",
                true,
            )
            .expect("refresh");
            assert_eq!(outcome, LinkOutcome::WouldCreate);
            assert!(
                !dir.path().join("git-remote-s3+https").exists(),
                "dry-run must not create the symlink"
            );
        }

        #[test]
        fn dry_run_reports_would_replace_when_symlink_present() {
            let dir = TempDir::new().expect("tempdir");
            make_target(dir.path(), "git-remote-s3-https");
            std::os::unix::fs::symlink("stale-target", dir.path().join("git-remote-s3+https"))
                .expect("seed stale symlink");

            let outcome = refresh_symlink(
                dir.path(),
                "git-remote-s3-https",
                "git-remote-s3+https",
                true,
            )
            .expect("refresh");
            assert_eq!(outcome, LinkOutcome::WouldReplace);
            // Still points at the stale target — dry-run must not refresh.
            let read = fs::read_link(dir.path().join("git-remote-s3+https")).expect("read link");
            assert_eq!(read, PathBuf::from("stale-target"));
        }

        #[test]
        fn idempotent_rerun_replaces_each_link() {
            let dir = TempDir::new().expect("tempdir");
            for (cargo_name, _) in HELPER_PAIRS {
                make_target(dir.path(), cargo_name);
            }

            // First pass — all four should be Created.
            for (cargo_name, plus_name) in HELPER_PAIRS {
                let outcome =
                    refresh_symlink(dir.path(), cargo_name, plus_name, false).expect("refresh");
                assert_eq!(outcome, LinkOutcome::Created);
            }
            // Second pass — all four should be Replaced.
            for (cargo_name, plus_name) in HELPER_PAIRS {
                let outcome =
                    refresh_symlink(dir.path(), cargo_name, plus_name, false).expect("refresh");
                assert_eq!(outcome, LinkOutcome::Replaced);
            }
            // Final state: every `+`-named link resolves to its sibling.
            for (cargo_name, plus_name) in HELPER_PAIRS {
                let read = fs::read_link(dir.path().join(plus_name)).expect("read link");
                assert_eq!(read, PathBuf::from(*cargo_name));
            }
        }
    }

    /// `HELPER_PAIRS` is the source of truth for the `+`-form symlink set.
    /// Five out-of-tree packaging scripts apply the same step at install
    /// time on their respective package formats (deb, rpm, apk, brew).
    /// A typo on one side will desync the install set silently; these
    /// tests assert every pair appears as an active install / removal
    /// line in each script.
    mod packaging_sync {
        use super::*;

        const WORKSPACE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/..");

        fn read(rel: &str) -> String {
            let path = format!("{WORKSPACE_ROOT}/{rel}");
            fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {path}: {err}"))
        }

        /// Files that create the `+`-form names. Each pair must appear on
        /// a single line — `ln -sf <cargo> <plus>`, `"cargo" => "plus"`,
        /// etc. — so that comments and package descriptions mentioning
        /// one name in isolation cannot mask the drift case where an
        /// active install line is deleted.
        const CREATE_SIDE_FILES: &[&str] = &[
            "cli/Cargo.toml",
            "packaging/debian/postinst",
            "packaging/alpine/APKBUILD.in",
            "packaging/homebrew/git-remote-object-store.rb.tmpl",
        ];

        /// Files that only remove the `+`-form names. They never mention
        /// the cargo-name, so the pair-on-same-line rule does not apply;
        /// substring search is sufficient because plus-form names only
        /// appear in active `rm` lines (header comments are generic).
        const REMOVE_SIDE_FILES: &[&str] = &["packaging/debian/prerm"];

        #[test]
        fn create_side_scripts_pair_cargo_and_plus_names_on_same_line() {
            for rel in CREATE_SIDE_FILES {
                let body = read(rel);
                for (cargo_name, plus_name) in HELPER_PAIRS {
                    let paired = body
                        .lines()
                        .any(|line| line.contains(cargo_name) && line.contains(plus_name));
                    assert!(
                        paired,
                        "{rel} has no line that mentions both {cargo_name} and \
                         {plus_name} together. Comments or package descriptions \
                         mentioning one name in isolation do not count — each \
                         create-side script must contain an active install line \
                         (`ln -sf …`, `\"cargo\" => \"plus\"`, etc.) that maps \
                         the cargo-name to the `+`-form. Source of truth: \
                         HELPER_PAIRS in xtask/src/install.rs."
                    );
                }
            }
        }

        #[test]
        fn remove_side_scripts_mention_every_plus_form() {
            for rel in REMOVE_SIDE_FILES {
                let body = read(rel);
                for (_, plus_name) in HELPER_PAIRS {
                    assert!(
                        body.contains(plus_name),
                        "{rel} is missing `+`-form name {plus_name} \
                         (HELPER_PAIRS drift: update xtask/src/install.rs \
                         and {rel} together)"
                    );
                }
            }
        }
    }
}
