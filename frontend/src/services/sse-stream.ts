import { getBaseUrl } from './api'
import { tryDecodeConversationStreamEnvelope } from './protocol'
import type { ConversationStreamEnvelope } from './types'

export type SseEventHandler = (envelope: ConversationStreamEnvelope) => void

export async function consumeSseStream(
  sessionId: string,
  cursor: string | null,
  onEnvelope: SseEventHandler,
  signal: AbortSignal
): Promise<'ended' | 'aborted'> {
  const params = cursor ? `?cursor=${encodeURIComponent(cursor)}` : ''
  const url = `${getBaseUrl()}/api/sessions/${encodeURIComponent(sessionId)}/stream${params}`
  console.debug('[sse] connecting', { url, cursor })

  let response: Response
  try {
    response = await fetch(url, {
      headers: {
        Accept: 'text/event-stream',
        'Cache-Control': 'no-cache',
      },
      signal,
    })
  } catch (err) {
    console.error('[sse] fetch failed', err)
    throw err
  }

  console.debug('[sse] response', { status: response.status, ok: response.ok })

  if (!response.ok) {
    const text = await response.text().catch(() => '')
    console.error('[sse] non-ok response', {
      status: response.status,
      body: text,
    })
    throw new Error(`SSE ${response.status}: ${text}`)
  }

  if (!response.body) {
    throw new Error('SSE response has no body')
  }

  const reader = response.body.getReader()
  const decoder = new TextDecoder()
  let buffer = ''
  let dataLines: string[] = []
  let eventType = 'message'

  const flushEvent = () => {
    if (dataLines.length === 0) {
      eventType = 'message'
      return
    }
    const payload = dataLines.join('\n')
    dataLines = []

    if (eventType === 'conversation') {
      try {
        const envelope = tryDecodeConversationStreamEnvelope(
          JSON.parse(payload)
        )
        if (!envelope) {
          console.warn('[sse] ignored malformed conversation event', payload)
          return
        }
        console.debug('[sse] event', envelope.delta.kind, envelope.cursor)
        onEnvelope(envelope)
      } catch (err) {
        console.warn('[sse] parse error', err, payload)
      }
    }
    eventType = 'message'
  }

  while (!signal.aborted) {
    const { value, done } = await reader.read()
    if (done) break

    buffer += decoder.decode(value, { stream: true })
    const lines = buffer.split(/\r?\n/)
    buffer = lines.pop() ?? ''

    for (const line of lines) {
      if (line === '') {
        flushEvent()
        continue
      }
      if (line.startsWith(':')) continue
      if (line.startsWith('id:')) {
        continue
      }
      if (line.startsWith('event:')) {
        const nextType = line.slice(6).trimStart()
        eventType = nextType || 'message'
        continue
      }
      if (line.startsWith('data:')) {
        dataLines.push(line.slice(5).trimStart())
      }
    }
  }

  // Flush remaining
  buffer += decoder.decode()
  if (buffer) {
    for (const line of buffer.split(/\r?\n/)) {
      if (line.startsWith('data:')) dataLines.push(line.slice(5).trimStart())
    }
  }
  flushEvent()

  return signal.aborted ? 'aborted' : 'ended'
}
