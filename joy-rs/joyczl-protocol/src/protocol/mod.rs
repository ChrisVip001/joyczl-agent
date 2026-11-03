//! Protocol versions.
//!
//! v1 is the migration-era compatibility layer (read-only, no new methods); all new APIs go into v2.

pub mod v2;

#[cfg(test)]
#[path = "v2_tests.rs"]
mod v2_tests;
