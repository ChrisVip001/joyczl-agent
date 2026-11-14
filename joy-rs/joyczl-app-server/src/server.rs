//! Server responsibilities, one file each: the `impl Server` blocks split by what they do.
//!
//! They live in a directory because they are one type's implementation, not five modules:
//! reading `lib.rs` should show a server, not a list of its parts.

pub(crate) mod controls;
pub(crate) mod events;
pub(crate) mod runtime;
pub(crate) mod settings;
pub(crate) mod tools;
pub use runtime::open;
