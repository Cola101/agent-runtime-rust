<script setup lang="ts">
import { computed, shallowRef, useId } from 'vue'

const props = withDefaults(defineProps<{
  submitting?: boolean
  error?: string | null
  acceptedId?: string | null
}>(), {
  submitting: false,
  error: null,
  acceptedId: null,
})
const emit = defineEmits<{ submit: [input: string] }>()

const inputId = useId()
const input = shallowRef('')
const localError = shallowRef<string | null>(null)
const byteLength = computed(() => new TextEncoder().encode(input.value).length)
const canSubmit = computed(() => input.value.trim().length > 0 && byteLength.value <= 32_768 && !props.submitting)

function submit() {
  localError.value = null
  if (input.value.trim().length === 0) {
    localError.value = '请输入新的运行指令。'
    return
  }
  if (byteLength.value > 32_768) {
    localError.value = '指令不能超过 32,768 个 UTF-8 字节。'
    return
  }
  emit('submit', input.value.trim())
}
</script>

<template>
  <form
    class="steering"
    aria-label="调整运行指令"
    @submit.prevent="submit"
  >
    <label
      class="steering__label"
      :for="inputId"
    >调整指令</label>
    <textarea
      :id="inputId"
      v-model="input"
      name="steeringInput"
      rows="2"
      :disabled="submitting"
      placeholder="停止当前模型输出，并继续执行新指令"
    />
    <div class="steering__footer">
      <small>{{ byteLength.toLocaleString('en-US') }} / 32,768 字节</small>
      <button
        type="submit"
        :disabled="!canSubmit"
      >
        {{ submitting ? '发送中…' : '调整' }}
      </button>
    </div>
    <p
      v-if="localError || error"
      class="steering__error"
      role="alert"
    >
      {{ localError || error }}
    </p>
    <p
      v-else-if="acceptedId"
      class="steering__success"
      role="status"
    >
      指令已受理：{{ acceptedId }}
    </p>
  </form>
</template>

<style scoped>
.steering { min-width: 230px; }
.steering__label { color: #314a61; display: block; font-size: .76rem; font-weight: 700; margin-bottom: .35rem; }
.steering textarea { border: 1px solid #cbd7e2; border-radius: 8px; box-sizing: border-box; color: #253548; font: inherit; padding: .55rem .65rem; resize: vertical; width: 100%; }
.steering textarea:focus-visible { border-color: #1674ad; outline: 3px solid #c8e7f8; }
.steering__footer { align-items: center; display: flex; gap: .5rem; justify-content: space-between; margin-top: .4rem; }
.steering__footer small { color: #728196; font-size: .68rem; }
.steering button { background: #123d5a; border: 0; border-radius: 8px; color: white; cursor: pointer; font-weight: 650; padding: .45rem .7rem; }
.steering button:disabled { cursor: not-allowed; opacity: .5; }
.steering__error, .steering__success { font-size: .72rem; margin: .45rem 0 0; }
.steering__error { color: #9c2e3c; }
.steering__success { color: #13734a; overflow-wrap: anywhere; }
</style>
