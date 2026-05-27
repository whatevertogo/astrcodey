import { getHostBridge } from '../lib/hostBridge'
import { isTauriEnvironment } from '../lib/tauri'
import {
  decodeActiveSelectionResponse,
  decodeAvailableModels,
  decodeConfigReloadResponse,
  decodeConfigView,
  decodeConversationSnapshot,
  decodeCreateSessionResponse,
  decodeCurrentModelInfo,
  decodeDeleteProjectResponse,
  decodeModelTestResult,
  decodePromptSubmitResponse,
  decodeSlashCommandListResponse,
  decodeSessionListResponse,
} from './protocol'
import type {
  CreateSessionResponse,
  PromptSubmitResponse,
  PromptAttachment,
  SessionListResponse,
  ConversationSnapshot,
  ConfigView,
  CurrentModelInfo,
  AvailableModel,
  ModelTestResult,
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
    throw new Error(`${response.status} ${response.statusText}: ${body}`)
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

export async function submitPrompt(
  sessionId: string,
  text: string,
  attachments: PromptAttachment[] = []
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
        body: JSON.stringify({
          text,
          attachments: attachments.length > 0 ? attachments : undefined,
        }),
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
  activeSmallModel?: string
): Promise<{ success: boolean; warning?: string }> {
  const body: Record<string, unknown> = { activeProfile, activeModel }
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
