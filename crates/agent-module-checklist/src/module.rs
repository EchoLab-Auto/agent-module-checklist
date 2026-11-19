//! 模块实现：`mount` 挂工具集构造契约 + 绑定命令域接缝 + 贡献提示词文本块。
//!
//! ## 三段接线
//!
//! | 接缝 | 谁注册 | 模块干什么 |
//! |---|---|---|
//! | [`provider::provider_key`] | 本模块 `mount`（`ctx.provide`） | 宿主装配期解析它造清单工具 |
//! | [`runtime::runtime_key`] | 组合根（进程级明确实例） | 摘要文本块与 `RequestChecklists` 读它 |
//! | [`host_keys::command_host_key`] | 组合根 | 命令域拿"人格存在性 + 事件出口" |
//!
//! 后两者都是**可选**依赖：老宿主 / 单测没有这些键时模块照常挂载（摘要与
//! 命令返回明确错误），不把整个模块拖下水——与内置工具集、shell 同款取舍。
//!
//! ## 文本块与门控
//!
//! 摘要文本块的**声明与内容同源**（都在本 crate）。工具被禁用时
//! [`ChecklistRuntime::summary_block`] 返回 `None`——不给模型注入"去更新一个
//! 不可用工具"的要求。

use std::sync::Arc;

use echo_module::{BuiltinModule, Module, ModuleManifest, ModuleTextBlock, MountContext};
use echo_module::ServiceSlot;

use crate::provider::{provider_key, ChecklistToolProvider, ChecklistTools};
use crate::runtime::{runtime_key, ChecklistRuntime};
use crate::host_keys::command_host_key;
use crate::{commands, MODULE_ID};

/// 模块声明文件——身份、依赖、提供的服务键全在这里（单一真值）。
pub const MANIFEST_TOML: &str = include_str!("../module.toml");

/// 模块清单：从 [`MANIFEST_TOML`] 解析，只把版本号留给宿主注入。
pub fn manifest(version: &str) -> ModuleManifest {
    ModuleManifest::from_toml(MANIFEST_TOML, version)
        .expect("agent-module-checklist 的 module.toml 解析失败")
}

/// 构造模块（宿主装配期调用一次）。
pub fn module(version: &str) -> Arc<dyn Module> {
    let runtime = ServiceSlot::<Arc<ChecklistRuntime>>::new();
    let host = ServiceSlot::<Arc<dyn crate::host_keys::ChecklistCommandHost>>::new();
    let (bind_runtime, text_runtime, cmd_runtime) =
        (runtime.clone(), runtime.clone(), runtime.clone());
    let (bind_host, cmd_host) = (host.clone(), host.clone());

    Arc::new(
        BuiltinModule::new(manifest(version), move |mctx: &MountContext| {
            let mut disposers = Vec::new();
            // 服务表可能不存在（裸注册表 / 单测）：此时只做门控式挂载，
            // 不提供键——与内置工具集同样的降级口径（缺服务表不是错误）。
            if let Some(ctx) = mctx.context() {
                let provider: Arc<dyn ChecklistToolProvider> = Arc::new(ChecklistTools);
                disposers.push(
                    ctx.provide(&provider_key(), provider)
                        .map_err(|e| echo_module::ModuleMountError::Mount(e.to_string()))?,
                );
            }
            // 两个宿主接缝都是**可选**的：没绑上时摘要 / 命令返回明确错误。
            if let Ok(disposer) = bind_runtime.bind(mctx, &runtime_key()) {
                disposers.push(disposer);
            }
            if let Ok(disposer) = bind_host.bind(mctx, &command_host_key()) {
                disposers.push(disposer);
            }
            Ok(disposers)
        })
        // 命令域：`RequestChecklists`（权威快照补拉）由本模块处理。
        .with_commands(commands::COMMANDS)
        // 清单摘要进系统提示词（文本块的声明与内容同源，都在本模块）。
        .with_text_blocks([ModuleTextBlock::new(
            crate::runtime::TEXT_BLOCK_KEY,
            "任务清单摘要",
            60,
        )])
        .with_text_block_content(move |key, context| {
            if key != crate::runtime::TEXT_BLOCK_KEY {
                return None;
            }
            text_runtime.get()?.summary_block(context)
        })
        .with_command_handler_async(move |name, payload| {
            let (runtime, host) = (cmd_runtime.clone(), cmd_host.clone());
            Box::pin(async move {
                let Some(host) = host.get() else {
                    return Err(format!(
                        "命令 {name} 到 {MODULE_ID} 时模块尚未挂载（清单命令域接缝未绑定）"
                    ));
                };
                commands::ChecklistCommandHandler::new(runtime.get(), host)
                    .handle(&name, payload)
                    .await
                    .unwrap_or_else(|| {
                        Err(format!(
                            "命令 {name} 被 {MODULE_ID} 声明，但清单命令域没有认领它"
                        ))
                    })
            })
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `module.toml` 里的身份必须与 Rust 常量同源：两边漂移会让面板显示
    /// 一个 id、服务键用另一个 id。
    #[test]
    fn manifest_matches_module_id() {
        let manifest = manifest("0.0.0-test");
        assert_eq!(manifest.id, MODULE_ID);
        assert_eq!(manifest.kind, "tool");
        assert_eq!(manifest.tag, "inner");
    }

    /// 声明与处理器必须同步：`with_commands` 里列了但处理不了 = 面板点到
    /// 一条永远报"未认领"的命令。
    #[test]
    fn declared_commands_are_handled() {
        assert_eq!(commands::COMMANDS, ["RequestChecklists"]);
    }

    /// 文本块 key 是面板契约，名字漂移等于摘要消失。
    #[test]
    fn text_block_key_is_stable() {
        assert_eq!(crate::runtime::TEXT_BLOCK_KEY, "tools.builtin.checklist");
    }
}
