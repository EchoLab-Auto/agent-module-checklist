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
| 权威补拉 | `src/commands.rs` | `RequestChecklists` → `ChecklistUpdated` / `ChecklistsSnapshot` |
| 提示词摘要 | `src/runtime.rs` | 模块文本块（键 `tools.builtin.checklist`） |
| 装配契约 | `src/provider.rs` | 宿主给端口，模块造工具并打自己的包标签 |

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

## 装配（宿主给端口，模块造工具）

`ChecklistToolProvider`（服务键 `echo-agent.checklist.tools`）是模块对外的唯一构造
契约：宿主递 [`ChecklistPort`](crates/agent-module-checklist/src/provider.rs)（持久
store / 冻结失败 / `None`=内存 store），模块产出打上 `echo-agent.checklist` 包标签的
工具集。宿主不认识工具名，模块不认识人格表——两边只在端口上相遇。

未注入时的降级是**显式**的，不是静默换实现：

| 端口 | 注册的工具 | 语义 |
|---|---|---|
| `Ready(store)` | `ChecklistTool` + 持久 store | 生产路径 |
| `Failed(error)` | `ChecklistUnavailableTool` | 每次调用返回同一可排查错误 |
| `None` | 内存 store | 单测 / 嵌入式；**绝不**用于磁盘加载失败 |

## 宿主侧接线（EchoAgentCore）

1. Core 的 `Cargo.toml` 加依赖，并在 `[patch]` 里把这几个 crate 拉回本地工作区
   （否则同一进程会有两份 `Tool` trait）：

   ```toml
   agent-module-checklist = { git = "https://github.com/EchoLab-Auto/agent-module-checklist", branch = "main" }

   [patch."https://github.com/EchoLab-Auto/EchoAgentCore"]
   agent-module-checklist = { path = "…" }   # 仅本仓仍以 path 方式引入时
   ```

2. 内置模块目录登记（`source/modules/core/src/builtin_modules.rs`）：

   ```rust
   push_module(&mut catalog, agent_module_checklist::module(version))?;
   ```

3. `source/modules/agent-module-loop/src/modules.rs` 的 `BUILTIN_MODULE_IDS` 加
   `CHECKLIST_MODULE_ID`；`scripts/update.sh` 的 `expected_ids` 加
   `echo-agent.checklist`（两边一致性由 `instance_manifests` 单测守护）。

4. 宿主侧三段接线（缺一不可）：

   | 接缝 | 谁实现 | 作用 |
   |---|---|---|
   | 服务键 `echo-agent.host.tools.checklist` | 组合根（进程级 `ChecklistRuntime`） | 摘要文本块 + 快照补拉的唯一数据源 |
   | 服务键 `echo-agent.host.checklist.commands` | 组合根的 `ChecklistCommandHost` | `resolve_registry`（人格存在性）+ `emit`（面板事件） |
   | `Tool::state_key` 绑定 | 组合根（`all_state_event_bindings()`） | 执行后把状态广播成 `BackendEvent::ChecklistUpdated` |

5. `RequestChecklists` 是**命令域**，宿主 `apply_command` 在落内置 match 之前会问一次
   模块——所以命令名只能出现在本仓的 `commands::COMMANDS` 里。

模块 id 是 `echo-agent.checklist`（运行期标识，出现在配置 / 面板门控 / 持久化状态
里），**不随 crate 名变**。

## 开发

```bash
cargo test                              # 69 单测（+1 忽略：子进程角色）
cargo clippy --all-targets -- -D warnings
```

上游 Core `main` 尚未包含清单相关协议时（`RequestChecklists` 等变体还没合入），本仓
独立构建会失败——把根 `Cargo.toml` 里注释的 `[patch]` 段打开、指向本机 Core 工作区
即可。

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

宿主接缝由本仓的 `src/host_keys.rs` **自己声明**（`ChecklistCommandHost`），不借用
`agent-module-tools` 的 `ToolsCommandHost`——模块之间互不依赖是硬纪律
（见 Core 的 `scripts/deps-policy.txt`）。
