//! # agent-module-checklist — 任务清单模块（`echo-agent.checklist` 的实现）
//!
//! 模型可直接调用的 `checklist` 工具，连同它的**持久化真值与观察面**：
//!
//! | 组成 | 位置 | 说明 |
//! |---|---|---|
//! | 工具 | [`tool::ChecklistTool`] | 参数 → store 的转发；会话键来自 `ToolContext` |
//! | 存储真值 | [`store::ChecklistStore`] | 版本检查 / 幂等 / 原子写 / 脱敏 |
//! | 运行时 | [`runtime::ChecklistRuntime`] | **per-persona store 的长期所有者**（人格重建复用同一 `Arc`） |
//! | 权威补拉 | [`commands`] | `RequestChecklists` → `ChecklistsUpdated` / `ChecklistsSnapshot` |
//! | 提示词摘要 | [`runtime::ChecklistRuntime::summary_block`] | 模块文本块（键 `tools.builtin.checklist`） |
//! | 装配契约 | [`provider`] | 宿主给端口，模块造工具并打包标签 |
//!
//! ## 为什么单独成模块（2026-11 拆出）
//!
//! 清单此前住在 `agent-module-tools`（内置工具集）里，两件事因此被绑在一起：
//!
//! - **想关掉清单必须关掉整个工具集**——反过来也一样，`calculator` 的启停
//!   与清单的启停共用同一个包标签；
//! - 清单有自己的持久化真值（`<config>/checklists/p-<hex>.json`）、自己的
//!   提示词块、自己的查询命令域，这些都不是"工具集"的语义。
//!
//! 拆开之后依赖方向也变干净：本 crate **不依赖 `agent-module-tools`**，宿主
//! 接缝由 [`host_keys::ChecklistCommandHost`] 自己声明（模块之间互不依赖，
//! 见 `scripts/deps-policy.txt` 纪律 4）。
//!
//! ## 依赖
//!
//! `echo-defs`（工具 / 脱敏词汇）+ `agent-module-management`（面板协议形状）+
//! `echo-module` / `echo-context`（模块机制）+ 基础 crate。**不依赖任何其它
//! 能力模块**，也不依赖组合根。

pub mod commands;
pub mod host_keys;
pub mod model;
pub mod module;
pub mod ops;
pub mod provider;
pub mod runtime;
pub mod serve;
pub mod store;
pub mod tool;

/// 模块 id（`module.toml` 与本常量必须一致，单测守着）。
pub const MODULE_ID: &str = "echo-agent.checklist";

pub use commands::{ChecklistCommandHandler, COMMANDS as CHECKLIST_COMMANDS};
pub use host_keys::{command_host_key, ChecklistCommandHost, COMMAND_HOST, TOOLS_PROVIDER};
pub use module::{manifest, module, MANIFEST_TOML};
pub use provider::{
    provider_key, resolve_provider, standalone_provider, ChecklistHostCtx, ChecklistPort,
    ChecklistToolProvider, ChecklistTools,
};
pub use runtime::{runtime_key, ChecklistFullSnapshot, ChecklistRuntime, ToolGate, TEXT_BLOCK_KEY};
pub use store::ChecklistStore;
pub use tool::{
    all_state_event_bindings, state_event_binding, ChecklistTool, ChecklistUnavailableTool,
};

/// 状态广播绑定（宿主注册到执行链）：`state_key` → 线上事件形状。
pub use tool::state_event_binding as checklist_state_event_binding;
