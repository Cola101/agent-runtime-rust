<script setup lang="ts">
import { computed } from 'vue'
import type { RunStatus } from '../types/runtime'

const props = defineProps<{ status: RunStatus }>()

const labels: Record<RunStatus, string> = {
  queued: '排队中',
  running: '运行中',
  waiting_approval: '等待审批',
  suspended: '已休眠',
  succeeded: '已完成',
  failed: '失败',
  cancelled: '已取消',
  timed_out: '已超时',
  indeterminate: '结果不确定',
}

const label = computed(() => labels[props.status])
</script>

<template>
  <span
    class="status"
    :class="`status--${status}`"
    data-testid="run-status"
  >
    <span
      class="status__dot"
      aria-hidden="true"
    />
    {{ label }}
  </span>
</template>

<style scoped>
.status {
  align-items: center;
  background: var(--status-bg, #eff3f8);
  border-radius: 999px;
  color: var(--status-fg, #38506a);
  display: inline-flex;
  font-size: 0.78rem;
  font-weight: 650;
  gap: 0.42rem;
  padding: 0.36rem 0.62rem;
  white-space: nowrap;
}

.status__dot { background: currentColor; border-radius: 50%; height: 0.42rem; width: 0.42rem; }
.status--running { --status-bg: #e8f6ef; --status-fg: #13734a; }
.status--waiting_approval { --status-bg: #fff4dc; --status-fg: #8a5a00; }
.status--succeeded { --status-bg: #e9f4ff; --status-fg: #075eaa; }
.status--failed, .status--indeterminate, .status--timed_out { --status-bg: #ffebed; --status-fg: #a32938; }
</style>
