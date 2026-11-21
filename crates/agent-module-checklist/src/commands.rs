//! 清单对外**方法**：`checklist.snapshot`（面板补拉权威快照）。
//!
//! ## 为什么归本模块
//!
//! 清单的真值在本模块——哪些会话有清单、空表是不是权威结果、`epoch` /
//! `revision` 与 `states` 是否来自同一次读。这些形状宿主**不再**认识：
//! 面板经通用调用通道（`InvokeModuleOp`）把请求送进来，本模块把结果作为
//! **返回值**交回去，宿主只做搬运，不解释。
//!
//! 这条路径取代了原先的 `RequestChecklists` 具名命令：那条命令是宿主协议里
//! 的一个变体，等于让 Core 的协议词汇表替模块定义业务接口；而且它的响应是
//! 一条宿主事件，宿主必须知道"清单长什么样"才能发出它。
//!
//! ## 形状与语义
//!
//! 返回值：
//!
//! ```json
//! {"team_id":"alix","epoch":"<uuid>","revision":7,
//!  "states":[{"session_id":"local:tui::u","state":{"lists":[],"epoch":"…","revision":7}}]}
//! ```
//!
//! - `states` 是**完整集合**：不在其中的会话视为没有清单；
//! - 空 `states` 也是权威结果（"这个人格没有任何清单"），调用方据此整体替换
//!   本地投影，而不是保留旧缓存；
//! - `epoch` / `revision` 是**文件级**版本，与 `states` 来自同一次存储读取
//!   （[`ChecklistRuntime::full_snapshot`]）——绝不先读 meta 再读 states；
//!   判新旧看 `revision`（跨 reset 单调递增），判"是不是同一条时间线"看 `epoch`。
//!
//! 失败（人格不存在 / 存储损坏 / 形状异常）→ `Err`（带原因）。**绝不用空
//! 快照冒充"没有清单"**：那会让面板把故障当成权威清空，然后把空的写回磁盘。

use serde_json::Value;

use crate::runtime::ChecklistRuntime;

/// 本模块对外声明的**方法名**（`Contribution::Command` 里的 `name`）。
///
/// 点分小写的模块词汇：宿主不认识它，面板经 `InvokeModuleOp.method` 点名调用。
pub const SNAPSHOT_METHOD: &str = "checklist.snapshot";

/// 权威快照载荷：版本与 `states` 来自同一次存储读取。
///
/// `team_id` 必须精确存在（`store_for` 会拒绝未注册人格），**绝不**回落到
/// "当前 / 默认人格"——那会把 A 人格的清单显示成 B 人格的。
pub fn snapshot_payload(runtime: &ChecklistRuntime, team_id: &str) -> Result<Value, String> {
    let full = runtime.full_snapshot(team_id)?;
    Ok(serde_json::json!({
        "team_id": team_id,
        "epoch": full.epoch,
        "revision": full.revision,
        "states": full.states,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_payload_carries_top_level_version_and_states() {
        let runtime = ChecklistRuntime::memory();
        // 空人格也是权威结果：必须带版本，不能糊成 `null`。
        let payload = snapshot_payload(&runtime, "alix").expect("空快照也是权威结果");
        assert_eq!(payload["team_id"], serde_json::json!("alix"));
        assert!(payload["epoch"].as_str().is_some_and(|e| !e.is_empty()));
        assert_eq!(payload["revision"], serde_json::json!(0));
        assert_eq!(payload["states"], serde_json::json!([]));
    }

    #[test]
    fn snapshot_payload_reports_missing_persona_instead_of_empty() {
        let runtime = ChecklistRuntime::memory();
        let error = snapshot_payload(&runtime, "").expect_err("空人格 id 必须显式失败");
        assert!(!error.is_empty());
    }
}
