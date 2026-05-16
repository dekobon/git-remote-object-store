//! `gc` subcommand for the management CLI (issue #66, Phase 5 of #52).
//!
//! Runs the two-phase mark-and-sweep flow defined in
//! [`crate::packchain::gc`]. Three operating modes:
//!
//! - **Default**: mark, then sweep. Tombstones from the current mark
//!   pass do not become eligible for sweep until at least
//!   `grace_hours` elapse, so the same invocation only sweeps
//!   tombstones from earlier runs. This is the cron-friendly shape:
//!   schedule `gc` weekly and previous weeks' tombstones age out
//!   while the current week's tombstones wait.
//! - **`--mark-only`**: produce a tombstone but do not sweep. Useful
//!   in CI to surface orphan counts without bucket mutation.
//! - **`--sweep-only`**: skip mark, only process pre-existing
//!   tombstones. Operators use this after a manual mark phase or
//!   to re-attempt deletions a previous sweep skipped.
//!
//! All output is human-readable on stdout; the management CLI may
//! write to stdout per `.claude/rules/protocol-stdout.md`. The formatter
//! lives in [`super::gc_output`] so this subcommand and
//! `compact --with-gc` (see [`super::compact`]) cannot drift apart.

use std::io::Write;
use std::sync::Arc;

use tracing::info;

use super::ManageError;
use super::gc_output::{format_mark_outcome, format_sweep_outcome};
use crate::object_store::ObjectStore;
use crate::packchain::gc;

/// Tunables for [`Gc::run`]. Field semantics mirror the CLI flags.
#[derive(Debug, Clone, Copy, Default)]
pub struct GcOpts {
    /// Operating mode. The two-boolean `mark_only`/`sweep_only`
    /// shape was prone to silently no-oping the conflicting
    /// combination; the enum makes the three valid states the only
    /// representable states.
    pub mode: GcMode,
    /// `force` mode for sweep: bypass grace window and the orphan
    /// re-check. Operator-asserted safe.
    pub force: bool,
    /// Grace window in hours before a tombstone becomes eligible for
    /// sweep. `None` falls back to [`crate::packchain::gc::grace_hours_from_env`]
    /// which honours `GIT_REMOTE_OBJECT_STORE_GC_GRACE_HOURS` (defaulting
    /// to [`gc::DEFAULT_GRACE_HOURS`] when unset).
    pub grace_hours: Option<u64>,
}

/// Which phases of the mark-and-sweep flow [`Gc::run`] executes.
///
/// The `--mark-only` / `--sweep-only` CLI flags translate into the
/// matching variant at the parser boundary; passing both flags is
/// rejected there (rather than silently degenerating to a no-op,
/// which the previous two-boolean representation would have done).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GcMode {
    /// Mark then sweep. Cron-friendly: previous runs' tombstones
    /// age out while the current run's tombstones wait the grace
    /// window.
    #[default]
    Default,
    /// Mark only — produce a tombstone, do not delete.
    MarkOnly,
    /// Sweep only — process pre-existing tombstones, do not mark.
    SweepOnly,
}

/// `gc` runner. Held by the CLI for the lifetime of one invocation.
pub struct Gc {
    store: Arc<dyn ObjectStore>,
    prefix: String,
    opts: GcOpts,
}

impl Gc {
    /// Construct a runner. `prefix` is the parsed remote URL's
    /// repository prefix without a trailing slash; pass an empty
    /// string for bucket-root repositories.
    #[must_use]
    pub fn new(store: Arc<dyn ObjectStore>, prefix: impl Into<String>, opts: GcOpts) -> Self {
        Self {
            store,
            prefix: prefix.into(),
            opts,
        }
    }

    /// Execute the configured flow.
    ///
    /// # Errors
    ///
    /// Returns [`ManageError::Store`] for transport failures and
    /// [`ManageError::Packchain`] for engine-level failures (corrupt
    /// `chain.json`, schema-version mismatch).
    pub async fn run(&self) -> Result<(), ManageError> {
        // Match `Compact::run` and the helper-protocol writers: pass
        // an unlocked `Stdout` rather than holding a `StdoutLock`
        // across `.await` points. Holding the lock through the
        // mark/sweep network round-trips would (a) make this future
        // `!Send` for no reason and (b) block any other `println!`
        // for the duration of the I/O. `Write` on `Stdout` already
        // takes the lock per-call.
        self.run_with_writer(&mut std::io::stdout()).await
    }

    async fn run_with_writer<W: Write>(&self, out: &mut W) -> Result<(), ManageError> {
        let store_ref = self.store.as_ref();

        if self.opts.mode != GcMode::SweepOnly {
            let mark_outcome = gc::mark(store_ref, &self.prefix, gc::MarkOpts::default()).await?;
            format_mark_outcome(out, &mark_outcome)?;
            if mark_outcome.orphan_count != 0 {
                info!(
                    run_id = %mark_outcome.run_id,
                    key = %mark_outcome.tombstone_key,
                    "gc mark completed",
                );
            }
        }

        if self.opts.mode != GcMode::MarkOnly {
            let grace_hours = self
                .opts
                .grace_hours
                .unwrap_or_else(gc::grace_hours_from_env);
            let sweep_outcome = gc::sweep(
                store_ref,
                &self.prefix,
                gc::SweepOpts {
                    grace_hours,
                    force: self.opts.force,
                },
            )
            .await?;
            format_sweep_outcome(out, &sweep_outcome)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::RefName;
    use crate::object_store::mock::MockStore;
    use crate::packchain::manifest::write_chain;
    use crate::packchain::schema::{ChainManifest, ChainSegment, Sha40};
    use bytes::Bytes;

    const SHA_TIP: &str = "0000000000000000000000000000000000000001";
    const SHA_PACK_LIVE: &str = "1111111111111111111111111111111111111111";
    const SHA_PACK_ORPHAN: &str = "2222222222222222222222222222222222222222";

    fn sha40(s: &str) -> Sha40 {
        Sha40::try_new(s).unwrap()
    }

    fn ref_main() -> RefName {
        RefName::new("refs/heads/main").unwrap()
    }

    async fn seed_state(store: &MockStore, prefix: Option<&str>) {
        let chain = ChainManifest {
            v: 1,
            tip: sha40(SHA_TIP),
            full_at: sha40(SHA_TIP),
            segments: vec![ChainSegment {
                sha: sha40(SHA_TIP),
                parent_sha: None,
                pack: format!("packs/{SHA_PACK_LIVE}.pack"),
                bytes: 1_024,
            }],
        };
        write_chain(store, prefix, &ref_main(), &chain)
            .await
            .unwrap();
        let live_pack = crate::packchain::keys::pack_key(prefix, &sha40(SHA_PACK_LIVE));
        let live_idx = crate::packchain::keys::pack_idx_key(prefix, &sha40(SHA_PACK_LIVE));
        store.insert(live_pack, Bytes::from_static(b"PACK"));
        store.insert(live_idx, Bytes::from_static(b"IDX"));
        let orphan_pack = crate::packchain::keys::pack_key(prefix, &sha40(SHA_PACK_ORPHAN));
        let orphan_idx = crate::packchain::keys::pack_idx_key(prefix, &sha40(SHA_PACK_ORPHAN));
        store.insert(orphan_pack, Bytes::from_static(b"PACK"));
        store.insert(orphan_idx, Bytes::from_static(b"IDX"));
    }

    #[tokio::test]
    async fn run_mark_only_writes_tombstone_without_sweep() {
        let store = Arc::new(MockStore::new());
        seed_state(&store, Some("repo")).await;
        let gc = Gc::new(
            Arc::clone(&store) as Arc<dyn ObjectStore>,
            "repo",
            GcOpts {
                mode: GcMode::MarkOnly,
                ..GcOpts::default()
            },
        );
        gc.run().await.unwrap();
        // Tombstone exists; orphan pack still on bucket (sweep never ran).
        let metas = store.list("repo/gc/").await.unwrap();
        assert_eq!(metas.len(), 1, "exactly one tombstone after mark-only");
        store
            .get_bytes(&format!("repo/packs/{SHA_PACK_ORPHAN}.pack"))
            .await
            .expect("orphan pack must survive mark-only");
    }

    #[tokio::test]
    async fn run_sweep_only_with_force_deletes_orphans() {
        let store = Arc::new(MockStore::new());
        seed_state(&store, Some("repo")).await;
        // Mark first to produce a tombstone.
        gc::mark(store.as_ref(), "repo", gc::MarkOpts::default())
            .await
            .unwrap();
        // Sweep with force: no grace, no recheck.
        let gc = Gc::new(
            Arc::clone(&store) as Arc<dyn ObjectStore>,
            "repo",
            GcOpts {
                mode: GcMode::SweepOnly,
                force: true,
                ..GcOpts::default()
            },
        );
        gc.run().await.unwrap();
        // Orphan pack gone; live pack survives.
        let err = store
            .get_bytes(&format!("repo/packs/{SHA_PACK_ORPHAN}.pack"))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            crate::object_store::ObjectStoreError::NotFound(_)
        ));
        store
            .get_bytes(&format!("repo/packs/{SHA_PACK_LIVE}.pack"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_mark_then_sweep_force_round_trips() {
        let store = Arc::new(MockStore::new());
        seed_state(&store, Some("repo")).await;
        let gc = Gc::new(
            Arc::clone(&store) as Arc<dyn ObjectStore>,
            "repo",
            GcOpts {
                force: true,
                ..GcOpts::default()
            },
        );
        gc.run().await.unwrap();
        // Force sweep ran in the same call → orphan deleted.
        let err = store
            .get_bytes(&format!("repo/packs/{SHA_PACK_ORPHAN}.pack"))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            crate::object_store::ObjectStoreError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn run_with_no_orphans_is_noop() {
        let store = Arc::new(MockStore::new());
        // Live chain referencing the only pack on bucket.
        let chain = ChainManifest {
            v: 1,
            tip: sha40(SHA_TIP),
            full_at: sha40(SHA_TIP),
            segments: vec![ChainSegment {
                sha: sha40(SHA_TIP),
                parent_sha: None,
                pack: format!("packs/{SHA_PACK_LIVE}.pack"),
                bytes: 1_024,
            }],
        };
        write_chain(store.as_ref(), Some("repo"), &ref_main(), &chain)
            .await
            .unwrap();
        let live_pack = crate::packchain::keys::pack_key(Some("repo"), &sha40(SHA_PACK_LIVE));
        store.insert(live_pack, Bytes::from_static(b"PACK"));
        let gc = Gc::new(
            Arc::clone(&store) as Arc<dyn ObjectStore>,
            "repo",
            GcOpts::default(),
        );
        gc.run().await.unwrap();
        let metas = store.list("repo/gc/").await.unwrap();
        assert!(metas.is_empty(), "no tombstone when no orphans");
    }
}
