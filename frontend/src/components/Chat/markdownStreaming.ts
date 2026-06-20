export interface StreamingMarkdownSplitState {
  text: string
  commitIndex: number
  committed: string
  tail: string
  hasNewline: boolean
  lastNewlineIndex: number
  lastParagraphBreakIndex: number
  fenceCount: number
  lastFenceStart: number
  lastNewlineBeforeLastFence: number
  lastParagraphBreakBeforeLastFence: number
}

const STREAMING_SPLIT_CACHE_LIMIT = 128
const streamingSplitCache = new Map<string, StreamingMarkdownSplitState>()

function emptySplitState(): StreamingMarkdownSplitState {
  return {
    text: '',
    commitIndex: -1,
    committed: '',
    tail: '',
    hasNewline: false,
    lastNewlineIndex: -1,
    lastParagraphBreakIndex: -1,
    fenceCount: 0,
    lastFenceStart: -1,
    lastNewlineBeforeLastFence: -1,
    lastParagraphBreakBeforeLastFence: -1,
  }
}

function cloneSplitState(
  state: StreamingMarkdownSplitState
): StreamingMarkdownSplitState {
  return { ...state }
}

function scanNewText(
  state: StreamingMarkdownSplitState,
  nextText: string,
  startIndex: number
): void {
  for (let index = startIndex; index < nextText.length; index++) {
    if (nextText.startsWith('```', index)) {
      state.fenceCount += 1
      state.lastFenceStart = index
      state.lastNewlineBeforeLastFence = state.lastNewlineIndex
      state.lastParagraphBreakBeforeLastFence = state.lastParagraphBreakIndex
      index += 2
      continue
    }

    if (nextText[index] === '\n') {
      state.hasNewline = true
      state.lastNewlineIndex = index
      if (index > 0 && nextText[index - 1] === '\n') {
        state.lastParagraphBreakIndex = index - 1
      }
    }
  }
}

function computeCommitIndex(state: StreamingMarkdownSplitState): number {
  if (!state.hasNewline) return -1

  if (state.fenceCount % 2 === 1) {
    if (state.lastFenceStart <= 0) return -1
    if (state.lastParagraphBreakBeforeLastFence !== -1) {
      return state.lastParagraphBreakBeforeLastFence + 1
    }
    return state.lastNewlineBeforeLastFence
  }

  if (state.lastParagraphBreakIndex !== -1) {
    return state.lastParagraphBreakIndex + 1
  }
  return state.lastNewlineIndex
}

function finalizeSplitState(
  state: StreamingMarkdownSplitState,
  text: string
): StreamingMarkdownSplitState {
  state.text = text
  state.commitIndex = computeCommitIndex(state)
  if (state.commitIndex === -1) {
    state.committed = ''
    state.tail = text
  } else {
    state.committed = text.slice(0, state.commitIndex + 1)
    state.tail = text.slice(state.commitIndex + 1)
  }
  return state
}

function rebuildSplitState(text: string): StreamingMarkdownSplitState {
  const state = emptySplitState()
  scanNewText(state, text, 0)
  return finalizeSplitState(state, text)
}

function appendedTextContainsFence(
  previousText: string,
  appended: string
): boolean {
  return `${previousText.slice(-2)}${appended}`.includes('```')
}

export function updateStreamingMarkdownSplit(
  previous: StreamingMarkdownSplitState | null,
  text: string
): StreamingMarkdownSplitState {
  if (!previous || !text.startsWith(previous.text)) {
    return rebuildSplitState(text)
  }

  if (text.length === previous.text.length) {
    return previous
  }

  const appended = text.slice(previous.text.length)
  if (appendedTextContainsFence(previous.text, appended)) {
    return rebuildSplitState(text)
  }

  const state = cloneSplitState(previous)
  scanNewText(state, text, previous.text.length)
  return finalizeSplitState(state, text)
}

function rememberStreamingSplit(
  cacheKey: string,
  state: StreamingMarkdownSplitState
): void {
  streamingSplitCache.delete(cacheKey)
  streamingSplitCache.set(cacheKey, state)
  while (streamingSplitCache.size > STREAMING_SPLIT_CACHE_LIMIT) {
    const oldestKey = streamingSplitCache.keys().next().value
    if (oldestKey === undefined) break
    streamingSplitCache.delete(oldestKey)
  }
}

export function cachedStreamingMarkdownSplit(
  cacheKey: string,
  text: string
): StreamingMarkdownSplitState {
  const next = updateStreamingMarkdownSplit(
    streamingSplitCache.get(cacheKey) ?? null,
    text
  )
  rememberStreamingSplit(cacheKey, next)
  return next
}

/** 统计 text 中 ``` 出现次数（奇数表示仍在代码块内）。 */
export function fenceCount(text: string): number {
  return rebuildSplitState(text).fenceCount
}

/**
 * Streaming 时在安全边界切分：优先段落（双换行），若在未闭合 fence 内则整段保持纯文本。
 */
export function findStreamingCommitIndex(text: string): number {
  return rebuildSplitState(text).commitIndex
}
