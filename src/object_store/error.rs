//! Error type shared by all `ObjectStore` backends.
//!
//! Centralises the mapping of backend-specific failure codes (S3 412/409,
//! Azure 412, etc.) onto a small set of variants. Implementation lands in
//! Phase 4.
