import { getBaseUrl } from './api';
import type { ConversationStreamEnvelope } from './types';

export type SseEventHandler = (envelope: ConversationStreamEnvelope) => void;

export async function consumeSseStream(
  sessionId: string,
  cursor: string | null,
  onEnvelope: SseEventHandler,
  signal: AbortSignal
): Promise<'ended' | 'aborted'> {
  const params = cursor ? `?cursor=${encodeURIComponent(cursor)}` : '';
  const url = `${getBaseUrl()}/api/sessions/${encodeURIComponent(sessionId)}/stream${params}`;

  const response = await fetch(url, {
    headers: {
      Accept: 'text/event-stream',
      'Cache-Control': 'no-cache',
    },
    signal,
  });

  if (!response.body) {
    throw new Error('SSE response has no body');
  }

  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = '';
  let dataLines: string[] = [];
  let eventType = 'message';

  const flushEvent = () => {
    if (dataLines.length === 0) {
      eventType = 'message';
      return;
    }
    const payload = dataLines.join('\n');
    dataLines = [];

    if (eventType === 'conversation') {
      try {
        const envelope = JSON.parse(payload) as ConversationStreamEnvelope;
        onEnvelope(envelope);
      } catch {
        // Ignore malformed JSON
      }
    }
    eventType = 'message';
  };

  while (!signal.aborted) {
    const { value, done } = await reader.read();
    if (done) break;

    buffer += decoder.decode(value, { stream: true });
    const lines = buffer.split(/\r?\n/);
    buffer = lines.pop() ?? '';

    for (const line of lines) {
      if (line === '') {
        flushEvent();
        continue;
      }
      if (line.startsWith(':')) continue;
      if (line.startsWith('id:')) {
        continue;
      }
      if (line.startsWith('event:')) {
        const nextType = line.slice(6).trimStart();
        eventType = nextType || 'message';
        continue;
      }
      if (line.startsWith('data:')) {
        dataLines.push(line.slice(5).trimStart());
      }
    }
  }

  // Flush remaining
  buffer += decoder.decode();
  if (buffer) {
    for (const line of buffer.split(/\r?\n/)) {
      if (line.startsWith('data:')) dataLines.push(line.slice(5).trimStart());
    }
  }
  flushEvent();

  return signal.aborted ? 'aborted' : 'ended';
}
