//! Conservative single-writer admission shared by all sessions in this runtime.
use joyczl_tools::resources::{Acquire, ResourceCoordinator, ResourceLease};
use std::sync::Arc;

#[derive(Default)]
pub(crate) struct Mutations {
    gate: Arc<tokio::sync::Mutex<()>>,
}
struct Permit {
    _guard: tokio::sync::OwnedMutexGuard<()>,
}
impl ResourceLease for Permit {}
impl ResourceCoordinator for Mutations {
    fn acquire(&self, operation: &str) -> Acquire {
        let gate = self.gate.clone();
        let operation = operation.to_string();
        Box::pin(async move {
            if let Ok(guard) = gate.clone().try_lock_owned() {
                return Ok(Arc::new(Permit { _guard: guard }) as joyczl_tools::resources::Lease);
            }
            eprintln!("(joy) {operation} waiting for another mutation or background job to settle");
            match tokio::time::timeout(std::time::Duration::from_secs(2), gate.lock_owned()).await {
                Ok(guard) => Ok(Arc::new(Permit { _guard: guard }) as joyczl_tools::resources::Lease),
                Err(_) => Err("Another mutation or background job still owns the execution resources; inspect its progress before retrying".into()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn cloned_permits_retain_exclusion_and_cancellation_releases_it() {
        let coordinator = Mutations::default();
        let lease = coordinator.acquire("first").await.unwrap();
        let held = lease.clone();
        drop(lease);
        assert!(coordinator.gate.clone().try_lock_owned().is_err());
        drop(held);
        assert!(coordinator.gate.clone().try_lock_owned().is_ok());
        let lease = coordinator.acquire("holder").await.unwrap();
        let task = tokio::spawn(coordinator.acquire("cancelled"));
        tokio::task::yield_now().await;
        task.abort();
        let _ = task.await;
        drop(lease);
        assert!(coordinator.acquire("next").await.is_ok());
    }

    #[tokio::test]
    async fn background_jobs_block_other_session_writes_until_settled() {
        if !joyczl_tools::exec::sandbox_available() {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let pool = joyczl_state::open(&home.path().join("state.db"))
            .await
            .unwrap();
        let server = crate::Server::boot(
            pool,
            joyczl_config::Settings {
                home: home.path().into(),
                provider: "test-unconfigured".into(),
                exec_enabled: true,
                exec_allow: vec!["sleep*".into()],
                write_enabled: true,
                approval: "never".into(),
                ..Default::default()
            },
        )
        .await;
        let first = server.tool_ctx("task", Some(workspace.path().into()), None);
        let result = server
            .tools
            .execute(
                first,
                "run_command",
                serde_json::json!({"command":"sleep 10","background":true}),
            )
            .await;
        assert!(result.starts_with("Running in the background:"), "{result}");
        let other = server.tool_ctx("chat", Some(workspace.path().into()), None);
        let args = serde_json::json!({"path":"result.txt","content":"second session"});
        let result = server
            .tools
            .execute(other.clone(), "write_file", args.clone())
            .await;
        assert!(
            result.contains("still owns the execution resources"),
            "{result}"
        );
        assert!(!workspace.path().join("result.txt").exists());
        let jobs = server.jobs.as_ref().unwrap();
        let id = jobs.list("task")[0].id.clone();
        jobs.kill("task", &id).await.unwrap();
        let done = jobs.output("task", &id, 0, true).await.unwrap();
        assert!(!done.running);
        let result = server.tools.execute(other, "write_file", args).await;
        assert!(!result.starts_with("Error:"), "{result}");
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("result.txt")).unwrap(),
            "second session"
        );
    }
}
