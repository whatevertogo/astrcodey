import assert from 'node:assert/strict'

import {
  assistantRunCopyText,
  assistantVisibleText,
  buildAssistantRunModel,
  buildMessageListItems,
  processSummaryTitle,
} from '../../target/frontend-assistant-run/assistantRunModel.js'
import {
  pendingAskUserHasVisibleBlock,
  recoveredAskUserBlock,
  remainingAutoSelectSeconds,
} from '../../target/frontend-assistant-run/tools/askUser.js'

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

assert.equal(
  assistantRunCopyText([
    assistant('a-copy-1', '<think-block>hidden</think-block>\nprogress'),
    tool('t-copy', 'read'),
    assistant('a-copy-2', 'final answer', 'complete', { storageSeq: 42 }),
  ]),
  'progress\n\nfinal answer',
  'copying a completed turn must include visible assistant text without thinking or tool output'
)

assert.equal(
  remainingAutoSelectSeconds(
    {
      sessionId: 'session-1',
      callId: 'call-1',
      questions: [],
      autoSelectAt: 1_060_000,
      serverTime: 1_000_000,
      receivedAtMonotonic: 5_000,
    },
    35_000
  ),
  30,
  'countdown must use server-relative time instead of the client wall clock'
)

const recoveredPendingQuestion = {
  sessionId: 'session-1',
  callId: 'ask-user-1',
  questions: [
    {
      header: 'Scope',
      question: 'Which option?',
      options: [
        { label: 'A', description: 'First option' },
        { label: 'B', description: 'Second option' },
      ],
    },
  ],
  metadata: { source: 'reconnect' },
  receivedAtMonotonic: 0,
}
const recoveredBlock = recoveredAskUserBlock(recoveredPendingQuestion)

assert.equal(
  pendingAskUserHasVisibleBlock([], recoveredPendingQuestion),
  false,
  'a pending snapshot without its lost live tool block needs recovery rendering'
)
assert.equal(recoveredBlock.id, recoveredPendingQuestion.callId)
assert.equal(recoveredBlock.name, 'askUser')
assert.equal(recoveredBlock.status, 'streaming')
assert.deepEqual(recoveredBlock.argumentsJson, {
  questions: recoveredPendingQuestion.questions,
  metadata: recoveredPendingQuestion.metadata,
})
assert.equal(
  pendingAskUserHasVisibleBlock([recoveredBlock], recoveredPendingQuestion),
  true,
  'an existing streaming askUser block must suppress duplicate recovery UI'
)
assert.equal(
  pendingAskUserHasVisibleBlock(
    [tool(recoveredPendingQuestion.callId, 'askUser', 'complete')],
    recoveredPendingQuestion
  ),
  false,
  'a reused call id with only an old terminal block still needs recovery rendering'
)
