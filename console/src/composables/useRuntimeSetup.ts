import { onMounted, readonly, ref, shallowRef } from 'vue'
import {
  createAgent,
  createAgentVersion,
  createModelPolicy,
  createModelProvider,
  createSession,
  createWorkspace,
  fetchResourceContext,
  publishSkillVersion,
} from '../api/resourceApi'
import type {
  ResourceContext,
  RuntimeSetupDraft,
  RuntimeSetupResult,
} from '../types/resources'

export function useRuntimeSetup() {
  const context = ref<ResourceContext | null>(null)
  const loading = shallowRef(true)
  const submitting = shallowRef(false)
  const completedSteps = shallowRef(0)
  const error = shallowRef<string | null>(null)

  async function loadContext() {
    loading.value = true
    error.value = null
    try {
      context.value = await fetchResourceContext()
    } catch {
      context.value = null
      error.value = '无法载入当前应用与项目，请检查登录授权。'
    } finally {
      loading.value = false
    }
  }

  async function provision(draft: RuntimeSetupDraft): Promise<RuntimeSetupResult | null> {
    submitting.value = true
    completedSteps.value = 0
    error.value = null
    try {
      const workspace = await createWorkspace({
        projectId: draft.projectId,
        name: draft.workspaceName,
      })
      completedSteps.value = 1

      const agent = await createAgent({ workspaceId: workspace.id, name: draft.agentName })
      completedSteps.value = 2

      const skill = await publishSkillVersion(draft.skill)
      completedSteps.value = 3

      const version = await createAgentVersion(agent.id, {
        instructions: draft.instructions,
        delegatedScopes: draft.delegatedScopes,
        skillVersionIds: [skill.id],
      })
      completedSteps.value = 4

      const providerIds: string[] = []
      for (const providerDraft of draft.providers) {
        const provider = await createModelProvider(providerDraft)
        providerIds.push(provider.id)
        completedSteps.value += 1
      }

      const policy = await createModelPolicy({
        workspaceId: workspace.id,
        name: draft.modelPolicyName,
        routing: draft.routing,
        providerIds,
      })
      completedSteps.value += 1

      const session = await createSession({
        workspaceId: workspace.id,
        title: draft.sessionTitle,
      })
      completedSteps.value += 1

      return {
        target: {
          sessionId: session.id,
          workspaceId: workspace.id,
          workspaceName: workspace.name,
          agentVersionId: version.id,
          agentName: agent.name,
          agentVersion: version.version,
          modelPolicyId: policy.id,
          modelPolicyName: policy.name,
        },
      }
    } catch {
      error.value = completedSteps.value === 0
        ? '配置创建失败，尚未创建任何资源。'
        : `配置在第 ${completedSteps.value + 1} 步失败；前 ${completedSteps.value} 个资源已保留。`
      return null
    } finally {
      submitting.value = false
    }
  }

  onMounted(loadContext)
  return {
    context: readonly(context),
    loading: readonly(loading),
    submitting: readonly(submitting),
    completedSteps: readonly(completedSteps),
    error: readonly(error),
    loadContext,
    provision,
  }
}
