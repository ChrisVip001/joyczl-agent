import io

p = "joy-rs/joyczl-app-server/src/subagent.rs"
d = io.open(p, encoding="utf-8").read()


def sub(old, new):
    global d
    assert old in d, f"缺锚点：{old[:80]}"
    d = d.replace(old, new, 1)


# 1) 签名带上 result_schema
sub(
    "    fn run(&self, task: String, max_iterations: Option<i32>) -> BoxFut {",
    "    fn run(\n        &self,\n        task: String,\n        max_iterations: Option<i32>,\n        result_schema: Option<serde_json::Value>,\n    ) -> BoxFut {",
)
sub(
    "use joyczl_provider::Resolved;",
    "use joyczl_provider::Resolved;\nuse serde_json::Value;",
)

# 2) 子代理 loop 抽成一个小函数，好让「结构化重试」复用同一条路
sub(
    "impl SubagentRunner for Delegated {",
    '''/// 跑一次子代理的 loop。抽出来是因为「结构化结果不合规」要**再跑一轮**，
/// 而那一轮除了历史与指令之外和第一轮完全一样。
#[allow(clippy::too_many_arguments)]
async fn run_child(
    resolved: &Resolved,
    tools: &ToolRegistry,
    ctx: joyczl_tools::ToolCtx,
    system: &str,
    history: Vec<joyczl_provider::Message>,
    user_message: String,
    max_iterations: i32,
) -> Result<joyczl_loop::LoopResult> {
    joyczl_loop::run(joyczl_loop::Turn {
        client: resolved.client.as_ref(),
        model: &resolved.model,
        system: system.to_string(),
        history,
        user_message,
        tools,
        ctx,
        max_iterations,
        max_tokens: SUBAGENT_MAX_TOKENS,
        tool_result_budget: crate::tool_result_budget(&crate::Settings::default()),
        observer: None,
        on_text: None,
        interrupt: None,
    })
    .await
}

/// 把子代理的结论压成「符合 schema 的 JSON 字符串」。
fn coerce_structured(reply: &str, schema: &Value) -> Result<String, String> {
    let Some(json) = joyczl_memory::gate::extract_json(reply) else {
        return Err("回复里没找到 JSON 对象".to_string());
    };
    let Ok(value) = serde_json::from_str::<Value>(&json) else {
        return Err("那段 JSON 解析不了".to_string());
    };
    joyczl_tools::validate_value(schema, &value)?;
    serde_json::to_string(&value).map_err(|e| e.to_string())
}

impl SubagentRunner for Delegated {''',
)

# 3) 把原来的 loop 调用换成 run_child + 结构化处理
sub(
    """            let result = joyczl_loop::run(joyczl_loop::Turn {
                client: resolved.client.as_ref(),
                model: &resolved.model,
                system,
                // 空历史：委派的意义就是把上下文的重担留在父轮那边。
                history: Vec::new(),
                user_message: task,
                tools: &child_tools,
                ctx,
                max_iterations: iterations,
                max_tokens: SUBAGENT_MAX_TOKENS,
                tool_result_budget: crate::tool_result_budget(&settings),
                observer: None,
                on_text: None,
                interrupt: None,
            })
            .await
            .map_err(|e| anyhow::anyhow!("模型调用失败：{e}"))?;

            let mut summary = result.reply.trim().to_string();""",
    """            let result = run_child(
                &resolved,
                &child_tools,
                ctx.clone(),
                &system,
                // 空历史：委派的意义就是把上下文的重担留在父轮那边。
                Vec::new(),
                task,
                iterations,
            )
            .await
            .map_err(|e| anyhow::anyhow!("模型调用失败：{e}"))?;

            let mut summary = result.reply.trim().to_string();

            // ---- 结构化结果：父模型给了 schema 就要求它是合法 JSON 且过校验。
            // 不合规**带着报错重试一次**（把上一轮的 messages 也带上，它看得见自己
            // 写了什么），仍不合规就回落成散文并写明原因 —— 一次没按格式回话，
            // 不该让整件活白跑（与「失败开放」同一条）。
            if let Some(schema) = &result_schema {
                match coerce_structured(&summary, schema) {
                    Ok(json) => summary = json,
                    Err(why) => {
                        let nudge = format!(
                            "上一次的结论不能用作结构化结果：{why}\\n\\
                             请**只**回复符合下面 schema 的 JSON（不要代码块、不要多余解释）：\\n{schema}"
                        );
                        let retry = run_child(
                            &resolved,
                            &child_tools,
                            ctx,
                            &system,
                            result.messages.clone(),
                            nudge,
                            iterations,
                        )
                        .await;
                        match retry {
                            Ok(second) => match coerce_structured(second.reply.trim(), schema) {
                                Ok(json) => {
                                    eprintln!("(joy) 子代理的结构化结果重试一次后通过");
                                    summary = json;
                                }
                                Err(again) => {
                                    eprintln!(
                                        "(joy) 子代理的结构化结果两次都没过校验：{again}"
                                    );
                                    summary.push_str(&format!(
                                        "\\n（结构化结果两次都没通过校验：{again}；上面是它的原始文字）"
                                    ));
                                }
                            },
                            Err(e) => {
                                summary.push_str(&format!("\\n（结构化重试没跑成：{e}）"));
                            }
                        }
                    }
                }
            }""",
)

io.open(p, "w", encoding="utf-8").write(d)
print("app-server/subagent.rs ok")
