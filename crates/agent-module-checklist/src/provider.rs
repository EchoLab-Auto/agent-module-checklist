//! 工具集构造契约：端口进，配好的工具箱出（与 shell / 内置工具集同款）。
//!
//! ## 为什么模块自己造工具
//!
//! "清单工具长什么样"是**本模块的知识**：它有独立的状态（per-persona store）、
//! 独立的包标签（`echo-agent.checklist`）、独立的失败形态（持久存储加载失败
//! 时注册同名失败工具）。宿主只负责给出端口——它知道的是"这个 persona 该用
//! 哪份 store"，不知道"工具怎么注册、标签怎么打"。
//!
//! ## 降级是显式的
//!
//! | 端口 | 注册的工具 | 语义 |
//! |---|---|---|
//! | [`ChecklistPort::Ready`] | [`ChecklistTool`] + 持久 store | 生产路径 |
//! | [`ChecklistPort::Failed`] | [`ChecklistUnavailableTool`] | 每次调用都返回同一条可排查错误 |
//! | `None` | [`ChecklistTool::new`]（内存 store） | 单测 / 嵌入式：仅本进程有效 |
//!
//! `None` **绝不**用于"磁盘加载失败"——那会用空 store 在下次写时覆盖原文件。

use std::sync::Arc;

use echo_defs::registry::{Tool, ToolRegistry};

use crate::host_keys::TOOLS_PROVIDER;
use crate::store::ChecklistStore;
use crate::tool::{ChecklistTool, ChecklistUnavailableTool};
use crate::MODULE_ID;

/// 本 persona 的清单端口。
///
/// - `Some(Ready(store))` = 持久 store（[`ChecklistRuntime`] 持有，人格重建
///   复用同一 `Arc`）；
/// - `Some(Failed(error))` = 加载失败：注册**同名失败工具**，调用必失败，
///   绝不以内存 store 顶上（那会在下一次写时覆盖原文件）；
/// - `None` = 未接线（单测 / 嵌入式）：内存 store，仅本进程有效。
#[derive(Clone)]
pub enum ChecklistPort {
    Ready(Arc<ChecklistStore>),
    Failed(String),
}

impl std::fmt::Debug for ChecklistPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ready(store) => f.debug_tuple("Ready").field(store).finish(),
            Self::Failed(error) => f.debug_tuple("Failed").field(error).finish(),
        }
    }
}

/// 工具装配需要的端口。
#[derive(Default, Clone, Debug)]
pub struct ChecklistHostCtx {
    /// 本 persona 的清单端口（`None` = 内存 store）。
    pub port: Option<ChecklistPort>,
}

/// 工具集构造契约（模块提供、宿主调用）。
///
/// 返回的注册表已按本模块打上包标签，于是面板按包启停与 persona 门控都
/// 作用于 `echo-agent.checklist` 而不是整个内置工具集。
pub trait ChecklistToolProvider: Send + Sync {
    fn registry(&self, cx: &ChecklistHostCtx) -> ToolRegistry;
}

/// 唯一实现（保留成类型而不是闭包：`ctx.provide` 需要 `'static + Any`）。
#[derive(Default)]
pub struct ChecklistTools;

impl ChecklistToolProvider for ChecklistTools {
    fn registry(&self, cx: &ChecklistHostCtx) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        // 清单工具**只注册一次**：端口给出 store / 失败工具 / 内存 store。
        let tool: Arc<dyn Tool> = match &cx.port {
            Some(ChecklistPort::Ready(store)) => {
                Arc::new(ChecklistTool::with_store(Arc::clone(store)))
            }
            Some(ChecklistPort::Failed(error)) => {
                Arc::new(ChecklistUnavailableTool::new(error.clone()))
            }
            None => Arc::new(ChecklistTool::new()),
        };
        registry.register(tool);
        // 打标签的知识归模块——它知道"这些是我装的"。
        for name in registry.names() {
            registry.set_package(&name, MODULE_ID);
        }
        registry
    }
}

/// 本模块的工具集服务键（与 [`TOOLS_PROVIDER`] 同源）。
pub fn provider_key() -> echo_context::ServiceKey<Arc<dyn ChecklistToolProvider>> {
    echo_context::ServiceKey::new(TOOLS_PROVIDER)
}

/// 便捷解析：宿主在工具装配点问一句"清单模块挂上了吗"。
///
/// 返回 `None` = 模块未挂载（还没挂 / 被禁用）——调用方据此回退
/// [`standalone_provider`]（与内置工具集、shell 同一个口径）。
pub fn resolve_provider(ctx: &echo_context::Ctx) -> Option<Arc<dyn ChecklistToolProvider>> {
    ctx.service(&provider_key())
}

/// 不依赖服务表直接构造（组合根的"模块尚未挂载"路径与单测用）。
pub fn standalone_provider() -> Arc<dyn ChecklistToolProvider> {
    Arc::new(ChecklistTools)
}
