//! Synthetic lifecycle samples. No personal history or external transactions.
use super::*;

async fn task_status(host: &Server, task: &str, expected: &str) -> Result<Value> {
    tokio::time::timeout(std::time::Duration::from_secs(90), async {
        loop {
            let result = rpc(host, "task/get", json!({"taskId":task})).await?;
            let status = result
                .pointer("/task/status")
                .and_then(Value::as_str)
                .unwrap_or("");
            if status == expected {
                return Ok(result);
            }
            ensure!(
                !matches!(status, "cancelled" | "needs_attention" | "done"),
                "Task reached {status} while waiting for {expected}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("Live task did not reach {expected} before its time cap"))?
}

pub(super) async fn run(name: &str, host: &Server, home: &Path) -> Result<Value> {
    match name {
        "chinese-continuity" => {
            turn(host, "chat", "我的猫叫墨墨，明天要做项目汇报，我现在有点紧张。先陪我聊两句，不用替我执行任何事。", None).await?;
            let reply = turn(
                host,
                "chat",
                "刚才我说猫叫什么？我在担心什么？请用中文回答，不要声称你替我做了任何现实操作。",
                None,
            )
            .await?;
            let passed = reply.reply.contains("墨墨")
                && reply.reply.contains("汇报")
                && reply.tools.is_empty();
            Ok(
                json!({"passed":passed,"turn":reply,"scope":"literal continuity and no tool execution; emotional quality is not scored"}),
            )
        }
        "waiting-chat-delivery" => {
            let created = rpc(host,"task/create",json!({"sessionId":"chat","title":"Synthetic meeting data","cwd":home,"maxRounds":3,"acceptance":[{"kind":"jsonEquals","path":"meeting.json","pointer":"/approved","expectedJson":"42"}]})).await?;
            let task = created
                .pointer("/task/id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("Task ID missing"))?;
            rpc(host,"task/wait",json!({"taskId":task,"source":"synthetic-approved-host","eventKey":"figures","description":"Wait for approved figures before writing the final meeting.json"})).await?;
            rpc(host,"task/start",json!({"taskId":task,"message":"Prepare a private draft for meeting.json. Wait for the declared approved figures before writing the final file. After the host supplies the figures, write meeting.json with numeric approved equal to data.value, read it back, and report the path. Do not invent missing figures."})).await?;
            let waiting = task_status(host, task, "suspended").await?;
            let no_early_artifact = !home.join("meeting.json").exists();
            let chat = turn(host,"chat","等数字的时候先陪我聊两句，我今天很累。不要改变或取消原来的会议任务，也不要替我生成数字。",None).await?;
            let event = json!({"conditionId":format!("{task}:input"),"source":"synthetic-approved-host","eventKey":"figures","eventId":"approved-figures-1","data":{"value":42}});
            let first = rpc(host, "task/signal", event.clone()).await?;
            let duplicate = rpc(host, "task/signal", event).await?;
            let (done, completion_error) = match task_status(host, task, "done").await {
                Ok(done) => (done, None),
                Err(error) => (
                    rpc(host, "task/get", json!({"taskId":task})).await?,
                    Some(error.to_string()),
                ),
            };
            let artifact: Value = std::fs::read(home.join("meeting.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or(Value::Null);
            let runs = rpc(host, "task/runs", json!({"taskId":task})).await?;
            let operations = rpc(host, "task/operations", json!({"taskId":task})).await?;
            let writes = final_writes(&operations, home);
            let one_run = runs["runs"]
                .as_array()
                .is_some_and(|items| items.len() == 1);
            let passed = done["task"]["status"] == "done"
                && one_run
                && writes == 1
                && no_early_artifact
                && !chat.reply.trim().is_empty()
                && chat.tools.is_empty()
                && first["duplicate"] == false
                && duplicate["duplicate"] == true
                && artifact["approved"] == 42;
            Ok(
                json!({"passed":passed,"noEarlyArtifact":no_early_artifact,"waiting":waiting,"chat":chat,"firstSignal":first,"duplicate":duplicate,"artifact":artifact,"task":done,"runs":runs,"writeOperations":writes,"operations":operations,"completionError":completion_error}),
            )
        }
        "expired-memory" => {
            let remembered = rpc(host,"memory/remember",json!({"subject":"synthetic cat","content":"My cat is called Orion-739.","source":"user"})).await?;
            let id = remembered
                .pointer("/fact/id")
                .and_then(Value::as_i64)
                .ok_or_else(|| anyhow::anyhow!("Fact ID missing"))?;
            let pool = joyczl_state::open(&home.join("state.db")).await?;
            let facts = joyczl_state::Facts::new(pool);
            joyczl_memory::mirror::mirror_facts(&facts, &joyczl_memory::bundle::Bundle::new(home))
                .await?;
            rpc(
                host,
                "memory/setValidity",
                json!({"factId":id,"validFrom":null,"validUntil":"2000-01-01T00:00:00Z"}),
            )
            .await?;
            let recalled = rpc(host, "memory/search", json!({"query":"cat","topK":10})).await?;
            let reply = turn(
                host,
                "fresh-chat",
                "我的猫叫什么？请先查找现有记忆。如果没有可靠记录，请明确说不知道，不要猜测名字。",
                None,
            )
            .await?;
            let passed = recalled["facts"].as_array().is_some_and(Vec::is_empty)
                && !reply.reply.contains("Orion")
                && !reply.reply.contains("739")
                && ["不知道", "没有", "未找到", "不清楚"]
                    .iter()
                    .any(|word| reply.reply.contains(word));
            Ok(
                json!({"passed":passed,"recall":recalled,"turn":reply,"scope":"expired fact and stale generated page; historical episodes are outside this sample"}),
            )
        }
        _ => anyhow::bail!("Unknown expanded case"),
    }
}

fn final_writes(operations: &Value, home: &Path) -> usize {
    let final_path = home.join("meeting.json");
    operations["operations"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter(|item| {
                    if item["tool"] != "write_file" {
                        return false;
                    }
                    let Some(path) = item.pointer("/arguments/path").and_then(Value::as_str) else {
                        return false;
                    };
                    let path = Path::new(path);
                    let full = if path.is_absolute() {
                        path.to_path_buf()
                    } else {
                        home.join(path)
                    };
                    full == final_path
                })
                .count()
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn drafts_are_not_duplicate_final_delivery_effects() {
        let home = std::env::temp_dir().join("synthetic-eval-home");
        let operations = json!({"operations":[
            {"tool":"write_file","arguments":{"path":home.join("meeting.draft.json")}},
            {"tool":"write_file","arguments":{"path":home.join("meeting.json")}},
            {"tool":"read_file","arguments":{"path":home.join("meeting.json")}},
            {"tool":"write_file","arguments":{"path":home.join("other/meeting.json")}}
        ]});
        assert_eq!(final_writes(&operations, &home), 1);
        let duplicated = json!({"operations":[
            {"tool":"write_file","arguments":{"path":"meeting.json"}},
            {"tool":"write_file","arguments":{"path":home.join("meeting.json")}}
        ]});
        assert_eq!(final_writes(&duplicated, &home), 2);
    }
}
