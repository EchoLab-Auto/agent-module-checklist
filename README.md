# agent-module-checklist

任务清单模块（EchoAgent 模块，**独立仓库交付**）：模块 id `echo-agent.checklist`。

从 `EchoAgentCore` 的内置模块 `source/modules/agent-module-checklist`（2026-11 由
`agent-module-tools` 拆出）迁出。工具、存储真值、快照补拉、提示词摘要全在本仓；
Core 只保留通用接线（见「宿主侧接线」）。

## 它做什么

模型做多步任务时会需要一个跨回合可靠的外置计划载体——上下文会被压缩，模型自己的
"我打算分三步"会被压掉，而清单是**唯一**能跨轮、跨压缩活下来的那一份。本模块提供：

| 组成 | 位置 | 说明 |
|---|---|---|
| 工具 | `src/tool.rs` | `checklist`（`list` / `create` / `add_items` / `update_item` / `batch` …） |
| 存储真值 | `src/store.rs` | 版本检查 / 幂等 / 原子写 / 脱敏；per-persona 一份 JSON |
| 运行时 | `src/runtime.rs` | **per-persona store 的长期所有者**（人格重建复用同一 `Arc`） |
| 权威补拉 | `src/commands.rs` | 方法 `checklist.snapshot`（面板经通用调用通道点名）→ 快照 JSON |
| 提示词摘要 | `src/runtime.rs` | 模块文本块（键 `tools.builtin.checklist`） |
| 面板界面 | `panel-ui/` | Panel 扩展：入口按钮 + 覆盖层 + 投影（壳里没有清单代码） |

## 状态协议（第二版）

快照形状：

```json
{"lists":[{"id","name","archived","items":[{"id","text","done"}],"done","total"}],
 "epoch","revision"}
```

- `epoch` 是文件级 UUID（`reset_session` 时推进），`revision` 每次成功修改 +1，
  跨 reset / 重载单调递增——**两者不比大小**，判新旧看 `revision`，判"是不是同一条
  时间线"看 `epoch`；
- 所有写都可带 `expected_epoch` / `expected_revision`（提供即校验，冲突明确报错）；
- 稳定 id 是 UUID：`list_id` / `item_id` 寻址不依赖位置；
- 旧式 `add` / `check` / `uncheck` / `remove` / `reset` 保留；其中**按位置修改**
  必须同时带两个版本号，否则报错。

落盘位置由宿主给出（`<core.toml 所在目录>/checklists/`），文件名规则在本仓的
`runtime.rs`（`p-<persona UTF-8 字节 hex>.json`）——Core 不拼文件名。

## 加载失败：显式失败，绝不"空 store 覆盖"

`ChecklistStore::open` 对损坏 / 未知版本 / 超限文件返回 `Err` 且**不写盘**。运行时
把错误按 persona 冻结缓存（不重试、不回退内存），宿主据此注册**同名失败工具**
（`ChecklistUnavailableTool`）：每次调用都返回同一条可排查错误，面板和模型都看得见
"清单不可用"，而不是拿到一个空清单再把它写回磁盘。

## 并发与取消

同路径协作写者用 `<状态文件>.save-lock` + 文件指纹（长度、纳秒 mtime、inode）防止
跨 Store / 跨 Core 进程的旧快照覆盖；指纹变化明确拒绝写入，不当成功。
`tests.rs` 里的 `cross_process_stale_writer_is_rejected` 通过启动自身测试二进制来真打
这条不变量（`--ignored --exact tool::tests::cross_process_child_rejects_stale_write`）。

## 装配（进程外形态）

进程外形态没有"端口注入"这一层：`serve.rs` 的 `ChecklistModule` 自己构造
`ChecklistRuntime`（数据目录来自 `Hello.config` 的 `data_dir`，启动时先用环境变量
或工作目录兜底），把工具、摘要文本块与快照方法作为 `Contribution` 报给宿主。

加载失败是**显式**的，不是静默换实现：

| 情形 | 表现 |
|---|---|
| store 打开成功 | `checklist` 工具正常读写（持久 store） |
| 文件损坏 / 未知版本 / 超限 | 工具每次调用返回**同一条可排查错误**，摘要跳过，快照方法回错误 |
| `data_dir` 没配 | 用启动兜底目录（不报错，但要确认它指向你期望的位置） |

## 宿主侧接线（EchoAgentCore）

**一句话：宿主不需要任何清单专用代码。** 本模块是"只有二进制、宿主完全不认识"
的第一个实例——Core 的依赖图里没有它、`BUILTIN_MODULE_IDS` 里没有它、协议里也
没有它的具名变体。宿主只在 `modules.toml` 里写一行：

```toml
[[module]]
id = "echo-agent.checklist"
kind = "stdio"
enabled = true
command = "/home/me/.local/libexec/echo-agent-core/modules/agent-module-checklist/agent-module-checklist"

[module.config]
data_dir = "/home/me/.config/echo-agent-core/checklists"
```

`data_dir` 不要省：不配的话模块只能用启动时的兜底（`ECHO_CHECKLIST_DATA_DIR`
环境变量，或进程工作目录下的 `checklists/`）——配置写在那里而实际读另一个目录
时**不会报错**，只会一直显示空清单。模块在 `configure` 里接收这个键。

## 与宿主的三个协议面（都不是专用通道）

| 面 | 协议 | 谁定义形状 |
|---|---|---|
| 工具 | `Contribution::Custom{kind:"echo-agent.tool"}` | 本模块（宿主用 `RemoteTool` 适配进注册表） |
| 摘要 | `Contribution::Text` + `text_block_content` | 本模块（宿主只按 `order` 排进提示词） |
| 快照方法 | `Contribution::Command{name:"checklist.snapshot"}` | 本模块（宿主经通用通道 `InvokeModuleOp` 转发，不认识该方法） |

状态广播走模块自己的事件名：写入成功后 `Emit("checklist.updated",
{session_id, team_id, state})` → 宿主的 `BackendEvent::ModuleEvent` 原样转给面板。
**Core 里没有 `ChecklistUpdated` / `ChecklistsSnapshot` / `RequestChecklists` 这三
个变体**（2026-11-21 删除）——它们是"宿主替模块定义业务接口"的旧形态。

## 面板（`panel-ui/`）

本模块自带 Panel UI 扩展，壳里**没有一行清单代码**：

| 面板能力 | 本模块怎么用 |
|---|---|
| `entries` | 入口行「清单」按钮；`when` = 模块真的挂载 **且** 当前人格允许用该工具，或本地已有投影 |
| `overlays` | 点开后的面板内容（`components/ChecklistOverlay.vue`，样式随组件） |
| `hooks.acceptEvent` | 拦下 `ModuleEvent`（`checklist.updated`）与 `ModuleInvokeResult`（快照应答）自己投影 |
| `hooks.setup` | 人格 / 会话 / 连接变化时补拉权威快照，重连先清版本基线 |

投影与版本判定在 `checklist-store.ts`（`epoch` 判时间线、`revision` 判新旧、
空 `states` 也是权威）。装配方式：把本仓的 `panel-ui/` 加进
`PANEL_UI_EXTENSIONS`（`:` 分隔），Panel 构建期把它编进前端包。

面板与模块之间的契约只有两个字符串：方法名 `checklist.snapshot` 与事件名
`checklist.updated`（都来自 `commands::SNAPSHOT_METHOD` / `serve::STATE_EVENT`，
本仓单测锁定）。

模块 id 是 `echo-agent.checklist`（运行期标识，出现在配置 / 面板门控 / 持久化状态
里），**不随 crate 名变**。

## 开发

```bash
cargo test                              # 69 单测（+1 忽略：子进程角色）
cargo clippy --all-targets -- -D warnings
```

本仓可以**脱离 Core 独立构建**：根 `Cargo.toml` 只依赖 `echo-fabric-arch`（按 tag）
与 `agent-module-management`（协议形状），没有 Core 的业务 crate。

框架类型（`echo-module`）按 tag 取自 `echo-fabric-arch`，与 Core 根 `Cargo.toml` 保
持同一 tag。改框架时要两边同时对齐，否则同一进程里会出现两份 `Box<dyn Module>`。

## 依赖方向

```
echo-fabric-arch（module / ctx）
      ↑
EchoAgentCore（echo-defs 词汇 + agent-module-management 协议形状）
      ↑
agent-module-checklist        ← 不依赖任何其它能力模块，也不依赖 Core 业务 crate
```

本仓不依赖任何 Core 业务 crate，也不依赖其它能力模块（见 Core 的
`scripts/deps-policy.txt`）。与宿主之间只有协议面：工具 / 文本块的 `Contribution`、
通用调用 `InvokeModuleOp`（快照方法）、`Emit`（状态事件）。
