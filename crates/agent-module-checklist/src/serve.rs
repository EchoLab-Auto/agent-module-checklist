//! 独立产物入口：把清单模块打包成**进程外模块**（`serve_stdio`）。
//!
//! 与 `module.rs` 的分工：那份是**进程内**装配（`BuiltinModule` + 同步闭包，
//! 由 Core 直接 `push_module` 编进二进制）；这份是**协议侧**入口
//! （`ModuleHandler`，宿主经 `Invoke` 调用）。两者共用同一份
//! [`ChecklistRuntime`] / [`ChecklistStore`] / [`ChecklistTool`]，业务逻辑
//! 一行都没复制。
//!
//! ## 何时用这份
//!
//! 模块化的目标是"**模块作为产物插入**"——独立构建、独立发布、运行期装载，
//! 而不只是"代码被共同编译"。Core 侧的三条消费通路（工具 / 命令 / 文本块）
//! 补齐之后，清单才能真的以独立产物形态跑起来；这个 entry 就是宿主能看见的
//! 那一面。
//!
//! ## 配置
//!
//! `Hello.config`（即 `modules.toml` 的 `[module.config]`）：
//!
//! ```toml
//! [module.config]
//! data_dir = "/home/me/.local/share/echo-agent-core/checklists"
//! ```
//!
//! `data_dir` 下每个 persona 一份 JSON（与进程内形态同一套 store 实现）。
//! 缺省取进程工作目录下的 `checklists/`——宿主拉起子进程时工作目录是宿主给
//! 的那个，通常不是想要的，所以生产部署应当显式配置。

use std::path::PathBuf;
use std::sync::Arc;

use echo_defs::registry::Tool;
use echo_module_sdk::echo_module_api::{CommandContribution, Contribution, TextBlockContribution};
use echo_module_sdk::{async_trait, InvokeOutcome, InvokeRequest, ModuleHandler};
use serde_json::Value;

use crate::runtime::{ChecklistRuntime, TEXT_BLOCK_KEY};
use crate::store::ChecklistStore;
use crate::tool::ChecklistTool;
use crate::CHECKLIST_COMMANDS;

/// 清单工具名（`Invoke` 的贡献名，与进程内 `Tool::name()` 一致）。
pub const TOOL_CONTRIBUTION: &str = "checklist";

/// 摘要块在提示词里的排序权重（与进程内 `module.rs` 的声明同值）。
pub const SUMMARY_BLOCK_ORDER: i32 = 60;

/// 会话缺省值：与 `tool.rs` 的 `FALLBACK_SESSION` 同值。宿主没给会话时
/// （进程级命令 / 单测）用同一个桶，避免两处各造一个 "__default" 之类的键。
const FALLBACK_SESSION: &str = "__global";

/// 协议侧清单模块。
pub struct ChecklistModule {
    runtime: Arc<ChecklistRuntime>,
}

impl ChecklistModule {
    /// 以数据目录构造。
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            runtime: Arc::new(ChecklistRuntime::new(data_dir, None)),
        }
    }

    /// 声明用的工具实例：只读 `name/description/parameters`，用内存 store
    /// 构造即可（不碰磁盘，也不会被调用）。
    ///
    /// 为什么从工具实例读而不是抄一份 schema：声明与执行必须同源，抄一份
    /// 就等于埋一处漂移——改了参数却忘了改声明，模型侧看到的仍是旧形状。
    fn declaration_tool() -> ChecklistTool {
        ChecklistTool::with_store(Arc::new(ChecklistStore::memory()))
    }

    /// 从 `InvokeContext` 取人格与会话。
    ///
    /// `team_id` 缺省时归到 `__global`：单人格部署（不挂实例、不分区）里宿主
    /// 根本不发 `team_id`，此时"唯一那个人格"就是合理归属。
    fn identity(req: &InvokeRequest) -> (String, String) {
        let team = req
            .ctx
            .team_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .unwrap_or(FALLBACK_SESSION)
            .to_string();
        let session = req
            .ctx
            .session_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .unwrap_or(FALLBACK_SESSION)
            .to_string();
        (team, session)
    }

    /// 工具调用：复用 `ChecklistTool::apply`（`cx = None` 时它从参数的
    /// `__session_id` 取会话——工具侧既有的兼容分支）。
    async fn invoke_tool(&self, req: InvokeRequest) -> InvokeOutcome {
        let (team, session) = Self::identity(&req);
        let store = match self.runtime.store_for(&team) {
            Ok(store) => store,
            Err(error) => {
                return InvokeOutcome::Error {
                    code: "checklist_unavailable".to_string(),
                    message: error,
                }
            }
        };
        // 进程外没有 `ToolContext` 可传，用工具的兼容键带上会话。
        let mut arguments = req.payload;
        if let Value::Object(map) = &mut arguments {
            map.entry("__session_id")
                .or_insert_with(|| Value::String(session));
        }
        let tool = ChecklistTool::with_store(store);
        match tool.execute(arguments).await {
            Ok(text) => InvokeOutcome::ok(text),
            Err(error) => InvokeOutcome::Error {
                code: "checklist_failed".to_string(),
                message: error.to_string(),
            },
        }
    }

    /// 摘要文本块：内容来自 `ChecklistRuntime::summary`（与进程内同一份实现）。
    fn invoke_summary(&self, req: &InvokeRequest) -> InvokeOutcome {
        let (team, session) = Self::identity(req);
        // `None` = 这个上下文下没话说（如该人格禁用了清单）。返回空串，
        // 与宿主侧"丢弃空块"的口径一致——别用错误冒充"没有内容"。
        InvokeOutcome::ok(self.runtime.summary(&team, &session).unwrap_or_default())
    }

    /// 命令域：`RequestChecklists` 的权威快照补拉。
    fn invoke_command(&self, req: &InvokeRequest) -> InvokeOutcome {
        let (team, _session) = Self::identity(req);
        let store = match self.runtime.store_for(&team) {
            Ok(store) => store,
            Err(error) => {
                return InvokeOutcome::Error {
                    code: "checklist_unavailable".to_string(),
                    message: error,
                }
            }
        };
        InvokeOutcome::data(store.full_snapshot())
    }
}

#[async_trait]
impl ModuleHandler for ChecklistModule {
    fn contributions(&self) -> Vec<Contribution> {
        let tool = Self::declaration_tool();
        let mut contributions = vec![
            // ── 工具（声明与执行同源）──
            tool_contribution(&tool),
            // ── 文本块（键与进程内声明同一个）──
            Contribution::Text(TextBlockContribution {
                key: TEXT_BLOCK_KEY.to_string(),
                label: "任务清单摘要".to_string(),
                order: SUMMARY_BLOCK_ORDER,
            }),
        ];
        // ── 命令域 ──
        for name in CHECKLIST_COMMANDS {
            contributions.push(Contribution::Command(CommandContribution {
                name: name.to_string(),
                description: String::new(),
            }));
        }
        contributions
    }

    async fn invoke(&self, req: InvokeRequest) -> InvokeOutcome {
        // 用 `if` 链而不是 `match` + const 模式：`&str` 常量作模式在这里容易
        // 被读成绑定（让分支静默失效），显式比较更难出错。
        let name = req.contribution.as_str();
        if name == TOOL_CONTRIBUTION {
            return self.invoke_tool(req).await;
        }
        if name == TEXT_BLOCK_KEY {
            return self.invoke_summary(&req);
        }
        if CHECKLIST_COMMANDS.contains(&name) {
            return self.invoke_command(&req);
        }
        InvokeOutcome::Error {
            code: "unknown_contribution".to_string(),
            message: format!("no such contribution: {name}"),
        }
    }
}

// ── 协议侧适配 ────────────────────────────────────────────────────────────
//
// 这层映射只有"进程外形态"需要（进程内由宿主直接把 `dyn Tool` 放进注册表），
// 所以放在 serve 这一侧，不污染 `tool.rs`。

/// 工具声明 → 协议贡献（`Custom{kind:"echo-agent.tool"}` 的 DTO 由宿主定义，
/// 这里的字段顺序与 `agent-module-loop::adapter::contract::ToolContribution`
/// 对齐）。
pub fn tool_contribution(tool: &ChecklistTool) -> Contribution {
    Contribution::Custom {
        kind: "echo-agent.tool".to_string(),
        name: tool.name().to_string(),
        payload: serde_json::json!({
            "name": tool.name(),
            "description": tool.description(),
            "parameters": tool.parameters(),
            "category": "module",
            "package": crate::MODULE_ID,
        }),
    }
}
