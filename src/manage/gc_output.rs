//! Operator-facing output for `gc` mark/sweep, shared by the
//! `gc` subcommand and `compact --with-gc`.
//!
//! Both call sites previously inlined the same four format strings
//! (issue #175). Centralising them here keeps wording in lockstep and
//! lets the byte-exact tests live next to the formatter, away from the
//! async control flow.
//!
//! All helpers take an injected `&mut impl Write` so callers may pass
//! `std::io::stdout().lock()` in production and a `Vec<u8>` in tests.
//! The management CLI is free to write to stdout per
//! `.claude/rules/protocol-stdout.md`; only the helper-protocol
//! binaries are forbidden from doing so.

use std::io::{self, Write};

use crate::packchain::gc::{MarkOutcome, SweepOutcome};

/// Singular/plural noun helper. `n == 1` selects `singular`; anything
/// else selects `plural`. Mirrors `super::fmt_partial_delete`'s
/// approach so the management surface speaks one dialect.
fn plural(n: usize, singular: &'static str, plural_form: &'static str) -> &'static str {
    if n == 1 { singular } else { plural_form }
}

/// Render the operator-visible line(s) for a completed `gc` mark pass.
///
/// Writes either "no orphan packs" or a count plus the run id. The
/// caller is responsible for any structured logging — this helper
/// only emits the human-readable line.
///
/// # Errors
///
/// Returns the underlying writer's [`io::Error`] verbatim.
pub(crate) fn format_mark_outcome<W: Write>(out: &mut W, outcome: &MarkOutcome) -> io::Result<()> {
    if outcome.orphan_count == 0 {
        writeln!(out, "gc mark: no orphan packs.")
    } else {
        let noun = plural(outcome.orphan_count, "pack", "packs");
        writeln!(
            out,
            "gc mark: {} orphan {noun} tombstoned (run id {}).",
            outcome.orphan_count, outcome.run_id,
        )
    }
}

/// Render the operator-visible line for a completed `gc` sweep pass.
///
/// Reports "no tombstones present" when neither swept nor deferred
/// counters fired; otherwise emits the four-counter summary with
/// noun pluralisation per counter.
///
/// # Errors
///
/// Returns the underlying writer's [`io::Error`] verbatim.
pub(crate) fn format_sweep_outcome<W: Write>(
    out: &mut W,
    outcome: &SweepOutcome,
) -> io::Result<()> {
    if outcome.swept_tombstones == 0 && outcome.deferred_tombstones == 0 {
        return writeln!(out, "gc sweep: no tombstones present.");
    }
    writeln!(
        out,
        "gc sweep: {} {} applied, {} {} deleted, {} repointed {} skipped, {} {} deferred.",
        outcome.swept_tombstones,
        plural(outcome.swept_tombstones, "tombstone", "tombstones"),
        outcome.deleted_objects,
        plural(outcome.deleted_objects, "object", "objects"),
        outcome.skipped_repointed_packs,
        plural(outcome.skipped_repointed_packs, "pack", "packs"),
        outcome.deferred_tombstones,
        plural(outcome.deferred_tombstones, "tombstone", "tombstones"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_mark(outcome: &MarkOutcome) -> String {
        let mut buf = Vec::new();
        format_mark_outcome(&mut buf, outcome).unwrap();
        String::from_utf8(buf).unwrap()
    }

    fn render_sweep(outcome: &SweepOutcome) -> String {
        let mut buf = Vec::new();
        format_sweep_outcome(&mut buf, outcome).unwrap();
        String::from_utf8(buf).unwrap()
    }

    const RUN_ID: &str = "11111111-2222-3333-4444-555555555555";

    fn mark(orphan_count: usize) -> MarkOutcome {
        MarkOutcome {
            run_id: RUN_ID.to_string(),
            orphan_count,
            tombstone_key: format!("repo/gc/{RUN_ID}.json"),
        }
    }

    #[test]
    fn mark_zero_orphans_renders_no_orphan_packs() {
        assert_eq!(render_mark(&mark(0)), "gc mark: no orphan packs.\n");
    }

    #[test]
    fn mark_one_orphan_uses_singular_noun() {
        assert_eq!(
            render_mark(&mark(1)),
            format!("gc mark: 1 orphan pack tombstoned (run id {RUN_ID}).\n"),
        );
    }

    #[test]
    fn mark_multiple_orphans_uses_plural_noun() {
        assert_eq!(
            render_mark(&mark(7)),
            format!("gc mark: 7 orphan packs tombstoned (run id {RUN_ID}).\n"),
        );
    }

    #[test]
    fn sweep_zero_swept_and_deferred_renders_no_tombstones() {
        let outcome = SweepOutcome::default();
        assert_eq!(render_sweep(&outcome), "gc sweep: no tombstones present.\n");
    }

    #[test]
    fn sweep_only_deferred_counter_still_renders_summary() {
        let outcome = SweepOutcome {
            swept_tombstones: 0,
            deferred_tombstones: 1,
            deleted_objects: 0,
            skipped_repointed_packs: 0,
        };
        assert_eq!(
            render_sweep(&outcome),
            "gc sweep: 0 tombstones applied, 0 objects deleted, 0 repointed packs skipped, 1 tombstone deferred.\n",
        );
    }

    #[test]
    fn sweep_all_singular_uses_singular_nouns() {
        let outcome = SweepOutcome {
            swept_tombstones: 1,
            deferred_tombstones: 1,
            deleted_objects: 1,
            skipped_repointed_packs: 1,
        };
        assert_eq!(
            render_sweep(&outcome),
            "gc sweep: 1 tombstone applied, 1 object deleted, 1 repointed pack skipped, 1 tombstone deferred.\n",
        );
    }

    #[test]
    fn sweep_all_plural_uses_plural_nouns() {
        let outcome = SweepOutcome {
            swept_tombstones: 3,
            deferred_tombstones: 2,
            deleted_objects: 6,
            skipped_repointed_packs: 4,
        };
        assert_eq!(
            render_sweep(&outcome),
            "gc sweep: 3 tombstones applied, 6 objects deleted, 4 repointed packs skipped, 2 tombstones deferred.\n",
        );
    }

    #[test]
    fn sweep_mixed_pluralisation_per_counter() {
        let outcome = SweepOutcome {
            swept_tombstones: 1,
            deferred_tombstones: 5,
            deleted_objects: 2,
            skipped_repointed_packs: 0,
        };
        assert_eq!(
            render_sweep(&outcome),
            "gc sweep: 1 tombstone applied, 2 objects deleted, 0 repointed packs skipped, 5 tombstones deferred.\n",
        );
    }
}
