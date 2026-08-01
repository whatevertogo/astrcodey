import assert from 'node:assert/strict'

import {
  assistantVisibleText,
  buildAssistantRunModel,
  buildMessageListItems,
  processSummaryTitle,
} from '../../target/frontend-assistant-run/assistantRunModel.js'

function assistant(id, text, status = 'complete', extra = {}) {
  return { kind: 'assistant', id, text, status, ...extra }
}

function tool(id, name, status = 'complete', extra = {}) {
  return {
    kind: 'toolCall',
    id,
    name,
    arguments: '',
    text: '',
    status,
    ...extra,
  }
}

const runWithThinkingToolFinal = buildAssistantRunModel([
  assistant('a1', '<think-block>read files</think-block>starting'),
  tool('t1', 'read', 'complete', {
    argumentsJson: { path: '/tmp/example.ts' },
    metadata: { path: '/tmp/example.ts', durationMs: 1200 },
  }),
  assistant('a2', 'final answer'),
])

assert.equal(runWithThinkingToolFinal.finalReplyBlock?.id, 'a2')
assert.deepEqual(
  runWithThinkingToolFinal.processEntries.map((entry) => entry.type),
  ['thinking', 'tool']
)
assert.deepEqual(
  runWithThinkingToolFinal.segments.map((segment) => segment.type),
  ['process', 'content', 'process', 'content']
)
assert.equal(
  runWithThinkingToolFinal.segments[1].block.id,
  'a1',
  'intermediate visible assistant text should remain visible'
)
assert.equal(runWithThinkingToolFinal.status, 'complete')
assert.equal(
  processSummaryTitle(runWithThinkingToolFinal.segments[2]),
  '已处理 1s'
)
assert.equal(
  runWithThinkingToolFinal.processEntries[1].activity.label,
  'example.ts'
)

const onlyTool = buildAssistantRunModel([
  tool('t2', 'shell', 'complete', {
    arguments: '{"command":"npm run check"}',
    argumentsJson: { command: 'npm run check' },
    metadata: { duration: 62 },
  }),
])

assert.equal(onlyTool.finalReplyBlock, null)
assert.deepEqual(
  onlyTool.processEntries.map((entry) => entry.type),
  ['tool']
)
assert.deepEqual(
  onlyTool.segments.map((segment) => segment.type),
  ['process']
)
assert.equal(processSummaryTitle(onlyTool.segments[0]), '已处理 1m 2s')

const onlyFinal = buildAssistantRunModel([assistant('a3', 'just final')])
assert.equal(onlyFinal.finalReplyBlock?.id, 'a3')
assert.equal(onlyFinal.processEntries.length, 0)
assert.deepEqual(
  onlyFinal.segments.map((segment) => segment.type),
  ['content']
)
assert.equal(onlyFinal.status, 'complete')

const streamingRun = buildAssistantRunModel([
  tool('t3', 'shell', 'streaming', {
    argumentsJson: { command: 'cargo test' },
  }),
])

assert.equal(streamingRun.status, 'streaming')
assert.equal(streamingRun.hasStreamingWork, true)
assert.equal(processSummaryTitle(streamingRun.segments[0]), '处理中')

for (const [status, expectedRunStatus] of [
  ['failed', 'error'],
  ['cancelled', 'complete'],
]) {
  const terminalRun = buildAssistantRunModel([
    tool(`terminal-${status}`, 'shell', status),
  ])
  assert.equal(terminalRun.status, expectedRunStatus)
  assert.equal(terminalRun.hasStreamingWork, false)
  assert.equal(terminalRun.hasAttention, false)
}

const messageItems = buildMessageListItems([
  { kind: 'user', id: 'u1', text: 'hi' },
  assistant('a4', 'thinking', 'complete'),
  tool('t4', 'read', 'complete'),
  assistant('a5', 'reply', 'streaming'),
  { kind: 'systemNote', id: 's1', text: 'note' },
])

assert.deepEqual(
  messageItems.map((item) => item.type),
  ['block', 'assistantRun', 'block', 'forkRow']
)
assert.equal(messageItems[1].id, 'assistant-run:a4')

const extendedMessageItems = buildMessageListItems([
  { kind: 'user', id: 'u1', text: 'hi' },
  assistant('a4', 'thinking', 'complete'),
  tool('t4', 'read', 'complete'),
  assistant('a5', 'reply', 'streaming'),
  tool('t5', 'shell', 'streaming'),
])
assert.equal(
  extendedMessageItems[1].id,
  messageItems[1].id,
  'appending work must not remount the active assistant run'
)

const longRunBlocks = [
  assistant('a-long', '', 'streaming'),
  ...Array.from({ length: 50 }, (_, index) =>
    tool(`t-long-${index}`, 'read', index === 49 ? 'streaming' : 'complete')
  ),
]
const longRunItems = buildMessageListItems([
  { kind: 'user', id: 'u-long', text: 'inspect everything' },
  ...longRunBlocks,
])
const longRunModel = buildAssistantRunModel(longRunBlocks)
assert.equal(longRunItems[1].id, 'assistant-run:a-long')
assert.equal(longRunModel.segments[0].id, 'process:t-long-0')
assert.equal(longRunModel.processEntries.length, 50)

assert.equal(
  assistantVisibleText(
    assistant('a7', '<think-block>hidden</think-block>\nvisible')
  ),
  'visible'
)
