import type { ConversationDelta } from '../../services/types'

const DEFAULT_MAX_DELTAS = 1_024
const DEFAULT_MAX_TEXT_CHARS = 256 * 1_024

interface ConversationDeltaFrame {
  deltas: ConversationDelta[]
  cursor: string | null
}

interface ConversationDeltaBufferOptions {
  maxDeltas?: number
  maxTextChars?: number
}

function deltaTextChars(delta: ConversationDelta): number {
  switch (delta.kind) {
    case 'patchBlock':
      return delta.textDelta.length
    case 'thinkingDelta':
    case 'toolOutput':
      return delta.delta.length
    case 'patchArguments':
      return delta.arguments.length
    case 'statusItemUpdate':
      return delta.text.length
    default:
      return 0
  }
}

/**
 * Collects one render frame of conversation deltas.
 *
 * Coalescing belongs to the reducer so ordering rules have one implementation.
 * The limits here only bound a delayed frame; reaching either limit asks the
 * caller to flush immediately without dropping data.
 */
export class ConversationDeltaFrameBuffer {
  private readonly maxDeltas: number
  private readonly maxTextChars: number
  private deltas: ConversationDelta[] = []
  private cursor: string | null = null
  private textChars = 0

  constructor(options: ConversationDeltaBufferOptions = {}) {
    this.maxDeltas = Math.max(1, options.maxDeltas ?? DEFAULT_MAX_DELTAS)
    this.maxTextChars = Math.max(
      1,
      options.maxTextChars ?? DEFAULT_MAX_TEXT_CHARS
    )
  }

  push(delta: ConversationDelta, cursor: string): boolean {
    this.cursor = cursor
    this.deltas.push(delta)
    this.textChars += deltaTextChars(delta)
    return this.shouldFlush()
  }

  drain(): ConversationDeltaFrame {
    const frame = {
      deltas: this.deltas,
      cursor: this.cursor,
    }
    this.deltas = []
    this.cursor = null
    this.textChars = 0
    return frame
  }

  clear(): void {
    this.deltas = []
    this.cursor = null
    this.textChars = 0
  }

  isEmpty(): boolean {
    return this.deltas.length === 0
  }

  private shouldFlush(): boolean {
    return (
      this.deltas.length >= this.maxDeltas ||
      this.textChars >= this.maxTextChars
    )
  }
}
