//! 清单持久化：加载（只读）、写门、幂等、原子写与发布。
//!
//! ## 为什么写门是 per-store async Mutex
//!
//! 每次修改都要走「校验 → 序列化 → 落盘 → 发布内存」这条长链。若不加门，
//! 两个并发写会各自从同一份旧快照出发，后落盘者覆盖先落盘者（丢更新）。
//! 门放在 `ChecklistStore` 内部，调用方不需要知道。
//!
//! **内存快照锁不跨 IO**：写门内先把手头状态 clone 出来，释放读锁；校验与
//! 序列化都在副本上做；`spawn_blocking` 完成 tmp+fsync+rename+目录 sync 后
//! 才短暂拿写锁发布。
//!
//! ## 取消安全
//!
//! `apply` 把整个执行体 `tokio::spawn` 出去，任务内部持有 `Arc<Inner>`；
//! 调用方 future 被丢弃时，任务仍会完成「磁盘提交 → 发布内存 → 记录幂等」。
//! 不靠"调用 future 被 poll"来保证发布——否则磁盘已提交、内存却没更新，
//! 下一次写会从旧状态出发把刚写的内容盖掉。
//!
//! 但"调用方取消"是**用户的意图**，不是"没 poll"：带取消信号的写会在
//! `write_gate` 之后、读状态之前检查一次（[`Inner::run`] 的取消闸门）。
//! 已取消的写明确报错、不改内存、不记幂等；已经在闸门之后进入磁盘提交的
//! 写仍会完成，保证磁盘/内存一致。检查点放在门后是关键：等待写门的旧写
//! 即便在调用方取消、另一个 reset 提交之后才醒来，也不会再执行。
//!
//! ## 跨 store / 跨进程写者
//!
//! 写门只覆盖**同一个 `Inner`**。同一路径被两个 `ChecklistStore`（或两个
//! Core 进程）打开时，各自的内存快照都可能是旧基线，后写者整文件 rename
//! 会静默覆盖新状态。这里采用与 session 持久化相同的协作写者模型：
//!
//! - 每次磁盘提交持有 sidecar `<path>.save-lock`（OS advisory lock）；
//! - 锁内先比较**文件指纹**（长度 + 纳秒 mtime + Unix inode）与加载/上次
//!   提交时的基线，不一致就 `Stale` 明确失败——不覆盖、不发布、不记幂等；
//! - tmp+rename 换 inode，因此"同长度、同一秒"的旧快照也不会被误判为新。
//!
//! 这是**协作**写者协议：不遵守 `.save-lock` / 指纹校验的外部进程仍然可以
//! 覆盖文件（任何普通文件都如此）。本模块保证自己的所有磁盘写者 fail-closed。
//!
//! ## 失败语义
//!
//! - rename **之前**任何失败（含锁/指纹/陈旧基线）：返回错误，内存不发布、
//!   不记录幂等，可重试；
//! - rename **之后**目录 sync 失败：文件已提交，**发布内存**并返回明确的
//!   "已提交但同步失败"错误——绝不无声当成功，也不假装没写过；
//! - 反序列化未知字段 / 版本 / 超限：`open` 直接失败，绝不回写覆盖。

use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant, SystemTime};

use echo_defs::sanitize::Redactor;
use echo_defs::tool::Cancellation;
use serde_json::{json, Map, Value};
use uuid::Uuid;

use echo_defs::registry::ToolError;

use super::model::{self, StoreState};
use super::ops::{self, Operation, Request};

/// 幂等记录直接持久化在 `StoreState` 里（重连 / 重启后可重放）。
type IdemOutcome = model::IdempotencyOutcome;
type IdemRecord = model::IdempotencyRecord;

enum IdemLookup {
    Miss,
    Match(IdemOutcome),
    /// 记录属于旧 epoch（reset_session 推进过文件 epoch）：拒绝重放。
    StaleEpoch,
    /// 同 key digest 但 payload 指纹不同：明确冲突。
    DifferentPayload,
}

/// 磁盘原子写的结果。
enum WriteOutcome {
    Committed(Option<FileStamp>),
    /// rename 已发生（文件已提交），但父目录 sync 失败：结果不确定。
    CommittedDirSyncFailed {
        error: String,
        stamp: Option<FileStamp>,
    },
    /// 锁内发现磁盘文件已不是本 store 的基线：拒绝覆盖（协作写者 fail-closed）。
    Stale {
        expected: String,
        actual: String,
    },
    /// 写 tmp / rename 等 IO 失败（文件未被本次写替换）。
    Failed(String),
}

/// 文件指纹（乐观并发基线）：长度 + mtime（含纳秒）+ Unix inode。
///
/// 与 `agent-module-session` 的 `FileStamp` 同思路：本模块的提交是
/// tmp+rename，每次成功落盘都会换 inode；不依赖"秒级 mtime"这种不可靠
/// 比较，也不在每次保存时把整份文件读回来做哈希。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    mtime: Option<SystemTime>,
    #[cfg(unix)]
    ino: u64,
}

impl FileStamp {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            len: metadata.len(),
            mtime: metadata.modified().ok(),
            #[cfg(unix)]
            ino: {
                use std::os::unix::fs::MetadataExt;
                metadata.ino()
            },
        }
    }

    /// 读取路径指纹；`Ok(None)` = 文件不存在。
    ///
    /// 路径是目录时返回错误：写入绝不能把目录当成"可替换的文件"。
    fn capture(path: &Path) -> std::io::Result<Option<Self>> {
        match std::fs::metadata(path) {
            Ok(metadata) if metadata.is_dir() => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "清单路径是目录而不是文件",
            )),
            Ok(metadata) => Ok(Some(Self::from_metadata(&metadata))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn describe(&self) -> String {
        let mut text = format!("len={}", self.len);
        #[cfg(unix)]
        {
            text.push_str(&format!(" ino={}", self.ino));
        }
        if let Some(mtime) = self.mtime {
            if let Ok(duration) = mtime.duration_since(std::time::UNIX_EPOCH) {
                text.push_str(&format!(
                    " mtime={}.{:09}",
                    duration.as_secs(),
                    duration.subsec_nanos()
                ));
            }
        }
        text
    }
}

fn describe_stamp(stamp: Option<FileStamp>) -> String {
    stamp
        .map(|stamp| stamp.describe())
        .unwrap_or_else(|| "absent".into())
}

/// 本 store 认为磁盘文件"应该是什么样"的基线。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileOrigin {
    /// 加载/上次提交时文件不存在：只允许"文件仍不存在"的第一次写。
    Missing,
    /// 加载或上次成功提交时看到的文件身份。
    At(FileStamp),
    /// 提交已发生但新指纹读不到：拒绝后续写，直到重新 open。
    Unknown,
}

impl FileOrigin {
    fn matches(&self, current: Option<FileStamp>) -> bool {
        match (self, current) {
            (Self::Missing, None) => true,
            (Self::At(expected), Some(actual)) => *expected == actual,
            _ => false,
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::Missing => "absent".into(),
            Self::At(stamp) => stamp.describe(),
            Self::Unknown => "unknown-after-commit".into(),
        }
    }
}

struct Inner {
    path: Option<PathBuf>,
    state: RwLock<StoreState>,
    /// per-store 写串行门。只在 `Inner::run` 的写路径持有。
    write_gate: tokio::sync::Mutex<()>,
    /// 磁盘基线：同一 store 的写由 `write_gate` 串行，跨 store 由锁内指纹比对。
    origin: std::sync::Mutex<FileOrigin>,
    /// 动态脱敏器：每次快照 / 摘要 / 写前重新取当前 Arc。
    redactor: RwLock<Option<Arc<dyn Redactor>>>,
    #[cfg(test)]
    fail_dir_sync: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    test_hooks: TestWriteHooks,
}

/// 仅测试：可控地让某个写持有写门 / 观察某个写开始等门。
#[cfg(test)]
#[derive(Default)]
struct TestWriteHooks {
    hold_gate: std::sync::Mutex<Option<GateHold>>,
    notify_gate_wait: std::sync::Mutex<Option<Arc<tokio::sync::Notify>>>,
}

#[cfg(test)]
struct GateHold {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Semaphore>,
}

impl Inner {
    fn new(
        path: Option<PathBuf>,
        state: StoreState,
        redactor: Option<Arc<dyn Redactor>>,
        origin: FileOrigin,
    ) -> Self {
        Self {
            path,
            state: RwLock::new(state),
            write_gate: tokio::sync::Mutex::new(()),
            origin: std::sync::Mutex::new(origin),
            redactor: RwLock::new(redactor),
            #[cfg(test)]
            fail_dir_sync: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            test_hooks: TestWriteHooks::default(),
        }
    }

    fn origin(&self) -> FileOrigin {
        *self
            .origin
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn set_origin(&self, origin: FileOrigin) {
        *self
            .origin
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = origin;
    }

    /// 仅测试：下一个写开始等写门前通知。
    #[cfg(test)]
    fn take_gate_wait(&self) -> Option<Arc<tokio::sync::Notify>> {
        self.test_hooks
            .notify_gate_wait
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    /// 仅测试：下一个写拿到写门后挂起，直到测试放行。
    #[cfg(test)]
    async fn maybe_hold_gate(&self) {
        let hold = self
            .test_hooks
            .hold_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(hold) = hold {
            hold.entered.notify_one();
            let _ = hold.release.acquire().await;
        }
    }
    fn redactor_snapshot(&self) -> Option<Arc<dyn Redactor>> {
        read_lock(&self.redactor).clone()
    }

    fn read_state(&self) -> StoreState {
        read_lock(&self.state).clone()
    }

    fn read_view(&self) -> StoreState {
        read_lock(&self.state).view()
    }

    fn publish(&self, state: StoreState) {
        *write_lock(&self.state) = state;
    }

    fn check_expected(&self, state: &StoreState, request: &Request) -> Result<(), ToolError> {
        if let Some(expected) = &request.expected.epoch {
            let expected = Uuid::parse_str(expected)
                .map_err(|_| ToolError::InvalidArguments("expected_epoch 必须是 UUID".into()))?;
            let current = Uuid::parse_str(&state.epoch).unwrap_or_default();
            if expected != current {
                return Err(self.conflict(state, request));
            }
        }
        if let Some(expected) = request.expected.revision {
            if expected != state.revision {
                return Err(self.conflict(state, request));
            }
        }
        Ok(())
    }

    fn conflict(&self, state: &StoreState, request: &Request) -> ToolError {
        ToolError::Execution(format!(
            "清单版本冲突：请求 expected_epoch={:?} expected_revision={:?}，当前 epoch={} revision={}；请先 list/get 刷新后重试",
            request.expected.epoch, request.expected.revision, state.epoch, state.revision
        ))
    }

    async fn run(
        self: &Arc<Self>,
        session_id: String,
        request: Request,
        cancel: Option<Arc<dyn Cancellation>>,
    ) -> Result<String, ToolError> {
        if !request.operation.is_write() {
            let state = self.read_view();
            let redactor = self.redactor_snapshot();
            let step = ops::run_read(&state, &session_id, &request.operation, redactor.as_deref())?;
            return Ok(render_result_text(&render_result(
                &state,
                &request.operation,
                step,
                redactor.as_deref(),
            )));
        }

        // 写路径：整个「校验 → 落盘 → 发布」串行化。
        //
        // 取消闸门必须在**拿到门之后、读状态之前**：调用方 future 被丢弃
        // 不会停止已经 spawn 的写任务，旧写可能排在另一个 reset 后面等门；
        // 如果它在 reset 之后才醒来，必须在这里看到取消并明确失败，绝不能
        // 把取消前的意图重新施加到 reset 后的状态上。
        #[cfg(test)]
        if let Some(waiter) = self.take_gate_wait() {
            waiter.notify_one();
        }
        let _gate = self.write_gate.lock().await;
        #[cfg(test)]
        self.maybe_hold_gate().await;
        if cancel.as_ref().is_some_and(|cancel| cancel.is_cancelled()) {
            return Err(ToolError::Execution(
                "清单写入已取消（未执行任何修改，也未记录幂等）".into(),
            ));
        }
        let redactor = self.redactor_snapshot();
        let mut candidate = self.read_state();

        // 幂等重放必须在 expected 校验之前：重试携带的是**原始**版本号，
        // 首次成功已把 revision 推进，先校验会把合法重试误判成冲突。
        // 记录随文件持久化，因此 store 重建后的重试同样不会重复执行。
        if let Some(key_digest) = request.request_key.as_deref() {
            match lookup_idem(&candidate, &session_id, key_digest, &request.fingerprint) {
                IdemLookup::Miss => {}
                IdemLookup::Match(IdemOutcome::Ok { result }) => {
                    // 结果以结构化 Value 持久化；重放时按 schema 动态脱敏，
                    // 不用"JSON 字符串替换"（那会破坏引号 / 转义）。
                    let mut result = result;
                    model::redact_result(&mut result, redactor.as_deref());
                    return Ok(render_result_text(&result));
                }
                IdemLookup::Match(IdemOutcome::Err { message }) => {
                    return Err(ToolError::Execution(model::redact(
                        redactor.as_deref(),
                        &message,
                    )))
                }
                IdemLookup::StaleEpoch => {
                    return Err(ToolError::InvalidArguments(format!(
                        "幂等键（摘要 {}…）属于旧 epoch（reset_session 后已失效，拒绝重放）；请换一个新的 request_id",
                        short_digest(key_digest)
                    )))
                }
                IdemLookup::DifferentPayload => {
                    return Err(ToolError::InvalidArguments(format!(
                        "幂等键（摘要 {}…）已用于不同的请求内容；请换一个 request_id",
                        short_digest(key_digest)
                    )))
                }
            }
        }

        self.check_expected(&candidate, &request)?;
        let step = ops::run_write(
            &mut candidate,
            &session_id,
            &request.operation,
            redactor.as_deref(),
        )?;

        let changed = step.changed;
        if changed {
            candidate.revision = candidate
                .revision
                .checked_add(1)
                .ok_or_else(|| ToolError::Execution("清单 revision 溢出".into()))?;
        }
        if let Some(epoch) = &step.new_epoch {
            candidate.epoch = epoch.clone();
            // reset_session 推进文件 epoch：其他会话的幂等记录随文件 epoch
            // 迁移（仍可重试）；目标会话的旧记录保留旧 epoch，命中即明确
            // 拒绝重放——不能拿 reset 前的成功结果声称现在仍然成功。
            for (record_session, records) in candidate.idempotency.iter_mut() {
                if record_session == &session_id {
                    continue;
                }
                for record in records {
                    record.epoch = epoch.clone();
                }
            }
        }

        // 先生成结果、再写入幂等记录，最后才序列化：重连重试拿到的结果
        // 与首次成功一致（记录里的 result 就是结构化结果）。
        let result = render_result(&candidate, &request.operation, step, redactor.as_deref());
        let record_epoch = candidate.epoch.clone();
        let mut persist = changed;
        if let Some(key_digest) = request.request_key.as_deref() {
            record_idem(
                &mut candidate,
                &session_id,
                key_digest,
                &request.fingerprint,
                &record_epoch,
                IdemOutcome::Ok {
                    result: result.clone(),
                },
            );
            // 即使本次没有状态变更（no-op ack），也把幂等记录随同一 candidate
            // 落盘：重启后重试同样返回已确认结果，而不是重新执行。
            persist = true;
        }
        if !persist {
            return Ok(render_result_text(&result));
        }

        // 写前逐字符串脱敏（含从旧文件加载、当时脱敏器尚不认识的内容，
        // 以及幂等记录里的结果内容字段；机器 id / epoch / revision 不动）。
        model::redact_state(&mut candidate, redactor.as_deref());
        let bytes = model::serialize_state(&candidate)?;

        if let Some(path) = &self.path {
            let path = path.clone();
            #[cfg(test)]
            let fail_dir_sync = self
                .fail_dir_sync
                .swap(false, std::sync::atomic::Ordering::SeqCst);
            #[cfg(not(test))]
            let fail_dir_sync = false;

            // 跨 store / 跨进程协作：锁覆盖「指纹检查 → tmp → rename」，两个
            // 独立写者不会同时通过检查后互相覆盖。等待是异步的（不 panic、
            // 不阻塞运行时线程）；超时明确失败，内存不发布。
            let _path_guard = SavePathLock::acquire(&path).await.map_err(|error| {
                ToolError::Execution(format!(
                    "清单保存失败（未能获取写锁，内存状态未变，可重试）: {error}"
                ))
            })?;
            let expected = self.origin();
            let written = tokio::task::spawn_blocking(move || {
                atomic_write(&path, &bytes, expected, fail_dir_sync)
            })
            .await;
            match written {
                Ok(WriteOutcome::Committed(stamp)) => {
                    self.set_origin(origin_after_commit(stamp));
                    self.publish(candidate);
                }
                Ok(WriteOutcome::CommittedDirSyncFailed { error, stamp }) => {
                    // 文件已经 rename 成功：以磁盘为准发布内存（含幂等记录），
                    // 但**明确报错**，绝不无声当成功。同一 request_id 的重试会
                    // 命中已发布记录并返回首次结果（不会重复执行）；新请求应
                    // 先 list/get 确认当前状态。
                    self.set_origin(origin_after_commit(stamp));
                    self.publish(candidate);
                    let message = format!(
                        "清单已提交到磁盘，但目录同步失败（结果可能不确定，重启后以磁盘内容为准）: {error}；同一 request_id 重试不会重复执行"
                    );
                    return Err(ToolError::Execution(message));
                }
                Ok(WriteOutcome::Stale { expected, actual }) => {
                    // 另一个 store / 进程已经提交了新状态。旧候选绝不能
                    // rename 覆盖它；内存也不发布失败候选、不记幂等。
                    return Err(ToolError::Execution(format!(
                        "清单文件已被其他写者更新（本 store 基线 {expected}，磁盘现状 {actual}）；本次未写入，内存状态未变，请重新加载（ChecklistStore::open）后再试"
                    )));
                }
                Ok(WriteOutcome::Failed(error)) => {
                    return Err(ToolError::Execution(format!(
                        "清单保存失败（内存状态未变，可重试）: {error}"
                    )))
                }
                Err(join_error) => {
                    return Err(ToolError::Execution(format!(
                        "清单保存任务异常终止（内存状态未变，可重试）: {join_error}"
                    )))
                }
            }
        } else {
            self.publish(candidate);
        }

        Ok(render_result_text(&result))
    }
}

/// 对外句柄：`Arc<ChecklistStore>` 可被工具、命令域、测试共享。
///
/// `ChecklistStore` 自身也可 clone（内部仍是同一个 `Arc<Inner>`）。
pub struct ChecklistStore {
    inner: Arc<Inner>,
}

impl Clone for ChecklistStore {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl fmt::Debug for ChecklistStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sessions = read_lock(&self.inner.state).sessions.len();
        f.debug_struct("ChecklistStore")
            .field("path", &self.inner.path)
            .field("sessions", &sessions)
            .finish()
    }
}

impl ChecklistStore {
    /// 进程内 store（测试 / 未配置持久化时使用）：没有文件 IO，修改后立即发布。
    pub fn memory() -> Self {
        Self {
            inner: Arc::new(Inner::new(
                None,
                StoreState::empty(),
                None,
                FileOrigin::Missing,
            )),
        }
    }

    /// 从 `path` **只读加载**：
    ///
    /// - 文件不存在 = 空状态（首次写入时才创建文件）；
    /// - 文件存在但损坏 / 未知版本 / 未知字段 / 超出限额 = `Err`，**不覆盖**；
    /// - 加载成功不会改写文件（mtime / 内容保持不变）；
    /// - 加载前后文件身份必须一致：加载期间被其他写者替换 = 明确 `Err`，
    ///   不把半新半旧的快照当成基线。
    pub fn open(path: PathBuf, redactor: Option<Arc<dyn Redactor>>) -> Result<Self, ToolError> {
        let before = FileStamp::capture(&path).map_err(|error| {
            model::redact_error(
                redactor.as_deref(),
                ToolError::Execution(format!("读取清单文件指纹失败: {error}")),
            )
        })?;
        let mut state =
            load_state(&path).map_err(|error| model::redact_error(redactor.as_deref(), error))?;
        let after = FileStamp::capture(&path).map_err(|error| {
            model::redact_error(
                redactor.as_deref(),
                ToolError::Execution(format!("读取清单文件指纹失败: {error}")),
            )
        })?;
        if before != after {
            return Err(model::redact_error(
                redactor.as_deref(),
                ToolError::Execution(
                    "清单文件在加载期间被其他写者修改（拒绝使用不一致的快照，请重试）".into(),
                ),
            ));
        }
        let origin = match after {
            Some(stamp) => FileOrigin::At(stamp),
            None => FileOrigin::Missing,
        };
        // 加载时只洗幂等记录（不写盘、不动清单内容）：这样旧参数/结果里的
        // 秘密不会从重放路径漏出，而加载本身仍保持只读语义。
        model::redact_idempotency(&mut state, redactor.as_deref());
        Ok(Self {
            inner: Arc::new(Inner::new(Some(path), state, redactor, origin)),
        })
    }

    /// 替换动态脱敏器（接线方注入 / 运行期更新）。
    pub fn set_redactor(&self, redactor: Option<Arc<dyn Redactor>>) {
        *write_lock(&self.inner.redactor) = redactor;
    }

    /// 持久化路径（内存 store 为 None）。
    pub fn path(&self) -> Option<&Path> {
        self.inner.path.as_deref()
    }

    /// 执行一次清单操作（无取消信号）。
    ///
    /// - 会话 id 由调用方显式传入（不读参数里的 `__session_id`）；
    /// - 读操作直接返回；写操作在后台任务中持有 `Arc<Inner>` 完成
    ///   「校验 → 落盘 → 发布 → 记录幂等」，调用方取消不会让磁盘/内存分叉。
    pub async fn apply(&self, session_id: &str, args: Value) -> Result<String, ToolError> {
        self.apply_with_cancel(session_id, args, None).await
    }

    /// 执行一次清单操作，并携带调用方的取消信号。
    ///
    /// 写路径在**拿到 `write_gate` 之后、读状态之前**检查取消：已取消则
    /// 明确 `Err`、不改内存、不记录幂等。已经越过该检查点的写会继续完成
    /// 磁盘提交与发布（磁盘/内存一致优先）。
    pub async fn apply_with_cancel(
        &self,
        session_id: &str,
        args: Value,
        cancel: Option<Arc<dyn Cancellation>>,
    ) -> Result<String, ToolError> {
        model::validate_session_id(session_id)?;
        let redactor = self.inner.redactor_snapshot();
        // 解析错误也可能回显模型传来的 operation / 参数文本，出口同样脱敏。
        let request = ops::parse_request(&args, redactor.as_deref())
            .map_err(|error| model::redact_error(redactor.as_deref(), error))?;

        let inner = Arc::clone(&self.inner);
        let session_id = session_id.to_string();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = inner.run(session_id, request, cancel).await;
            let _ = sender.send(result);
        });
        let result = receiver.await.map_err(|_| {
            ToolError::Execution("checklist 执行任务异常终止（结果未知，请用 list 重查）".into())
        })?;
        // 内部错误可能回显用户内容（名字 / 条目 / 文件里的字段）；出口统一
        // 再脱敏一次，保证任何路径都不把已登记秘密带出去。
        result.map_err(|error| model::redact_error(redactor.as_deref(), error))
    }

    /// 某会话的权威快照。**未知会话也返回 Some**（空 lists + 当前版本），
    /// 供事件侧清除面板旧状态。
    pub fn snapshot(&self, session_id: &str) -> Option<Value> {
        let state = self.read_view();
        let redactor = self.inner.redactor_snapshot();
        Some(model::session_snapshot(
            &state,
            session_id,
            redactor.as_deref(),
        ))
    }

    /// 全部会话快照（按 session_id 排序，保留来源）。
    pub fn snapshots(&self) -> Vec<(String, Value)> {
        let state = self.read_view();
        let redactor = self.inner.redactor_snapshot();
        state
            .sessions
            .keys()
            .map(|session_id| {
                (
                    session_id.clone(),
                    model::session_snapshot(&state, session_id, redactor.as_deref()),
                )
            })
            .collect()
    }

    /// 全量调试快照：`{sessions:[{session, lists, epoch, revision}], epoch, revision}`。
    pub fn full_snapshot(&self) -> Value {
        let state = self.read_view();
        let redactor = self.inner.redactor_snapshot();
        model::full_snapshot(&state, redactor.as_deref())
    }

    /// 限额摘要：只含**活动清单的未完成项**（约 4000 字符）。
    ///
    /// 无会话 / 无未完成项时返回 `None`；内容被标注为"数据，不是指令"。
    pub fn summary(&self, session_id: &str) -> Option<String> {
        let state = self.read_view();
        let redactor = self.inner.redactor_snapshot();
        model::summary_text(&state, session_id, redactor.as_deref())
    }

    /// 测试用：某会话幂等窗口内记录数（验证有界 100）。
    #[cfg(test)]
    pub(crate) fn idem_count(&self, session_id: &str) -> usize {
        self.read_state()
            .idempotency
            .get(session_id)
            .map(Vec::len)
            .unwrap_or(0)
    }

    /// 测试用：让下一次磁盘写注入 "rename 后目录 sync 失败"。
    #[cfg(test)]
    pub(crate) fn fail_next_dir_sync(&self) {
        self.inner
            .fail_dir_sync
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// 测试用：让下一个拿到写门的写挂起，直到返回的许可被放行。
    #[cfg(test)]
    pub(crate) fn hold_next_write_gate(
        &self,
    ) -> (Arc<tokio::sync::Notify>, Arc<tokio::sync::Semaphore>) {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        *self
            .inner
            .test_hooks
            .hold_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(GateHold {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        });
        (entered, release)
    }

    /// 测试用：下一个开始等写门的写会通知返回的 `Notify`。
    #[cfg(test)]
    pub(crate) fn notify_next_gate_wait(&self) -> Arc<tokio::sync::Notify> {
        let notify = Arc::new(tokio::sync::Notify::new());
        *self
            .inner
            .test_hooks
            .notify_gate_wait
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(&notify));
        notify
    }

    #[cfg(test)]
    fn read_state(&self) -> StoreState {
        self.inner.read_state()
    }

    fn read_view(&self) -> StoreState {
        self.inner.read_view()
    }
}

/// 渲染结构化结果（幂等记录持久化的就是它）。
///
/// 只做**字段级**脱敏（summary/display 是内容）；`data` 里的 list 数据已在
/// ops 层逐字段脱敏，机器 id/epoch/revision 原样保留。
fn render_result(
    state: &StoreState,
    operation: &Operation,
    step: ops::StepResult,
    redactor: model::RedactorRef<'_>,
) -> Value {
    let mut object = Map::new();
    object.insert("ok".into(), json!(true));
    object.insert("operation".into(), json!(operation.name()));
    object.insert("epoch".into(), json!(state.epoch));
    object.insert("revision".into(), json!(state.revision));
    object.insert("changed".into(), json!(step.changed));
    object.insert(
        "summary".into(),
        json!(model::redact(redactor, &step.summary)),
    );
    for (key, value) in step.data {
        object.insert(key, value);
    }
    if let Some(display) = step.display {
        object.insert("display".into(), json!(model::redact(redactor, &display)));
    }
    Value::Object(object)
}

/// 结果对象 → 线上字符串。`Value` 序列化不会失败；兜底仅防御性保留。
fn render_result_text(result: &Value) -> String {
    serde_json::to_string(result)
        .unwrap_or_else(|_| "{\"ok\":false,\"summary\":\"结果序列化失败\"}".to_string())
}

/// 冲突报错只回显摘要前缀，不回显原始 key（它可能是秘密）。
fn short_digest(digest: &str) -> &str {
    digest.get(..12).unwrap_or(digest)
}

// ── 加载 ────────────────────────────────────────────────────────────

fn load_state(path: &Path) -> Result<StoreState, ToolError> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // 首次使用：空状态；写入时才创建文件/目录。
            return Ok(StoreState::empty());
        }
        Err(error) => {
            return Err(ToolError::Execution(format!(
                "读取清单文件元信息失败: {error}"
            )))
        }
    };
    if metadata.is_dir() {
        return Err(ToolError::Execution(format!(
            "清单路径是目录而不是文件: {}",
            path.display()
        )));
    }
    if metadata.len() > model::MAX_FILE_BYTES {
        return Err(ToolError::Execution(format!(
            "清单文件大小 {} 字节超过 {} MiB 上限（拒绝加载/覆盖）",
            metadata.len(),
            model::MAX_FILE_BYTES / 1024 / 1024
        )));
    }
    let bytes = std::fs::read(path)
        .map_err(|error| ToolError::Execution(format!("读取清单文件失败: {error}")))?;
    if bytes.len() as u64 > model::MAX_FILE_BYTES {
        return Err(ToolError::Execution(format!(
            "清单文件读取后超过 {} MiB 上限（拒绝加载/覆盖）",
            model::MAX_FILE_BYTES / 1024 / 1024
        )));
    }
    if bytes.is_empty() {
        return Err(ToolError::Execution(
            "清单文件为空（损坏；拒绝加载/覆盖）".into(),
        ));
    }
    let state: StoreState = serde_json::from_slice(&bytes).map_err(|error| {
        ToolError::Execution(format!(
            "清单文件损坏或包含未知字段/版本（拒绝加载/覆盖）: {error}"
        ))
    })?;
    model::validate_state(&state, "清单文件")?;
    Ok(state)
}

// ── 原子写 / 协作写者锁 ────────────────────────────────────────────

/// `<path>.save-lock`（与 session 持久化同约定）。
pub(super) fn lock_path(path: &Path) -> PathBuf {
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".save-lock");
    PathBuf::from(lock_path)
}

fn origin_after_commit(stamp: Option<FileStamp>) -> FileOrigin {
    match stamp {
        Some(stamp) => FileOrigin::At(stamp),
        None => FileOrigin::Unknown,
    }
}

/// 同一路径的跨 store / 跨进程协作写锁（OS advisory lock）。
///
/// 用 `<path>.save-lock` 作为锁文件，`File::try_lock` 非阻塞尝试 + 异步
/// 等待（5ms 轮询，30s 超时）。锁随文件句柄关闭/进程退出释放；等待不会
/// 阻塞运行时线程，也不会 panic——超时明确 Err。
struct SavePathLock {
    _file: std::fs::File,
}

impl SavePathLock {
    async fn acquire(path: &Path) -> Result<Self, String> {
        let lock_path = lock_path(path);
        if let Some(parent) = lock_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("创建写锁目录 {} 失败：{error}", parent.display()))?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|error| format!("打开写锁文件 {} 失败：{error}", lock_path.display()))?;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(format!("等待写锁超时：{}", lock_path.display()));
                }
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(format!("获取写锁失败（{}）：{error}", lock_path.display()));
                }
            }
        }
    }
}

/// 锁内原子提交：先比对基线指纹，再 tmp+fsync+rename+目录 sync。
///
/// 调用方必须持有对应的 [`SavePathLock`]；这样"检查 → 序列化 → rename"
/// 对协作写者是互斥的，后到者在锁内会看到新指纹并明确 `Stale`。
fn atomic_write(
    path: &Path,
    bytes: &[u8],
    expected: FileOrigin,
    fail_dir_sync: bool,
) -> WriteOutcome {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if let Err(error) = std::fs::create_dir_all(parent) {
        return WriteOutcome::Failed(format!("创建父目录失败: {error}"));
    }

    // 写前指纹：锁内读取，和加载/上次提交的基线比较。不一致 = 其他
    // store/进程已经写过，旧快照绝不能覆盖新状态。
    let current = match FileStamp::capture(path) {
        Ok(stamp) => stamp,
        Err(error) => return WriteOutcome::Failed(format!("读取文件指纹失败: {error}")),
    };
    if !expected.matches(current) {
        return WriteOutcome::Stale {
            expected: expected.describe(),
            actual: describe_stamp(current),
        };
    }

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("checklist.json");
    let tmp = parent.join(format!(".{file_name}.tmp.{}", Uuid::new_v4()));

    let write_result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return WriteOutcome::Failed(format!("写入临时文件失败: {error}"));
    }

    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return WriteOutcome::Failed(format!("替换清单文件失败: {error}"));
    }

    // rename 已发生：新文件身份就是后续写的基线（即使下面目录 sync 失败）。
    let stamp = FileStamp::capture(path).ok().flatten();

    // rename 之后：不能再把失败当"没写过"。
    if fail_dir_sync {
        return WriteOutcome::CommittedDirSyncFailed {
            error: "注入的目录同步失败（测试）".into(),
            stamp,
        };
    }
    #[cfg(unix)]
    {
        match std::fs::File::open(parent) {
            Ok(directory) => match directory.sync_all() {
                Ok(()) => WriteOutcome::Committed(stamp),
                Err(error) => WriteOutcome::CommittedDirSyncFailed {
                    error: format!("目录 sync_all 失败: {error}"),
                    stamp,
                },
            },
            Err(error) => WriteOutcome::CommittedDirSyncFailed {
                error: format!("打开父目录失败: {error}"),
                stamp,
            },
        }
    }
    #[cfg(not(unix))]
    {
        let _ = parent;
        WriteOutcome::Committed(stamp)
    }
}

fn lookup_idem(
    state: &StoreState,
    session_id: &str,
    key_digest: &str,
    fingerprint: &str,
) -> IdemLookup {
    let Some(records) = state.idempotency.get(session_id) else {
        return IdemLookup::Miss;
    };
    let Some(record) = records
        .iter()
        .find(|record| record.key_digest == key_digest)
    else {
        return IdemLookup::Miss;
    };
    if record.epoch != state.epoch {
        // reset_session 之后旧结果不能再声称成功（也不复活已清空的数据）。
        return IdemLookup::StaleEpoch;
    }
    if record.fingerprint == fingerprint {
        IdemLookup::Match(record.outcome.clone())
    } else {
        IdemLookup::DifferentPayload
    }
}

fn record_idem(
    state: &mut StoreState,
    session_id: &str,
    key_digest: &str,
    fingerprint: &str,
    epoch: &str,
    outcome: IdemOutcome,
) {
    let records = state.idempotency.entry(session_id.to_string()).or_default();
    if let Some(position) = records
        .iter()
        .position(|record| record.key_digest == key_digest)
    {
        records.remove(position);
    }
    records.push(IdemRecord {
        key_digest: key_digest.to_string(),
        fingerprint: fingerprint.to_string(),
        epoch: epoch.to_string(),
        outcome,
    });
    while records.len() > model::MAX_IDEM_RECORDS {
        records.remove(0);
    }
}

// ── 锁辅助（中毒不 panic：脱敏/状态路径必须在任何情况下安全返回）──────

fn read_lock<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write_lock<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
