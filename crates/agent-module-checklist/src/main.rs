//! `agent-module-checklist` —— 清单模块的**独立产物**入口。
//!
//! 宿主经 `modules.toml` 的 `kind = "stdio"` 条目拉起它：
//!
//! ```toml
//! [[module]]
//! id = "checklist"
//! kind = "stdio"
//! name = "agent-module-checklist"
//! command = "/home/me/.local/libexec/echo-agent-core/modules/agent-module-checklist/agent-module-checklist"
//! enabled = true
//!
//! [module.config]
//! data_dir = "/home/me/.local/share/echo-agent-core/checklists"
//! ```
//!
//! 数据目录也可以先用环境变量给（宿主没配 `data_dir` 时的兜底）：
//!
//! ```text
//! ECHO_CHECKLIST_DATA_DIR=/path/to/checklists agent-module-checklist
//! ```
//!
//! 退出码约定由 SDK 决定：0 = 正常排空 / 收到 `Dispose`，非 0 = 协议或
//! 模块自身出错（宿主按崩溃处理并重启）。

use std::path::PathBuf;

use agent_module_checklist::serve::ChecklistModule;
use echo_module_sdk::serve_stdio;

/// 宿主没配 `data_dir` 时的兜底目录（相对当前工作目录）。
const DEFAULT_DATA_SUBDIR: &str = "checklists";

fn resolve_data_dir() -> PathBuf {
    // 优先级：环境变量 > 工作目录下的 checklists/。
    //
    // 为什么环境变量在最前：`Hello.config` 要握手之后才到，而模块可能在
    // 那之前就收到了调用——数据目录必须在启动瞬间就确定下来。宿主要指定
    // 位置时，`[module.env]` 是最直接的通道（`modules.toml` 本来就支持）。
    if let Some(dir) = std::env::var_os("ECHO_CHECKLIST_DATA_DIR") {
        let dir = PathBuf::from(dir);
        if !dir.as_os_str().is_empty() {
            return dir;
        }
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(DEFAULT_DATA_SUBDIR)
}

#[tokio::main]
async fn main() {
    // 日志走 stderr：stdout 是协议帧的独占通道，混进去就是帧损坏。
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let data_dir = resolve_data_dir();
    tracing::info!(data_dir = %data_dir.display(), "checklist module starting (standalone)");

    if let Err(error) = serve_stdio(ChecklistModule::new(data_dir)).await {
        tracing::error!(%error, "checklist module exited with an error");
        std::process::exit(1);
    }
}
