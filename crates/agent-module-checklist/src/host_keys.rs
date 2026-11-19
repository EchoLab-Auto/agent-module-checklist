//! 宿主接缝的**键**与 trait（组合根注册，模块消费）。
//!
//! ## 为什么这里也有一个 trait
//!
//! 清单命令域需要两件宿主能力：
//!
//! | 能力 | 为什么在宿主 |
//! |---|---|
//! | [`ChecklistCommandHost::resolve_registry`] | "这个 persona 是否存在/在运行"是宿主的事实 |
//! | [`ChecklistCommandHost::emit`] | 面板事件出口（`BackendEvent`） |
//!
//! 这两件原先由 `agent-module-tools` 的 `ToolsCommandHost` 兼着提供——那是
//! 清单与工具集同处一个 crate 的副产品。模块拆开之后，两个 crate 互不依赖
//! （`scripts/deps-policy.txt` 纪律 4），所以清单声明自己的最小接缝，宿主
//! 侧的实现同时满足两边（同一份 `emit` / `resolve_registry`）。
//!
//! 键名一旦发布就是部署契约：`module.toml` 的 `requires` 与本文件必须同名。

use std::sync::Arc;

use agent_module_management::protocol::BackendEvent;
use echo_context::ServiceKey;
use echo_defs::registry::ToolRegistry;

/// 命令域宿主接缝（`module.toml` 的 `requires` 同名）。
pub const COMMAND_HOST: &str = "echo-agent.host.checklist.commands";

/// 工具集构造契约的服务键（`module.toml` 的 `provides` 同名）。
pub const TOOLS_PROVIDER: &str = "echo-agent.checklist.tools";

/// 清单命令域需要的两件宿主能力。
pub trait ChecklistCommandHost: Send + Sync {
    /// 解析**指定人格**的工具注册表（用于校验人格存在）。
    ///
    /// 默认实现 = 明确拒绝：没有证据证明该人格存在时，查询绝不能回落到
    /// "当前/默认注册表"——那会把 A 人格的清单显示成 B 人格的。
    fn resolve_registry(&self, team: &str) -> Result<Arc<ToolRegistry>, String> {
        Err(format!("智能体 {team} 不存在或未运行"))
    }

    /// 事件出口（面板投影）。
    fn emit(&self, event: BackendEvent);
}

/// 命令域宿主接缝的强类型键。
pub fn command_host_key() -> ServiceKey<Arc<dyn ChecklistCommandHost>> {
    ServiceKey::new(COMMAND_HOST)
}
