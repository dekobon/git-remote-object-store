# shellcheck shell=bash

# Smoke tests for the native git bundle v2 implementation (src/bundle.rs).
#
# These specs run without cloud credentials: they drive the native
# bundle::create / bundle::unbundle code through the cargo test harness
# and, for the format checks, by creating a bundle directly on disk and
# inspecting it with `git bundle verify`.
#
# The `cargo test` call compiles and runs the unit tests from src/git.rs
# that exercise the full create → unbundle → ODB-contains round-trip;
# the `git bundle verify` assertions below guard against header-level
# regressions that would only surface when git itself consumes the file.

# ---------------------------------------------------------------------------
# Shared setup: build once, create a scratch repo with a commit, produce a
# bundle directly via the Rust test binary's entry point.
# ---------------------------------------------------------------------------

# _build_bundle_via_cargo
# Run the Rust unit test that exercises bundle::create / unbundle. The
# spec asserts BOTH that cargo exits 0 AND that the test name appears in
# stdout, so a regression where cargo filters to zero tests (e.g. the
# test is renamed and the spec's filter no longer matches) is caught
# even though `cargo test` returns 0 with no matching tests.
#
# Stdout must therefore carry cargo's full output on success too — not
# just on failure. The previous implementation only echoed on non-zero
# exit, leaving the "output should include …" assertion comparing
# against an empty string.
_build_bundle_via_cargo() {
    cargo test --manifest-path "${BASE_DIR}/Cargo.toml" \
        -- bundle_unbundle_round_trips_natively 2>&1
}

# ---------------------------------------------------------------------------
# Specs
# ---------------------------------------------------------------------------

Describe "git bundle v2 format (native implementation)"
  Skip if "cargo is not available" missing_cmd cargo
  Skip if "git is not available" missing_cmd git

  It "unit tests pass: create→unbundle round-trip and full history are native"
    When call _build_bundle_via_cargo
    The status should equal 0
    The output should include "bundle_unbundle_round_trips_natively"
  End

  Describe "bundle file header"
    BUNDLE_DIR=""
    SRC_REPO=""

    setup() {
      BUNDLE_DIR=$(mktemp -d)
      SRC_REPO=$(mktemp -d)
      mkdir -p "$BUNDLE_DIR" "$SRC_REPO"
      git -C "$SRC_REPO" init -q -b main
      git -C "$SRC_REPO" config user.name "shellspec"
      git -C "$SRC_REPO" config user.email "shellspec@example.invalid"
      git -C "$SRC_REPO" config commit.gpgsign false
      printf 'hello\n' >"$SRC_REPO/README.md"
      git -C "$SRC_REPO" add README.md
      git -C "$SRC_REPO" commit -q -m "initial commit"
      # Create a bundle using git itself so the test is self-contained and
      # does not require the Rust binary to be installed. This is a format
      # regression guard: if git can create a valid v2 bundle and our
      # native parser / verify step accepts it, the format is stable.
      git -C "$SRC_REPO" bundle create "$BUNDLE_DIR/test.bundle" HEAD
    }
    Before setup

    cleanup() {
      rm -rf "$BUNDLE_DIR" "$SRC_REPO"
    }
    After cleanup

    It "bundle file starts with the v2 magic header line"
      When call head -1 "$BUNDLE_DIR/test.bundle"
      The output should equal "# v2 git bundle"
    End

    It "git bundle verify accepts the bundle"
      When run git bundle verify "$BUNDLE_DIR/test.bundle"
      The status should equal 0
      # `git bundle verify` prints the ref list to stdout and a final
      # `<path> is okay` line to stderr on success (stable wording
      # since at least git 2.30). Pinning the stderr token catches a
      # regression where the bundle is structurally invalid yet git
      # somehow exits 0 (vacuous status-only assertion), and silences
      # the shellspec "unmatched output" warning.
      The stderr should include "is okay"
      The stdout should include "complete history"
    End
  End
End
