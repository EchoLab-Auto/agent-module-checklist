//! Checklist tool: maintain multi-step task checklists for the agent.
//!
//! 状态按**会话**隔离，并且可以持久化到每个 persona 的一份 JSON 文件
//! （[`ChecklistStore::open`]）。工具本身只做参数 > store 的转发：
//! 版本检查、幂等、原子写、脱敏全在 [`ChecklistStore`] 里。
//!
//! ## 协议（2026-11 第二版）
//!
//! - 结构化快照：
//!   `{lists:[{id,name,archived,items:[{id,text,done}],done,total}], epoch, revision}`；
//! - `epoch` 是 UUID（文件级），`revision` 每次成功修改 +1；
//! - 所有写都可带 `expected_epoch` / `expected_revision`（提供即校验）；
//! - 稳定 id 是 UUID：`list_id` / `item_id` 修改不依赖位置；
//! - 旧式 `add` / `check` / `uncheck` / `remove` / `reset` 临时兼容，
//!   其中**按位置修改**必须同时带 `expected_epoch` 和 `expected_revision`，否则明确报错。
//!
//! 状态广播：本模块**不再**向宿主注册"状态 key → 宿主事件"的绑定。进程外
//! 形态自己经 `Emit("checklist.updated", {session_id, team_id, state})` 主动
//! 广播；面板订阅的是这个模块事件名，不是宿主的具名事件。`state_key()` 仍
//! 保留给执行链判"这次调用产生了可见状态"。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use echo_defs::registry::{Tool, ToolError};

pub use crate::store::ChecklistStore;


/// 清单存储加载失败时注册的同名工具：**显式失败**，不伪造空清单。
///
/// 为什么不注册 [`ChecklistTool::new`]：内存 store 会成功执行 `add` 等写操作，
/// 下一次（修好文件后）重新打开时，运行期内存里的"成功"没有落盘；更糟的是
/// 若实现选择"重新打开并覆写"，会把用户原文件盖成空清单。这里让每次调用
/// 都返回同一条可排查错误，面板/模型都能看见"清单不可用"。
pub struct ChecklistUnavailableTool {
    error: Arc<str>,
}

impl ChecklistUnavailableTool {
    pub fn new(error: impl Into<String>) -> Self {
        Self {
            error: error.into().into(),
        }
    }
}

#[async_trait]
impl Tool for ChecklistUnavailableTool {
    fn name(&self) -> &str {
        "checklist"
    }

    fn description(&self) -> &str {
        "任务清单当前不可用：持久存储加载失败（原文件已保留，未以空清单覆盖）。请检查 Core 日志中的清单存储错误。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "operation": {"type": "string", "description": "清单操作（当前不可用）"}
            }
        })
    }

    async fn execute(&self, _arguments: Value) -> Result<String, ToolError> {
        Err(ToolError::Execution(format!(
            "任务清单不可用：{}（未写入任何数据）",
            self.error
        )))
    }

    fn snapshot(&self) -> Option<Value> {
        None
    }

    fn snapshot_for_session(&self, _session_id: &str) -> Option<Value> {
        None
    }

    // 不声明 state_key：失败调用没有"新状态"可广播。
}

/// 会话缺失时的兜底键（联邦远程调用 / 直接 registry 执行没有会话上下文；
/// 它们共享一份"全局"清单——本地会话均有注入的 `__session_id`）。
const FALLBACK_SESSION: &str = "__global";

/// 清单工具。`new()` 是内存版（单测 / 未接线），生产用
/// [`ChecklistTool::with_store`] 注入共享 store。
#[derive(Clone)]
pub struct ChecklistTool {
    store: Arc<ChecklistStore>,
}

impl std::fmt::Debug for ChecklistTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChecklistTool")
            .field("store", &self.store)
            .finish()
    }
}

impl Default for ChecklistTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ChecklistTool {
    /// 内存 store（测试用）：进程内状态，不落盘。
    pub fn new() -> Self {
        Self::with_store(Arc::new(ChecklistStore::memory()))
    }

    /// 注入共享 store（接线方从外部持有 `Arc<ChecklistStore>`）。
    pub fn with_store(store: Arc<ChecklistStore>) -> Self {
        Self { store }
    }

    /// 取回共享 store（接线方 / 命令域需要同一份状态时）。
    pub fn store(&self) -> Arc<ChecklistStore> {
        Arc::clone(&self.store)
    }

    /// 本次调用的会话键。
    ///
    /// **优先取调用上下文**（`Tool::execute_in` 递进来的 `cx.session_id`）；
    /// 直接经注册表执行（联邦远程调用 / 面板调试 / 单测）时没有上下文，
    /// 那时仍认历史上的 `__session_id` 参数——但模型永远看不到这个键（宿主
    /// 不再往参数里塞它），所以它不是契约的一部分，只是兼容退路。
    fn session_key(cx: Option<&echo_defs::tool::ToolContext>, arguments: &Value) -> String {
        let from_context = cx
            .map(|cx| cx.session_id.as_str())
            .filter(|id| !id.is_empty());
        from_context
            .or_else(|| {
                arguments
                    .get("__session_id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or(FALLBACK_SESSION)
            .to_string()
    }

    /// 某会话的清单快照（权威空：未知会话也返回 Some，epoch/revision 为当前值）。
    pub fn snapshot_of(&self, session_id: &str) -> Option<Value> {
        self.store.snapshot(session_id)
    }

    /// 全量调试快照（所有会话，带 `session` 来源字段）。
    pub fn state_snapshot(&self) -> Option<Value> {
        Some(self.store.full_snapshot())
    }

    /// 限额摘要（只活动清单未完成项，约 4000 字符；不是指令）。
    pub fn summary_of(&self, session_id: &str) -> Option<String> {
        self.store.summary(session_id)
    }

    /// 两个入口的共同实现。
    async fn apply(
        &self,
        cx: Option<&echo_defs::tool::ToolContext>,
        arguments: Value,
    ) -> Result<String, ToolError> {
        let session = Self::session_key(cx, &arguments);
        // 写路径的取消闸门在 store 内（拿到写门之后、读状态之前检查）：
        // 只把 turn 的取消信号转发进去，解析/会话键仍由这里决定。
        let cancel = cx.and_then(|cx| cx.cancel.clone());
        self.store
            .apply_with_cancel(&session, arguments, cancel)
            .await
    }
}

#[async_trait]
impl Tool for ChecklistTool {
    fn name(&self) -> &str {
        "checklist"
    }

    fn description(&self) -> &str {
        "管理任务检查清单（按会话隔离、可持久化、带 epoch/revision 版本检查）。\
操作: list 枚举所有清单(带 name/list_id 时返回该清单完整内容), get 取清单完整数据, \
create 创建清单(可带 items 字符串数组), add_items 添加多项, update_item 按 item_id 修改文本/完成状态, \
remove_item 按 item_id 删除项, archive 归档, delete 删除清单, reset_session 清空本会话并推进 epoch, \
batch 原子批量写(需要 request_id 做幂等)。\
所有写可带 expected_epoch/expected_revision；提供即校验，冲突会明确报错。\
旧式 add/check/uncheck/remove/reset 仍可用；任何按 index 的位置修改（含 batch 子操作）必须同时带 expected_epoch 和 expected_revision。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": [
                        "list", "get", "create", "add_items", "update_item", "remove_item",
                        "archive", "delete", "reset_session", "batch",
                        "add", "check", "uncheck", "remove", "reset"
                    ],
                    "description": "要执行的操作"
                },
                "name": {
                    "type": "string",
                    "description": "清单名称（旧接口/按名寻址；默认 default）"
                },
                "list_id": {
                    "type": "string",
                    "description": "清单稳定 id（UUID；优先于 name）"
                },
                "item_id": {
                    "type": "string",
                    "description": "清单项稳定 id（UUID；update_item/remove_item 用）"
                },
                "item": {
                    "type": "string",
                    "description": "单项文本（add/add_items 的便捷写法）"
                },
                "items": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "多项文本（create/add_items）"
                },
                "index": {
                    "type": "integer",
                    "description": "清单项序号，从 1 开始（旧式 check/uncheck/remove 或按位置修改）"
                },
                "text": {
                    "type": "string",
                    "description": "update_item 的新文本"
                },
                "done": {
                    "type": "boolean",
                    "description": "update_item 的完成状态"
                },
                "archived": {
                    "type": "boolean",
                    "description": "archive 操作的目标状态，默认 true"
                },
                "expected_epoch": {
                    "type": "string",
                    "description": "期望的 epoch（UUID）；不匹配则拒绝写入"
                },
                "expected_revision": {
                    "type": "integer",
                    "description": "期望的 revision；不匹配则拒绝写入"
                },
                "request_id": {
                    "type": "string",
                    "description": "幂等键（字母数字及 -_.:，同 key 同 payload 返回同一结果）"
                },
                "operations": {
                    "type": "array",
                    "items": {"type": "object"},
                    "description": "batch 的子操作数组（只支持写操作，最多 64 个）"
                }
            },
            "required": ["operation"]
        })
    }

    async fn execute(&self, arguments: Value) -> Result<String, ToolError> {
        self.apply(None, arguments).await
    }

    /// 带上下文的执行：会话来自 [`ToolContext`]，不再依赖魔术参数。
    async fn execute_in(
        &self,
        cx: &echo_defs::tool::ToolContext,
        arguments: Value,
    ) -> Result<echo_defs::tool::ToolResult, ToolError> {
        self.apply(Some(cx), arguments)
            .await
            .map(echo_defs::tool::ToolResult::text)
    }

    fn snapshot(&self) -> Option<Value> {
        self.state_snapshot()
    }

    fn snapshot_for_session(&self, session_id: &str) -> Option<Value> {
        self.snapshot_of(session_id)
    }

    /// 快照里的机器字段：稳定 id / 文件级 epoch / 会话键。
    ///
    /// 这些值是 UUID 或宿主生成的会话键，不是用户内容；注册表出口脱敏
    /// 必须保留它们（`update_item` / `remove_item` 靠 `item_id` 寻址，
    /// 面板靠 `id` / `epoch` 做版本对账）。`name` / `text` / `summary` /
    /// `display` 等用户内容**不在**此列，仍逐字符串脱敏。
    ///
    /// 键名是常量 schema（见快照形状注释），用户内容只进值、不进键。
    fn snapshot_machine_fields(&self) -> &'static [&'static str] {
        &["id", "list_id", "item_id", "epoch", "session", "session_id"]
    }

    /// 声明"我有可见状态"：执行链在结果落盘后广播它（键与面板协议一致）。
    /// 此前这是执行链里的 `if call.name == "checklist"`。
    fn state_key(&self) -> Option<&'static str> {
        Some("checklist")
    }
}

// 测试模块沿用原布局：`tests.rs` 里写的是 `super::model::…` / `super::store::…`，
// 而 `mod model` / `mod ops` / `mod store` 现在挂在 crate 根（`lib.rs`）。
// 这三个别名让测试的 `use super::*` 仍能看见它们——搬迁不是重写测试。
#[cfg(test)]
use crate::{model, ops, store};

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

/// 注册表出口脱敏 × 清单快照的集成回归（机器字段 + UUID 形态秘密）。
///
/// 单独放 inline（不改 `checklist/tests.rs`）：验证的是
/// `snapshot_machine_fields` 声明与注册表脱敏的配合，不是 store/ops 的
/// 内部行为。
#[cfg(test)]
mod snapshot_machine_field_tests {
    use super::*;
    use echo_defs::registry::ToolRegistry;
    use echo_defs::sanitize::Redactor;
    use echo_test_utils::FakeRedactor;

    #[tokio::test]
    async fn registry_keeps_checklist_machine_ids_but_redacts_uuid_shaped_secret_text() {
        const SECRET_UUID: &str = "550e8400-e29b-41d4-a716-446655440000";
        const SESSION: &str = "local:tui::local_user";

        let store = Arc::new(ChecklistStore::memory());
        store
            .apply(
                SESSION,
                json!({
                    "operation": "create",
                    "name": format!("plan-{SECRET_UUID}"),
                    "items": [format!("rotate {SECRET_UUID}")],
                }),
            )
            .await
            .expect("create checklist");

        // 先记录机器字段原值（id 是 UUID，epoch 也是 UUID）。
        let before = store.snapshot(SESSION).expect("store snapshot");
        let list_id = before["lists"][0]["id"]
            .as_str()
            .expect("list id")
            .to_owned();
        let item_id = before["lists"][0]["items"][0]["id"]
            .as_str()
            .expect("item id")
            .to_owned();
        let epoch = before["epoch"].as_str().expect("epoch").to_owned();
        // store 未注入 redactor：原始快照里秘密仍在，证明下面的脱敏来自
        // 注册表出口（而不是 store 自己提前洗掉了）。
        assert!(
            before["lists"][0]["items"][0]["text"]
                .as_str()
                .expect("raw text")
                .contains(SECRET_UUID),
            "测试前提：store 原始快照应保留秘密文本"
        );

        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(ChecklistTool::with_store(Arc::clone(&store))));
        let redactor = Arc::new(FakeRedactor::new());
        redactor.register_secret(SECRET_UUID, "uuid-secret");
        registry.set_redactor(Some(redactor));

        let snapshot = registry
            .snapshot_for_session("checklist", SESSION)
            .expect("registry snapshot");

        // 机器字段原样保留：UUID 形态不是"秘密形态"的豁免依据。
        assert_eq!(snapshot["lists"][0]["id"], list_id);
        assert_eq!(snapshot["lists"][0]["items"][0]["id"], item_id);
        assert_eq!(snapshot["epoch"], epoch);

        // 用户内容仍强制脱敏：name / text 里的 UUID 形态秘密必须消失。
        let name = snapshot["lists"][0]["name"].as_str().expect("name");
        let text = snapshot["lists"][0]["items"][0]["text"]
            .as_str()
            .expect("text");
        assert!(!name.contains(SECRET_UUID), "name leaked: {name}");
        assert!(!text.contains(SECRET_UUID), "text leaked: {text}");
        assert!(name.contains("【已隐藏:uuid-secret】"), "name: {name}");
        assert!(text.contains("【已隐藏:uuid-secret】"), "text: {text}");

        // 全量快照里的 session 来源字段同样是机器字段。
        let full = registry.snapshot("checklist").expect("full snapshot");
        assert_eq!(full["sessions"][0]["session"], SESSION);
        assert_eq!(full["sessions"][0]["lists"][0]["id"], list_id);
        assert_eq!(full["epoch"], epoch);
    }
}
