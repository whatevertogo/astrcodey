import { getHostBridge } from '../lib/hostBridge'
import type {
  CreateSessionResponse,
  PromptSubmitResponse,
  CompactSessionResponse,
  SessionListResponse,
  ConversationSnapshot,
  ConfigView,
  CurrentModelInfo,
  AvailableModel,
  ModelTestResult,
} from './types'

let baseUrl = ''

export function setServerPort(port: number): void {
  baseUrl = `http://127.0.0.1:${port}`
}

export function getBaseUrl(): string {
  return baseUrl
}

export function initBaseUrl(): void {
  const origin = getHostBridge().getServerOrigin()
  if (origin) {
    baseUrl = origin
  }
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(`${baseUrl}${path}`, {
    ...init,
    headers: {
      'Content-Type': 'application/json',
      ...init?.headers,
    },
  })
  if (!response.ok) {
    const body = await response.text().catch(() => '')
    throw new Error(`${response.status} ${response.statusText}: ${body}`)
  }
  if (response.status === 204) {
    return undefined as T
  }
  return response.json()
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
  return request<ConversationSnapshot>(
    `/api/sessions/${encodeURIComponent(sessionId)}/conversation`
  )
}

export async function submitPrompt(
  sessionId: string,
  text: string
): Promise<PromptSubmitResponse> {
  return request<PromptSubmitResponse>(
    `/api/sessions/${encodeURIComponent(sessionId)}/prompt`,
    {
      method: 'POST',
      body: JSON.stringify({ text }),
    }
  )
}

export async function compactSession(
  sessionId: string
): Promise<CompactSessionResponse> {
  return request<CompactSessionResponse>(
    `/api/sessions/${encodeURIComponent(sessionId)}/compact`,
    { method: 'POST', body: JSON.stringify({}) }
  )
}

export async function abortSession(sessionId: string): Promise<void> {
  await request<void>(`/api/sessions/${encodeURIComponent(sessionId)}/abort`, {
    method: 'POST',
  })
}

export async function deleteSession(sessionId: string): Promise<void> {
  await request<void>(`/api/sessions/${encodeURIComponent(sessionId)}`, {
    method: 'DELETE',
  })
}

export async function deleteProject(
  workingDir: string
): Promise<{ deletedCount: number }> {
  return request<{ deletedCount: number }>(
    `/api/projects?workingDir=${encodeURIComponent(workingDir)}`,
    { method: 'DELETE' }
  )
}

export async function healthCheck(): Promise<boolean> {
  try {
    const response = await fetch(`${baseUrl}/api/sessions`, {
      headers: { 'Content-Type': 'application/json' },
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

export async function reloadConfig(): Promise<{
  activeProfile: string
  activeModel: string
}> {
  return request<{ activeProfile: string; activeModel: string }>(
    '/api/config/reload',
    {
      method: 'POST',
    }
  )
}

export async function updateActiveSelection(
  activeProfile: string,
  activeModel: string
): Promise<{ success: boolean; warning?: string }> {
  return request<{ success: boolean; warning?: string }>(
    '/api/config/active-selection',
    {
      method: 'POST',
      body: JSON.stringify({ activeProfile, activeModel }),
    }
  )
}

export async function getCurrentModel(): Promise<CurrentModelInfo> {
  return request<CurrentModelInfo>('/api/models/current')
}

export async function listModels(): Promise<AvailableModel[]> {
  const response = await request<{ models: AvailableModel[] }>('/api/models')
  return response.models
}

export async function testModel(): Promise<ModelTestResult> {
  return request<ModelTestResult>('/api/models/test', { method: 'POST' })
}
