// Types aligned with astrcode-protocol/src/http.rs DTOs

export type Phase = 'idle' | 'thinking' | 'streaming' | 'calling_tool' | 'compacting' | 'error';
export type ToolOutputStream = 'stdout' | 'stderr';
export type BlockStatus = 'streaming' | 'complete' | 'error';

// ── Request/Response ──

export interface CreateSessionRequest {
  workingDir: string;
}

export interface CreateSessionResponse {
  sessionId: string;
}

export interface PromptRequest {
  text: string;
}

export type PromptSubmitResponse =
  | {
      kind: 'accepted';
      sessionId: string;
      turnId: string;
      branchedFromSessionId?: string;
    }
  | {
      kind: 'handled';
      sessionId: string;
      message: string;
    };

export interface CompactSessionResponse {
  accepted: boolean;
  deferred: boolean;
  newSessionId?: string;
  message: string;
}

// ── Session List ──

export interface SessionListItem {
  sessionId: string;
  workingDir: string;
  displayName: string;
  title: string;
  createdAt: string;
  updatedAt: string;
  parentSessionId?: string;
  parentStorageSeq?: number;
  phase: Phase;
}

export interface SessionListResponse {
  sessions: SessionListItem[];
}

// ── Conversation Snapshot ──

export interface ConversationCursor {
  value: string;
}

export interface ConversationControlState {
  phase: Phase;
  canSubmitPrompt: boolean;
  canRequestCompact: boolean;
  compactPending: boolean;
  compacting: boolean;
  currentModeId?: string;
  activeTurnId?: string;
}

export type ConversationBlock =
  | { kind: 'user'; id: string; text: string }
  | { kind: 'assistant'; id: string; text: string; status: BlockStatus }
  | { kind: 'toolCall'; id: string; name: string; text: string; status: BlockStatus }
  | { kind: 'error'; id: string; message: string }
  | { kind: 'systemNote'; id: string; text: string };

export interface ConversationSnapshot {
  sessionId: string;
  sessionTitle: string;
  cursor: ConversationCursor;
  phase: Phase;
  control: ConversationControlState;
  blocks: ConversationBlock[];
}

// ── SSE Stream ──

export interface ConversationStreamEnvelope {
  sessionId: string;
  cursor: ConversationCursor;
  delta: ConversationDelta;
}

export type ConversationDelta =
  | { kind: 'appendBlock'; block: ConversationBlock }
  | { kind: 'patchBlock'; blockId: string; textDelta: string }
  | { kind: 'completeBlock'; blockId: string }
  | { kind: 'updateControlState'; control: ConversationControlState }
  | { kind: 'rehydrateRequired' }
  | {
      kind: 'sessionContinued';
      parentSessionId: string;
      newSessionId: string;
      parentCursor: ConversationCursor;
    }
  | { kind: 'toolOutput'; callId: string; stream: ToolOutputStream; delta: string };

// ── App State ──

export interface ConnectionState {
  status: 'disconnected' | 'connecting' | 'connected' | 'error';
  error?: string;
}
