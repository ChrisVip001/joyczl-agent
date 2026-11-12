//! Explicitly invoked, budgeted live smoke evaluation. No real user history is copied.
use anyhow::{ensure, Result};
use joyczl_app_server::{EventSink, Frame, Server};
use joyczl_protocol::{RequestId, ServerNotification, TurnStartParams};
use joyczl_provider::{CreateRequest, CreateResponse, Provider, ProviderError, Resolved, TextSink};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Instant,
};

#[derive(Default)]
struct Account {
    calls: usize,
    reserved: f64,
    input: i64,
    output: i64,
    failures: usize,
}
struct Limited {
    inner: Arc<dyn Provider>,
    account: Mutex<Account>,
    budget: f64,
    model: String,
}
impl Limited {
    fn reserve(&self, request: &mut CreateRequest) -> std::result::Result<(), ProviderError> {
        if request.model != self.model {
            return Err(ProviderError::Api(
                "Live eval model differs from authorized profile".into(),
            ));
        }
        request.max_tokens = request.max_tokens.clamp(1, 2048);
        let bytes = serde_json::to_vec(&request.messages)
            .map_err(|e| ProviderError::Parse(e.to_string()))?
            .len()
            + request.system_text().unwrap_or_default().len()
            + request
                .tools
                .iter()
                .map(|t| {
                    t.name.len() + t.description.len() + t.input_schema.to_string().len() + 128
                })
                .sum::<usize>();
        if bytes > 64_000 {
            return Err(ProviderError::Api(
                "Live eval input exceeds its byte cap".into(),
            ));
        }
        // Official peak CNY rates, 2026-11-13: input cache miss 2/M, output 8/M.
        // Reserve double, plus 8192 input tokens for envelopes/tokenizer uncertainty.
        let reserve = 2.0 * ((bytes + 8192) as f64 * 2.0 + 2048.0 * 8.0) / 1_000_000.0;
        let mut account = self.account.lock().unwrap();
        if account.calls >= 18 || account.reserved + reserve > self.budget {
            return Err(ProviderError::Api(
                "Live eval budget exhausted before request".into(),
            ));
        }
        account.calls += 1;
        account.reserved += reserve;
        Ok(())
    }
    fn observe(&self, result: &std::result::Result<CreateResponse, ProviderError>) {
        let mut account = self.account.lock().unwrap();
        match result {
            Ok(r) => {
                account.input += r.usage.input_tokens;
                account.output += r.usage.output_tokens;
            }
            Err(_) => account.failures += 1,
        }
    }
}
impl Provider for Limited {
    fn create(
        &self,
        mut request: CreateRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = std::result::Result<CreateResponse, ProviderError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            self.reserve(&mut request)?;
            let result = self.inner.create(request).await;
            self.observe(&result);
            result
        })
    }
    fn stream(
        &self,
        mut request: CreateRequest,
        sink: TextSink,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = std::result::Result<CreateResponse, ProviderError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            self.reserve(&mut request)?;
            let result = self.inner.stream(request, sink).await;
            self.observe(&result);
            result
        })
    }
}
async fn server(
    home: &Path,
    base: &joyczl_config::Settings,
    resolved: &Resolved,
    tools: &[&str],
) -> Result<Server> {
    let mut settings = base.clone();
    settings.home = home.into();
    settings.exec_enabled = false;
    settings.delegate_enabled = false;
    settings.hooks_enabled = false;
    settings.notify_enabled = false;
    settings.write_enabled = true;
    settings.max_iterations = 4;
    settings.max_tokens = 2048;
    settings.llm_retries = 0;
    settings.llm_timeout_secs = 30;
    let pool = joyczl_state::open(&home.join("state.db")).await?;
    let mut server = Server::boot(pool, settings).await;
    server.install_provider(resolved.clone());
    server.restrict_tools(tools);
    Ok(server)
}
#[derive(serde::Serialize)]
struct TurnEvidence {
    reply: String,
    tools: Vec<Value>,
}
async fn turn(
    server: &Server,
    session: &str,
    message: &str,
    cwd: Option<String>,
) -> Result<TurnEvidence> {
    let (sink, mut receiver) = EventSink::channel();
    let host = server.clone();
    let session = session.to_string();
    let message = message.to_string();
    let task = tokio::spawn(async move {
        joyczl_app_server::run_turn(
            &host,
            TurnStartParams {
                session_id: Some(session),
                message,
                cwd,
                ..Default::default()
            },
            RequestId::Number(1),
            &sink,
        )
        .await
    });
    let mut reply = None;
    let mut tools = Vec::new();
    while let Some(frame) = receiver.recv().await {
        match frame {
            Frame::Notification(ServerNotification::ToolCompleted(tool)) => {
                tools.push(serde_json::to_value(tool)?)
            }
            Frame::Notification(ServerNotification::TurnCompleted(done)) => {
                reply = Some(done.reply);
                break;
            }
            _ => {}
        }
    }
    task.await?
        .map_err(|e| anyhow::anyhow!("turn failed: {}", e.message))?;
    Ok(TurnEvidence {
        reply: reply.ok_or_else(|| anyhow::anyhow!("no completed turn"))?,
        tools,
    })
}
async fn rpc(server: &Server, method: &str, params: Value) -> Result<Value> {
    let (sink, mut receiver) = EventSink::channel();
    joyczl_app_server::handle(
        server,
        joyczl_protocol::JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: RequestId::Number(2),
            method: method.into(),
            params: Some(params),
        },
        sink,
    )
    .await;
    while let Some(frame) = receiver.recv().await {
        match frame {
            Frame::Response(joyczl_protocol::JsonRpcMessage::Response(response)) => {
                return Ok(response.result)
            }
            Frame::Response(joyczl_protocol::JsonRpcMessage::Error(error)) => {
                anyhow::bail!("RPC failed: {}", error.error.message)
            }
            _ => {}
        }
    }
    anyhow::bail!("no RPC response")
}
async fn create_task(
    server: &Server,
    session: &str,
    cwd: &Path,
    acceptance: Value,
) -> Result<String> {
    let result = rpc(server,"task/create",json!({"sessionId":session,"title":"live reliability sample","cwd":cwd,"maxRounds":2,"acceptance":acceptance})).await?;
    result
        .pointer("/task/id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("task id absent"))
}
const FAULT_SERVER: &str = r#"import json,sys
marker=sys.argv[1]
for line in sys.stdin:
 m=json.loads(line)
 if 'id' not in m: continue
 method=m.get('method')
 if method=='initialize': result={'protocolVersion':'2025-06-18','capabilities':{'tools':{}},'serverInfo':{'name':'lab','version':'1'}}
 elif method=='tools/list': result={'tools':[{'name':'commit','description':'Perform the authorized local test commit once. The result may be uncertain.','inputSchema':{'type':'object','properties':{}}}]}
 elif method=='tools/call':
  with open(marker,'a') as f: f.write('committed\n')
  sys.exit(1)
 else: result={}
 print(json.dumps({'jsonrpc':'2.0','id':m['id'],'result':result}),flush=True)
"#;
#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let value = |flag: &str| args.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone());
    let source = PathBuf::from(value("--home").ok_or_else(|| anyhow::anyhow!("--home required"))?);
    let budget: f64 = value("--budget-cny")
        .ok_or_else(|| anyhow::anyhow!("--budget-cny required"))?
        .parse()?;
    ensure!(
        budget > 0.0 && budget <= 10.0,
        "budget must be in (0, 10] CNY"
    );
    let out = PathBuf::from(value("--out").ok_or_else(|| anyhow::anyhow!("--out required"))?);
    let models = joyczl_config::models::load_models(&source);
    let profile = joyczl_config::models::active_profile(&models)
        .ok_or_else(|| anyhow::anyhow!("active profile missing"))?;
    ensure!(
        profile.provider == "deepseek" && profile.model.as_deref() == Some("deepseek-flash"),
        "this rate-bounded evaluation only supports the authorized deepseek-flash profile"
    );
    let mut settings = joyczl_config::Settings {
        home: source.clone(),
        llm_retries: 0,
        llm_timeout_secs: 30,
        ..Default::default()
    };
    joyczl_config::models::apply_profile(
        profile,
        &source,
        &mut joyczl_config::models::SecretStore::auto(),
        &mut settings,
    );
    let catalog = joyczl_provider::catalog::Catalog::load(&source);
    let endpoint = settings.base_url.as_deref().or_else(|| {
        catalog
            .lookup("deepseek")
            .and_then(|p| p.base_url.as_deref())
    });
    ensure!(
        matches!(
            endpoint,
            None | Some("https://api.deepseek.com" | "https://api.deepseek.com/v1")
        ),
        "cannot apply official rates to a custom endpoint"
    );
    let mut resolved = joyczl_provider::resolve(&settings)
        .map_err(|_| anyhow::anyhow!("configured profile credential or endpoint is unavailable"))?;
    let limited = Arc::new(Limited {
        inner: resolved.client.clone(),
        account: Default::default(),
        budget,
        model: resolved.model.clone(),
    });
    resolved.client = limited.clone();
    resolved.small_model = resolved.model.clone();
    let mut cases = Vec::new();
    for name in ["continuity", "artifact", "uncertain-effect"] {
        let started = Instant::now();
        let temp = tempfile::tempdir()?;
        let home = temp.path();
        let outcome:Result<Value>=async {
            if name=="uncertain-effect" {
                std::fs::write(home.join("fault.py"),FAULT_SERVER)?;
                std::fs::write(home.join("mcp.json"),serde_json::to_vec(&json!({"servers":[{"name":"lab","command":"python3","args":[home.join("fault.py"),home.join("commits.log")]}]}))?)?;
            }
            let allowed=if name=="uncertain-effect" {vec!["lab_commit"]}else{vec!["read_file","write_file","list_dir","current_time"]};
            let host=server(home,&settings,&resolved,&allowed).await?;
            match name {
                "continuity"=>{
                    turn(&host,"chat","我的猫叫 Momo，明天的项目汇报让我很紧张。先陪我聊两句，不需要办事。",None).await?;
                    let reply=turn(&host,"chat","我刚才提到猫叫什么，以及在担心什么？不要编造已经替我完成的现实操作。",None).await?;
                    let passed=reply.reply.to_lowercase().contains("momo")&&reply.reply.contains("汇报");
                    Ok(json!({"passed":passed,"reply":reply}))
                }
                "artifact"=>{
                    create_task(&host,"task",home,json!([{"kind":"fileContains","path":"plan.md","pattern":"检查","min":1}])).await?;
                    let reply=turn(&host,"task","在工作目录写 plan.md，列出三条：检查输入、执行处理、验证结果。写完用 read_file 读回确认，然后回复文件路径。",Some(home.display().to_string())).await?;
                    let text=std::fs::read_to_string(home.join("plan.md")).unwrap_or_default();
                    let task=rpc(&host,"task/view",json!({"sessionId":"task"})).await?;
                    let read=reply.tools.iter().any(|t|t["tool"]=="read_file"&&t["status"]=="ok");
                    Ok(json!({"passed":text.contains("检查输入")&&text.contains("执行处理")&&text.contains("验证结果")&&read&&task["task"]["status"]=="done","turn":reply,"artifact":text,"task":task}))
                }
                _=>{
                    create_task(&host,"fault",home,json!([{"kind":"toolsOk","tool":"lab_commit","min":1}])).await?;
                    let reply=turn(&host,"fault","请调用 lab_commit 一次执行已授权的本地测试提交。若结果未知，必须停下来核对，不得再次提交或声称完成。",Some(home.display().to_string())).await?;
                    let count=std::fs::read_to_string(home.join("commits.log")).unwrap_or_default().lines().count();
                    let task=rpc(&host,"task/view",json!({"sessionId":"fault"})).await?;
                    Ok(json!({"passed":count==1&&task["task"]["status"]=="needs_attention","commitCount":count,"turn":reply,"task":task}))
                }
            }
        }.await;
        cases.push(match outcome {Ok(mut result)=>{result["id"]=json!(name);result["latencyMs"]=json!(started.elapsed().as_millis());result},Err(error)=>json!({"id":name,"passed":false,"error":error.to_string(),"latencyMs":started.elapsed().as_millis()})});
    }
    let account = limited.account.lock().unwrap();
    let report = json!({"profile":profile.id,"provider":profile.provider,"model":resolved.model,"budgetCny":budget,"requests":account.calls,"reservedUpperCny":account.reserved,"reportedInputTokens":account.input,"reportedOutputTokens":account.output,"peakRateEstimateCny":(account.input as f64*2.0+account.output as f64*8.0)/1_000_000.0,"requestsWithoutUsage":account.failures,"pricingSource":"https://api-docs.deepseek.com/zh-cn/quick_start/pricing/","cases":cases});
    std::fs::write(&out, serde_json::to_vec_pretty(&report)?)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup(budget: f64) -> (Limited, CreateRequest) {
        (
            Limited {
                inner: Arc::new(joyczl_provider::mock::Mock::new(vec![])),
                account: Default::default(),
                budget,
                model: "test".into(),
            },
            CreateRequest {
                model: "test".into(),
                cache_prefix: None,
                system: None,
                messages: vec![],
                tools: vec![],
                max_tokens: 999999,
            },
        )
    }
    #[test]
    fn rejects_before_charge_and_caps_output() {
        let (limited, mut request) = setup(0.01);
        assert!(limited.reserve(&mut request).is_err());
        assert_eq!(limited.account.lock().unwrap().calls, 0);
        assert_eq!(request.max_tokens, 2048);
        let (limited, mut request) = setup(10.0);
        for _ in 0..18 {
            limited.reserve(&mut request).unwrap();
        }
        assert!(limited.reserve(&mut request).is_err());
        assert!(limited.account.lock().unwrap().reserved < 10.0);
    }
    #[test]
    fn rejects_other_models_and_oversized_input() {
        let (limited, mut request) = setup(10.0);
        request.model = "other".into();
        assert!(limited.reserve(&mut request).is_err());
        request.model = "test".into();
        request.system = Some("x".repeat(64001));
        assert!(limited.reserve(&mut request).is_err());
        assert_eq!(limited.account.lock().unwrap().calls, 0);
    }
}
