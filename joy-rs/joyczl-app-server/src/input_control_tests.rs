use super::input_wake_tests::{fixture, rpc, wait_status};
use crate::EventSink;
use serde_json::json;

#[tokio::test]
async fn an_expiry_observation_cannot_replace_a_later_human_pause() {
    let (_home, server, _model) = fixture().await;
    rpc(
        &server,
        "task/start",
        json!({"taskId":"input-task","message":"Prepare the report"}),
    )
    .await
    .unwrap();
    wait_status(&server, joyczl_state::TaskStatus::Suspended).await;
    let wait = joyczl_state::external_waits::ExternalWaits::new(server.pool.clone())
        .waiting("input-task")
        .await
        .unwrap()
        .unwrap();
    rpc(
        &server,
        "task/suspend",
        json!({"taskId":"input-task","reason":joyczl_state::external_waits::INPUT_WAIT_REASON}),
    )
    .await
    .unwrap();
    let (sink, _receiver) = EventSink::channel();
    crate::input_wake::review_expired(&server, &sink, &wait).await;
    assert_eq!(
        server
            .tasks
            .get("input-task")
            .await
            .unwrap()
            .unwrap()
            .status,
        joyczl_state::TaskStatus::Suspended
    );
}

#[tokio::test]
async fn failed_human_pause_does_not_orphan_an_input_wait() {
    let (_home, server, _model) = fixture().await;
    rpc(
        &server,
        "task/start",
        json!({"taskId":"input-task","message":"Prepare the report"}),
    )
    .await
    .unwrap();
    wait_status(&server, joyczl_state::TaskStatus::Suspended).await;
    let rows = joyczl_state::external_waits::ExternalWaits::new(server.pool.clone());
    let wait = rows.waiting("input-task").await.unwrap().unwrap();
    sqlx::query("CREATE TRIGGER fail_input_pause BEFORE INSERT ON task_pause_fences BEGIN SELECT RAISE(FAIL,'injected input pause failure'); END").execute(&server.pool).await.unwrap();
    assert!(rpc(&server, "task/suspend", json!({"taskId":"input-task"}))
        .await
        .is_err());
    assert_eq!(
        rows.waiting("input-task").await.unwrap().unwrap().state,
        "waiting"
    );
    assert_eq!(
        server
            .checkpoints
            .get(&wait.turn_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "waiting_input"
    );
    assert!(
        !joyczl_state::human_pauses::HumanPauses::new(server.pool.clone())
            .is_paused("input-task")
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn failed_verification_persistence_cannot_advertise_task_completion() {
    let (home, server, _model) = fixture().await;
    std::fs::write(home.path().join("approved.txt"), "Approved figures: 42").unwrap();
    sqlx::query("DELETE FROM task_input_conditions WHERE task_id='input-task'")
        .execute(&server.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE agent_tasks SET cwd=?,acceptance_json=? WHERE id='input-task'")
        .bind(home.path().display().to_string())
        .bind(json!([{"kind":"fileExists","path":"approved.txt"}]).to_string())
        .execute(&server.pool)
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER fail_verification BEFORE UPDATE OF checks_json ON agent_tasks BEGIN SELECT RAISE(FAIL,'injected verification persistence failure'); END").execute(&server.pool).await.unwrap();
    rpc(
        &server,
        "task/start",
        json!({"taskId":"input-task","message":"Verify the approved file"}),
    )
    .await
    .unwrap();
    let status = tokio::time::timeout(std::time::Duration::from_secs(8), async {
        loop {
            let status = server
                .tasks
                .get("input-task")
                .await
                .unwrap()
                .unwrap()
                .status;
            if matches!(
                status,
                joyczl_state::TaskStatus::Done
                    | joyczl_state::TaskStatus::NeedsAttention
                    | joyczl_state::TaskStatus::Failed
            ) {
                break status;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(status, joyczl_state::TaskStatus::NeedsAttention);
}
