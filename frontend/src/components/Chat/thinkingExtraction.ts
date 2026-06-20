const OPEN_TAG = '<think-block>'
const CLOSE_TAG = '</think-block>'
const OPEN_TAG_LENGTH = OPEN_TAG.length
const CLOSE_TAG_LENGTH = CLOSE_TAG.length
const THINKING_EXTRACTION_CACHE_LIMIT = 128
const thinkingExtractionCache = new Map<string, ThinkingExtractionState>()

export interface ThinkingExtraction {
  visibleText: string
  thinkingBlocks: string[]
}

export interface ThinkingExtractionState extends ThinkingExtraction {
  sourceText: string
  visibleRaw: string
  mode: 'visible' | 'thinking'
  pendingVisible: string
  pendingThinking: string
  thinkingRaw: string
  openTagText: string
  thinkingSet: Set<string>
}

function emptyThinkingState(): ThinkingExtractionState {
  return {
    sourceText: '',
    visibleText: '',
    visibleRaw: '',
    thinkingBlocks: [],
    mode: 'visible',
    pendingVisible: '',
    pendingThinking: '',
    thinkingRaw: '',
    openTagText: '',
    thinkingSet: new Set(),
  }
}

function cloneThinkingState(
  state: ThinkingExtractionState
): ThinkingExtractionState {
  return {
    ...state,
    thinkingBlocks: [...state.thinkingBlocks],
    thinkingSet: new Set(state.thinkingSet),
  }
}

function shiftFirstChar(text: string): [string, string] {
  const first = Array.from(text)[0] ?? ''
  return [first, text.slice(first.length)]
}

function flushVisibleHoldback(state: ThinkingExtractionState): void {
  while (state.pendingVisible.length > OPEN_TAG_LENGTH - 1) {
    const [first, rest] = shiftFirstChar(state.pendingVisible)
    state.visibleRaw += first
    state.pendingVisible = rest
  }
}

function flushThinkingHoldback(state: ThinkingExtractionState): void {
  while (state.pendingThinking.length > CLOSE_TAG_LENGTH - 1) {
    const [first, rest] = shiftFirstChar(state.pendingThinking)
    state.thinkingRaw += first
    state.pendingThinking = rest
  }
}

function pushThinkingBlock(
  state: ThinkingExtractionState,
  content: string
): void {
  const normalized = content.trim()
  if (!normalized || state.thinkingSet.has(normalized)) return
  state.thinkingSet.add(normalized)
  state.thinkingBlocks.push(normalized)
}

function appendVisibleChar(state: ThinkingExtractionState, char: string): void {
  state.pendingVisible += char
  if (state.pendingVisible.toLowerCase().endsWith(OPEN_TAG)) {
    state.visibleRaw += state.pendingVisible.slice(0, -OPEN_TAG_LENGTH)
    state.mode = 'thinking'
    state.openTagText = state.pendingVisible.slice(-OPEN_TAG_LENGTH)
    state.pendingVisible = ''
    state.pendingThinking = ''
    state.thinkingRaw = ''
    return
  }
  flushVisibleHoldback(state)
}

function appendThinkingChar(
  state: ThinkingExtractionState,
  char: string
): void {
  state.pendingThinking += char
  if (state.pendingThinking.toLowerCase().endsWith(CLOSE_TAG)) {
    const beforeClose = state.pendingThinking.slice(0, -CLOSE_TAG_LENGTH)
    pushThinkingBlock(state, state.thinkingRaw + beforeClose)
    state.mode = 'visible'
    state.pendingThinking = ''
    state.thinkingRaw = ''
    state.openTagText = ''
    return
  }
  flushThinkingHoldback(state)
}

function appendThinkingText(
  state: ThinkingExtractionState,
  text: string
): ThinkingExtractionState {
  for (const char of text) {
    if (state.mode === 'visible') {
      appendVisibleChar(state, char)
    } else {
      appendThinkingChar(state, char)
    }
  }
  state.sourceText += text
  state.visibleText = currentVisibleText(state)
  return state
}

function currentVisibleText(state: ThinkingExtractionState): string {
  if (state.mode === 'thinking') {
    return (
      state.visibleRaw +
      state.pendingVisible +
      state.openTagText +
      state.thinkingRaw +
      state.pendingThinking
    ).trim()
  }
  return (state.visibleRaw + state.pendingVisible).trim()
}

export function updateThinkingExtractionState(
  previous: ThinkingExtractionState | null,
  text: string
): ThinkingExtractionState {
  if (!previous || !text.startsWith(previous.sourceText)) {
    return appendThinkingText(emptyThinkingState(), text)
  }

  if (text.length === previous.sourceText.length) {
    return previous
  }

  const next = cloneThinkingState(previous)
  return appendThinkingText(next, text.slice(previous.sourceText.length))
}

function rememberThinkingExtraction(
  cacheKey: string,
  state: ThinkingExtractionState
): void {
  thinkingExtractionCache.delete(cacheKey)
  thinkingExtractionCache.set(cacheKey, state)
  while (thinkingExtractionCache.size > THINKING_EXTRACTION_CACHE_LIMIT) {
    const oldestKey = thinkingExtractionCache.keys().next().value
    if (oldestKey === undefined) break
    thinkingExtractionCache.delete(oldestKey)
  }
}

export function cachedThinkingExtraction(
  cacheKey: string,
  text: string
): ThinkingExtraction {
  const next = updateThinkingExtractionState(
    thinkingExtractionCache.get(cacheKey) ?? null,
    text
  )
  rememberThinkingExtraction(cacheKey, next)
  return {
    visibleText: next.visibleText,
    thinkingBlocks: next.thinkingBlocks,
  }
}

export function extractThinkingBlocks(text: string): ThinkingExtraction {
  const state = updateThinkingExtractionState(null, text)
  return {
    visibleText: state.visibleText,
    thinkingBlocks: state.thinkingBlocks,
  }
}
