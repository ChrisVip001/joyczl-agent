//! 脚本化的假 provider。
//!
//! loop 和检索门的测试全靠它：不用起 HTTP 服务、不用真 key，就能精确控制
//! 模型每一轮说什么。这也是把「模型调用」收敛到一个 trait 后白拿的东西。

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Mutex;

use serde_json::Value;

use crate::{
    ContentBlock, CreateRequest, CreateResponse, Provider, ProviderError, StopReason, TextSink,
    Usage,
};

pub struct Mock {
    /// 队列里的应答按序弹出；空了就报错 —— 这样「模型说的话比测试预期的多」
    /// 会立刻暴露，而不是悄悄沿用上一条。
    responses: Mutex<VecDeque<CreateResponse>>,
    /// 收到的请求，供断言用（比如确认 system 和 tools 真的传进去了）。
    pub received: Mutex<Vec<CreateRequest>>,
    /// true = `stream()` 会把文本拆成小块逐个回调，用来测流式路径。
    deltas: bool,
    /// 每次 stream() 被调用时记一笔 —— 测试用它断言「真的走了流式」。
    pub streamed: Mutex<Vec<bool>>,
}

impl Mock {
    pub fn new(responses: Vec<CreateResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            received: Mutex::new(Vec::new()),
            deltas: false,
            streamed: Mutex::new(Vec::new()),
        }
    }

    /// 同 `new`，但走流式路径（文本拆块回调）。
    pub fn streaming(responses: Vec<CreateResponse>) -> Self {
        Self {
            deltas: true,
            ..Self::new(responses)
        }
    }

    fn take(&self, request: CreateRequest) -> Result<CreateResponse, ProviderError> {
        self.received.lock().expect("锁").push(request);
        self.responses
            .lock()
            .expect("锁")
            .pop_front()
            .ok_or_else(|| ProviderError::Api("mock 的应答用完了".to_string()))
    }

    /// 一条纯文本应答，最常用的形状。
    pub fn text(text: &str) -> CreateResponse {
        CreateResponse {
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
            },
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        }
    }

    /// 一次工具调用。
    pub fn tool_use(id: &str, name: &str, input: Value) -> CreateResponse {
        CreateResponse {
            stop_reason: StopReason::ToolUse,
            usage: Usage {
                input_tokens: 20,
                output_tokens: 8,
            },
            content: vec![ContentBlock::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input,
                extra: None,
            }],
        }
    }
}

impl Provider for Mock {
    fn create(
        &self,
        request: CreateRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<CreateResponse, ProviderError>> + Send + '_>>
    {
        let response = self.take(request);
        Box::pin(std::future::ready(response))
    }

    fn stream(
        &self,
        request: CreateRequest,
        on_text: TextSink,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<CreateResponse, ProviderError>> + Send + '_>>
    {
        let response = self.take(request);
        self.streamed.lock().expect("锁").push(true);
        let text = match response.as_ref().ok().and_then(|r| r.content.first()) {
            Some(ContentBlock::Text { text }) => Some(text.clone()),
            _ => None,
        };
        if self.deltas {
            if let Some(text) = text {
                // 两字一切，块间小睡：跟真 SSE 一样有 await 点且给消费方
                // 留出调度余量，turn/interrupt 的取消令牌才能确定性地在
                // 流中途插进来。
                return Box::pin(async move {
                    for chunk in split_chunks(&text, 2) {
                        on_text(&chunk);
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                    response
                });
            }
        }
        Box::pin(std::future::ready(response))
    }
}

fn split_chunks(text: &str, size: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(size.max(1))
        .map(|c| c.iter().collect())
        .collect()
}
