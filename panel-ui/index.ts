//! 清单模块的 Panel UI 扩展入口。
//!
//! ## 边界
//!
//! 面板壳（`@echo-agent/panel`）只提供四件事：**扩展注册表**（入口按钮、
//! 覆盖层、主区视图、边栏卡片）、**事件钩子**（`acceptEvent`）、**命令发送**、
//! **插槽上下文**（`teamId` / `sessionId` / `core`）。它不知道"清单"是什么，
//! 也没有任何清单相关的字段、类型或 reducer。
//!
//! 本文件把清单接到那四个点上：
//!
//! | 面板能力 | 清单怎么用 |
//! |---|---|
//! | `entries` | 入口行的「清单」按钮；门控 = 模块真的挂载 + 当前人格允许用 |
//! | `overlays` | 点开后的面板内容（本模块自己的组件与样式） |
//! | `hooks.acceptEvent` | 拦下本模块的两类帧（`ModuleEvent` / `ModuleInvokeResult`），自己投影 |
//! | `hooks.setup` | 人格/会话/连接变化时补拉权威快照 |
//!
//! 数据通路（宿主与壳都不解释内容）：
//!
//! ```text
//! 模块 --Emit("checklist.updated")--> Core --ModuleEvent--> 本扩展
//! 本扩展 --InvokeModuleOp("checklist.snapshot")--> Core --Invoke--> 模块
//!       <--ModuleInvokeResult.data-----------------┘
//! ```

import { watch } from 'vue';
import {
  currentTeam,
  definePanelExtension,
  moduleMounted,
  registerPanelExtension,
  state,
  teamModuleEnabled,
  teamToolAvailable,
} from '@echo-agent/panel';
import ChecklistOverlay from './components/ChecklistOverlay.vue';
import {
  MODULE_ID,
  SNAPSHOT_METHOD,
  applySnapshot,
  consumeModuleEvent,
  listsFor,
  requestSnapshot,
  resetForReconnect,
  storeRevision,
} from './checklist-store';

/** 当前人格是否**允许**用清单工具（工具级黑白名单 + 包级门控）。 */
function toolAllowed(teamId: string | null): boolean {
  const team = teamId === null ? currentTeam(state) : state.teams.find((item) => item.id === teamId) ?? null;
  if (!team) return false;
  return teamModuleEnabled(team, MODULE_ID) && teamToolAvailable(team, 'checklist');
}

/** 入口是否出现：模块真的挂载 + 人格允许用；或者本地已有投影（只读回看）。
 *
 * 第三个条件不能省：工具被禁 / 模块被停用之后，用户仍应看得到既有清单——
 * 否则"关掉工具"等于"丢掉历史"，这是数据丢失而不是权限控制。
 */
function entryVisible(teamId: string | null): boolean {
  if (moduleMounted(state, MODULE_ID) && toolAllowed(teamId)) return true;
  return listsFor(state.activeRegion ?? null, teamId, null).length > 0;
}

registerPanelExtension(
  definePanelExtension({
    id: MODULE_ID,
    hooks: {
      /** 本模块的帧自己消费（返回 false = 不进壳的通用 reducer）。 */
      acceptEvent(event) {
        const frame = event as Record<string, unknown>;
        const moduleEvent = frame.ModuleEvent as
          | { module?: string; name?: string; payload?: unknown; core?: string | null }
          | undefined;
        if (moduleEvent && consumeModuleEvent(moduleEvent)) return false;

        const result = frame.ModuleInvokeResult as
          | { module?: string; method?: string; ok?: boolean; data?: unknown; error?: string }
          | undefined;
        if (result && result.module === MODULE_ID && result.method === SNAPSHOT_METHOD) {
          if (result.ok) {
            const data = result.data as { team_id?: unknown } | null;
            const team = typeof data?.team_id === 'string' ? data.team_id : null;
            if (team) {
              // 应答本身不带 core（只有模块与方法名）：用当前区域。多上游下
              // 这条应答是**按命令路由**回来的，必然属于发出它的那个区域，
              // 而命令正是本扩展在当前区域发出的——两者一致。
              applySnapshot(state.activeRegion ?? null, team, result.data);
            }
          } else {
            console.warn('[checklist] 快照拉取失败：', result.error ?? '未知原因');
          }
          return false;
        }
        return true;
      },
      /** 人格 / 会话 / 连接变化：补拉权威快照；重连时先清基线。 */
      setup(context) {
        watch(
          () => state.connected,
          (connected, previous) => {
            if (!connected) return;
            if (previous === false) resetForReconnect();
            requestSnapshot(context.core.value, context.teamId.value, context.sessionId.value);
          },
          { immediate: true },
        );
        watch(
          () => [context.core.value, context.teamId.value, context.sessionId.value] as const,
          () => {
            requestSnapshot(context.core.value, context.teamId.value, context.sessionId.value);
          },
        );
      },
    },
    entries: [
      {
        id: 'checklist',
        label: '清单',
        icon: 'list',
        title: '任务清单',
        when: (context) => entryVisible(context.teamId),
        prefetch: (context) => requestSnapshot(context.core, context.teamId, context.sessionId),
      },
    ],
    overlays: [
      {
        id: 'checklist',
        label: '清单',
        component: ChecklistOverlay,
      },
    ],
  }),
);

// `storeRevision` 只用于让门控在数据到达后重算（`when` 读的是非响应式 Map，
// 组件通过这个 ref 建依赖）。
export { storeRevision };
