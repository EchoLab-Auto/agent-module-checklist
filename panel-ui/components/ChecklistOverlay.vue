<script setup lang="ts">
//! 清单面板（入口行按钮打开的覆盖层）。
//!
//! 本组件属于**模块**，不属于面板壳：壳只给它 `teamId` / `sessionId` 两个
//! prop 和一个 `SessionOverlay` 容器，剩下的（读哪个 store、怎么排、长什么样）
//! 全是模块自己的事。所以这里没有任何 `state.checklists` 之类的壳内字段。

import { computed, watch } from 'vue';
import { state } from '@echo-agent/panel';
import PanelIcon from '@echo-agent/panel/components/PanelIcon';
import { isSynced, listsFor, requestSnapshot, storeRevision } from '../checklist-store';

const props = defineProps<{
  teamId: string | null;
  sessionId: string;
}>();

const core = computed(() => state.activeRegion ?? null);

// 打开（组件挂载）/ 人格或会话变化时补拉权威快照：快照是"当前真值"的唯一
// 来源，本地投影可能来自别的连接周期。
watch(
  () => [core.value, props.teamId, props.sessionId, storeRevision.value] as const,
  () => requestSnapshot(core.value, props.teamId, props.sessionId),
  { immediate: true },
);

const projections = computed(() => listsFor(core.value, props.teamId, props.sessionId));
const lists = computed(() => projections.value.flatMap((p) => p.lists));
const aggregated = computed(() => props.sessionId === 'global');
const synced = computed(() => isSynced(core.value, props.teamId));

function sourceLabel(session: string): string {
  // 会话 id 形如 `local:workspace:echo-agent/conv:local_user`：只展示区分度
  // 最高的两段（中间与末尾），避免把整串塞进一行。
  const parts = session.split(':').filter(Boolean);
  if (parts.length <= 2) return session;
  return `${parts[0]}:${parts[parts.length - 1]}`;
}
</script>

<template>
  <div class="checklist-panel">
    <div v-if="!synced" class="checklist-panel__unsynced" title="尚未收到模块的权威快照：当前显示可能不是最新">
      未同步
    </div>
    <p v-if="lists.length === 0" class="checklist-panel__empty">（暂无清单）</p>
    <div
      v-for="(list, index) in lists"
      :key="`${list.name}-${index}`"
      class="checklist-panel__card"
    >
      <div class="checklist-panel__head">
        <span class="checklist-panel__name" :class="{ 'is-archived': list.archived }">
          {{ list.name }}<span v-if="list.archived" class="checklist-panel__archived">（已归档）</span>
        </span>
        <span class="checklist-panel__progress">{{ list.done }}/{{ list.total }}</span>
      </div>
      <div v-if="aggregated" class="checklist-panel__source">
        {{ sourceLabel(projections[index]?.session ?? '') }}
      </div>
      <div class="checklist-panel__bar">
        <div
          class="checklist-panel__bar-fill"
          :style="{ width: `${list.total ? (list.done / list.total) * 100 : 0}%` }"
        />
      </div>
      <div
        v-for="(item, itemIndex) in list.items"
        :key="`${item.id ?? itemIndex}`"
        class="checklist-panel__item"
        :class="{ 'is-done': item.done }"
      >
        <PanelIcon
          :name="item.done ? 'check-square' : 'square'"
          size="sm"
          :tone="item.done ? 'ok' : 'muted'"
          class="checklist-panel__item-icon"
        />
        <span class="checklist-panel__item-text">{{ item.text }}</span>
      </div>
    </div>
  </div>
</template>

<style scoped>
/* 样式随组件走：模块提供内容也提供外观，壳不写死任何清单的视觉。 */
.checklist-panel {
  display: flex;
  flex-direction: column;
  gap: 10px;
  max-width: 640px;
  padding: 4px 2px;
}
.checklist-panel__unsynced {
  align-self: flex-start;
  font-size: 11px;
  padding: 1px 6px;
  border-radius: 6px;
  background: var(--panel-hover);
  color: var(--nm-text-secondary);
}
.checklist-panel__empty {
  margin: 0;
  font-size: 12px;
  color: var(--nm-text-secondary);
}
.checklist-panel__card {
  border-radius: 10px;
  padding: 10px 12px;
  background: var(--panel-surface, transparent);
  border: 1px solid var(--panel-hover);
}
.checklist-panel__head {
  display: flex;
  justify-content: space-between;
  gap: 8px;
}
.checklist-panel__name {
  font-weight: 700;
}
.checklist-panel__name.is-archived {
  color: var(--nm-text-secondary);
}
.checklist-panel__archived {
  font-size: 11px;
  font-weight: 400;
}
.checklist-panel__progress {
  color: var(--nm-text-secondary);
}
.checklist-panel__source {
  font-size: 11px;
  color: var(--nm-text-secondary);
  margin-top: 2px;
}
.checklist-panel__bar {
  height: 6px;
  border-radius: 3px;
  background: var(--panel-hover);
  margin: 6px 0;
  overflow: hidden;
}
.checklist-panel__bar-fill {
  height: 100%;
  border-radius: 3px;
  background: var(--panel-ok);
}
.checklist-panel__item {
  display: flex;
  align-items: flex-start;
  gap: 6px;
  font-size: 13px;
  line-height: 1.45;
}
.checklist-panel__item.is-done {
  color: var(--nm-text-secondary);
}
.checklist-panel__item.is-done .checklist-panel__item-text {
  text-decoration: line-through;
}
.checklist-panel__item-icon {
  margin-top: 1px;
}
.checklist-panel__item-text {
  min-width: 0;
  overflow-wrap: anywhere;
}
</style>
