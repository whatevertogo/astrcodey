import assert from 'node:assert/strict'

import {
  fenceCount,
  findStreamingCommitIndex,
  updateStreamingMarkdownSplit,
} from '../../target/frontend-streaming/markdownStreaming.js'
import {
  extractThinkingBlocks,
  updateThinkingExtractionState,
} from '../../target/frontend-streaming/thinkingExtraction.js'

function legacyFenceCount(text) {
  return text.match(/```/g)?.length ?? 0
}

function legacyCommitIndex(text) {
  if (!text.includes('\n')) return -1

  if (legacyFenceCount(text) % 2 === 1) {
    const fenceStart = text.lastIndexOf('```')
    if (fenceStart <= 0) return -1
    const beforeFence = text.slice(0, fenceStart)
    const paragraphBreak = beforeFence.lastIndexOf('\n\n')
    if (paragraphBreak !== -1) return paragraphBreak + 1
    return beforeFence.lastIndexOf('\n')
  }

  const paragraphBreak = text.lastIndexOf('\n\n')
  if (paragraphBreak !== -1) return paragraphBreak + 1

  return text.lastIndexOf('\n')
}

function legacyThinkingExtraction(text) {
  const thinkingBlocks = []
  const visibleText = text
    .replace(/<think-block>([\s\S]*?)<\/think-block>/gi, (_match, content) => {
      const normalized = content.trim()
      if (normalized && !thinkingBlocks.includes(normalized)) {
        thinkingBlocks.push(normalized)
      }
      return ''
    })
    .trim()
  return { visibleText, thinkingBlocks }
}

const markdownCases = [
  'single line',
  'first line\nsecond line',
  'intro\n\nparagraph\nstill paragraph',
  'before\n```ts\nconst x = 1\n',
  'before\n\n```ts\nconst x = 1\n```\nafter\n',
]

for (const text of markdownCases) {
  let state = null
  for (let index = 0; index <= text.length; index++) {
    const prefix = text.slice(0, index)
    state = updateStreamingMarkdownSplit(state, prefix)
    assert.equal(state.commitIndex, legacyCommitIndex(prefix), prefix)
    assert.equal(findStreamingCommitIndex(prefix), legacyCommitIndex(prefix))
    assert.equal(fenceCount(prefix), legacyFenceCount(prefix))
  }
}

const thinkingCases = [
  'plain answer',
  'emoji🙂 before <think-block> hidden </think-block> after',
  'before <think-block> hidden </think-block> after',
  '<think-block> first </think-block>\nvisible\n<think-block> first </think-block>',
  'start <think-block> still open',
  'a <THINK-BLOCK> caps </THINK-BLOCK> z',
]

for (const text of thinkingCases) {
  let state = null
  for (let index = 0; index <= text.length; index++) {
    const prefix = text.slice(0, index)
    state = updateThinkingExtractionState(state, prefix)
    assert.deepEqual(
      {
        visibleText: state.visibleText,
        thinkingBlocks: state.thinkingBlocks,
      },
      legacyThinkingExtraction(prefix),
      prefix
    )
    assert.deepEqual(
      extractThinkingBlocks(prefix),
      legacyThinkingExtraction(prefix)
    )
  }
}
