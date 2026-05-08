import { getHostBridge } from '../lib/hostBridge'
import {
  decodeActiveSelectionResponse,
  decodeAvailableModels,
  decodeCompactSessionResponse,
  decodeConfigReloadResponse,
  decodeConfigView,
  decodeConversationSnapshot,
  decodeCreateSessionResponse,
  decodeCurrentModelInfo,
  decodeDeleteProjectResponse,
  decodeModelTestResult,
  decodePromptSubmitResponse,
  decodeSessionListResponse,
} from './protocol'
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

async function request(path: string, init?: RequestInit): Promise<unknown> {
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
  text: string
): Promise<PromptSubmitResponse> {
  console.log('[api] submitPrompt →', { sessionId, text })
  try {
    const result = decodePromptSubmitResponse(
      await request(`/api/sessions/${encodeURIComponent(sessionId)}/prompt`, {
        method: 'POST',
        body: JSON.stringify({ text }),
      })
    )
    console.log('[api] submitPrompt ←', result)
    return result
  } catch (err) {
    console.error('[api] submitPrompt FAILED', err)
    throw err
  }
}

export async function compactSession(
  sessionId: string
): Promise<CompactSessionResponse> {
  return decodeCompactSessionResponse(
    await request(`/api/sessions/${encodeURIComponent(sessionId)}/compact`, {
      method: 'POST',
      body: JSON.stringify({}),
    })
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
  return decodeConfigView(await request('/api/config'))
}

export async function reloadConfig(): Promise<{
  activeProfile: string
  activeModel: string
}> {
  return decodeConfigReloadResponse(
    await request('/api/config/reload', {
      method: 'POST',
    })
  )
}

export async function updateActiveSelection(
  activeProfile: string,
  activeModel: string
): Promise<{ success: boolean; warning?: string }> {
  return decodeActiveSelectionResponse(
    await request('/api/config/active-selection', {
      method: 'POST',
      body: JSON.stringify({ activeProfile, activeModel }),
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
