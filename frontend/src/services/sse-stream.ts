import { openConversationStream } from './api'
import { decodeConversationStreamEnvelope } from './protocol'
import { SessionStreamProtocolError } from './streamErrors'
import type { ConversationStreamEnvelope } from './types'

export type SseEventHandler = (envelope: ConversationStreamEnvelope) => void
export type SseOpenHandler = () => void

function isAbortError(err: unknown): boolean {
  return err instanceof DOMException && err.name === 'AbortError'
}

export async function consumeSseStream(
  sessionId: string,
  cursor: string | null,
  onEnvelope: SseEventHandler,
  signal: AbortSignal,
  onOpen: SseOpenHandler
): Promise<'ended' | 'aborted'> {
  let response: Response
  try {
    response = await openConversationStream(sessionId, cursor, signal)
  } catch (err) {
    if (signal.aborted || isAbortError(err)) {
      return 'aborted'
    }
    console.error('[sse] fetch failed', err)
    throw err
  }

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

  onOpen()
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
        const envelope = decodeConversationStreamEnvelope(JSON.parse(payload))
        if (envelope.sessionId !== sessionId) {
          throw new Error(
            `SSE session mismatch: expected ${sessionId}, received ${envelope.sessionId}`
          )
        }
        onEnvelope(envelope)
      } catch (err) {
        throw new SessionStreamProtocolError(
          err instanceof Error ? err.message : String(err)
        )
      }
    }
    eventType = 'message'
  }

  try {
    while (!signal.aborted) {
      let chunk: ReadableStreamReadResult<Uint8Array>
      try {
        chunk = await reader.read()
      } catch (err) {
        if (signal.aborted || isAbortError(err)) {
          return 'aborted'
        }
        throw err
      }

      const { value, done } = chunk
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

    buffer += decoder.decode()
    if (buffer) {
      for (const line of buffer.split(/\r?\n/)) {
        if (line.startsWith('data:')) dataLines.push(line.slice(5).trimStart())
      }
    }
    flushEvent()

    return signal.aborted ? 'aborted' : 'ended'
  } catch (error) {
    await reader.cancel(error).catch(() => undefined)
    throw error
  } finally {
    reader.releaseLock()
  }
}
