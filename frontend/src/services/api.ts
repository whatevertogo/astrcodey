import { getHostBridge } from '../lib/hostBridge'
import { isTauriEnvironment } from '../lib/tauri'
import {
  decodeActiveSelectionResponse,
  decodeApplyProviderPresetResponse,
  decodeAvailableModels,
  decodeCommandCompletionResponse,
  decodeCommandInvokeResponse,
  decodeConfigReloadResponse,
  decodeConfigView,
  decodeConversationSnapshot,
  decodeCreateSessionResponse,
  decodeCurrentModelInfo,
  decodeDeleteProjectResponse,
  decodeExtensionListResponse,
  decodeExtensionReloadResponse,
  decodeModelTestResult,
  decodeProviderCatalog,
  decodePromptSubmitResponse,
  decodeRemoveProviderPresetResponse,
  decodeSetExtensionEnabledResponse,
  decodeSlashCommandListResponse,
  decodeSessionListResponse,
} from './protocol'
import type {
  CreateSessionResponse,
  CommandCompletionResponse,
  CommandInvokeResponse,
  PromptAttachmentWire,
  PromptSubmitResponse,
  SessionListResponse,
  ConversationSnapshot,
  ApplyProviderPresetRequest,
  ApplyProviderPresetResponse,
  ConfigView,
  CurrentModelInfo,
  AvailableModel,
  ModelTestResult,
  ProviderCatalogView,
  RemoveProviderPresetResponse,
  SlashCommandListResponse,
  ExtensionStateView,
} from './types'

let baseUrl = ''
let authToken = ''

let _tauriFetch: typeof window.fetch | null = null

async function resolveFetch(): Promise<typeof window.fetch> {
  if (isTauriEnvironment() && !_tauriFetch) {
    const { fetch } = await import('@tauri-apps/plugin-http')
    _tauriFetch = fetch as unknown as typeof window.fetch
  }
  return _tauriFetch ?? window.fetch
}

export function setServerPort(port: number, token?: string): void {
  baseUrl = `http://127.0.0.1:${port}`
  if (token) authToken = token
}

export function setAuthToken(token: string): void {
  authToken = token
}

export function getBaseUrl(): string {
  return baseUrl
}

export function authHeaders(): Record<string, string> {
  if (!authToken) return {}
  return { Authorization: `Bearer ${authToken}` }
}

export function initBaseUrl(): void {
  const origin = getHostBridge().getServerOrigin()
  if (origin) {
    baseUrl = origin
  }
}

async function formatRequestError(
  response: Response,
  body: string
): Promise<string> {
  try {
    const parsed = JSON.parse(body) as { code?: string; message?: string }
    if (parsed.code === 'no_active_turn') {
      return '当前 turn 已结束，无法 inject（消息会保留在 queue 中）'
    }
    if (parsed.message) {
      return parsed.message
    }
  } catch {
    // ignore JSON parse errors
  }
  if (response.status === 404 && !body.trim()) {
    return '接口不存在，请重新编译并重启 astrcode 服务'
  }
  return `${response.status} ${response.statusText}: ${body}`
}

async function request(path: string, init?: RequestInit): Promise<unknown> {
  const fetchFn = await resolveFetch()
  const headers: Record<string, string> = {
    'Content-Type': 'application/json',
  }
  if (authToken) {
    headers['Authorization'] = `Bearer ${authToken}`
  }
  const response = await fetchFn(`${baseUrl}${path}`, {
    ...init,
    headers: {
      ...headers,
      ...init?.headers,
    },
  })
  if (!response.ok) {
    const body = await response.text().catch(() => '')
    throw new Error(await formatRequestError(response, body))
  }
  if (response.status === 204) {
    return undefined
  }
  return response.json()
}

export async function createSession(
  workingDir: string
): Promise<CreateSessionResponse> {
  console.log('[api] createSession → POST /api/sessions', { workingDir })
  try {
    const result = decodeCreateSessionResponse(
      await request('/api/sessions', {
        method: 'POST',
        body: JSON.stringify({ workingDir }),
      })
    )
    console.log('[api] createSession ←', result)
    return result
  } catch (err) {
    console.error('[api] createSession FAILED', err)
    throw err
  }
}

export async function listSessions(): Promise<SessionListResponse> {
  return decodeSessionListResponse(await request('/api/sessions'))
}

export async function getConversation(
  sessionId: string
): Promise<ConversationSnapshot> {
  return decodeConversationSnapshot(
    await request(`/api/sessions/${encodeURIComponent(sessionId)}/conversation`)
  )
}

/** Mid-turn steer: requires an active turn (unlike `submitPrompt`, which queues while busy). */
export async function injectMessage(
  sessionId: string,
  text: string
): Promise<PromptSubmitResponse> {
  return decodePromptSubmitResponse(
    await request(`/api/sessions/${encodeURIComponent(sessionId)}/inject`, {
      method: 'POST',
      body: JSON.stringify({ text }),
    })
  )
}

export async function submitPrompt(
  sessionId: string,
  text: string,
  attachments: PromptAttachmentWire[] = []
): Promise<PromptSubmitResponse> {
  console.log('[api] submitPrompt →', {
    sessionId,
    text,
    attachmentCount: attachments.length,
  })
  try {
    const result = decodePromptSubmitResponse(
      await request(`/api/sessions/${encodeURIComponent(sessionId)}/prompt`, {
        method: 'POST',
        body: JSON.stringify({ text, attachments }),
      })
    )
    console.log('[api] submitPrompt ←', result)
    return result
  } catch (err) {
    console.error('[api] submitPrompt FAILED', err)
    throw err
  }
}

export async function listCommands(
  sessionId: string
): Promise<SlashCommandListResponse> {
  return decodeSlashCommandListResponse(
    await request(`/api/sessions/${encodeURIComponent(sessionId)}/commands`)
  )
}

/** 执行扩展斜杠命令（与 CLI `ExecuteExtensionCommand` 对齐，不受 turn 忙碌影响）。 */
export async function executeExtensionCommand(
  sessionId: string,
  command: string,
  argumentsText = ''
): Promise<CommandInvokeResponse> {
  return decodeCommandInvokeResponse(
    await request(
      `/api/sessions/${encodeURIComponent(sessionId)}/commands/${encodeURIComponent(command)}`,
      {
        method: 'POST',
        body: JSON.stringify({ arguments: argumentsText }),
      }
    )
  )
}

export async function completeExtensionCommand(
  sessionId: string,
  command: string,
  argument = '',
  cursor?: number
): Promise<CommandCompletionResponse> {
  return decodeCommandCompletionResponse(
    await request(
      `/api/sessions/${encodeURIComponent(sessionId)}/commands/${encodeURIComponent(command)}/complete`,
      {
        method: 'POST',
        body: JSON.stringify({ argument, cursor }),
      }
    )
  )
}

export async function abortSession(sessionId: string): Promise<void> {
  await request(`/api/sessions/${encodeURIComponent(sessionId)}/abort`, {
    method: 'POST',
  })
}

export async function deleteSession(sessionId: string): Promise<void> {
  await request(`/api/sessions/${encodeURIComponent(sessionId)}`, {
    method: 'DELETE',
  })
}

export async function deleteProject(
  workingDir: string
): Promise<{ deletedCount: number }> {
  return decodeDeleteProjectResponse(
    await request(
      `/api/projects?workingDir=${encodeURIComponent(workingDir)}`,
      {
        method: 'DELETE',
      }
    )
  )
}

export async function healthCheck(): Promise<boolean> {
  try {
    const fetchFn = await resolveFetch()
    const response = await fetchFn(`${baseUrl}/api/sessions`, {
      headers: { 'Content-Type': 'application/json', ...authHeaders() },
    })
    return response.ok
  } catch {
    return false
  }
}

// ── Config / Models ──

export async function getConfig(): Promise<ConfigView> {
  return decodeConfigView(await request('/api/config'))
}

export async function getProviderCatalog(): Promise<ProviderCatalogView> {
  return decodeProviderCatalog(await request('/api/config/provider-catalog'))
}

export async function applyProviderPreset(
  preset: ApplyProviderPresetRequest
): Promise<ApplyProviderPresetResponse> {
  return decodeApplyProviderPresetResponse(
    await request('/api/config/provider-preset/apply', {
      method: 'POST',
      body: JSON.stringify(preset),
    })
  )
}

export async function removeProviderPreset(
  profileName: string
): Promise<RemoveProviderPresetResponse> {
  return decodeRemoveProviderPresetResponse(
    await request('/api/config/provider-preset/remove', {
      method: 'POST',
      body: JSON.stringify({ profileName }),
    })
  )
}

export async function reloadConfig(): Promise<{
  activeProfile: string
  activeModel: string
  activeSmallProfile?: string
  activeSmallModel?: string
}> {
  return decodeConfigReloadResponse(
    await request('/api/config/reload', {
      method: 'POST',
    })
  )
}

export async function updateActiveSelection(
  activeProfile: string,
  activeModel: string,
  activeSmallProfile?: string,
  activeSmallModel?: string,
  approvalMode: 'manual' | 'yolo' = 'manual'
): Promise<{ success: boolean; warning?: string }> {
  const body: Record<string, unknown> = {
    activeProfile,
    activeModel,
    approvalMode,
  }
  if (activeSmallProfile) body.activeSmallProfile = activeSmallProfile
  if (activeSmallModel) body.activeSmallModel = activeSmallModel
  return decodeActiveSelectionResponse(
    await request('/api/config/active-selection', {
      method: 'POST',
      body: JSON.stringify(body),
    })
  )
}

export async function getCurrentModel(): Promise<CurrentModelInfo> {
  return decodeCurrentModelInfo(await request('/api/models/current'))
}

export async function listModels(): Promise<AvailableModel[]> {
  return decodeAvailableModels(await request('/api/models'))
}

export async function testModel(): Promise<ModelTestResult> {
  return decodeModelTestResult(
    await request('/api/models/test', { method: 'POST' })
  )
}

export async function listExtensions(): Promise<ExtensionStateView[]> {
  const response = decodeExtensionListResponse(await request('/api/extensions'))
  return response.extensions
}

export async function reloadExtensions(): Promise<{ reloadErrors: string[] }> {
  return decodeExtensionReloadResponse(
    await request('/api/extensions/reload', { method: 'POST' })
  )
}

export async function setExtensionEnabled(
  extensionId: string,
  enabled: boolean
): Promise<{ success: boolean; reloadErrors: string[] }> {
  return decodeSetExtensionEnabledResponse(
    await request('/api/extensions/set-enabled', {
      method: 'POST',
      body: JSON.stringify({ extensionId, enabled }),
    })
  )
}

/** Tool Approval UI 提交（如 askUser 问卷）。 */
export async function submitToolUiRespond(
  sessionId: string,
  callId: string,
  toolName: string,
  answers: Record<string, string>
): Promise<{ accepted: boolean }> {
  const response = await (
    await resolveFetch()
  )(
    `${baseUrl}/api/sessions/${encodeURIComponent(sessionId)}/tool-calls/${encodeURIComponent(callId)}/tool-ui/respond`,
    {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        ...authHeaders(),
      },
      body: JSON.stringify({ toolName, answers }),
    }
  )

  if (response.status === 501) {
    throw new Error(
      'Tool UI 提交接口尚未实现（POST …/tool-ui/respond）。见 docs/tool-ui-architecture.md'
    )
  }

  if (!response.ok) {
    const body = await response.text()
    if (response.status === 404 && !body.trim()) {
      throw new Error(
        'Tool UI 提交接口尚未实现（POST …/tool-ui/respond）。见 docs/tool-ui-architecture.md'
      )
    }
    throw new Error(await formatRequestError(response, body))
  }

  return (await response.json()) as { accepted: boolean }
}

export type ToolGateApprovalDecision =
  | 'allow_once'
  | 'deny_once'
  | 'allow_always'
  | 'deny_always'

export async function submitToolGateApproval(
  sessionId: string,
  callId: string,
  decision: ToolGateApprovalDecision
): Promise<void> {
  const response = await (
    await resolveFetch()
  )(`${baseUrl}/api/sessions/${encodeURIComponent(sessionId)}/approve`, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      ...authHeaders(),
    },
    body: JSON.stringify({ callId, decision }),
  })

  if (!response.ok) {
    const body = await response.text()
    throw new Error(await formatRequestError(response, body))
  }
}
