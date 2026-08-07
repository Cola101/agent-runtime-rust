<script setup lang="ts">
import ApprovalCard from './ApprovalCard.vue'
import { useApprovals } from '../../composables/useApprovals'

const {
  approvals,
  loading,
  decidingId,
  error,
  notice,
  refresh,
  decide,
} = useApprovals()
</script>

<template>
  <section
    class="approval-inbox"
    aria-labelledby="approval-inbox-title"
  >
    <header class="approval-inbox__heading">
      <div>
        <p class="approval-inbox__eyebrow">
          Human checkpoint
        </p>
        <h2 id="approval-inbox-title">
          待审批 Tool
        </h2>
        <p>核对执行参数与副作用，再决定是否放行。</p>
      </div>
      <button
        type="button"
        class="approval-inbox__refresh"
        :disabled="loading"
        @click="refresh"
      >
        刷新审批
      </button>
    </header>
    <p
      v-if="notice"
      class="notice notice--success"
      role="status"
    >
      {{ notice }}
    </p>
    <p
      v-if="loading"
      class="notice"
      role="status"
    >
      正在载入待审批项…
    </p>
    <p
      v-else-if="error"
      class="notice notice--error"
      role="alert"
    >
      {{ error }}
    </p>
    <p
      v-else-if="approvals.length === 0"
      class="approval-inbox__empty"
    >
      当前没有待审批项。
    </p>
    <div
      v-else
      class="approval-inbox__list"
    >
      <ApprovalCard
        v-for="approval in approvals"
        :key="approval.id"
        :approval="approval"
        :deciding="decidingId === approval.id"
        @decide="decide"
      />
    </div>
  </section>
</template>

<style scoped>
.approval-inbox { background: #edf4f7; border: 1px solid #dbe7ed; border-radius: 22px; margin: 2rem 0; padding: 1.35rem; }
.approval-inbox__heading { align-items: end; display: flex; gap: 1.5rem; justify-content: space-between; margin-bottom: 1rem; }
.approval-inbox__eyebrow { color: #8a5a00 !important; font: 700 .7rem/1.4 ui-monospace, SFMono-Regular, Menlo, monospace; letter-spacing: .1em; margin: 0 0 .35rem !important; text-transform: uppercase; }
.approval-inbox__heading h2 { color: #172536; font-size: 1.35rem; margin: 0; }
.approval-inbox__heading p { color: #60758a; margin: .45rem 0 0; }
.approval-inbox__refresh { background: #fff; border: 1px solid #bdd0dc; border-radius: 10px; color: #23465e; cursor: pointer; flex: 0 0 auto; font-weight: 700; min-height: 44px; padding: .65rem .9rem; }
.approval-inbox__refresh:disabled { cursor: wait; opacity: .6; }
.approval-inbox__refresh:focus-visible { outline: 3px solid #78bce8; outline-offset: 3px; }
.approval-inbox__list { display: grid; gap: 1rem; grid-template-columns: repeat(2, minmax(0, 1fr)); }
.approval-inbox__empty { color: #526a7c; margin: .5rem 0 0; padding: 1rem 0; text-align: center; }
.notice { background: #f8fbfd; border-radius: 12px; color: #486178; padding: .85rem; }
.notice--error { background: #fff0f1; color: #9c2e3c; }
.notice--success { background: #e8f6ef; color: #13734a; }
@media (max-width: 800px) { .approval-inbox__list { grid-template-columns: 1fr; } }
@media (max-width: 560px) {
  .approval-inbox { margin: 1.5rem 0; padding: 1rem; }
  .approval-inbox__heading { align-items: stretch; flex-direction: column; gap: .8rem; }
  .approval-inbox__refresh { width: 100%; }
}
</style>
