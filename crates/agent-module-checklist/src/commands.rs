//! 清单查询命令域：`RequestChecklists`（2026-11 清单可靠化）。
//!
//! ## 为什么归本模块
//!
//! 清单的**真值**属于 `agent-module-tools`（`ChecklistStore` 与
//! `ChecklistRuntime`）。面板要“补拉权威快照”时，形状（哪些会话、每条
//! 状态、空表也是权威结果）只有本模块知道；宿主只提供两件事：
//!
//! | 宿主能力 | 为什么在宿主 |
//! |---|---|
//! | [`ChecklistCommandHost::resolve_registry`] | “这个 persona 是否存在”是运行中人格表的事实 |
//! | [`ChecklistCommandHost::emit`] | 面板事件出口 |
//!
//! ## 语义
//!
//! - `session_id = None` → [`BackendEvent::ChecklistsSnapshot`]：该人格全部
//!   会话（`states` 是**完整集合**：不在其中的会话视为无清单），**空 states
//!   也发**（权威“没有清单”）。`epoch` / `revision` 是**文件级**版本，与
//!   `states` 来自同一次存储读取（[`ChecklistRuntime::full_snapshot`]）：
//!   前端整体替换该人格的清单投影，再按顶层 `revision`（低 revision 的 full
//!   永不覆盖更新的 live）与每条 state 的 `epoch`/`revision` 拒绝旧快照。
//! - `session_id = Some(s)` → [`BackendEvent::ChecklistUpdated`]：单会话
//!   权威状态（同样包含空 lists，用于清掉旧投影）。
//! - `team_id` 必须精确存在：`resolve_registry` 失败直接回命令错误，
//!   **绝不**回落到“当前/默认人格”。
//! - 工具被禁用时仍允许查询（只读观察既有数据）；模型摘要另受门控，见
//!   `checklist_runtime.rs`。
//! - 存储加载失败 / 快照形状异常 → **命令错误**（带原因），绝不用空快照
//!   冒充“该人格没有清单”。

use std::sync::Arc;

use agent_module_management::protocol::{BackendCommand, BackendEvent};
use serde_json::Value;

use crate::runtime::ChecklistRuntime;
use crate::host_keys::ChecklistCommandHost;

/// 本命令域处理的名字。
pub const COMMANDS: [&str; 1] = ["RequestChecklists"];

/// `RequestChecklists` 的处理器。
pub struct ChecklistCommandHandler {
    runtime: Option<Arc<ChecklistRuntime>>,
    host: Arc<dyn ChecklistCommandHost>,
}

impl ChecklistCommandHandler {
    /// `runtime = None` 表示宿主没注册清单运行时（降级路径）：命令会明确
    /// 报错，而不是返回空快照冒充“没有清单”。
    pub fn new(runtime: Option<Arc<ChecklistRuntime>>, host: Arc<dyn ChecklistCommandHost>) -> Self {
        Self { runtime, host }
    }

    /// 处理一条命令；`None` = 不是本域。
    pub async fn handle(&self, name: &str, payload: Value) -> Option<Result<Value, String>> {
        if !COMMANDS.contains(&name) {
            return None;
        }
        let tagged = serde_json::json!({ name: payload });
        let cmd: BackendCommand = match serde_json::from_value(tagged) {
            Ok(cmd) => cmd,
            Err(error) => return Some(Err(format!("清单命令载荷解析失败（{name}）: {error}"))),
        };
        Some(self.apply(cmd).await.map(|()| Value::Null))
    }

    async fn apply(&self, cmd: BackendCommand) -> Result<(), String> {
        match cmd {
            BackendCommand::RequestChecklists {
                team_id,
                session_id,
            } => {
                let Some(runtime) = &self.runtime else {
                    return Err(format!(
                        "清单运行时未接线：无法读取智能体 {team_id} 的清单（未注册任何替代实现）"
                    ));
                };
                // 精确校验人格存在（宿主实现按运行中注册表查）；不存在即拒绝。
                self.host.resolve_registry(&team_id)?;

                match session_id {
                    None => {
                        // **一次一致读**：`epoch`/`revision` 与 `states` 来自
                        // 同一次 store 全量快照，绝不先读 meta 再读 states。
                        let full = runtime.full_snapshot(&team_id)?;
                        self.host.emit(BackendEvent::ChecklistsSnapshot {
                            team_id,
                            epoch: full.epoch,
                            revision: full.revision,
                            states: full.states,
                        });
                    }
                    Some(session_id) => {
                        let store = runtime.store_for(&team_id)?;
                        let state = store.snapshot(&session_id).ok_or_else(|| {
                            format!(
                                "智能体 {team_id} 的会话 {session_id} 清单快照不可用（存储未返回权威状态）"
                            )
                        })?;
                        self.host.emit(BackendEvent::ChecklistUpdated {
                            session_id,
                            state,
                            team_id: Some(team_id),
                        });
                    }
                }
                Ok(())
            }
            other => Err(format!(
                "清单命令域声明了 {} 但处理器没有对应分支",
                other.kind_name()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use echo_defs::registry::ToolRegistry;

    #[derive(Default)]
    struct FakeHost {
        /// 允许查询的人格（精确匹配；空 = 全都不存在）。
        known: Mutex<Vec<String>>,
        events: Mutex<Vec<BackendEvent>>,
    }

    impl FakeHost {
        fn with_teams(teams: &[&str]) -> Self {
            Self {
                known: Mutex::new(teams.iter().map(|t| t.to_string()).collect()),
                events: Mutex::new(Vec::new()),
            }
        }
    }

    impl ChecklistCommandHost for FakeHost {
        fn resolve_registry(&self, team: &str) -> Result<Arc<ToolRegistry>, String> {
            if self.known.lock().unwrap().iter().any(|id| id == team) {
                Ok(Arc::new(ToolRegistry::new()))
            } else {
                Err(format!("智能体 {team} 不存在或未运行"))
            }
        }

        fn emit(&self, event: BackendEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    async fn runtime_with_state() -> Arc<ChecklistRuntime> {
        let runtime = Arc::new(ChecklistRuntime::memory());
        let store = runtime.store_for("alix").unwrap();
        store
            .apply(
                "s1",
                serde_json::json!({"operation": "create", "name": "p", "items": ["a", "b"]}),
            )
            .await
            .unwrap();
        runtime
    }

    #[tokio::test]
    async fn whole_persona_emits_authoritative_snapshot() {
        let runtime = runtime_with_state().await;
        let host = Arc::new(FakeHost::with_teams(&["alix"]));
        let handler = ChecklistCommandHandler::new(Some(runtime.clone()), host.clone());
        handler
            .handle("RequestChecklists", serde_json::json!({"team_id": "alix"}))
            .await
            .expect("命令领域应认领")
            .unwrap();
        let events = host.events.lock().unwrap();
        let snapshot = events
            .iter()
            .find_map(|event| match event {
                BackendEvent::ChecklistsSnapshot {
                    team_id,
                    epoch,
                    revision,
                    states,
                    ..
                } => Some((team_id.clone(), epoch.clone(), *revision, states.clone())),
                _ => None,
            })
            .expect("必须发 ChecklistsSnapshot");
        let (team, epoch, revision, states) = snapshot;
        assert_eq!(team, "alix");
        assert!(!epoch.is_empty(), "顶层 epoch 必填（UUID）");
        assert!(revision > 0, "一次成功修改后 revision 必须推进");
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].session_id, "s1");
        assert_eq!(states[0].state["lists"][0]["name"], "p");
        // 命令域的版本与 runtime 的一次一致读全量快照逐字相同。
        let full = runtime.full_snapshot("alix").unwrap();
        assert_eq!(epoch, full.epoch);
        assert_eq!(revision, full.revision);
        assert_eq!(states[0].state["revision"].as_u64(), Some(revision));
    }

    #[tokio::test]
    async fn empty_persona_still_emits_an_empty_authoritative_snapshot() {
        let runtime = Arc::new(ChecklistRuntime::memory());
        let host = Arc::new(FakeHost::with_teams(&["alix"]));
        let handler = ChecklistCommandHandler::new(Some(runtime), host.clone());
        handler
            .handle("RequestChecklists", serde_json::json!({"team_id": "alix"}))
            .await
            .unwrap()
            .unwrap();
        let events = host.events.lock().unwrap();
        assert!(matches!(
            events.as_slice(),
            [BackendEvent::ChecklistsSnapshot { team_id, epoch, revision, states, .. }]
                if team_id == "alix"
                    && states.is_empty()
                    && !epoch.is_empty()
                    && *revision == 0
        ));
    }

    #[tokio::test]
    async fn broken_store_is_a_command_error_not_an_empty_snapshot() {
        let root = std::env::temp_dir().join(format!(
            "echo-checklist-commands-broken-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&root).unwrap();
        // 先按正常路径问一次 store 拿文件名（不写盘），再把它写坏：这样测试
        // 不依赖文件名编码的具体规则（该规则由 runtime 自己拥有）。
        let path = {
            let probe = ChecklistRuntime::new(root.clone(), None);
            probe
                .store_for("alix")
                .unwrap()
                .path()
                .unwrap()
                .to_path_buf()
        };
        std::fs::write(&path, b"not-json").unwrap();
        let runtime = Arc::new(ChecklistRuntime::new(root.clone(), None));
        let host = Arc::new(FakeHost::with_teams(&["alix"]));
        let handler = ChecklistCommandHandler::new(Some(runtime), host.clone());
        let error = handler
            .handle("RequestChecklists", serde_json::json!({"team_id": "alix"}))
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.contains("拒绝以空清单覆盖"), "{error}");
        assert!(
            host.events.lock().unwrap().is_empty(),
            "坏文件不得发（空）权威快照"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn specified_session_emits_checklist_updated_with_team() {
        let runtime = runtime_with_state().await;
        let host = Arc::new(FakeHost::with_teams(&["alix"]));
        let handler = ChecklistCommandHandler::new(Some(runtime), host.clone());
        handler
            .handle(
                "RequestChecklists",
                serde_json::json!({"team_id": "alix", "session_id": "s1"}),
            )
            .await
            .unwrap()
            .unwrap();
        let events = host.events.lock().unwrap();
        assert!(matches!(
            events.as_slice(),
            [BackendEvent::ChecklistUpdated { session_id, state, team_id: Some(team) }]
                if session_id == "s1" && team == "alix" && state["lists"][0]["name"] == "p"
        ));
    }

    #[tokio::test]
    async fn unknown_team_is_rejected_without_emitting() {
        let runtime = runtime_with_state().await;
        let host = Arc::new(FakeHost::with_teams(&["alix"]));
        let handler = ChecklistCommandHandler::new(Some(runtime), host.clone());
        let error = handler
            .handle("RequestChecklists", serde_json::json!({"team_id": "ghost"}))
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.contains("ghost"));
        assert!(host.events.lock().unwrap().is_empty(), "拒绝时不得发空快照");
    }

    #[tokio::test]
    async fn missing_runtime_is_explicit_not_an_empty_snapshot() {
        let host = Arc::new(FakeHost::with_teams(&["alix"]));
        let handler = ChecklistCommandHandler::new(None, host.clone());
        let error = handler
            .handle("RequestChecklists", serde_json::json!({"team_id": "alix"}))
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.contains("未接线"));
        assert!(host.events.lock().unwrap().is_empty());
    }
}
