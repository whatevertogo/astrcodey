import assert from 'node:assert/strict'

import {
  applyCoalescedDeltas,
  coalesceDeltas,
} from '../../target/frontend-delta/coalesce.js'
import { reduceConversationDeltas } from '../../target/frontend-delta/applyDelta.js'

const ordered = coalesceDeltas([
  { kind: 'patchBlock', blockId: 'assistant-1', textDelta: 'a' },
  {
    kind: 'toolOutput',
    callId: 'tool-1',
    stream: 'stdout',
    delta: 'out',
  },
  { kind: 'patchBlock', blockId: 'assistant-1', textDelta: 'b' },
  { kind: 'thinkingDelta', blockId: 'assistant-1', delta: 'think ' },
  { kind: 'thinkingDelta', blockId: 'assistant-1', delta: 'more' },
  { kind: 'patchBlock', blockId: 'assistant-1', textDelta: 'c' },
])

assert.deepEqual(
  ordered.map((delta) => delta.kind),
  ['patchBlock', 'toolOutput', 'patchBlock', 'thinkingDelta', 'patchBlock']
)
assert.equal(ordered[0].textDelta, 'a')
assert.equal(ordered[2].textDelta, 'b')
assert.equal(ordered[3].delta, 'think more')
assert.equal(ordered[4].textDelta, 'c')

const created = applyCoalescedDeltas(
  [],
  [
    { kind: 'patchBlock', blockId: 'assistant-1', textDelta: 'hello' },
    {
      kind: 'toolOutput',
      callId: 'tool-1',
      parts: [{ stream: 'stdout', delta: 'out' }],
    },
  ]
)

assert.equal(created.blocks.length, 2)
assert.deepEqual(created.blocks[0], {
  kind: 'assistant',
  id: 'assistant-1',
  text: 'hello',
  status: 'streaming',
})
assert.deepEqual(created.blocks[1], {
  kind: 'toolCall',
  id: 'tool-1',
  name: '',
  arguments: '',
  text: 'out',
  status: 'streaming',
})

const patchedCreatedTool = applyCoalescedDeltas(
  [],
  [
    {
      kind: 'toolOutput',
      callId: 'tool-2',
      parts: [{ stream: 'stdout', delta: 'ready' }],
    },
    {
      kind: 'patchArguments',
      blockId: 'tool-2',
      arguments: 'run command',
      argumentsJson: { command: 'test' },
    },
  ]
)

assert.equal(patchedCreatedTool.blocks.length, 1)
assert.deepEqual(patchedCreatedTool.blocks[0], {
  kind: 'toolCall',
  id: 'tool-2',
  name: '',
  arguments: 'run command',
  argumentsJson: { command: 'test' },
  text: 'ready',
  status: 'streaming',
})

const frameState = {
  blocks: [],
  control: null,
  cursor: '1',
  phase: 'idle',
  compactSubmitting: false,
  agentSessions: [
    {
      childSessionId: 'child-1',
      toolCallId: 'tool-agent',
      status: 'running',
      phase: 'calling_tool',
      currentTool: 'read',
    },
  ],
  statusItems: {},
  transientHint: null,
}
const framePatch = reduceConversationDeltas(
  frameState,
  [
    { kind: 'patchBlock', blockId: 'assistant-frame', textDelta: 'hello ' },
    { kind: 'patchBlock', blockId: 'assistant-frame', textDelta: 'world' },
    {
      kind: 'agentSessionUpdated',
      agentSession: {
        childSessionId: 'child-1',
        phase: 'calling_tool',
        currentTool: 'read',
      },
    },
    {
      kind: 'updateControlState',
      control: {
        phase: 'streaming',
        canSubmitPrompt: false,
        canRequestCompact: true,
        compactPending: false,
        compacting: false,
      },
    },
    { kind: 'statusItemUpdate', id: 'branch', text: 'main' },
  ],
  '6'
)

assert.equal(framePatch.blocks?.[0].text, 'hello world')
assert.equal(framePatch.phase, 'streaming')
assert.equal(framePatch.cursor, '6')
assert.deepEqual(framePatch.statusItems, { branch: 'main' })
assert.equal(
  'agentSessions' in framePatch,
  false,
  'duplicate child-agent projections should not notify subscribers'
)

console.log('delta coalescing tests passed')
