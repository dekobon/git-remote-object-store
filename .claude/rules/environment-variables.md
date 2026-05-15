## Environment Variables

Every environment variable read by the crate is documented in
[`docs/environment-variables.md`](../../docs/environment-variables.md). That
page is the single index — README, getting-started, and the man pages link
to it.

### When you add a new env var

A new env var is a public API surface, even when it has a default. The
checklist is non-negotiable:

1. **Define a constant.** `pub const ENV_<NAME>: &str = "..."` at the read
   site. Never spread the literal string across code and tests.
2. **Document it.** Add a row to `docs/environment-variables.md` in the
   correct section — `Helper runtime`, `Tests and development`, etc. Include
   the default, the effect, and the file path of the read site.
3. **Surface it where operators will look.** If it is helper-runtime
   visible, update the `ENVIRONMENT` section of the matching man page
   (`man/git-remote-*.1`). If it is operator-facing, add a mention to
   `docs/getting-started.md` (usually in the troubleshooting section).
4. **Test the read.** Add a unit test that exercises the read via the
   constant — never duplicate the literal string in the test.
5. **CHANGELOG entry** under `Added` / `Changed`, per
   `.claude/rules/changelog.md`.

`tests/env_var_doc_sync.rs` enforces step 2 mechanically — it scans every
`pub` / `pub(crate)` `const ENV_*` declaration under `src/` and fails if the
literal value is missing from `docs/environment-variables.md`. If `cargo
test` flags a missing row, add the row; do not "fix" the test by relaxing
the scan.

### When you remove or rename one

Same checklist in reverse: remove the row from
`docs/environment-variables.md`, remove the man-page entry, remove any
getting-started mention, and add a `Removed` / `Changed` CHANGELOG entry.

If a stale env var is still being read by the code but not documented, the
fix is to **document it**, not to remove the read silently.
