import { create } from 'zustand';
import * as api from '../services/api';
import { consumeSseStream } from '../services/sse-stream';
import { getHostBridge } from '../lib/hostBridge';
import type {
  ConversationBlock,
  ConversationControlState,
  ConversationDelta,
  SessionListItem,
  Phase,
} from '../services/types';

interface ConversationState {
  serverPort: number | null;
  connectionStatus: 'disconnected' | 'connecting' | 'connected' | 'error';
  connectionError: string | null;

  sessions: SessionListItem[];
  activeSessionId: string | null;
  activeSessionTitle: string | null;
  workingDir: string | null;

  blocks: ConversationBlock[];
  control: ConversationControlState | null;
  cursor: string | null;
  phase: Phase;

  streamAbortController: AbortController | null;

  initServer: () => Promise<void>;
  refreshSessions: () => Promise<void>;
  createSession: (workingDir: string) => Promise<void>;
  switchSession: (sessionId: string) => Promise<void>;
  submitPrompt: (text: string) => Promise<void>;
  abortCurrentTurn: () => Promise<void>;
  applyDelta: (delta: ConversationDelta) => void;
}

function phaseFromControl(control: ConversationControlState | null): Phase {
  return control?.phase ?? 'idle';
}

export const useAppStore = create<ConversationState>((set, get) => ({
  serverPort: null,
  connectionStatus: 'disconnected',
  connectionError: null,
  sessions: [],
  activeSessionId: null,
  activeSessionTitle: null,
  workingDir: null,
  blocks: [],
  control: null,
  cursor: null,
  phase: 'idle',
  streamAbortController: null,

  initServer: async () => {
    set({ connectionStatus: 'connecting', connectionError: null });

    const bridge = getHostBridge();

    if (bridge.isDesktopHost) {
      try {
        const { invoke } = await import('@tauri-apps/api/core');
        const port = await invoke<number>('start_server');
        api.setServerPort(port);
        set({ serverPort: port });
      } catch (err) {
        set({
          connectionStatus: 'error',
          connectionError: err instanceof Error ? err.message : String(err),
        });
        return;
      }
    } else {
      api.initBaseUrl();
    }

    set({ connectionStatus: 'connected' });
    await get().refreshSessions();
  },

  refreshSessions: async () => {
    try {
      const response = await api.listSessions();
      set({ sessions: response.sessions });
    } catch (err) {
      console.error('Failed to refresh sessions:', err);
    }
  },

  createSession: async (workingDir: string) => {
    const response = await api.createSession(workingDir);
    await get().refreshSessions();
    await get().switchSession(response.sessionId);
  },

  switchSession: async (sessionId: string) => {
    const state = get();

    state.streamAbortController?.abort();

    set({
      activeSessionId: sessionId,
      blocks: [],
      control: null,
      cursor: null,
      phase: 'idle',
    });

    try {
      const snapshot = await api.getConversation(sessionId);
      const sessionItem = state.sessions.find((s) => s.sessionId === sessionId);

      set({
        blocks: snapshot.blocks,
        control: snapshot.control,
        cursor: snapshot.cursor.value,
        phase: phaseFromControl(snapshot.control),
        activeSessionTitle: snapshot.sessionTitle,
        workingDir: sessionItem?.workingDir ?? null,
      });

      const abortController = new AbortController();
      set({ streamAbortController: abortController });

      const sessionIdRef = sessionId;
      void consumeSseStream(
        sessionIdRef,
        (envelope) => {
          const current = get();
          if (current.activeSessionId !== sessionIdRef) return;
          if (envelope.cursor) {
            set({ cursor: envelope.cursor.value });
          }
          current.applyDelta(envelope.delta);
        },
        abortController.signal
      ).catch((err) => {
        if (!abortController.signal.aborted) {
          console.error('SSE stream error:', err);
        }
      });
    } catch (err) {
      console.error('Failed to switch session:', err);
    }
  },

  submitPrompt: async (text: string) => {
    const { activeSessionId, control } = get();
    if (!activeSessionId || !control?.canSubmitPrompt) return;

    await api.submitPrompt(activeSessionId, text);
  },

  abortCurrentTurn: async () => {
    const { activeSessionId } = get();
    if (!activeSessionId) return;

    await api.abortSession(activeSessionId);
  },

  applyDelta: (delta: ConversationDelta) => {
    const state = get();

    switch (delta.kind) {
      case 'appendBlock':
        set({ blocks: [...state.blocks, delta.block] });
        break;

      case 'patchBlock': {
        const blocks = state.blocks.map((b) => {
          const id = 'id' in b ? b.id : '';
          if (id !== delta.blockId) return b;
          if (b.kind === 'assistant' || b.kind === 'toolCall') {
            return { ...b, text: b.text + delta.textDelta };
          }
          return b;
        });
        set({ blocks });
        break;
      }

      case 'completeBlock': {
        const blocks = state.blocks.map((b) => {
          const id = 'id' in b ? b.id : '';
          if (id !== delta.blockId) return b;
          if (b.kind === 'assistant' || b.kind === 'toolCall') {
            return { ...b, status: 'complete' as const };
          }
          return b;
        });
        set({ blocks });
        break;
      }

      case 'updateControlState':
        set({
          control: delta.control,
          phase: phaseFromControl(delta.control),
        });
        break;

      case 'toolOutput': {
        const blocks = state.blocks.map((b) => {
          if (b.kind === 'toolCall' && b.id === delta.callId) {
            const prefix = delta.stream === 'stderr' ? '\n[stderr] ' : '\n';
            return { ...b, text: b.text + prefix + delta.delta };
          }
          return b;
        });
        set({ blocks });
        break;
      }

      case 'rehydrateRequired': {
        const sessionId = state.activeSessionId;
        if (sessionId) {
          void get().switchSession(sessionId);
        }
        break;
      }

      case 'sessionContinued': {
        void get().refreshSessions();
        void get().switchSession(delta.newSessionId);
        break;
      }
    }
  },
}));
