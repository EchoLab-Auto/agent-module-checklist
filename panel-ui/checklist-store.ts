//! 清单在**面板扩展**里的投影。
//!
//! 这份状态不归 Panel 壳：壳只提供"事件订阅 + 命令发送 + 插槽"三件事，
//! 清单的数据形状、版本判定、同步基线全在这里。Core 与 Panel 都不认识
//! `lists` / `epoch` / `state` 这些词——它们只看到 `ModuleEvent` 的自由 JSON
//! 和 `ModuleInvokeResult.data`。
//!
//! ## 两条数据来源
//!
//! | 来源 | 时机 | 语义 |
//! |---|---|---|
//! | `checklist.snapshot` 调用 | 打开面板 / 切人格 / 重连 | **权威全量**：`states` 是完整集合，不在其中的会话没有清单 |
//! | `checklist.updated` 事件 | 每次工具写入之后 | **单会话投影**：只更新那一个会话 |
//!
//! ## 版本判定（照抄旧实现的不变量，别简化掉）
//!
//! 1. live 投影同 `epoch` 内按 `revision` 单调递增，低版本的旧事件丢弃；
//!    `epoch` 变了 = 换了一条时间线（`reset_session`），直接接受。
//! 2. full 快照按**顶层** `revision` 单调递增：低于已见 watermark 的 full
//!    不得覆盖更新的 live（Core 重启后 revision 归零，重连基线会清零，
//!    见 `resetForReconnect`）。
//! 3. 空 `states` 也是权威：它表示"这个人格没有任何清单"，必须整体清掉
//!    该人格的旧投影——否则用户删完清单后界面永远留着上一份。

import { ref } from 'vue';
import { sendCommand } from '@echo-agent/panel';

export const MODULE_ID = 'echo-agent.checklist';
/** 模块主动广播的事件名（与模块侧 `serve::STATE_EVENT` 同值）。 */
export const STATE_EVENT = 'checklist.updated';
/** 模块对外方法名（与模块侧 `commands::SNAPSHOT_METHOD` 同值）。 */
export const SNAPSHOT_METHOD = 'checklist.snapshot';

export interface ChecklistItem {
  id?: string;
  text: string;
  done: boolean;
}

export interface ChecklistList {
  id?: string;
  name: string;
  archived?: boolean;
  done: number;
  total: number;
  items: ChecklistItem[];
}

/** 单会话投影。 */
interface SessionProjection {
  lists: ChecklistList[];
  epoch: string;
  revision: number;
  /** 会话来源标签（聚合视图里区分"这份清单属于哪个会话"）。 */
  session: string;
}

/** 人格级同步基线。 */
interface PersonaState {
  /** 已见到的最高文件级 revision（full 快照不得低于它）。 */
  watermark: number;
  /** 是否收到过权威 full 快照（false = 显示"未同步"）。 */
  synced: boolean;
}

const sessions = new Map<string, SessionProjection>();
const personas = new Map<string, PersonaState>();
let lastRequest: { key: string; at: number } | null = null;

/** 结构版本号：组件对它的依赖驱动重算（Map 本身不是响应式的）。 */
export const storeRevision = ref(0);

function bump(): void {
  storeRevision.value += 1;
}

function coreKey(core: string | null | undefined): string {
  return core ?? '';
}

function sessionKey(core: string | null | undefined, team: string, session: string): string {
  return `${coreKey(core)}\u0000${team}\u0000${session}`;
}

function personaKey(core: string | null | undefined, team: string): string {
  return `${coreKey(core)}\u0000${team}`;
}

function personaOf(core: string | null | undefined, team: string): PersonaState {
  const key = personaKey(core, team);
  let state = personas.get(key);
  if (!state) {
    state = { watermark: -1, synced: false };
    personas.set(key, state);
  }
  return state;
}

function normalizeState(raw: unknown): { lists: ChecklistList[]; epoch: string; revision: number } | null {
  if (!raw || typeof raw !== 'object') return null;
  const value = raw as { lists?: unknown; epoch?: unknown; revision?: unknown };
  if (!Array.isArray(value.lists)) return null;
  if (typeof value.epoch !== 'string' || typeof value.revision !== 'number') return null;
  const lists: ChecklistList[] = [];
  for (const entry of value.lists) {
    if (!entry || typeof entry !== 'object') continue;
    const list = entry as Partial<ChecklistList> & { items?: unknown };
    if (typeof list.name !== 'string') continue;
    const items: ChecklistItem[] = [];
    for (const rawItem of Array.isArray(list.items) ? list.items : []) {
      if (!rawItem || typeof rawItem !== 'object') continue;
      const item = rawItem as Partial<ChecklistItem>;
      if (typeof item.text !== 'string') continue;
      items.push({ id: item.id, text: item.text, done: item.done === true });
    }
    lists.push({
      id: list.id,
      name: list.name,
      archived: list.archived === true,
      done: typeof list.done === 'number' ? list.done : items.filter((i) => i.done).length,
      total: typeof list.total === 'number' ? list.total : items.length,
      items,
    });
  }
  return { lists, epoch: value.epoch, revision: value.revision };
}

/** 单会话 live 投影：`checklist.updated`。 */
export function applyLive(
  core: string | null | undefined,
  team: string,
  session: string,
  raw: unknown,
): void {
  const state = normalizeState(raw);
  if (!state) return;
  const key = sessionKey(core, team, session);
  const previous = sessions.get(key);
  if (previous && previous.epoch === state.epoch && state.revision < previous.revision) return;
  sessions.set(key, { ...state, session });
  const persona = personaOf(core, team);
  persona.watermark = Math.max(persona.watermark, state.revision);
  bump();
}

/** 权威全量：`checklist.snapshot`。 */
export function applySnapshot(
  core: string | null | undefined,
  team: string,
  raw: unknown,
): void {
  if (!raw || typeof raw !== 'object') return;
  const payload = raw as { states?: unknown; revision?: unknown };
  if (!Array.isArray(payload.states) || typeof payload.revision !== 'number') return;
  const persona = personaOf(core, team);
  // 低 revision 的 full 不得覆盖更高的 live（Core 重启会归零，由重连基线处理）。
  if (payload.revision < persona.watermark) return;
  const prefix = `${coreKey(core)}\u0000${team}\u0000`;
  for (const key of [...sessions.keys()]) {
    if (key.startsWith(prefix)) sessions.delete(key);
  }
  for (const entry of payload.states) {
    if (!entry || typeof entry !== 'object') continue;
    const item = entry as { session_id?: unknown; state?: unknown };
    if (typeof item.session_id !== 'string') continue;
    const state = normalizeState(item.state);
    if (!state) continue;
    sessions.set(sessionKey(core, team, item.session_id), { ...state, session: item.session_id });
  }
  persona.watermark = payload.revision;
  persona.synced = true;
  bump();
}

/** 当前会话的清单（`session === 'global'` 时聚合该人格全部会话）。 */
export function listsFor(
  core: string | null | undefined,
  team: string | null,
  session: string | null,
): SessionProjection[] {
  void storeRevision.value;
  if (!team) return [];
  const prefix = `${coreKey(core)}\u0000${team}\u0000`;
  const result: SessionProjection[] = [];
  for (const [key, value] of sessions) {
    if (!key.startsWith(prefix)) continue;
    if (session && session !== 'global' && value.session !== session) continue;
    result.push(value);
  }
  return result;
}

/** 该人格是否已收到权威快照（false → 界面标"未同步"）。 */
export function isSynced(core: string | null | undefined, team: string | null): boolean {
  void storeRevision.value;
  if (!team) return false;
  return personaOf(core, team).synced;
}

/** 重连基线归零：新连接可能连到**重启过的** Core（revision 从 0 重新开始），
 * 旧 watermark 会永久压住新快照。已展示的投影保留（标脏），由这次连接的
 * 权威快照重建基线。 */
export function resetForReconnect(): void {
  for (const state of personas.values()) {
    state.watermark = -1;
    state.synced = false;
  }
  bump();
}

/** 请求权威快照。`team` 必填（人格是清单的归属单位）。
 *
 * 拉的是**整人格**快照（`states` 是完整集合），会话过滤在本地做：权威语义
 * 要求"不在快照里的会话 = 没有清单"，只拉单个会话就判不出这一点。
 */
export function requestSnapshot(
  core: string | null | undefined,
  team: string | null,
  _session: string | null,
): void {
  if (!team) return;
  const key = personaKey(core, team);
  const now = Date.now();
  // 轻节流：hover / 打开 / 切会话可能连着触发，短时间内只发一次。
  if (lastRequest && lastRequest.key === key && now - lastRequest.at < 500) return;
  lastRequest = { key, at: now };
  sendCommand({
    InvokeModuleOp: {
      module: MODULE_ID,
      method: SNAPSHOT_METHOD,
      payload: { team_id: team },
    },
  });
}

/** 注入来自 `ModuleEvent` 的一帧（返回 true = 已被清单消费）。 */
export function consumeModuleEvent(frame: {
  module?: string;
  name?: string;
  payload?: unknown;
  core?: string | null;
}): boolean {
  if (frame.module !== MODULE_ID || frame.name !== STATE_EVENT) return false;
  const payload = frame.payload as { session_id?: unknown; team_id?: unknown; state?: unknown } | null;
  if (!payload || typeof payload !== 'object') return false;
  const team = typeof payload.team_id === 'string' ? payload.team_id : '';
  const session = typeof payload.session_id === 'string' ? payload.session_id : '';
  if (!team || !session) return false;
  applyLive(frame.core ?? null, team, session, payload.state);
  return true;
}
