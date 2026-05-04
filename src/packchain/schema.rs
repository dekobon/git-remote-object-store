//! On-bucket JSON schemas for the packchain engine.
//!
//! Two files live per branch:
//!
//! - **`chain.json`** — newest-first ordered list of pack segments,
//!   plus the tip and the last full-snapshot SHA (`full_at`). Read on
//!   every fetch and rewritten on every push.
//! - **`path-index.json`** — nested-tree map from repo paths to blob
//!   SHAs at the current tip. Used by Phase 4's `read_blob` API for
//!   single-file access without running git. Nested rather than flat
//!   per the user's Phase 1 design decision (issue #52, Open Q3).
//!
//! Both files carry an explicit schema version (`v: 1`). A future v=2
//! reader refuses older clients via [`PackchainError::UnsupportedSchemaVersion`];
//! the versions are independent so `chain.json` and `path-index.json`
//! can evolve separately.
//!
//! [`PackchainError::UnsupportedSchemaVersion`]: super::PackchainError::UnsupportedSchemaVersion

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::PackchainError;

/// 40-character lowercase-hex SHA-1, validated on every deserialise.
///
/// `#[serde(try_from)]` runs [`Sha40::try_new`] before the value lands
/// in the schema struct, so a malformed sha in `chain.json` /
/// `path-index.json` surfaces as [`PackchainError::InvalidSha`] at
/// parse time rather than leaking into engine logic. Distinct from
/// [`crate::git::Sha`] (which wraps `gix_hash::ObjectId`) because the
/// schema layer is intentionally serde-aware while `git::Sha` is not;
/// converting between them is a one-line `Sha::from_hex(sha40.as_str())`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct Sha40(String);

impl Sha40 {
    /// Validate `s` and wrap it.
    ///
    /// # Errors
    ///
    /// Returns [`PackchainError::InvalidSha`] when `s` is not exactly
    /// 40 ASCII lowercase-hex characters.
    pub(crate) fn try_new(s: impl Into<String>) -> Result<Self, PackchainError> {
        let s = s.into();
        if s.len() != 40 || !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(PackchainError::InvalidSha { found: s });
        }
        Ok(Self(s))
    }

    /// Borrow as a plain `&str` (always 40 lowercase hex characters).
    #[must_use]
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Sha40 {
    type Error = PackchainError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<Sha40> for String {
    fn from(value: Sha40) -> Self {
        value.0
    }
}

/// `chain.json` — newest-first ordered chain manifest for one branch.
///
/// Schema version (`v`) is currently `1`. The wire format pretty-prints
/// for readability when an operator inspects the bucket; the size cost
/// is negligible at the volumes these files reach.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ChainManifest {
    /// Schema version. Always [`ChainManifest::SCHEMA_VERSION`] when
    /// written; rejected during [`from_json_bytes`] for any other value.
    pub(crate) v: u32,
    /// SHA-1 of the current tip commit on the branch.
    pub(crate) tip: Sha40,
    /// SHA-1 of the tip at the time of the last full-snapshot bundle.
    /// Always set after the first push; never null.
    pub(crate) full_at: Sha40,
    /// Pack segments newest-first. Always non-empty after a successful
    /// push: even the first push writes a single segment (`parent_sha
    /// = None`) alongside the baseline bundle so the chain has a pack
    /// to install during Phase 3 fetch. An empty Vec is still a valid
    /// round-trip shape (Phase 5 GC may produce one transiently during
    /// compaction) but no Phase 2 push writes one.
    pub(crate) segments: Vec<ChainSegment>,
}

impl ChainManifest {
    /// On-bucket schema version this build reads and writes.
    pub(crate) const SCHEMA_VERSION: u32 = 1;

    /// Parse `bytes` as `chain.json`, validating the schema version
    /// before returning. Malformed JSON, missing fields, an invalid
    /// [`Sha40`], or a wrong `v` all surface as [`PackchainError`].
    ///
    /// # Errors
    ///
    /// - [`PackchainError::ParseJson`] for malformed JSON / missing
    ///   fields / type mismatches.
    /// - [`PackchainError::InvalidSha`] when any sha40 field fails
    ///   validation.
    /// - [`PackchainError::UnsupportedSchemaVersion`] when `v` is not
    ///   [`Self::SCHEMA_VERSION`].
    pub(crate) fn from_json_bytes(bytes: &[u8]) -> Result<Self, PackchainError> {
        let parsed: Self = serde_json::from_slice(bytes)?;
        if parsed.v != Self::SCHEMA_VERSION {
            return Err(PackchainError::UnsupportedSchemaVersion {
                found: parsed.v,
                expected: Self::SCHEMA_VERSION,
            });
        }
        Ok(parsed)
    }

    /// Render to pretty-printed JSON bytes suitable for `put_bytes`.
    ///
    /// # Errors
    ///
    /// `serde_json::to_vec_pretty` is infallible for this schema (no
    /// custom serialisers can fail), but the function returns
    /// `Result` for forward compatibility with future fields.
    pub(crate) fn to_json_pretty(&self) -> Result<Vec<u8>, PackchainError> {
        Ok(serde_json::to_vec_pretty(self)?)
    }
}

/// One entry in [`ChainManifest::segments`]. Each segment corresponds
/// to a single pack file uploaded by one push.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ChainSegment {
    /// Tip SHA at the time this segment was pushed.
    pub(crate) sha: Sha40,
    /// Parent's tip SHA, or `None` for the first push of a branch.
    pub(crate) parent_sha: Option<Sha40>,
    /// Bucket-relative key of the pack file
    /// (`packs/<content-sha>.pack`). Stored as a String rather than a
    /// typed key to keep the schema oblivious to prefix concerns —
    /// the key builder centralises that.
    pub(crate) pack: String,
    /// Pack file size in bytes. Used by compaction's "rewrite when
    /// segments-since-full > N" heuristic without an extra HEAD call.
    pub(crate) bytes: u64,
}

/// `path-index.json` — nested-tree map from repo paths to blob SHAs at
/// the current tip commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PathIndex {
    /// Schema version. Always [`PathIndex::SCHEMA_VERSION`] when
    /// written.
    pub(crate) v: u32,
    /// SHA-1 of the commit this index reflects.
    pub(crate) commit: Sha40,
    /// Top-level entries at the repo root.
    pub(crate) tree: BTreeMap<String, PathNode>,
}

impl PathIndex {
    /// On-bucket schema version this build reads and writes.
    pub(crate) const SCHEMA_VERSION: u32 = 1;

    /// Parse `bytes` as `path-index.json`, validating the schema
    /// version before returning. See [`ChainManifest::from_json_bytes`]
    /// for the error contract.
    ///
    /// Phase 2 push writes `path-index.json` but never reads it back —
    /// Phase 3 fetch / Phase 4 direct file access will. The reader
    /// landed in Phase 1 alongside the writer so the wire format is
    /// pinned by tests; until Phase 3, the function is exercised by
    /// the schema's own round-trip tests.
    ///
    /// # Errors
    ///
    /// See [`ChainManifest::from_json_bytes`].
    #[allow(dead_code)]
    pub(crate) fn from_json_bytes(bytes: &[u8]) -> Result<Self, PackchainError> {
        let parsed: Self = serde_json::from_slice(bytes)?;
        if parsed.v != Self::SCHEMA_VERSION {
            return Err(PackchainError::UnsupportedSchemaVersion {
                found: parsed.v,
                expected: Self::SCHEMA_VERSION,
            });
        }
        Ok(parsed)
    }

    /// Render to pretty-printed JSON bytes.
    ///
    /// # Errors
    ///
    /// See [`ChainManifest::to_json_pretty`].
    pub(crate) fn to_json_pretty(&self) -> Result<Vec<u8>, PackchainError> {
        Ok(serde_json::to_vec_pretty(self)?)
    }
}

/// Either a blob (leaf, value is a sha) or a subtree (interior, value
/// is a map of child names → nodes).
///
/// `#[serde(untagged)]` discriminates by JSON shape: a string value
/// matches [`Self::Blob`] and validates as [`Sha40`]; an object value
/// matches [`Self::Tree`]. The blob shape is tried first because a
/// JSON string can never be misread as a tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum PathNode {
    /// Leaf — a 40-hex blob SHA.
    Blob(Sha40),
    /// Interior — a non-empty (or possibly empty) subtree.
    Tree(BTreeMap<String, PathNode>),
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA_A: &str = "0123456789abcdef0123456789abcdef01234567";
    const SHA_B: &str = "fedcba9876543210fedcba9876543210fedcba98";
    const SHA_C: &str = "1111111111111111111111111111111111111111";

    fn sha40(s: &str) -> Sha40 {
        Sha40::try_new(s).expect("valid 40-hex sha in test fixture")
    }

    // --- Sha40 ----------------------------------------------------------

    #[test]
    fn sha40_accepts_40_lowercase_hex() {
        let s = Sha40::try_new(SHA_A).unwrap();
        assert_eq!(s.as_str(), SHA_A);
    }

    #[test]
    fn sha40_rejects_uppercase() {
        // 40-char SHA but uppercase A — must reject. Distinct from
        // `git::Sha::from_hex` which canonicalises to lowercase.
        // The on-bucket invariant is "always lowercase"; fail loud
        // rather than silently rewrite.
        let err = Sha40::try_new("0123456789ABCDEF0123456789abcdef01234567").unwrap_err();
        assert!(matches!(err, PackchainError::InvalidSha { .. }));
    }

    #[test]
    fn sha40_rejects_wrong_length() {
        for len in [0_usize, 1, 39, 41, 80] {
            let candidate = "0".repeat(len);
            let err = Sha40::try_new(&candidate).expect_err(&format!("len {len} must reject"));
            assert!(matches!(err, PackchainError::InvalidSha { .. }));
        }
    }

    #[test]
    fn sha40_rejects_non_hex_characters() {
        // 40 chars, last is `g` (non-hex).
        let err = Sha40::try_new("0123456789abcdef0123456789abcdef0123456g").unwrap_err();
        assert!(matches!(err, PackchainError::InvalidSha { .. }));
    }

    #[test]
    fn sha40_serializes_as_plain_json_string() {
        let s = sha40(SHA_A);
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, format!("\"{SHA_A}\""));
    }

    #[test]
    fn sha40_deserialize_validates() {
        // Valid passes through.
        let s: Sha40 = serde_json::from_str(&format!("\"{SHA_A}\"")).unwrap();
        assert_eq!(s.as_str(), SHA_A);
        // Invalid 40-char string surfaces via serde as a parse error
        // carrying the InvalidSha display message.
        let err = serde_json::from_str::<Sha40>("\"not-a-sha\"").unwrap_err();
        assert!(
            err.to_string().contains("invalid 40-hex sha"),
            "expected InvalidSha display in {err}",
        );
    }

    // --- ChainManifest --------------------------------------------------

    fn fixture_chain() -> ChainManifest {
        ChainManifest {
            v: ChainManifest::SCHEMA_VERSION,
            tip: sha40(SHA_A),
            full_at: sha40(SHA_B),
            segments: vec![ChainSegment {
                sha: sha40(SHA_A),
                parent_sha: Some(sha40(SHA_B)),
                pack: format!("packs/{SHA_C}.pack"),
                bytes: 4_096,
            }],
        }
    }

    #[test]
    fn chain_manifest_round_trips_via_json() {
        let chain = fixture_chain();
        let bytes = chain.to_json_pretty().unwrap();
        let decoded = ChainManifest::from_json_bytes(&bytes).unwrap();
        assert_eq!(decoded, chain);
    }

    #[test]
    fn chain_manifest_handles_empty_segments() {
        // No Phase 2 push writes an empty `segments` Vec, but Phase 5
        // GC may produce one transiently during compaction; the wire
        // format must still round-trip cleanly.
        let chain = ChainManifest {
            v: ChainManifest::SCHEMA_VERSION,
            tip: sha40(SHA_A),
            full_at: sha40(SHA_A),
            segments: Vec::new(),
        };
        let bytes = chain.to_json_pretty().unwrap();
        let decoded = ChainManifest::from_json_bytes(&bytes).unwrap();
        assert_eq!(decoded.segments.len(), 0);
    }

    #[test]
    fn chain_manifest_segment_with_null_parent() {
        // First-push segment has parent_sha = None. The wire-format
        // contract: the field is *present* and *null* (not omitted) so
        // an operator inspecting `chain.json` sees the explicit
        // first-push marker. Verify by parsing the output and asserting
        // the field's JSON shape — decouples from serde_json's exact
        // pretty-print spacing (which is not part of the contract).
        let chain = ChainManifest {
            v: ChainManifest::SCHEMA_VERSION,
            tip: sha40(SHA_A),
            full_at: sha40(SHA_A),
            segments: vec![ChainSegment {
                sha: sha40(SHA_A),
                parent_sha: None,
                pack: format!("packs/{SHA_C}.pack"),
                bytes: 1_024,
            }],
        };
        let bytes = chain.to_json_pretty().unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let parent_field = &parsed["segments"][0]["parent_sha"];
        assert!(
            parent_field.is_null(),
            "parent_sha must be present as JSON null (not omitted), got {parent_field}",
        );
        let decoded = ChainManifest::from_json_bytes(&bytes).unwrap();
        assert_eq!(decoded.segments[0].parent_sha, None);
    }

    #[test]
    fn chain_manifest_rejects_unsupported_version() {
        let mut chain = fixture_chain();
        chain.v = 2;
        let bytes = chain.to_json_pretty().unwrap();
        let err = ChainManifest::from_json_bytes(&bytes).unwrap_err();
        assert!(
            matches!(
                err,
                PackchainError::UnsupportedSchemaVersion {
                    found: 2,
                    expected: 1,
                },
            ),
            "expected UnsupportedSchemaVersion(2, 1), got {err:?}",
        );
    }

    #[test]
    fn chain_manifest_rejects_v_zero() {
        let mut chain = fixture_chain();
        chain.v = 0;
        let bytes = chain.to_json_pretty().unwrap();
        let err = ChainManifest::from_json_bytes(&bytes).unwrap_err();
        assert!(matches!(
            err,
            PackchainError::UnsupportedSchemaVersion { found: 0, .. },
        ));
    }

    #[test]
    fn chain_manifest_rejects_invalid_sha_in_tip() {
        // Hand-craft JSON with a malformed tip; deserialise must fail
        // at the Sha40 validator, not at the version check.
        let json = format!(r#"{{"v":1,"tip":"not-a-sha","full_at":"{SHA_B}","segments":[]}}"#);
        let err = ChainManifest::from_json_bytes(json.as_bytes()).unwrap_err();
        assert!(matches!(err, PackchainError::ParseJson(_)));
        assert!(
            err.to_string().contains("invalid 40-hex sha"),
            "expected InvalidSha display in {err}",
        );
    }

    // --- PathIndex / PathNode ------------------------------------------

    fn fixture_path_index() -> PathIndex {
        let src_subtree = BTreeMap::from([
            ("main.rs".to_string(), PathNode::Blob(sha40(SHA_A))),
            ("lib.rs".to_string(), PathNode::Blob(sha40(SHA_B))),
        ]);
        PathIndex {
            v: PathIndex::SCHEMA_VERSION,
            commit: sha40(SHA_A),
            tree: BTreeMap::from([
                ("Cargo.toml".to_string(), PathNode::Blob(sha40(SHA_C))),
                ("src".to_string(), PathNode::Tree(src_subtree)),
            ]),
        }
    }

    #[test]
    fn path_index_round_trips_via_json() {
        let index = fixture_path_index();
        let bytes = index.to_json_pretty().unwrap();
        let decoded = PathIndex::from_json_bytes(&bytes).unwrap();
        assert_eq!(decoded, index);
    }

    #[test]
    fn path_node_blob_serializes_as_string_value() {
        let bytes = serde_json::to_vec(&PathNode::Blob(sha40(SHA_A))).unwrap();
        assert_eq!(bytes, format!("\"{SHA_A}\"").into_bytes());
    }

    #[test]
    fn path_node_tree_serializes_as_object() {
        let children = BTreeMap::from([("a".to_string(), PathNode::Blob(sha40(SHA_A)))]);
        let bytes = serde_json::to_vec(&PathNode::Tree(children)).unwrap();
        // BTreeMap iterates sorted, so `a` is the only key and the
        // outer shape is a JSON object.
        assert_eq!(bytes, format!("{{\"a\":\"{SHA_A}\"}}").into_bytes());
    }

    #[test]
    fn path_node_untagged_round_trips_nested_shape() {
        // Hand-crafted JSON exercising the discriminator: leaves are
        // strings, subtrees are objects. Phase 4 (read_blob) walks
        // exactly this shape.
        let json = format!(
            r#"{{"v":1,"commit":"{SHA_A}","tree":{{"src":{{"main.rs":"{SHA_B}","mod":{{"inner.rs":"{SHA_C}"}}}}}}}}"#,
        );
        let decoded = PathIndex::from_json_bytes(json.as_bytes()).unwrap();
        // Walk: root.src must be a Tree containing main.rs (Blob) and
        // mod (Tree).
        let src = decoded.tree.get("src").expect("src present");
        let PathNode::Tree(src_children) = src else {
            panic!("expected src to be a Tree, got {src:?}");
        };
        assert!(matches!(
            src_children.get("main.rs"),
            Some(PathNode::Blob(_))
        ));
        assert!(matches!(src_children.get("mod"), Some(PathNode::Tree(_))));
    }

    #[test]
    fn path_index_rejects_unsupported_version() {
        let mut index = fixture_path_index();
        index.v = 2;
        let bytes = index.to_json_pretty().unwrap();
        let err = PathIndex::from_json_bytes(&bytes).unwrap_err();
        assert!(matches!(
            err,
            PackchainError::UnsupportedSchemaVersion {
                found: 2,
                expected: 1
            },
        ));
    }

    #[test]
    fn path_index_rejects_invalid_blob_sha() {
        let json = format!(r#"{{"v":1,"commit":"{SHA_A}","tree":{{"a":"not-a-sha"}}}}"#);
        let err = PathIndex::from_json_bytes(json.as_bytes()).unwrap_err();
        // Note: with `serde(untagged)`, an untagged enum that fails
        // every variant produces a generic "untagged enum" parse
        // error; the inner Sha40 validation message is folded into
        // the chain. Assert on the variant rather than the exact
        // message so the test does not couple to serde's wording.
        assert!(matches!(err, PackchainError::ParseJson(_)));
    }
}
