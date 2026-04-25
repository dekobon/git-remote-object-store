//! Backend-neutral object-store trait shared by the S3 and Azure Blob
//! implementations.
//!
//! See §2.1 of `execution-plan.md` for the trait sketch. Implementation
//! lands in Phase 4 (trait + mock) and Phases 5 / 11 (S3 / Azure backends).

pub mod azure;
pub mod error;
pub mod s3;
