import type { ConversationDelta } from '../../services/types'
import type { AppState } from '../types'

export function applyConversationDeltaEffects(
  deltas: ConversationDelta[],
  get: () => AppState
): void {
  let refreshSessions = false
  let refreshExtensions = false
  let rehydrate = false
  let continuation:
    | Extract<ConversationDelta, { kind: 'sessionContinued' }>
    | undefined

  for (const delta of deltas) {
    if (delta.kind === 'appendBlock' && delta.block.kind === 'user') {
      refreshSessions = true
    } else if (delta.kind === 'sessionContinued') {
      refreshSessions = true
      continuation = delta
    } else if (delta.kind === 'rehydrateRequired') {
      rehydrate = true
    } else if (delta.kind === 'extensionRegistryChanged') {
      refreshExtensions = true
    }
  }

  if (refreshSessions) {
    void get().refreshSessions()
  }
  if (refreshExtensions) {
    void get().refreshExtensionData()
    void get().refreshCommands()
  }

  if (continuation) {
    if (continuation.newSessionId === continuation.parentSessionId) {
      void get().refreshConversationSnapshot()
    } else {
      void get().switchSession(continuation.newSessionId)
    }
    return
  }

  if (rehydrate) {
    const sessionId = get().activeSessionId
    if (sessionId) {
      void get().switchSession(sessionId)
    }
  }
}
