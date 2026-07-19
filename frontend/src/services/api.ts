import { getHostBridge } from '../lib/hostBridge'
import { isTauriEnvironment } from '../lib/tauri'
import { decodeConversationSnapshot } from './protocol'
import type {
  ConfigReloadResponseDto,
  DeleteProjectResponseDto,
  ExtensionListResponseDto,
  ExtensionReloadResponseDto,
  ModelListResponseDto,
  SetExtensionEnabledResponseDto,
  ToolApprovalRequest,
  ToolUiRespondResponse,
  UpdateActiveSelectionRequest,
  UpdateActiveSelectionResponseDto,
} from './generated'
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
  ApprovalMode,
  ConfigView,
  CurrentModelInfo,
  ModelTestResult,
  ProviderCatalogView,
  RemoveProviderPresetResponse,
  SlashCommandListResponse,
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

async function request<T>(path: string, init?: RequestInit): Promise<T> {
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
    return undefined as T
  }
  return response.json() as Promise<T>
}

export async function createSession(
  workingDir: string
): Promise<CreateSessionResponse> {
  return request<CreateSessionResponse>('/api/sessions', {
    method: 'POST',
    body: JSON.stringify({ workingDir }),
  })
}

export async function listSessions(): Promise<SessionListResponse> {
  return request<SessionListResponse>('/api/sessions')
}

export async function getConversation(
  sessionId: string
): Promise<ConversationSnapshot> {
  return decodeConversationSnapshot(
    await request<unknown>(
      `/api/sessions/${encodeURIComponent(sessionId)}/conversation`
    )
  )
}

/** Mid-turn steer: requires an active turn (unlike `submitPrompt`, which queues while busy). */
export async function injectMessage(
  sessionId: string,
  text: string
): Promise<PromptSubmitResponse> {
  return request<PromptSubmitResponse>(
    `/api/sessions/${encodeURIComponent(sessionId)}/inject`,
    {
      method: 'POST',
      body: JSON.stringify({ text }),
    }
  )
}

export async function submitPrompt(
  sessionId: string,
  text: string,
  attachments: PromptAttachmentWire[] = []
): Promise<PromptSubmitResponse> {
  return request<PromptSubmitResponse>(
    `/api/sessions/${encodeURIComponent(sessionId)}/prompt`,
    {
      method: 'POST',
      body: JSON.stringify({ text, attachments }),
    }
  )
}

export async function listCommands(
  sessionId: string
): Promise<SlashCommandListResponse> {
  return request<SlashCommandListResponse>(
    `/api/sessions/${encodeURIComponent(sessionId)}/commands`
  )
}

/** 执行扩展斜杠命令（与 CLI `ExecuteExtensionCommand` 对齐，不受 turn 忙碌影响）。 */
export async function executeExtensionCommand(
  sessionId: string,
  command: string,
  argumentsText = ''
): Promise<CommandInvokeResponse> {
  return request<CommandInvokeResponse>(
    `/api/sessions/${encodeURIComponent(sessionId)}/commands/${encodeURIComponent(command)}`,
    {
      method: 'POST',
      body: JSON.stringify({ arguments: argumentsText }),
    }
  )
}

export async function completeExtensionCommand(
  sessionId: string,
  command: string,
  argument = '',
  cursor?: number
): Promise<CommandCompletionResponse> {
  return request<CommandCompletionResponse>(
    `/api/sessions/${encodeURIComponent(sessionId)}/commands/${encodeURIComponent(command)}/complete`,
    {
      method: 'POST',
      body: JSON.stringify({ argument, cursor }),
    }
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
): Promise<DeleteProjectResponseDto> {
  return request<DeleteProjectResponseDto>(
    `/api/projects?workingDir=${encodeURIComponent(workingDir)}`,
    { method: 'DELETE' }
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
  return request<ConfigView>('/api/config')
}

export async function getProviderCatalog(): Promise<ProviderCatalogView> {
  return request<ProviderCatalogView>('/api/config/provider-catalog')
}

export async function applyProviderPreset(
  preset: ApplyProviderPresetRequest
): Promise<ApplyProviderPresetResponse> {
  return request<ApplyProviderPresetResponse>(
    '/api/config/provider-preset/apply',
    {
      method: 'POST',
      body: JSON.stringify(preset),
    }
  )
}

export async function removeProviderPreset(
  profileName: string
): Promise<RemoveProviderPresetResponse> {
  return request<RemoveProviderPresetResponse>(
    '/api/config/provider-preset/remove',
    {
      method: 'POST',
      body: JSON.stringify({ profileName }),
    }
  )
}

export async function reloadConfig(): Promise<ConfigReloadResponseDto> {
  return request('/api/config/reload', { method: 'POST' })
}

export async function updateActiveSelection(
  activeProfile: string,
  activeModel: string,
  activeSmallProfile?: string,
  activeSmallModel?: string,
  approvalMode: ApprovalMode = 'manual'
): Promise<UpdateActiveSelectionResponseDto> {
  const body: UpdateActiveSelectionRequest = {
    activeProfile,
    activeModel,
    activeSmallProfile: activeSmallProfile || undefined,
    activeSmallModel: activeSmallModel || undefined,
    approvalMode,
  }
  return request('/api/config/active-selection', {
    method: 'POST',
    body: JSON.stringify(body),
  })
}

export async function getCurrentModel(): Promise<CurrentModelInfo> {
  return request<CurrentModelInfo>('/api/models/current')
}

export async function listModels(): Promise<ModelListResponseDto['models']> {
  const response = await request<ModelListResponseDto>('/api/models')
  return response.models
}

export async function testModel(): Promise<ModelTestResult> {
  return request<ModelTestResult>('/api/models/test', { method: 'POST' })
}

export async function listExtensions(): Promise<
  ExtensionListResponseDto['extensions']
> {
  const response = await request<ExtensionListResponseDto>('/api/extensions')
  return response.extensions
}

export async function reloadExtensions(): Promise<ExtensionReloadResponseDto> {
  return request('/api/extensions/reload', { method: 'POST' })
}

export async function setExtensionEnabled(
  extensionId: string,
  enabled: boolean
): Promise<SetExtensionEnabledResponseDto> {
  return request('/api/extensions/set-enabled', {
    method: 'POST',
    body: JSON.stringify({ extensionId, enabled }),
  })
}

/** Tool Approval UI 提交（如 askUser 问卷）。 */
export async function submitToolUiRespond(
  sessionId: string,
  callId: string,
  toolName: string,
  answers: Record<string, string>
): Promise<ToolUiRespondResponse> {
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

  return (await response.json()) as ToolUiRespondResponse
}

export type ToolGateApprovalDecision = ToolApprovalRequest['decision']

export async function submitToolGateApproval(
  sessionId: string,
  callId: string,
  decision: ToolGateApprovalDecision
): Promise<void> {
  const body: ToolApprovalRequest = { callId, decision }
  const response = await (
    await resolveFetch()
  )(`${baseUrl}/api/sessions/${encodeURIComponent(sessionId)}/approve`, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      ...authHeaders(),
    },
    body: JSON.stringify(body),
  })

  if (!response.ok) {
    const body = await response.text()
    throw new Error(await formatRequestError(response, body))
  }
}
