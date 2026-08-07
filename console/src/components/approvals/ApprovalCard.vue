<script setup lang="ts">
import { computed } from 'vue'
import type { ApprovalDecision, ApprovalSummary } from '../../types/runtime'

const props = defineProps<{
  approval: ApprovalSummary
  deciding: boolean
}>()

const emit = defineEmits<{
  decide: [approval: ApprovalSummary, decision: ApprovalDecision]
}>()

const effectLabels: Record<ApprovalSummary['effect'], string> = {
  pure: '只读 / 无副作用',
  idempotent: '可安全重试',
  non_idempotent: '不可自动重试',
  unknown: '副作用未知',
}

const sandboxLabels: Record<ApprovalSummary['sandbox'], string> = {
  restricted_container: '受限容器',
  kata: 'Kata 强隔离',
  trusted_native: '本机可信进程',
}

const formattedArguments = computed(() => JSON.stringify(props.approval.arguments, null, 2))
const canAllowSession = computed(() =>
  props.approval.availableDecisions.includes('allow_session'),
)
</script>

<template>
  <article class="approval-card">
    <header class="approval-card__heading">
      <div>
        <p class="approval-card__agent">
          {{ approval.agentName }}
        </p>
        <h3>{{ approval.toolName }}</h3>
      </div>
      <span class="approval-card__state">等待决定</span>
    </header>
    <dl class="approval-card__facts">
      <div><dt>工作区</dt><dd>{{ approval.workspaceName }}</dd></div>
      <div><dt>副作用</dt><dd>{{ effectLabels[approval.effect] }}</dd></div>
      <div><dt>执行环境</dt><dd>{{ sandboxLabels[approval.sandbox] }}</dd></div>
      <div><dt>请求时间</dt><dd><time :datetime="approval.createdAt">{{ approval.createdAt }}</time></dd></div>
    </dl>
    <div class="approval-card__arguments">
      <p>待执行参数</p>
      <pre>{{ formattedArguments }}</pre>
    </div>
    <details class="approval-card__binding">
      <summary>查看不可变绑定</summary>
      <dl>
        <div><dt>Run</dt><dd><code>{{ approval.runId }}</code></dd></div>
        <div><dt>Tool Call</dt><dd><code>{{ approval.toolCallId }}</code></dd></div>
        <div><dt>绑定摘要</dt><dd><code>{{ approval.bindingDigest }}</code></dd></div>
        <div v-if="approval.policyDigest">
          <dt>策略摘要</dt><dd><code>{{ approval.policyDigest }}</code></dd>
        </div>
        <div v-if="approval.sessionScopeDigest">
          <dt>会话范围摘要</dt><dd><code>{{ approval.sessionScopeDigest }}</code></dd>
        </div>
        <div><dt>审批版本</dt><dd>{{ approval.version }}</dd></div>
      </dl>
    </details>
    <p
      v-if="canAllowSession"
      class="approval-card__session-note"
    >
      会话授权仅在参数、Agent 版本和 Tool 策略完全一致时复用；任何变化都会再次询问。
    </p>
    <footer class="approval-card__actions">
      <button
        type="button"
        class="button button--deny"
        data-testid="approval-deny"
        :disabled="deciding"
        @click="emit('decide', approval, 'deny')"
      >
        拒绝
      </button>
      <button
        v-if="canAllowSession"
        type="button"
        class="button button--allow-session"
        data-testid="approval-allow-session"
        :disabled="deciding"
        title="仅对本会话内完全相同的参数、Agent 版本和 Tool 策略生效"
        @click="emit('decide', approval, 'allow_session')"
      >
        {{ deciding ? '正在提交…' : '本会话相同请求' }}
      </button>
      <button
        type="button"
        class="button button--allow"
        data-testid="approval-allow-once"
        :disabled="deciding"
        @click="emit('decide', approval, 'allow_once')"
      >
        {{ deciding ? '正在提交…' : '仅允许本次' }}
      </button>
    </footer>
  </article>
</template>

<style scoped>
.approval-card { background: #fff; border: 1px solid #dce6ee; border-radius: 18px; box-shadow: 0 10px 28px rgb(25 66 93 / 8%); padding: 1.25rem; }
.approval-card__heading { align-items: start; display: flex; gap: 1rem; justify-content: space-between; }
.approval-card__agent { color: #60758a; font-size: .82rem; font-weight: 650; margin: 0 0 .3rem; }
.approval-card__heading h3 { color: #172536; font: 700 1.08rem/1.35 ui-monospace, SFMono-Regular, Menlo, monospace; margin: 0; overflow-wrap: anywhere; }
.approval-card__state { background: #fff4dc; border-radius: 999px; color: #8a5a00; flex: 0 0 auto; font-size: .76rem; font-weight: 700; padding: .35rem .62rem; }
.approval-card__facts { display: grid; gap: .8rem; grid-template-columns: repeat(2, minmax(0, 1fr)); margin: 1.15rem 0; }
.approval-card__facts div { min-width: 0; }
.approval-card__facts dt, .approval-card__arguments p, .approval-card__binding dt { color: #718196; font-size: .72rem; font-weight: 700; letter-spacing: .04em; margin: 0 0 .25rem; text-transform: uppercase; }
.approval-card__facts dd, .approval-card__binding dd { color: #26384a; margin: 0; overflow-wrap: anywhere; }
.approval-card__arguments { background: #f4f7f9; border-radius: 12px; padding: .85rem; }
.approval-card__arguments pre { color: #24384a; font: 500 .82rem/1.55 ui-monospace, SFMono-Regular, Menlo, monospace; margin: .45rem 0 0; overflow-x: auto; white-space: pre-wrap; word-break: break-word; }
.approval-card__binding { color: #4e6478; margin-top: .9rem; }
.approval-card__binding summary { cursor: pointer; font-size: .86rem; font-weight: 650; min-height: 44px; padding: .65rem 0; }
.approval-card__binding dl { display: grid; gap: .65rem; margin: .4rem 0 0; }
.approval-card__binding code { overflow-wrap: anywhere; }
.approval-card__session-note { color: #49685d; font-size: .82rem; line-height: 1.5; margin: .7rem 0 0; }
.approval-card__actions { display: flex; gap: .75rem; justify-content: flex-end; margin-top: 1.1rem; }
.button { border: 0; border-radius: 10px; cursor: pointer; font-weight: 700; min-height: 44px; padding: .7rem 1rem; }
.button:disabled { cursor: wait; opacity: .6; }
.button:focus-visible { outline: 3px solid #78bce8; outline-offset: 3px; }
.button--deny { background: #f7e9eb; color: #8f2936; }
.button--allow { background: #126446; color: #fff; }
.button--allow-session { background: #e4f0eb; color: #14563f; }
@media (max-width: 560px) {
  .approval-card__facts { grid-template-columns: 1fr; }
  .approval-card__actions { display: grid; grid-template-columns: 1fr; }
}
</style>
