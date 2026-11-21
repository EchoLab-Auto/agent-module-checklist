//! # agent-module-checklist — 任务清单模块（`echo-agent.checklist` 的实现）
//!
//! 模型可直接调用的 `checklist` 工具，连同它的**持久化真值与观察面**：
//!
//! | 组成 | 位置 | 说明 |
//! |---|---|---|
//! | 工具 | [`tool::ChecklistTool`] | 参数 → store 的转发；会话键来自 `ToolContext` |
//! | 存储真值 | [`store::ChecklistStore`] | 版本检查 / 幂等 / 原子写 / 脱敏 |
//! | 运行时 | [`runtime::ChecklistRuntime`] | **per-persona store 的长期所有者**（人格重建复用同一 `Arc`） |
//! | 权威补拉 | [`commands`] | `checklist.snapshot` 方法（面板经通用调用通道点名） → 快照 JSON |
//! | 提示词摘要 | [`runtime::ChecklistRuntime::summary_block`] | 模块文本块（键 `tools.builtin.checklist`） |
//! | 模块身份 | [`manifest`] | 从 `module.toml` 读（单一真值） |
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
//! 拆开之后依赖方向也变干净：本 crate **不依赖 `agent-module-tools`**，与宿主
//! 之间只剩三条**通用**协议面——工具 / 文本块贡献、通用调用 `InvokeModuleOp`
//! （快照方法）、`Emit`（状态事件）。宿主没有一行清单代码，协议里也没有清单的
//! 具名变体（见 `scripts/deps-policy.txt` 纪律 4）。
//!
//! ## 依赖
//!
//! `echo-defs`（工具 / 脱敏词汇）+ `agent-module-management`（面板协议形状）+
//! `echo-module` / `echo-context`（模块机制）+ 基础 crate。**不依赖任何其它
//! 能力模块**，也不依赖组合根。

pub mod commands;
pub mod manifest;
pub mod model;
pub mod ops;
pub mod runtime;
pub mod serve;
pub mod store;
pub mod tool;

/// 模块 id（`module.toml` 与本常量必须一致，单测守着）。
pub const MODULE_ID: &str = "echo-agent.checklist";

pub use commands::{snapshot_payload, SNAPSHOT_METHOD};
pub use manifest::{manifest, MANIFEST_TOML};
pub use runtime::{runtime_key, ChecklistFullSnapshot, ChecklistRuntime, ToolGate, TEXT_BLOCK_KEY};
pub use store::ChecklistStore;
pub use tool::{ChecklistTool, ChecklistUnavailableTool};

/// 状态事件名（模块自报，经 `Emit` 主动广播给面板）。
///
/// 与 `serve::STATE_EVENT` 同值；放在这里是为了让**面板扩展**与模块侧共用
/// 一个常量来源（面板写死字符串是跨仓契约的一部分，改名必须两边一起改）。
pub use serve::STATE_EVENT as CHECKLIST_STATE_EVENT;
