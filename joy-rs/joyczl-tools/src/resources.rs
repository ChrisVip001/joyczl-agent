//! Host-injected coordination of mutating operations. The host owns the policy.
use std::{future::Future, pin::Pin, sync::Arc};

/// A shareable permit. Background work must retain it until effects and settlement end.
pub trait ResourceLease: Send + Sync {}
pub type Lease = Arc<dyn ResourceLease>;
pub type Acquire = Pin<Box<dyn Future<Output = Result<Lease, String>> + Send>>;
pub trait ResourceCoordinator: Send + Sync {
    fn acquire(&self, operation: &str) -> Acquire;
}
