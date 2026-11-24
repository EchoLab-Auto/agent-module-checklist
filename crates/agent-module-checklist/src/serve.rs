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
use echo_module_sdk::echo_module_api::{
    CommandContribution, Contribution, ModuleManifestClaim, TextBlockContribution,
};
use echo_module_sdk::{async_trait, HostClient, InvokeOutcome, InvokeRequest, ModuleHandler};
use serde_json::Value;

use crate::runtime::{ChecklistRuntime, TEXT_BLOCK_KEY};
use crate::store::ChecklistStore;
use crate::tool::ChecklistTool;
use crate::commands::{snapshot_payload, SNAPSHOT_METHOD};

/// 清单工具名（`Invoke` 的贡献名，与进程内 `Tool::name()` 一致）。
pub const TOOL_CONTRIBUTION: &str = "checklist";

/// 进程外形态的来源标签。
///
/// `module.toml` 写的是 `inner`（编译进 Core 的那份）。同一份源码打成独立
/// 二进制后，来源变了——面板分组读的就是这个字段，继续用 `inner` 会让用户
/// 以为它随 Core 发布。
pub const EXTERNAL_TAG: &str = "outside";

/// 摘要块在提示词里的排序权重（与进程内 `module.rs` 的声明同值）。
pub const SUMMARY_BLOCK_ORDER: i32 = 60;

/// 会话缺省值：与 `tool.rs` 的 `FALLBACK_SESSION` 同值。宿主没给会话时
/// （进程级命令 / 单测）用同一个桶，避免两处各造一个 "__default" 之类的键。
const FALLBACK_SESSION: &str = "__global";

/// 协议侧清单模块。
/// 面板 UI 贡献的 `Contribution::Custom` 分类名（宿主的词汇）。
///
/// 与宿主 `core::panel_ui::PANEL_UI_CONTRIBUTION_KIND` 是同一条字符串契约：
/// 两侧各写一份字面量，改一边等于本模块的界面入口静默消失。
pub const PANEL_UI_CONTRIBUTION_KIND: &str = "panel_ui";

/// 本模块面板 UI bundle 的站点相对路径（固定，不带 hash）。
pub const PANEL_UI_BUNDLE: &str = "panel-extensions/echo-agent.checklist/index.js";

/// 面板 UI SDK 契约版本（与 Panel 仓 `panel-shared.entries.json` 一致）。
pub const PANEL_UI_SDK_VERSION: &str = "1";

pub struct ChecklistModule {
    /// 当前运行时。`configure` 可能把它**换掉**（宿主在 `Hello.config` 里
    /// 给出的 `data_dir` 优先于启动时的兜底目录），所以放进 `Mutex` 而不是
    /// 直接持 `Arc`。
    ///
    /// 为什么必须在 `configure` 里换：数据目录要在**第一个调用之前**确定，
    /// 而 `Hello.config` 握手后才到——`configure` 正是 SDK 保证的
    /// "`contributions` 之前调用一次"的那个时点。启动瞬间只能给兜底
    /// （环境变量或工作目录）。
    runtime: std::sync::Mutex<Arc<ChecklistRuntime>>,
    /// 当前生效的数据目录（启动兜底，`configure` 可覆盖）。
    data_dir: std::sync::Mutex<PathBuf>,
    /// `on_ready` 交付的宿主句柄。用它**主动广播**状态变化（`Emit`）——
    /// 进程内形态走 `state_event_binding` 的同步闭包，进程外没有那条路。
    host: std::sync::Mutex<Option<HostClient>>,
}

/// 状态变更事件的线名（面板据此更新清单面板）。
pub const STATE_EVENT: &str = "checklist.updated";

impl ChecklistModule {
    /// 以数据目录构造。
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            runtime: std::sync::Mutex::new(Arc::new(ChecklistRuntime::new(
                data_dir.clone(),
                None,
            ))),
            data_dir: std::sync::Mutex::new(data_dir),
            host: std::sync::Mutex::new(None),
        }
    }

    /// 取当前运行时（`configure` 换过就是新的那份）。
    fn runtime(&self) -> Arc<ChecklistRuntime> {
        Arc::clone(
            &self
                .runtime
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// 广播某会话的最新权威状态。
    ///
    /// 载荷形状与进程内 `ChecklistUpdated` **完全一致**（`{session_id,
    /// team_id, state}`，`state` 带 `epoch`/`revision`）——面板因此能用同一条
    /// 版本判定逻辑消费两种来源，不必为新形态写第二套。
    fn broadcast_state(&self, team: &str, session: &str, store: &ChecklistStore) {
        let Some(host) = self
            .host
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        else {
            // 宿主还没交付句柄（`on_ready` 之前）：不报错，调用本就发生在
            // 握手完成之后，这里只是防御。
            return;
        };
        let Some(state) = store.snapshot(session) else {
            return;
        };
        host.emit(
            STATE_EVENT,
            serde_json::json!({
                "session_id": session,
                "team_id": team,
                "state": state,
            }),
        );
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
        let runtime = self.runtime();
        let store = match runtime.store_for(&team) {
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
                .or_insert_with(|| Value::String(session.clone()));
        }
        let tool = ChecklistTool::with_store(Arc::clone(&store));
        let outcome = tool.execute(arguments).await;
        match &outcome {
            // 只读操作（list/get）不会改状态，广播它们只会让面板白刷一帧。
            // 判据用"状态确实变了"——`execute` 成功后直接比对没必要，简单点：
            // 所有成功调用都广播（工具内部已经区分过成功/失败，写操作的
            // revision 必然 +1，面板的版本判定会丢掉重复帧）。
            Ok(_) => self.broadcast_state(&team, &session, &store),
            Err(_) => {}
        }
        match outcome {
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
        InvokeOutcome::ok(self.runtime().summary(&team, &session).unwrap_or_default())
    }

    /// 方法 `checklist.snapshot`：权威快照补拉。
    ///
    /// 返回的是**本次调用的应答**（`InvokeOutcome::data`），不是宿主事件——
    /// 面板拿到 `{team_id, epoch, revision, states}` 后自己投影，Core 不知道
    /// 里面有什么。
    fn invoke_command(&self, req: &InvokeRequest) -> InvokeOutcome {
        let (team, _session) = Self::identity(req);
        match snapshot_payload(&self.runtime(), &team) {
            Ok(payload) => InvokeOutcome::data(payload),
            Err(error) => InvokeOutcome::Error {
                code: "checklist_unavailable".to_string(),
                message: error,
            },
        }
    }
}

#[async_trait]
impl ModuleHandler for ChecklistModule {
    /// 接收宿主下发配置：`[module.config] data_dir` 决定清单落盘位置。
    ///
    /// 为什么这里必须处理而不是只认环境变量：`modules.toml` 的
    /// `[module.config]` 是部署方**唯一的**配置面（写在这一行里、跟着这一行
    /// 走），而 `[module.env]` 是给"必须在 exec 之前就知道"的场景留的后门。
    /// 只认环境变量会得到一个很难查的故障：配置写得没错、模块也起来了，但
    /// 它读的是工作目录下的另一个目录——面板永远是空的，而且不报错。
    ///
    /// `data_dir` 缺省 / 非法时**保留兜底**（构造时给的那个），不是报错：
    /// 宿主没配这个键是完全合法的用法（模块自己有默认位置）。
    async fn configure(&self, config: Value) -> Result<(), String> {
        let dir = config
            .get("data_dir")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let Some(dir) = dir else {
            return Ok(());
        };
        let dir = PathBuf::from(dir);
        let mut current = self
            .data_dir
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // 同一个目录就不重建：重建会丢掉已打开的 store（`Arc` 身份是"同一
        // 份真值"的凭据，热重载 / 反复 configure 不该换掉它）。
        if *current == dir {
            return Ok(());
        }
        *current = dir.clone();
        *self
            .runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Arc::new(ChecklistRuntime::new(dir, None));
        Ok(())
    }


    /// 自述身份：与进程内形态那份 `module.toml` **同源**（同一个
    /// `manifest()`），所以两种形态在面板里是同一行，不会因为运输方式
    /// 不同而换名字 / 换类别。
    ///
    /// 三处刻意的差异（都因为"进程外"这件事本身）：
    ///
    /// - `requires` 不声明。`module.toml` 里的服务键是**进程内接缝**（组合根
    ///   注册进 `Ctx` 服务表，模块 mount 时解析）。进程外形态下没有 `Ctx` 可查
    ///   （模块经协议 `HostCall` 回呼），把它照抄过来只会让宿主以为"这个外部
    ///   模块依赖一个它没注册的键"。
    /// - `builtin` / `runtime` 不在 claim 里（协议层就没有这两个字段）：
    ///   由宿主填——同一份二进制可以被 stdio 或 dylib 装进来。
    /// - `tag` 不照抄 `inner`：进程外形态是**外来的**，面板分组用 `outside`
    ///   才对；来源是"从模块目录装入"，不是"编译进 Core"。
    fn manifest_claim(&self) -> Option<ModuleManifestClaim> {
        // 版本取 crate 自己的版本（独立产物自报家门），不借宿主的版本号。
        let manifest = crate::manifest(env!("CARGO_PKG_VERSION"));
        Some(
            ModuleManifestClaim::new(manifest.id, manifest.kind)
                .with_name(manifest.name)
                .with_version(manifest.version)
                .with_description(manifest.description)
                .with_tag(EXTERNAL_TAG)
                .with_author(manifest.author),
        )
    }

    async fn on_ready(&self, host: HostClient) {
        *self
            .host
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(host);
    }

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
        // 面板经通用调用通道点名的方法（`InvokeModuleOp.method`）。名字是
        // 本模块的词汇：宿主不认识它，Core 的协议里没有对应的具名变体。
        contributions.push(Contribution::Command(CommandContribution {
            name: SNAPSHOT_METHOD.to_string(),
            description: "权威清单快照（{team_id, epoch, revision, states}）".to_string(),
        }));
        // ── 面板 UI 贡献（2026-11）──
        //
        // 本模块是**进程外**形态（stdio），拿不到宿主的 `Ctx`，所以不能像
        // 进程内模块那样直接调 `PanelUiRegistrar`。框架的
        // `Contribution::Custom { custom_kind, payload }` 是"宿主自定义贡献"的
        // 官方入口（框架只搬运、不解释），宿主在这里解释 `panel_ui`。
        //
        // `name` 承载扩展 id：它本来就参与 `Invoke` 寻址，与扩展 id 是同一个
        // 字符串。`payload` 形状由本模块与宿主约定。
        contributions.push(Contribution::Custom {
            kind: PANEL_UI_CONTRIBUTION_KIND.to_string(),
            name: crate::MODULE_ID.to_string(),
            payload: serde_json::json!({
                "name": "任务清单",
                "bundle": PANEL_UI_BUNDLE,
                "sdk_version": PANEL_UI_SDK_VERSION,
            }),
        });
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
        if name == SNAPSHOT_METHOD {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 自述身份必须与进程内那份 `module.toml` 对齐：同一份代码的两种形态
    /// 在面板里是同一行，不能因为运输方式不同而换名字 / 换类别。
    #[test]
    fn manifest_claim_matches_the_in_process_manifest() {
        let module = ChecklistModule::new(std::env::temp_dir());
        let claim = module.manifest_claim().expect("进程外形态必须自述身份");
        let inproc = crate::manifest(env!("CARGO_PKG_VERSION"));

        assert_eq!(claim.id, inproc.id);
        assert_eq!(claim.id, crate::MODULE_ID, "id 是模块的稳定键");
        assert_eq!(claim.name, inproc.name);
        assert_eq!(claim.kind, inproc.kind);
        assert_eq!(claim.description, inproc.description);
    }

    /// 进程外形态**不声明** Ctx 服务依赖：`module.toml` 里没有 requires，
    /// claim 里也不能凭空出现（宿主会以为它缺依赖、拒绝或延迟挂载）。
    #[test]
    fn manifest_claim_declares_no_service_dependencies() {
        let module = ChecklistModule::new(std::env::temp_dir());
        let claim = module.manifest_claim().expect("claim");
        assert!(
            claim.requires.is_empty(),
            "进程外形态不声明 Ctx 服务依赖（实际：{:?}）",
            claim.requires
        );
        assert!(claim.optional_requires.is_empty());
        assert!(claim.provides.is_empty());
    }

    /// `configure` 给出的 `data_dir` 真的换掉了运行时：只认环境变量会得到
    /// "配置写了、模块也起来了、读的却是另一个目录"的静默故障。
    #[tokio::test]
    async fn configure_switches_the_data_dir() {
        let root = std::env::temp_dir().join(format!(
            "echo-checklist-configure-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let module = ChecklistModule::new(root.join("fallback"));
        module
            .configure(serde_json::json!({ "data_dir": root.join("configured").to_string_lossy() }))
            .await
            .expect("合法配置不该被拒绝");
        // 写入一份该目录下的清单，再从快照里读回来：路径生效 = 看到数据。
        let runtime = module.runtime();
        let store = runtime.store_for("alix").expect("store 可打开");
        store
            .apply(
                "probe-session",
                serde_json::json!({ "operation": "create", "name": "probe" }),
            )
            .await
            .expect("创建清单");
        let payload = crate::commands::snapshot_payload(&runtime, "alix").expect("快照");
        assert_eq!(payload["states"][0]["state"]["lists"][0]["name"], "probe");
        assert!(
            root.join("configured").exists(),
            "数据必须落在 configure 给的目录，而不是启动兜底"
        );
        assert!(
            !root.join("fallback").exists(),
            "兜底目录不该被创建"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 来源标签必须是 `outside`：面板分组读它，`inner` 会让人以为随 Core 发布。
    #[test]
    fn manifest_claim_marks_the_binary_as_external() {
        let module = ChecklistModule::new(std::env::temp_dir());
        assert_eq!(module.manifest_claim().expect("claim").tag, EXTERNAL_TAG);
        assert_eq!(EXTERNAL_TAG, "outside");
    }
}
