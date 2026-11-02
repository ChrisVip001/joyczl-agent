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
//! * **输出随写随落**：每份输出同时追加进 `<home>/jobs/<pid>-<id>.log`（1 MiB
//!   封顶，超了明说）。内存环管「活着的时候读」，这个文件管「死后读」。
//! * **台账落库**：起跑/收尾都写 state.db 的 jobs 表（迁移 0009）。进程死了，
//!   「跑过什么、跑到哪」还在；启动对账把挂着的标成 interrupted —— **不自动
//!   重跑**（重跑是事故放大器）。

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
/// 输出日志文件的字节上限。比内存环宽（16 倍）——事后回查比实时看需要更多
/// 上下文；但一个刷屏的命令照样不能把磁盘写穿，超了明说。
const LOG_CAP: u64 = 1024 * 1024;

/// 落盘的输出日志：随写随落，封顶后丢弃并留一句标记。
struct Log {
    file: Option<std::fs::File>,
    written: u64,
    truncated: bool,
}

impl Log {
    fn open(path: &std::path::Path) -> Self {
        let file = std::fs::File::create(path)
            .map_err(|e| eprintln!("(joy) 作业日志建不了（{}）：{e}", path.display()))
            .ok();
        Self {
            file,
            written: 0,
            truncated: false,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        use std::io::Write as _;
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if self.truncated {
            return;
        }
        let room = LOG_CAP.saturating_sub(self.written) as usize;
        let take = bytes.len().min(room);
        if take > 0 && file.write_all(&bytes[..take]).is_ok() {
            self.written += take as u64;
        }
        if take < bytes.len() {
            self.truncated = true;
            let note = format!(
                "\n[output truncated at the {LOG_CAP}-byte log cap; the command kept running]\n"
            );
            let _ = file.write_all(note.as_bytes());
        }
    }
}

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

/// 进程内的作业表。**台账在 state.db**（见模块头）；内存里只有活着的那部分。
pub struct Jobs {
    policy: ExecPolicy,
    home: std::path::PathBuf,
    jobs: Arc<Mutex<HashMap<JobId, Job>>>,
    next: AtomicU64,
    rows: Arc<joyczl_state::Jobs>,
    /// 通知器（M3a）：作业收尾往外推——旁路，没有就不发。
    notify: Option<Arc<dyn joyczl_tools::notify::Notifier>>,
    /// 历史作业的内存快照（reconcile时装入）。台账查询是异步的，而
    /// `JobRegistry::list` 是同步的 —— 与其搭同步桥，不如在启动时把历史
    /// 一次装进来：历史不会在本进程里新增（新收尾的作业本来就在内存表里）。
    history: Arc<Mutex<Vec<joyczl_state::JobRow>>>,
}

impl Jobs {
    pub fn new(policy: ExecPolicy, home: std::path::PathBuf, rows: joyczl_state::Jobs) -> Self {
        Self {
            policy,
            home,
            jobs: Arc::new(Mutex::new(HashMap::new())),
            next: AtomicU64::new(1),
            rows: Arc::new(rows),
            history: Arc::new(Mutex::new(Vec::new())),
            notify: None,
        }
    }

    /// 装上通知器（boot 时调；测试里不装）。
    pub fn with_notifier(
        mut self,
        notify: Option<Arc<dyn joyczl_tools::notify::Notifier>>,
    ) -> Self {
        self.notify = notify;
        self
    }

    /// 启动对账：上次崩溃时还在跑的作业标成 interrupted。**不自动重跑** ——
    /// 重跑的决定留给看着台账的人或模型；这条日志就是给他们的「发生了什么」。
    pub async fn reconcile(&self) {
        match self.rows.reconcile_interrupted().await {
            Ok(rows) if rows.is_empty() => {}
            Ok(rows) => {
                // 快照在清账**之前**拍：那些行马上要被标成 interrupted 了，
                // 但历史列表正需要它们。
                *self.history.lock().expect("history 锁不该中毒") = rows.clone();
                for row in &rows {
                    eprintln!(
                        "(joy) 上次退出时仍在跑的作业 {}（{}）已标记 interrupted；\
                         输出保留在 {}。要不要重跑由你决定。",
                        row.id,
                        row.command,
                        row.log_path.as_deref().unwrap_or("（无日志）")
                    );
                }
            }
            Err(e) => eprintln!("(joy) 作业台账对账失败（不影响启动）：{e}"),
        }
    }

    /// 「查不到」就这一句话，**两条路径共用**：读与停必须给出同一个答复 ——
    /// 措辞一旦分岔，就等于把「有这个东西但不是你的」泄露出去。
    fn unknown(id: &str) -> String {
        format!("没有叫 {id} 的作业（用 job_list 看看有哪些）")
    }

    /// 台账回查：作业不在内存表里（进程重启过 / 已收尾清掉）。owner 围栏
    /// 照样核对 —— 历史记录不因为「旧」就放开归属。输出读日志文件全文；
    /// 历史作业不会再有增量，一次给完。
    async fn read_from_ledger(
        rows: &Arc<joyczl_state::Jobs>,
        owner: &str,
        id: &str,
        _cursor: u64,
    ) -> Result<JobRead, String> {
        let row = rows
            .get(id)
            .await
            .map_err(|e| format!("台账查不了：{e}"))?
            .filter(|row| row.owner == owner)
            .ok_or_else(|| Self::unknown(id))?;
        let text = row
            .log_path
            .as_deref()
            .map(std::fs::read_to_string)
            .transpose()
            .ok()
            .flatten()
            .unwrap_or_default();
        Ok(JobRead {
            cursor: text.len() as u64,
            running: false,
            lossy: false,
            dropped: 0,
            exit_code: row.exit_code,
            text,
        })
    }

    /// 找一个作业，并确认它属于这个会话。别人的 id 猜到了也没用。
    fn owned<'a>(
        jobs: &'a mut HashMap<JobId, Job>,
        owner: &str,
        id: &str,
    ) -> Result<&'a mut Job, String> {
        let job = jobs.get_mut(id).ok_or_else(|| Self::unknown(id))?;
        if job.owner != owner {
            // 不透露「有这个东西但不是你的」：只说查不到。
            return Err(Self::unknown(id));
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
        let rows = self.rows.clone();
        // self 的引用不进 async 块（生命周期不活那么久）—— 在块外 clone。
        let notifier = self.notify.clone();
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

            // 输出日志：随写随落，进程死了输出还在。带 pid 防撞名（job id
            // 是进程内自增的），与 outbox 文件同一个理由。
            let log_dir = home.join("jobs");
            let _ = std::fs::create_dir_all(&log_dir);
            let log_path = log_dir.join(format!("{}-{job_id}.log", std::process::id()));
            let log = Arc::new(Mutex::new(Log::open(&log_path)));
            if let Err(e) = rows
                .record_started(&job_id, &owner, &command, &log_path.display().to_string())
                .await
            {
                // 台账写失败不拦作业 —— 作业还能跑、还能读，只是「死后可查」
                // 这一层缺了。但必须说，不能静默。
                eprintln!("(joy) 作业 {job_id} 没记进台账：{e}");
            }

            // 两条读管道并发抽干：先读完 stdout 再读 stderr 会在 stderr 塞满时
            // 互相锁死（前台那条路上也是同一个理由）。
            if let Some(mut stdout) = child.stdout.take() {
                let ring = ring.clone();
                let log = log.clone();
                tokio::spawn(async move {
                    let mut chunk = [0u8; 8192];
                    while let Ok(read) = stdout.read(&mut chunk).await {
                        if read == 0 {
                            break;
                        }
                        ring.lock().expect("ring 锁不该中毒").push(&chunk[..read]);
                        log.lock().expect("log 锁不该中毒").push(&chunk[..read]);
                    }
                });
            }
            if let Some(mut stderr) = child.stderr.take() {
                let ring = ring.clone();
                let log = log.clone();
                tokio::spawn(async move {
                    let mut chunk = [0u8; 8192];
                    while let Ok(read) = stderr.read(&mut chunk).await {
                        if read == 0 {
                            break;
                        }
                        ring.lock().expect("ring 锁不该中毒").push(&chunk[..read]);
                        log.lock().expect("log 锁不该中毒").push(&chunk[..read]);
                    }
                });
            }

            let (killer, killed) = oneshot::channel::<()>();
            let watched_ring = ring.clone();
            let watched_status = status.clone();
            let watched_command = command.clone();
            let watched_home = home.clone();
            let watched_id = job_id.clone();
            let watched_rows = rows.clone();
            tokio::spawn(async move {
                // 等它自己结束，或者等到「有人要停它」。停下的记 killed ——
                // 「失败」和「被人叫停」在台账里是两回事。
                let (exit, was_killed) = tokio::select! {
                    exit = child.wait() => (exit, false),
                    _ = killed => {
                        let _ = child.kill().await;
                        (child.wait().await, true)
                    }
                };
                let code = exit.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
                {
                    let mut status = watched_status.lock().expect("status 锁不该中毒");
                    status.running = false;
                    status.exit_code = Some(code);
                }
                let db_status = if was_killed {
                    "killed"
                } else if code == 0 {
                    "completed"
                } else {
                    "failed"
                };
                if let Err(e) = watched_rows
                    .record_finished(&watched_id, db_status, Some(code))
                    .await
                {
                    eprintln!("(joy) 作业 {watched_id} 的收尾没写进台账：{e}");
                }
                if let Some(notifier) = &notifier {
                    notifier.notify(joyczl_tools::notify::NotifyEvent::JobFinished {
                        id: watched_id.clone(),
                        command: watched_command.clone(),
                        status: db_status.to_string(),
                        exit_code: Some(code),
                    });
                }
                // 收尾落盘：进程退出后仍然回查得到（不是「重启后继续跑」）。
                let bytes = watched_ring.lock().expect("ring 锁不该中毒").snapshot();
                let dir = watched_home.join("outbox").join("jobs");
                if std::fs::create_dir_all(&dir).is_ok() {
                    let body = format!(
                        "# {watched_id}: {watched_command}\n退出码 {code}\n\n{}",
                        String::from_utf8_lossy(&bytes)
                    );
                    // 文件名带 **pid**：job id 是进程内自增的（j1、j2…），两个
                    // Joy 进程会生成同名文件互相覆盖。加 pid 之后撞名只剩
                    // 「同进程同 id」—— 而那不可能（id 唯一）。
                    let file = dir.join(format!("{}-{watched_id}.txt", std::process::id()));
                    if let Err(e) = std::fs::write(&file, body) {
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
        let rows = self.rows.clone();
        Box::pin(async move {
            // 先把要等的东西取出来，**不跨 await 持锁**。
            let table = table.clone();
            // 活表里没有 → 去台账回查。守卫必须先放掉再 await（std
            // MutexGuard 不是 Send，跨 await 持有整条 future 都不 Send）。
            let found = {
                let jobs = table.lock().expect("jobs 锁不该中毒");
                jobs.get(&id)
                    .filter(|job| job.owner == owner)
                    .map(|job| (job.ring.clone(), job.status.clone()))
            };
            let (ring, status) = match found {
                Some(handles) => handles,
                None => return Self::read_from_ledger(&rows, &owner, &id, cursor).await,
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
        drop(jobs);
        // 台账里的历史（进程重启前的那些）也列出来 —— 「上次跑到一半的那个」
        // 得看得见，否则 interrupted 就只是个没人知道的墓碑。历史来自启动时
        // 装的快照，纯同步，不做任何阻塞查询。
        for row in self.history.lock().expect("history 锁不该中毒").iter() {
            if row.owner != owner || out.iter().any(|summary| summary.id == row.id) {
                continue;
            }
            let bytes = row
                .log_path
                .as_deref()
                .and_then(|path| std::fs::metadata(path).ok())
                .map(|meta| meta.len())
                .unwrap_or(0);
            out.push(JobSummary {
                id: row.id.clone(),
                command: row.command.clone(),
                running: false,
                exit_code: row.exit_code,
                bytes,
            });
        }
        // 按 id 排：id 是递增的，于是就是「起的先后」。
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }
}
