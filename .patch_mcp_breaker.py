import io

p = "joy-rs/joyczl-mcp/src/lib.rs"
d = io.open(p, encoding="utf-8").read()


def sub(old, new):
    global d
    assert old in d, f"缺锚点：{old[:80]}"
    d = d.replace(old, new, 1)


# 1) 熔断器 + 连接字段
sub(
    """pub struct Connection {
    name: String,
    transport: Mutex<Transport>,
}""",
    """/// 熔断参数。默认抄 hermes：**连续 3 次失败开 60 秒断路器**。
#[derive(Debug, Clone, Copy)]
pub struct BreakerPolicy {
    pub failures: u32,
    pub cooldown: std::time::Duration,
}

impl Default for BreakerPolicy {
    fn default() -> Self {
        Self {
            failures: 3,
            cooldown: std::time::Duration::from_secs(60),
        }
    }
}

/// 一台服务器的健康账。**按连接隔离**：一台挂了不该拖累另一台。
#[derive(Debug, Default)]
struct Breaker {
    consecutive_failures: u32,
    /// 断路器开到这个时刻（`None` = 关着）。
    open_until: Option<std::time::Instant>,
}

impl Breaker {
    /// 现在还能不能敲它的门。开着的话返回还要等多久。
    fn blocked_for(&mut self, now: std::time::Instant) -> Option<std::time::Duration> {
        match self.open_until {
            Some(until) if until > now => Some(until - now),
            // 冷却到了：**半开** —— 放一次探针过去，成不成看这一次。
            Some(_) => {
                self.open_until = None;
                None
            }
            None => None,
        }
    }

    fn record(&mut self, ok: bool, policy: &BreakerPolicy, now: std::time::Instant) {
        if ok {
            // 一次成功就清零：这是我们唯一能拿到的「它缓过来了」的证据。
            self.consecutive_failures = 0;
            self.open_until = None;
            return;
        }
        self.consecutive_failures += 1;
        if self.consecutive_failures >= policy.failures {
            self.open_until = Some(now + policy.cooldown);
        }
    }
}

pub struct Connection {
    name: String,
    transport: Mutex<Transport>,
    breaker: Mutex<Breaker>,
    policy: BreakerPolicy,
}""",
)

# 2) call 走熔断
sub(
    """    pub async fn call(&self, tool: &str, args: Value) -> String {
        let mut transport = self.transport.lock().await;
        match tokio::time::timeout(TIMEOUT, transport.call_tool(tool, args)).await {
            Ok(Ok(text)) => text,
            Ok(Err(error)) => format!("MCP 调用 {}_{tool} 失败：{error}", self.name),
            Err(_) => format!(
                "MCP 调用 {}_{tool} 失败：超过 {} 秒没有回话",
                self.name,
                TIMEOUT.as_secs()
            ),
        }
    }""",
    """    pub async fn call(&self, tool: &str, args: Value) -> String {
        // 熔断：连续失败到阈值就先别敲门了 —— 一台挂掉的服务器会让每一轮
        // 都白等一个超时，而模型还在那儿一遍遍地试。
        let now = std::time::Instant::now();
        if let Some(remaining) = self
            .breaker
            .lock()
            .expect("breaker 锁不该中毒")
            .blocked_for(now)
        {
            return format!(
                "MCP {}_{tool} 被跳过了：这台服务器连续失败过 {} 次，断路器还开着（还有 {} 秒）—— \\
                 等它凉下来会自动放一次探针。",
                self.name,
                self.policy.failures,
                remaining.as_secs().max(1)
            );
        }

        let mut transport = self.transport.lock().await;
        let outcome = tokio::time::timeout(TIMEOUT, transport.call_tool(tool, args)).await;
        drop(transport);

        let (ok, text) = match outcome {
            Ok(Ok(text)) => (true, text),
            Ok(Err(error)) => (
                false,
                format!("MCP 调用 {}_{tool} 失败：{error}", self.name),
            ),
            Err(_) => (
                false,
                format!(
                    "MCP 调用 {}_{tool} 失败：超过 {} 秒没有回话",
                    self.name,
                    TIMEOUT.as_secs()
                ),
            ),
        };
        let mut breaker = self.breaker.lock().expect("breaker 锁不该中毒");
        breaker.record(ok, &self.policy, std::time::Instant::now());
        let opened = breaker.open_until.is_some() && !ok;
        drop(breaker);
        if opened {
            eprintln!(
                "(joy) MCP 服务器 '{}' 连续失败 {} 次，断路器打开 {} 秒",
                self.name,
                self.policy.failures,
                self.policy.cooldown.as_secs()
            );
        }
        text
    }""",
)

io.open(p, "w", encoding="utf-8").write(d)
print("Connection 熔断 ok")
