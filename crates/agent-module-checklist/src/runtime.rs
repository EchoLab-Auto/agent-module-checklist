//! 清单运行时：**per-persona `ChecklistStore` 的长期所有者**（2026-11）。
//!
//! ## 为什么需要这一层
//!
//! 持久化清单是**人格级真值**：同一人格的所有会话共用一份文件，人格重建
//! （禁用/启用、配置保存、Core 内重新装配）必须复用同一份 store，而不是
//! 让每个 `Agent` 各自打开一次——那会造出两代写者（旧 Agent 退场前还在
//! 后台提交，新 Agent 又打开同一文件）。
//!
//! `ChecklistRuntime` 因此**不属于任何 Agent**：组合根在启动时构造**一个
//! 明确实例**（不是进程级 global 槽），make_agent 按 persona 索取 store；
//! Agent 退场只丢自己的 `Arc`，store 仍由 runtime 持有。
//!
//! ## 路径
//!
//! 存储根由组合根给出：`<config 文件所在目录>/checklists/`，每个 persona
//! 一个 `<safe-id>.json`。路径规则只在这里定义一次（Core 不拼文件名）。
//!
//! ## 加载失败：显式失败，绝不“空 store 覆盖”
//!
//! `ChecklistStore::open` 对损坏/未知版本/超限文件返回 `Err`，且**不写盘**。
//! 本运行时把错误按 persona 缓存起来：
//!
//! - [`ChecklistRuntime::store_for`] 永远返回同一个错误（不重试、不回退内存）；
//! - 组合根据此注册 `ChecklistUnavailableTool`（同名工具，调用必失败）；
//! - 补拉/摘要路径遇到错误时**跳过并发警告**，不会发一份空快照把面板旧
//!   状态清掉，也不会给模型注入“去更新清单”的假任务。
//!
//! ## 摘要与工具门控
//!
//! 摘要文本由本模块的 `ModuleTextBlock` 贡献（见 `module.rs`），内容从
//! [`ChecklistRuntime::summary`] 按 context 里的 `team_id` / `session_id`
//! 读取。工具被禁用时（persona 名单 / 全局禁用 / 模块白名单），Core 注入的
//! 门控探针会让 `summary` 返回 `None`：**绝不要求模型调用一个不可用的工具**。
//! 只读查询（面板补拉）不经过这道门——工具关闭后仍允许观察既有清单。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use agent_module_management::protocol::{BackendEvent, ChecklistSnapshotEntry};
use echo_defs::sanitize::Redactor;

use crate::store::ChecklistStore;

/// 宿主服务键：模块 `mount` 时按它解析清单运行时（Core 注册）。
pub const RUNTIME_KEY: &str = "echo-agent.host.tools.checklist";

/// 模块文本块 key（提示词面板的“任务清单摘要”）。
pub const TEXT_BLOCK_KEY: &str = "tools.builtin.checklist";

/// 强类型服务键（与 [`RUNTIME_KEY`] 同源）：模块 `mount` / 组合根注册共用。
pub fn runtime_key() -> echo_context::ServiceKey<Arc<ChecklistRuntime>> {
    echo_context::ServiceKey::new(RUNTIME_KEY)
}

/// 工具门控探针：`(team_id, tool_name) -> 该工具当前对该人格是否可用`。
///
/// 由组合根注入（它同时看得见人格档案、全局禁用策略与模块白名单）。
/// 模块侧只把它当事实来源，不自己重算一份名单。
pub type ToolGate = Arc<dyn Fn(&str, &str) -> bool + Send + Sync>;

enum StoreSource {
    /// 组合根给出的持久化根 + 脱敏器。
    Disk {
        root: PathBuf,
        redactor: Option<Arc<dyn Redactor>>,
    },
    /// 进程内（测试 / 嵌入式用法）：绝不回退到这里替代加载失败的磁盘 store。
    Memory,
}

enum StoreSlot {
    Ready(Arc<ChecklistStore>),
    /// 加载失败的**冻结**原因（同一 persona 后续调用返回同一错误）。
    Failed(String),
}

/// per-persona 持久清单 store 的运行时持有者。
///
/// 线程安全：内部用 `std::sync::Mutex`（只在同步装配/查询路径上短暂持锁，
/// 不在锁内 await——`ChecklistStore::apply` 自己负责后台提交）。
pub struct ChecklistRuntime {
    source: StoreSource,
    stores: Mutex<HashMap<String, StoreSlot>>,
    gate: RwLock<Option<ToolGate>>,
}

impl std::fmt::Debug for ChecklistRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let loaded = self
            .stores
            .lock()
            .map(|stores| stores.len())
            .unwrap_or_default();
        f.debug_struct("ChecklistRuntime")
            .field("root", &self.root())
            .field("personas", &loaded)
            .finish_non_exhaustive()
    }
}

impl ChecklistRuntime {
    /// 持久化运行时：根目录下的 `<safe-persona>.json`。
    ///
    /// **不创建目录、不读文件**——真正的打开发生在第一次
    /// [`store_for`](Self::store_for)（启动期按人格装配时）。
    pub fn new(root: PathBuf, redactor: Option<Arc<dyn Redactor>>) -> Self {
        Self {
            source: StoreSource::Disk { root, redactor },
            stores: Mutex::new(HashMap::new()),
            gate: RwLock::new(None),
        }
    }

    /// 进程内运行时（测试 / 嵌入式用法）。
    ///
    /// 明确区别于磁盘加载失败：生产路径**只**用 [`new`](Self::new)，加载
    /// 失败时由组合根注册失败工具，而不是构造一个内存 runtime 顶上。
    pub fn memory() -> Self {
        Self {
            source: StoreSource::Memory,
            stores: Mutex::new(HashMap::new()),
            gate: RwLock::new(None),
        }
    }

    /// 持久化根（内存运行时为 `None`）。
    pub fn root(&self) -> Option<&Path> {
        match &self.source {
            StoreSource::Disk { root, .. } => Some(root),
            StoreSource::Memory => None,
        }
    }

    /// 取某人格的 store；同一人格永远得到**同一个 `Arc`**（人格重建复用）。
    ///
    /// 加载失败时错误按人格缓存：不重试、不回退空 store、不触碰原文件。
    /// 非法人格 id（空 / 超过 [`MAX_PERSONA_ID_BYTES`]）在进入缓存前直接
    /// 返回错误：不建目录、不写盘，内存后端同样拒绝。
    pub fn store_for(&self, team_id: &str) -> Result<Arc<ChecklistStore>, String> {
        // 路径规则同时定义 id 的合法性：先校验，避免非法 id 进入 stores
        // 缓存，或走到 ChecklistStore::open 才由文件系统报错。
        if let Err(error) = safe_file_stem(team_id) {
            tracing::error!(
                id_bytes = team_id.len(),
                %error,
                "checklist persona id rejected; tool disabled for this persona"
            );
            return Err(error);
        }
        let mut stores = self
            .stores
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(slot) = stores.get(team_id) {
            return match slot {
                StoreSlot::Ready(store) => Ok(Arc::clone(store)),
                StoreSlot::Failed(error) => Err(error.clone()),
            };
        }

        let opened = match &self.source {
            StoreSource::Memory => Ok(ChecklistStore::memory()),
            StoreSource::Disk { root, redactor } => {
                let path = store_path(root, team_id)?;
                // `open` 对损坏 / 未知版本 / 超限文件返回 Err 且不写盘；
                // 首次使用（文件不存在）是空状态，写盘发生在第一次成功 apply。
                ChecklistStore::open(path.clone(), redactor.clone()).map_err(|error| {
                    format!(
                        "清单存储加载失败（{}）：{error}；已保留原文件，拒绝以空清单覆盖",
                        path.display()
                    )
                })
            }
        };

        match opened {
            Ok(store) => {
                let store = Arc::new(store);
                stores.insert(team_id.to_string(), StoreSlot::Ready(Arc::clone(&store)));
                Ok(store)
            }
            Err(error) => {
                tracing::error!(team = %team_id, %error, "checklist store load failed; tool disabled for this persona");
                stores.insert(team_id.to_string(), StoreSlot::Failed(error.clone()));
                Err(error)
            }
        }
    }

    /// 某人格的**一次一致读**全量快照：文件级版本（`epoch`/`revision`）与
    /// `states` 来自同一次 [`ChecklistStore::full_snapshot`]（内部单次
    /// `read_view`），**不会**把两个时刻的 meta 与 states 拼在一起。
    ///
    /// 空 states 也带版本（新文件 = 新 epoch + revision 0）：面板据此判断
    /// “这份空 full 比手上的 live 更新还是更旧”，旧空 full 不得清掉新 live。
    /// 形状异常（缺字段 / 类型不对 / 条目缺 `session`）返回明确 `Err`：
    /// 宁可让调用方显式失败，也不发布一份“看起来权威”的空快照。
    pub fn full_snapshot(&self, team_id: &str) -> Result<ChecklistFullSnapshot, String> {
        let store = self.store_for(team_id)?;
        parse_full_snapshot(team_id, store.full_snapshot())
    }

    /// 某人格全部会话的权威状态表（空表也合法）。
    ///
    /// 只是 [`full_snapshot`](Self::full_snapshot) 的 `states` 切片视图；
    /// 要发快照事件请走 [`snapshot_event`](Self::snapshot_event)（版本在事件
    /// 顶层，只拿 states 会丢掉判新旧所需的版本）。
    pub fn snapshots(&self, team_id: &str) -> Result<Vec<ChecklistSnapshotEntry>, String> {
        Ok(self.full_snapshot(team_id)?.states)
    }

    /// 某人格的权威快照事件（`RequestState` 启动补全用）。
    ///
    /// **失败 = 显式错误事件**，既不是 `None` 也不空快照：空快照会清掉面板
    /// 旧投影并掩盖“文件坏了”。签名保持 `Option`（调用侧 `filter_map`
    /// 不必为错误单独分支），当前实现的结果集是「成功快照」与「显式错误」。
    pub fn snapshot_event(&self, team_id: &str) -> Option<BackendEvent> {
        match self.full_snapshot(team_id) {
            Ok(full) => Some(BackendEvent::ChecklistsSnapshot {
                team_id: team_id.to_string(),
                epoch: full.epoch,
                revision: full.revision,
                states: full.states,
            }),
            Err(error) => {
                tracing::error!(
                    team = %team_id, %error,
                    "checklist snapshot failed; emitting explicit error event"
                );
                Some(BackendEvent::Error {
                    session_id: None,
                    message: format!("智能体 {team_id} 的清单快照不可用：{error}"),
                })
            }
        }
    }

    /// 某人格 / 某会话的限额摘要；工具当前不可用时返回 `None`。
    ///
    /// 门控探针由 Core 注入，直接读取目标 Agent 的实际工具注册表；这里
    /// 只消费事实，不重算配置策略。没有探针（单测 / 嵌入式）时按“可用”处理。
    pub fn summary(&self, team_id: &str, session_id: &str) -> Option<String> {
        if !self.tool_enabled(team_id, "checklist") {
            return None;
        }
        let store = self.store_for(team_id).ok()?;
        store
            .summary(session_id)
            .filter(|text| !text.trim().is_empty())
    }

    /// `ModuleTextBlock` 的文本来源：从 context 里取 `team_id` / `session_id`。
    /// ID 是精确机器键，不 trim；否则空白后缀会读成另一人格/会话。
    /// 缺任一键 = 没有摘要，避免无会话路径注入会话私有数据。
    pub fn summary_block(&self, context: &serde_json::Value) -> Option<String> {
        let team_id = context
            .get("team_id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.is_empty())?;
        let session_id = context
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.is_empty())?;
        self.summary(team_id, session_id)
    }

    /// 注入工具门控探针（组合根启动期一次；运行期不再变）。
    pub fn set_tool_gate(&self, gate: ToolGate) {
        *self
            .gate
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(gate);
    }

    /// 某工具当前对某人格是否可用（没有探针 = 可用）。
    pub fn tool_enabled(&self, team_id: &str, tool_name: &str) -> bool {
        self.gate
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|gate| gate(team_id, tool_name))
            .unwrap_or(true)
    }
}

/// 一次一致读的全量快照：文件级 `epoch` / `revision` + 各会话权威状态。
///
/// 三个字段**必须**来自同一次存储读取（见
/// [`ChecklistRuntime::full_snapshot`]）：分开读 meta 与 states 会在两次读
/// 之间夹进一次修改，让“新版本 + 旧内容”或“旧版本 + 新内容”冒充权威。
#[derive(Debug, Clone, PartialEq)]
pub struct ChecklistFullSnapshot {
    /// 文件级 epoch（UUID）：`reset_session` 时推进。
    pub epoch: String,
    /// 文件级 revision：每次成功修改 +1，跨 reset / 重载保持单调。
    pub revision: u64,
    /// 各会话的权威状态（`{session_id, state}`，state 内仍自带 epoch/revision）。
    pub states: Vec<ChecklistSnapshotEntry>,
}

/// 把 `ChecklistStore::full_snapshot()` 的线形状解析成强类型快照。
///
/// 线形状：`{sessions:[{session, lists, epoch, revision}], epoch, revision}`。
/// 条目里的 `session` 只用于取会话 id，**不进** `state`（`state` 与
/// `ChecklistUpdated.state` 同口径：`{lists, epoch, revision}`）。
///
/// 任何形状异常都返回 `Err`（带智能体 id 与原因），绝不返回空快照：
/// “解析失败 = 没有清单”会让面板把故障当成权威清空。
fn parse_full_snapshot(
    team_id: &str,
    snapshot: serde_json::Value,
) -> Result<ChecklistFullSnapshot, String> {
    let serde_json::Value::Object(mut object) = snapshot else {
        return Err(format!(
            "清单全量快照不是 JSON 对象（智能体 {team_id}）：拒绝发布权威快照"
        ));
    };
    let epoch = object
        .remove("epoch")
        .and_then(|value| value.as_str().map(str::to_string))
        .filter(|epoch| !epoch.is_empty())
        .ok_or_else(|| format!("清单全量快照缺少非空字符串 epoch（智能体 {team_id}）"))?;
    let revision = object
        .remove("revision")
        .and_then(|value| value.as_u64())
        .ok_or_else(|| format!("清单全量快照缺少 u64 revision（智能体 {team_id}）"))?;
    let sessions = object
        .remove("sessions")
        .and_then(|value| match value {
            serde_json::Value::Array(sessions) => Some(sessions),
            _ => None,
        })
        .ok_or_else(|| format!("清单全量快照缺少 sessions 数组（智能体 {team_id}）"))?;

    let mut states = Vec::with_capacity(sessions.len());
    for (index, session) in sessions.into_iter().enumerate() {
        let serde_json::Value::Object(mut entry) = session else {
            return Err(format!(
                "清单全量快照的 sessions[{index}] 不是 JSON 对象（智能体 {team_id}）"
            ));
        };
        let session_id = entry
            .remove("session")
            .and_then(|value| value.as_str().map(str::to_string))
            .filter(|session_id| !session_id.is_empty())
            .ok_or_else(|| {
                format!("清单全量快照的 sessions[{index}] 缺少非空 session（智能体 {team_id}）")
            })?;
        states.push(ChecklistSnapshotEntry {
            session_id,
            state: serde_json::Value::Object(entry),
        });
    }
    Ok(ChecklistFullSnapshot {
        epoch,
        revision,
        states,
    })
}

/// 人格 id 的 UTF-8 字节数上限。
///
/// 文件名是 `p-<hex>`：120 字节 id 编码为 240 个十六进制字符，加前缀与
/// `.json` 共 247 字节，低于常见文件系统的 `NAME_MAX`（255）；超限 id 由
/// [`ChecklistRuntime::store_for`] 明确拒绝，不依赖配置层限制 id。
const MAX_PERSONA_ID_BYTES: usize = 120;

/// 持久化路径：`<root>/p-<hex>.json`；非法 id 返回错误（调用方不得落盘）。
fn store_path(root: &Path, team_id: &str) -> Result<PathBuf, String> {
    Ok(root.join(format!("{}.json", safe_file_stem(team_id)?)))
}

/// persona id → 安全文件名：**整个 id 的 UTF-8 字节统一 hex 编码**，前缀 `p-`。
///
/// 不做 `trim` 或字符替换：hex 对字节单射，`a` / `a `、`a/b` / `a_b` 必然
/// 得到不同文件名，正常名字与“转义后的名字”也不可能同名（跨人格状态串写
/// 比“文件名不好看”严重得多）。空 id 与超过 [`MAX_PERSONA_ID_BYTES`] 的 id
/// 返回明确错误，由 `store_for` 转成 `Err` 且不触碰磁盘。
fn safe_file_stem(team_id: &str) -> Result<String, String> {
    if team_id.is_empty() {
        return Err("人格 id 为空：拒绝映射清单文件（不写盘）".to_string());
    }
    if team_id.len() > MAX_PERSONA_ID_BYTES {
        return Err(format!(
            "人格 id 过长（{} 字节，上限 {} 字节）：拒绝映射清单文件（不写盘）",
            team_id.len(),
            MAX_PERSONA_ID_BYTES
        ));
    }
    let mut stem = String::with_capacity(2 + team_id.len() * 2);
    stem.push_str("p-");
    use std::fmt::Write as _;
    for byte in team_id.as_bytes() {
        write!(stem, "{byte:02x}").expect("写入 String 不会失败");
    }
    Ok(stem)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "echo-checklist-runtime-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn safe_file_stem_hex_encodes_ids_without_trim_or_alias() {
        assert_eq!(safe_file_stem("alix").unwrap(), "p-616c6978");
        assert_eq!(safe_file_stem("a").unwrap(), "p-61");
        assert_eq!(safe_file_stem("a ").unwrap(), "p-6120");
        assert_ne!(
            safe_file_stem("a/b").unwrap(),
            safe_file_stem("a_b").unwrap()
        );
        assert_eq!(safe_file_stem(" a").unwrap(), "p-2061");
        assert_eq!(safe_file_stem("a_b").unwrap(), "p-615f62");
        assert_eq!(safe_file_stem("é").unwrap(), "p-c3a9", "按 UTF-8 字节编码");
        assert_eq!(safe_file_stem("   ").unwrap(), "p-202020", "不 trim");
        assert!(!safe_file_stem("a/b").unwrap().contains('/'));
        assert!(safe_file_stem("").is_err(), "空 id 不是人格");
    }

    #[test]
    fn store_for_rejects_overlong_persona_id_without_touching_disk() {
        let root = temp_root("too-long");
        let runtime = ChecklistRuntime::new(root.clone(), None);
        let overlong = "x".repeat(MAX_PERSONA_ID_BYTES + 1);
        let error = runtime.store_for(&overlong).unwrap_err();
        assert!(error.contains("过长"), "{error}");
        assert_eq!(
            std::fs::read_dir(&root).unwrap().count(),
            0,
            "非法 id 绝不能在存储根留下文件"
        );

        // 边界值仍可用，且编码后的文件名低于 NAME_MAX。
        let store = runtime
            .store_for(&"x".repeat(MAX_PERSONA_ID_BYTES))
            .unwrap();
        let name = store.path().unwrap().file_name().unwrap().to_str().unwrap();
        assert_eq!(name.len(), 2 + MAX_PERSONA_ID_BYTES * 2 + ".json".len());
    }

    #[test]
    fn store_for_reuses_the_same_arc_for_persona_rebuilds() {
        let runtime = ChecklistRuntime::new(temp_root("reuse"), None);
        let first = runtime.store_for("alix").unwrap();
        let second = runtime.store_for("alix").unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_ne!(
            Arc::as_ptr(&first),
            Arc::as_ptr(&runtime.store_for("bob").unwrap())
        );
    }

    #[test]
    fn load_error_is_cached_and_never_replaced_by_an_empty_store() {
        let root = temp_root("corrupt");
        let path = store_path(&root, "alix").unwrap();
        std::fs::write(&path, b"not-json").unwrap();
        let before = std::fs::read(&path).unwrap();

        let runtime = ChecklistRuntime::new(root, None);
        let first = runtime.store_for("alix").unwrap_err();
        let second = runtime.store_for("alix").unwrap_err();
        assert_eq!(first, second, "同一 persona 必须返回同一冻结错误");
        assert!(first.contains("拒绝以空清单覆盖"));
        assert!(runtime.snapshots("alix").is_err());
        assert!(runtime.summary("alix", "s").is_none());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "加载失败绝不能改写原文件"
        );
    }

    #[tokio::test]
    async fn summary_respects_the_injected_tool_gate() {
        let runtime = ChecklistRuntime::memory();
        let store = runtime.store_for("alix").unwrap();
        store
            .apply(
                "s",
                serde_json::json!({"operation": "create", "name": "p", "items": ["a"]}),
            )
            .await
            .unwrap();
        assert!(store.summary("s").is_some());
        runtime.set_tool_gate(Arc::new(|team, tool| team != "alix" || tool != "checklist"));
        assert!(runtime.summary("alix", "s").is_none());
        assert!(runtime.summary("bob", "s").is_none()); // bob 无 store，同样 None
    }

    #[tokio::test]
    async fn summary_block_does_not_trim_persona_or_session_identity() {
        let runtime = ChecklistRuntime::memory();
        runtime
            .store_for("a")
            .unwrap()
            .apply(
                "s",
                serde_json::json!({"operation":"create","name":"plain","items":["one"]}),
            )
            .await
            .unwrap();
        runtime
            .store_for("a ")
            .unwrap()
            .apply(
                "s ",
                serde_json::json!({"operation":"create","name":"spaced","items":["two"]}),
            )
            .await
            .unwrap();
        let summary = runtime
            .summary_block(&serde_json::json!({"team_id":"a ","session_id":"s "}))
            .unwrap();
        assert!(summary.contains("spaced"));
        assert!(!summary.contains("plain"));
    }
    #[test]
    fn snapshot_event_is_authoritative_even_when_empty_and_carries_version() {
        let runtime = ChecklistRuntime::memory();
        let store = runtime.store_for("alix").unwrap();
        let event = runtime.snapshot_event("alix").expect("不存在的会话=空快照");
        match event {
            BackendEvent::ChecklistsSnapshot {
                team_id,
                epoch,
                revision,
                states,
                ..
            } => {
                assert_eq!(team_id, "alix");
                assert!(states.is_empty());
                assert_eq!(revision, 0, "空 store 的 revision 是 0，但仍然必须带版本");
                assert_eq!(
                    epoch,
                    store.full_snapshot()["epoch"].as_str().unwrap(),
                    "事件的 epoch 必须来自 store 的同一次全量快照"
                );
                assert!(!epoch.is_empty(), "空 states 也必须有 epoch（UUID）");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn snapshot_event_version_matches_the_same_store_read_as_states() {
        let runtime = ChecklistRuntime::memory();
        let store = runtime.store_for("alix").unwrap();
        store
            .apply(
                "s1",
                serde_json::json!({"operation": "create", "name": "p", "items": ["a", "b"]}),
            )
            .await
            .unwrap();
        let full = runtime.full_snapshot("alix").unwrap();
        let raw = store.full_snapshot();
        assert_eq!(full.epoch, raw["epoch"].as_str().unwrap());
        assert_eq!(full.revision, raw["revision"].as_u64().unwrap());
        assert!(full.revision > 0, "一次成功修改必须推进 revision");
        assert_eq!(full.states.len(), 1);
        let entry = &full.states[0];
        assert_eq!(entry.session_id, "s1", "会话 id 取自 sessions[].session");
        assert!(
            entry.state.get("session").is_none(),
            "state 与 ChecklistUpdated.state 同口径，不含 session 键"
        );
        assert_eq!(
            entry.state["revision"].as_u64(),
            Some(full.revision),
            "条目的版本与顶层版本来自同一次读取"
        );
        assert_eq!(entry.state["lists"][0]["name"], "p");

        // 事件与 full_snapshot 同源：版本、states 完全一致。
        match runtime.snapshot_event("alix").unwrap() {
            BackendEvent::ChecklistsSnapshot {
                team_id,
                epoch,
                revision,
                states,
                ..
            } => {
                assert_eq!(team_id, "alix");
                assert_eq!(epoch, full.epoch);
                assert_eq!(revision, full.revision);
                assert_eq!(states.len(), full.states.len());
                assert_eq!(states[0].session_id, "s1");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn broken_file_emits_explicit_error_event_not_an_authoritative_empty() {
        let root = temp_root("broken-event");
        let path = store_path(&root, "alix").unwrap();
        std::fs::write(&path, b"not-json").unwrap();
        let before = std::fs::read(&path).unwrap();

        let runtime = ChecklistRuntime::new(root, None);
        // `full_snapshot` / `snapshots` 走错误通道（调用方可据此回命令错误）。
        assert!(runtime.full_snapshot("alix").is_err());
        assert!(runtime.snapshots("alix").is_err());

        // 启动补全走显式错误事件：绝不发“看起来权威”的空快照。
        match runtime.snapshot_event("alix") {
            Some(BackendEvent::Error {
                session_id,
                message,
            }) => {
                assert!(session_id.is_none(), "人格级故障不是某个会话的错误");
                assert!(message.contains("alix"), "{message}");
                assert!(message.contains("不可用"), "{message}");
                assert!(
                    message.contains("拒绝以空清单覆盖"),
                    "错误必须可排查（带加载失败原因）: {message}"
                );
            }
            other => panic!("坏文件必须发显式 Error 事件，实际: {other:?}"),
        }
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "报错路径绝不能改写原文件"
        );
    }

    #[test]
    fn malformed_store_snapshot_shapes_are_rejected_explicitly() {
        let epoch = serde_json::json!("e");
        let revision = serde_json::json!(1);
        // 缺 epoch / 缺 revision / sessions 不是数组 / 条目缺 session：全部 Err。
        assert!(parse_full_snapshot(
            "alix",
            serde_json::json!({"revision": revision, "sessions": []})
        )
        .is_err());
        assert!(
            parse_full_snapshot("alix", serde_json::json!({"epoch": epoch, "sessions": []}))
                .is_err()
        );
        assert!(parse_full_snapshot(
            "alix",
            serde_json::json!({"epoch": "e", "revision": 1, "sessions": {}})
        )
        .is_err());
        assert!(parse_full_snapshot(
            "alix",
            serde_json::json!({"epoch": "e", "revision": 1, "sessions": [{"lists": []}]})
        )
        .is_err());
        assert!(parse_full_snapshot("alix", serde_json::json!([])).is_err());
        // 合法形状：顶层版本 + 条目去掉 session 后的 state。
        let parsed = parse_full_snapshot(
            "alix",
            serde_json::json!({
                "epoch": "e",
                "revision": 2,
                "sessions": [{"session": "s", "lists": [], "epoch": "e", "revision": 2}],
            }),
        )
        .unwrap();
        assert_eq!(parsed.epoch, "e");
        assert_eq!(parsed.revision, 2);
        assert_eq!(parsed.states[0].session_id, "s");
        assert_eq!(
            parsed.states[0].state,
            serde_json::json!({"lists": [], "epoch": "e", "revision": 2})
        );
    }
}
