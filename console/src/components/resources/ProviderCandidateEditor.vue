<script setup lang="ts">
import type { ModelProviderDraft, ProviderProtocol } from '../../types/resources'

const props = defineProps<{
  candidate: ModelProviderDraft
  index: number
  disabled: boolean
  removable: boolean
}>()

const emit = defineEmits<{
  update: [candidate: ModelProviderDraft]
  remove: []
}>()

function update<K extends keyof ModelProviderDraft>(field: K, value: ModelProviderDraft[K]) {
  emit('update', { ...props.candidate, [field]: value })
}

function textValue(event: Event): string {
  return (event.target as HTMLInputElement).value
}

function protocolValue(event: Event): ProviderProtocol {
  return (event.target as HTMLSelectElement).value as ProviderProtocol
}
</script>

<template>
  <fieldset class="provider">
    <legend class="provider__legend">
      <span>{{ index === 0 ? '主要 Provider' : `备用 Provider ${index}` }}</span>
      <button
        v-if="removable"
        type="button"
        class="provider__remove"
        :disabled="disabled"
        :aria-label="`移除备用 Provider ${index}`"
        @click="emit('remove')"
      >
        移除
      </button>
    </legend>

    <div class="provider__grid">
      <label class="field">
        <span>名称</span>
        <input
          :name="`providerName-${index}`"
          :value="candidate.name"
          maxlength="200"
          :disabled="disabled"
          placeholder="例如：主要模型"
          required
          @input="update('name', textValue($event))"
        >
      </label>
      <label class="field">
        <span>协议</span>
        <select
          :name="`providerProtocol-${index}`"
          :value="candidate.protocol"
          :disabled="disabled"
          required
          @change="update('protocol', protocolValue($event))"
        >
          <option value="openai_compatible">OpenAI-compatible Chat Completions</option>
          <option value="openai_responses">OpenAI Responses</option>
          <option value="anthropic_messages">Anthropic Messages</option>
        </select>
      </label>
      <label class="field provider__endpoint">
        <span>Endpoint</span>
        <input
          :name="`providerEndpoint-${index}`"
          :value="candidate.endpoint"
          maxlength="2048"
          inputmode="url"
          :disabled="disabled"
          placeholder="https://provider.example/v1/responses"
          required
          @input="update('endpoint', textValue($event))"
        >
      </label>
      <label class="field">
        <span>模型</span>
        <input
          :name="`providerModel-${index}`"
          :value="candidate.model"
          maxlength="200"
          :disabled="disabled"
          placeholder="模型标识"
          required
          @input="update('model', textValue($event))"
        >
      </label>
      <label class="field">
        <span>API Key</span>
        <input
          :name="`providerApiKey-${index}`"
          :value="candidate.apiKey"
          type="password"
          maxlength="8192"
          autocomplete="new-password"
          spellcheck="false"
          :disabled="disabled"
          placeholder="仅本次提交使用"
          required
          @input="update('apiKey', textValue($event))"
        >
      </label>
    </div>
    <p class="provider__notice">
      API Key 只作为写入输入，页面不会再次读取；Worker 不会获得明文。
    </p>
  </fieldset>
</template>

<style scoped>
.provider { border: 1px solid #b8cdd4; border-radius: 14px; margin: 0; padding: 1rem; }
.provider__legend { align-items: center; color: #24475b; display: flex; font-size: .85rem; font-weight: 750; gap: 1rem; padding: 0 .35rem; width: calc(100% - .7rem); }
.provider__remove { background: transparent; border: 0; color: #9c2e3c; cursor: pointer; font-size: .75rem; font-weight: 700; margin-left: auto; min-height: 36px; padding: .35rem .5rem; }
.provider__remove:disabled { cursor: not-allowed; opacity: .5; }
.provider__remove:focus-visible { outline: 3px solid #eebbc1; outline-offset: 1px; }
.provider__grid { display: grid; gap: .8rem; grid-template-columns: repeat(2, minmax(0, 1fr)); }
.provider__endpoint { grid-column: 1 / -1; }
.field { color: #415064; display: grid; font-size: .8rem; font-weight: 650; gap: .42rem; }
.field select, .field input { background: #fff; border: 1px solid #b8cdd4; border-radius: 10px; color: #172536; min-height: 44px; padding: .7rem .8rem; width: 100%; }
.field select:focus-visible, .field input:focus-visible { border-color: #167072; outline: 3px solid #bfe7e5; outline-offset: 1px; }
.provider__notice { color: #657787; font-size: .74rem; margin: .75rem 0 0; }
@media (max-width: 720px) {
  .provider__grid { grid-template-columns: 1fr; }
  .provider__endpoint { grid-column: auto; }
}
</style>
