//! 后台作业的宿主实现：进程 + 有界环 + 完成落盘。
//!
//! 工具层只知道 `JobRegistry` 那个 trait（见 `joyczl-tools/src/jobs.rs`）；真正的
//! 东西在这儿 —— 与子代理、批准同一条注入纪律：依赖方向只从 app-server 指向 tools。
//!
//! 三条实现要点：
//!
//! * **闸门与前台同一条**：起作业前先过 `pass_gate`（`run_command` 的 handler 做），
//!   用的是同一份 `ExecPolicy` —— 后台不是绕过批准的侧门。
//! * **环是有界的**：溢出丢最旧的字节并记下丢了多少，读的时候如实说。一个刷屏的
//!   命令不该把内存吃光，也不该让「读一次」变成失败。
//! * **收尾写 outbox**：作业结束时把环里剩下的内容写到 `<home>/outbox/jobs/`，
//!   于是进程退出后仍然回查得到（不是「重启后继续跑」，作业本身随进程生死）。

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use joyczl_tools::exec::ExecPolicy;
use joyczl_tools::jobs::{JobFut, JobId, JobRead, JobRegistry, JobSummary};
use tokio::io::AsyncReadExt;
use tokio::sync::oneshot;

/// 输出环的容量（字节）。64 KiB 够看进度，也够不着内存。
const RING_BYTES: usize = 64 * 1024;
/// `wait=true` 的最长等待：比一次工具调用该花的时间短，等不到就如实说还在跑。
const WAIT_LIMIT: std::time::Duration = std::time::Duration::from_secs(20);

pub(crate) struct Ring {
    buf: VecDeque<u8>,
    cap: usize,
    /// 已经丢掉的字节数（读的时候要告诉模型「前面这些你读不到了」）。
    dropped: u64,
}

impl Ring {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::new(),
            cap,
            dropped: 0,
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) {
        if bytes.len() >= self.cap {
            // 一次就超过整个环：只留最后 cap 字节，前面的全算丢掉。
            let keep = bytes.len() - self.cap;
            self.dropped += self.buf.len() as u64 + keep as u64;
            self.buf.clear();
            self.buf.extend(bytes[keep..].iter().copied());
            return;
        }
        self.buf.extend(bytes.iter().copied());
        if self.buf.len() > self.cap {
            let overflow = self.buf.len() - self.cap;
            self.buf.drain(..overflow);
            self.dropped += overflow as u64;
        }
    }

    /// 从绝对位置 `cursor` 读到现在。返回（文本, 新 cursor, 是否丢过东西）。
    pub(crate) fn read_from(&self, cursor: u64) -> (String, u64, bool) {
        let start = cursor.max(self.dropped);
        let lossy = cursor < self.dropped;
        let offset = (start - self.dropped) as usize;
        let bytes: Vec<u8> = self.buf.iter().skip(offset).copied().collect();
        (
            String::from_utf8_lossy(&bytes).to_string(),
            self.dropped + self.buf.len() as u64,
            lossy,
        )
    }

    fn snapshot(&self) -> Vec<u8> {
        self.buf.iter().copied().collect()
    }

    pub(crate) fn total(&self) -> u64 {
        self.dropped + self.buf.len() as u64
    }
}

struct Status {
    running: bool,
    exit_code: Option<i32>,
}

struct Job {
    owner: String,
    command: String,
    ring: Arc<Mutex<Ring>>,
    status: Arc<Mutex<Status>>,
    /// 停它：唤醒那个正在等子进程的 task，让它 kill（子进程归它持有）。
    killer: Option<oneshot::Sender<()>>,
}

/// 进程内的作业表。
pub struct Jobs {
    policy: ExecPolicy,
    home: std::path::PathBuf,
    jobs: Arc<Mutex<HashMap<JobId, Job>>>,
    next: AtomicU64,
}

impl Jobs {
    pub fn new(policy: ExecPolicy, home: std::path::PathBuf) -> Self {
        Self {
            policy,
            home,
            jobs: Arc::new(Mutex::new(HashMap::new())),
            next: AtomicU64::new(1),
        }
    }

    /// 找一个作业，并确认它属于这个会话。别人的 id 猜到了也没用。
    fn owned<'a>(
        jobs: &'a mut HashMap<JobId, Job>,
        owner: &str,
        id: &str,
    ) -> Result<&'a mut Job, String> {
        let job = jobs
            .get_mut(id)
            .ok_or_else(|| format!("没有叫 {id} 的作业（用 job_list 看看有哪些）"))?;
        if job.owner != owner {
            // 不透露「有这个东西但不是你的」：只说查不到。
            return Err(format!("没有叫 {id} 的作业（用 job_list 看看有哪些）"));
        }
        Ok(job)
    }
}

impl JobRegistry for Jobs {
    fn spawn(
        &self,
        owner: &str,
        command: &str,
        cwd: Option<String>,
    ) -> JobFut<Result<JobId, String>> {
        let owner = owner.to_string();
        let command = command.to_string();
        let policy = self.policy.clone();
        let home = self.home.clone();
        let table = self.jobs.clone();
        let id = format!("j{}", self.next.fetch_add(1, Ordering::SeqCst));

        // 可写的根：显式给的工作目录（或进程当前目录）+ home —— 与前台
        // `run_command` 完全一致的三条来源（外加策略里的 extra_roots，由
        // `spawn_sandboxed` 内部并进来）。
        let mut writable: Vec<std::path::PathBuf> = Vec::new();
        if let Some(cwd) = cwd
            .map(std::path::PathBuf::from)
            .filter(|path| path.is_dir())
            .or_else(|| std::env::current_dir().ok())
        {
            writable.push(cwd);
        }
        writable.push(home.clone());

        let job_id = id.clone();
        Box::pin(async move {
            let mut child = joyczl_tools::exec::spawn_sandboxed(&command, &policy, &writable)?;

            let ring = Arc::new(Mutex::new(Ring::new(RING_BYTES)));
            let status = Arc::new(Mutex::new(Status {
                running: true,
                exit_code: None,
            }));

            // 两条读管道并发抽干：先读完 stdout 再读 stderr 会在 stderr 塞满时
            // 互相锁死（前台那条路上也是同一个理由）。
            if let Some(mut stdout) = child.stdout.take() {
                let ring = ring.clone();
                tokio::spawn(async move {
                    let mut chunk = [0u8; 8192];
                    while let Ok(read) = stdout.read(&mut chunk).await {
                        if read == 0 {
                            break;
                        }
                        ring.lock().expect("ring 锁不该中毒").push(&chunk[..read]);
                    }
                });
            }
            if let Some(mut stderr) = child.stderr.take() {
                let ring = ring.clone();
                tokio::spawn(async move {
                    let mut chunk = [0u8; 8192];
                    while let Ok(read) = stderr.read(&mut chunk).await {
                        if read == 0 {
                            break;
                        }
                        ring.lock().expect("ring 锁不该中毒").push(&chunk[..read]);
                    }
                });
            }

            let (killer, killed) = oneshot::channel::<()>();
            let watched_ring = ring.clone();
            let watched_status = status.clone();
            let watched_command = command.clone();
            let watched_home = home.clone();
            let watched_id = job_id.clone();
            tokio::spawn(async move {
                // 等它自己结束，或者等到「有人要停它」。
                let exit = tokio::select! {
                    exit = child.wait() => exit,
                    _ = killed => {
                        let _ = child.kill().await;
                        child.wait().await
                    }
                };
                let code = exit.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
                {
                    let mut status = watched_status.lock().expect("status 锁不该中毒");
                    status.running = false;
                    status.exit_code = Some(code);
                }
                // 收尾落盘：进程退出后仍然回查得到（不是「重启后继续跑」）。
                let bytes = watched_ring.lock().expect("ring 锁不该中毒").snapshot();
                let dir = watched_home.join("outbox").join("jobs");
                if std::fs::create_dir_all(&dir).is_ok() {
                    let body = format!(
                        "# {watched_id}: {watched_command}\n退出码 {code}\n\n{}",
                        String::from_utf8_lossy(&bytes)
                    );
                    if let Err(e) = std::fs::write(dir.join(format!("{watched_id}.txt")), body) {
                        eprintln!("(joy) 后台作业 {watched_id} 的输出没写成文件：{e}");
                    }
                }
                eprintln!("(joy) 后台作业 {watched_id} 结束（退出码 {code}）");
            });

            table.lock().expect("jobs 锁不该中毒").insert(
                job_id.clone(),
                Job {
                    owner,
                    command,
                    ring,
                    status,
                    killer: Some(killer),
                },
            );
            Ok(job_id)
        })
    }

    fn output(
        &self,
        owner: &str,
        id: &JobId,
        cursor: u64,
        wait: bool,
    ) -> JobFut<Result<JobRead, String>> {
        let owner = owner.to_string();
        let id = id.clone();
        let table = self.jobs.clone();
        Box::pin(async move {
            // 先把要等的东西取出来，**不跨 await 持锁**。
            let table = table.clone();
            let (ring, status) = {
                let jobs = table.lock().expect("jobs 锁不该中毒");
                let job = jobs
                    .get(&id)
                    .filter(|job| job.owner == owner)
                    .ok_or_else(|| format!("没有叫 {id} 的作业（用 job_list 看看有哪些）"))?;
                (job.ring.clone(), job.status.clone())
            };

            if wait {
                let deadline = std::time::Instant::now() + WAIT_LIMIT;
                loop {
                    let running = status.lock().expect("status 锁不该中毒").running;
                    if !running || std::time::Instant::now() >= deadline {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }

            let running = status.lock().expect("status 锁不该中毒").running;
            let exit_code = status.lock().expect("status 锁不该中毒").exit_code;
            let (text, cursor, lossy, dropped) = {
                let ring = ring.lock().expect("ring 锁不该中毒");
                let (text, cursor, lossy) = ring.read_from(cursor);
                (text, cursor, lossy, ring.dropped)
            };
            Ok(JobRead {
                text,
                cursor,
                running,
                lossy,
                dropped,
                exit_code,
            })
        })
    }

    fn kill(&self, owner: &str, id: &JobId) -> JobFut<Result<bool, String>> {
        let owner = owner.to_string();
        let id = id.clone();
        let table = self.jobs.clone();
        Box::pin(async move {
            let mut jobs = table.lock().expect("jobs 锁不该中毒");
            let job = Self::owned(&mut jobs, &owner, &id)?;
            let running = job.status.lock().expect("status 锁不该中毒").running;
            if !running {
                return Ok(false);
            }
            if let Some(killer) = job.killer.take() {
                let _ = killer.send(());
            }
            // 立刻标记为「停了」：等那个 task 真的收完进程还要一会儿，而用户
            // 现在问的是「停了吗」。
            job.status.lock().expect("status 锁不该中毒").running = false;
            Ok(true)
        })
    }

    fn list(&self, owner: &str) -> Vec<JobSummary> {
        let jobs = self.jobs.lock().expect("jobs 锁不该中毒");
        let mut out: Vec<JobSummary> = jobs
            .iter()
            .filter(|(_, job)| job.owner == owner)
            .map(|(id, job)| {
                let status = job.status.lock().expect("status 锁不该中毒");
                JobSummary {
                    id: id.clone(),
                    command: job.command.clone(),
                    running: status.running,
                    exit_code: status.exit_code,
                    bytes: job.ring.lock().expect("ring 锁不该中毒").total(),
                }
            })
            .collect();
        // 按 id 排：id 是递增的，于是就是「起的先后」。
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }
}
