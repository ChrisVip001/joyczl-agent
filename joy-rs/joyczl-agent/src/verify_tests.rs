//! Acceptance-check tests: one question each, and the answers must be measurements.
//!
//! The two that matter most are the last two: a path outside the workspace is refused, and no
//! workspace means no path check at all — never "fall back to home and hope".

use joyczl_state::{NewStep, Steps};

use crate::verify::{all_passed, parse, Acceptance, Verifier};

/// A workspace on disk + a step ledger in its own state.db.
async fn fixture() -> (tempfile::TempDir, Steps, String) {
    let dir = tempfile::tempdir().expect("临时目录");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).expect("建 home");
    let pool = joyczl_state::open(&home.join("state.db"))
        .await
        .expect("打开库");
    (dir, Steps::new(pool), "task1".to_string())
}

fn verifier(workspace: &std::path::Path, steps: &Steps, task_id: &str) -> Verifier {
    Verifier {
        run_id: "run1".into(),
        root: Some(workspace.to_path_buf()),
        steps: steps.clone(),
        task_id: task_id.to_string(),
    }
}

#[tokio::test]
async fn an_existing_file_passes_and_reports_its_size() {
    let (dir, steps, task) = fixture().await;
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).expect("建工作目录");
    std::fs::write(workspace.join("report.md"), "结论：可以上线\n").expect("写文件");

    let verifier = verifier(&workspace, &steps, &task);
    let results = verifier
        .run(&[Acceptance::FileExists {
            path: "report.md".to_string(),
        }])
        .await;
    assert_eq!(results.len(), 1);
    assert!(results[0].passed, "文件在，该通过：{:?}", results[0].detail);
    assert!(
        results[0].detail.contains("report.md") && results[0].detail.contains('B'),
        "通过的证据要带文件与大小：{}",
        results[0].detail
    );
}

#[tokio::test]
async fn a_missing_file_fails_with_the_measurement_not_a_guess() {
    let (dir, steps, task) = fixture().await;
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).expect("建工作目录");

    let verifier = verifier(&workspace, &steps, &task);
    let results = verifier
        .run(&[Acceptance::FileExists {
            path: "report.md".to_string(),
        }])
        .await;
    assert!(!results[0].passed);
    assert!(
        results[0].detail.contains("does not exist"),
        "失败要说实测：{}",
        results[0].detail
    );
    assert!(!all_passed(&results));
}

#[tokio::test]
async fn file_contains_counts_a_plain_substring() {
    let (dir, steps, task) = fixture().await;
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).expect("建工作目录");
    std::fs::write(workspace.join("report.md"), "结论: A\n结论: B\n结论: C\n").expect("写文件");

    let verifier = verifier(&workspace, &steps, &task);
    let passed = verifier
        .run(&[Acceptance::FileContains {
            path: "report.md".to_string(),
            pattern: "结论:".to_string(),
            min: 3,
        }])
        .await;
    assert!(passed[0].passed, "3 处该够：{}", passed[0].detail);

    let short = verifier
        .run(&[Acceptance::FileContains {
            path: "report.md".to_string(),
            pattern: "结论:".to_string(),
            min: 4,
        }])
        .await;
    assert!(!short[0].passed);
    assert!(
        short[0].detail.contains("only 3 ×"),
        "要说清实测几处：{}",
        short[0].detail
    );
}

#[tokio::test]
async fn tools_ok_counts_successful_steps_only() {
    let (dir, steps, task) = fixture().await;
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).expect("建工作目录");

    for (seq, status) in [(1, "ok"), (2, "ok"), (3, "failed")] {
        let key = format!("turn1:{seq}");
        let id = steps
            .begin(NewStep {
                task_id: &task,
                run_id: Some("run1"),
                seq,
                kind: "tool",
                tool: Some("write_file"),
                input_json: None,
                idempotency_key: Some(&key),
            })
            .await
            .expect("记一步");
        steps.close(id, status, None, None).await.expect("收一步");
    }

    let verifier = verifier(&workspace, &steps, &task);
    let enough = verifier
        .run(&[Acceptance::ToolsOk {
            tool: "write_file".to_string(),
            min: 2,
        }])
        .await;
    assert!(enough[0].passed, "成功 2 次该够：{}", enough[0].detail);

    let too_many = verifier
        .run(&[Acceptance::ToolsOk {
            tool: "write_file".to_string(),
            min: 3,
        }])
        .await;
    assert!(
        !too_many[0].passed,
        "只认成功的：失败的第三次不算 —— {}",
        too_many[0].detail
    );
}

#[tokio::test]
async fn a_path_outside_the_workspace_is_refused() {
    let (dir, steps, task) = fixture().await;
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).expect("建工作目录");
    // A real file outside, so the refusal cannot be explained by "it just is not there".
    std::fs::write(dir.path().join("outside.txt"), "机密").expect("写外面的文件");

    let verifier = verifier(&workspace, &steps, &task);
    let results = verifier
        .run(&[Acceptance::FileExists {
            path: "../outside.txt".to_string(),
        }])
        .await;
    assert!(!results[0].passed, "逃逸必须被拒");
    assert!(
        results[0].detail.contains("outside the working directory"),
        "拒绝要说清在范围之外：{}",
        results[0].detail
    );
}

#[tokio::test]
async fn no_workspace_refuses_every_path_check() {
    let (_dir, steps, task) = fixture().await;
    let verifier = Verifier {
        run_id: "run1".into(),
        root: None,
        steps,
        task_id: task,
    };
    let results = verifier
        .run(&[Acceptance::FileExists {
            // Deliberately a path that exists on every machine: a fallback to home would pass.
            path: "state.db".to_string(),
        }])
        .await;
    assert!(!results[0].passed, "没有工作目录就不做路径检查");
    assert!(
        results[0].detail.contains("working directory"),
        "要说清原因：{}",
        results[0].detail
    );
}

#[test]
fn a_malformed_condition_is_dropped_with_its_reason() {
    // An unknown kind must never silently count as passed.
    let parsed = parse(r#"[{"kind":"fileExist","path":"a.md"}]"#);
    assert!(parsed.is_empty(), "拼错的类型不该被当成条件");

    let parsed = parse(r#"[{"kind":"fileContains","path":"a.md"}]"#);
    assert!(parsed.is_empty(), "缺 pattern 的条件不该被收");

    let parsed = parse(r#"[{"kind":"toolsOk","tool":"write_file","min":2}]"#);
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].label(), "write_file succeeds at least 2 times");
}

#[test]
fn labels_read_as_conditions() {
    assert_eq!(
        Acceptance::FileExists {
            path: "report.md".to_string()
        }
        .label(),
        "report.md exists"
    );
    assert_eq!(
        Acceptance::FileContains {
            path: "report.md".to_string(),
            pattern: "结论".to_string(),
            min: 3
        }
        .label(),
        "report.md contains at least 3 × “结论”"
    );
}

#[tokio::test]
async fn successes_from_an_older_run_cannot_satisfy_current_acceptance() {
    let (dir, steps, task) = fixture().await;
    let old = steps
        .begin(NewStep {
            task_id: &task,
            run_id: Some("older-run"),
            seq: 1,
            kind: "tool",
            tool: Some("write_file"),
            input_json: None,
            idempotency_key: Some("older:1"),
        })
        .await
        .unwrap();
    steps
        .close(old, "ok", None, Some("old output"))
        .await
        .unwrap();
    let verifier = verifier(dir.path(), &steps, &task);
    let condition = [Acceptance::ToolsOk {
        tool: "write_file".into(),
        min: 1,
    }];
    assert!(
        !verifier.run(&condition).await[0].passed,
        "an old delivery is not evidence for this execution"
    );
    let current = steps
        .begin(NewStep {
            task_id: &task,
            run_id: Some("run1"),
            seq: 1,
            kind: "tool",
            tool: Some("write_file"),
            input_json: None,
            idempotency_key: Some("current:1"),
        })
        .await
        .unwrap();
    steps
        .close(current, "ok", None, Some("new output"))
        .await
        .unwrap();
    assert!(verifier.run(&condition).await[0].passed);
}
