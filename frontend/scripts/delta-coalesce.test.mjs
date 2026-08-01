import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'

import {
  applyCoalescedDeltas,
  coalesceDeltas,
} from '../../target/frontend-delta/coalesce.js'
import {
  mergePendingAskUserSnapshot,
  reduceConversationDeltas,
} from '../../target/frontend-delta/applyDelta.js'
import { ConversationDeltaFrameBuffer } from '../../target/frontend-delta/frameBuffer.js'

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

assert.equal(created.length, 2)
assert.deepEqual(created[0], {
  kind: 'assistant',
  id: 'assistant-1',
  text: 'hello',
  status: 'streaming',
})
assert.deepEqual(created[1], {
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

assert.equal(patchedCreatedTool.length, 1)
assert.deepEqual(patchedCreatedTool[0], {
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
  statusItemRevisions: {},
  pendingAskUserQuestions: {},
  resolvedAskUserCallIds: {},
  pendingAskUserRefreshSessionId: null,
  askUserEventRevision: 0,
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
assert.deepEqual(framePatch.statusItemRevisions, { branch: 1 })
assert.equal(
  'agentSessions' in framePatch,
  false,
  'duplicate child-agent projections should not notify subscribers'
)

const statusAbaPatch = reduceConversationDeltas(frameState, [
  { kind: 'statusItemUpdate', id: 'branch', text: 'main' },
  { kind: 'statusItemUpdate', id: 'branch', text: 'main' },
  { kind: 'statusItemUpdate', id: 'branch', text: 'feature' },
  { kind: 'statusItemUpdate', id: 'branch', text: 'main' },
  { kind: 'statusItemUpdate', id: 'transient', text: 'visible' },
  { kind: 'statusItemUpdate', id: 'transient', text: '' },
  { kind: 'statusItemUpdate', id: 'transient', text: '' },
])
assert.deepEqual(statusAbaPatch.statusItems, { branch: 'main' })
assert.deepEqual(statusAbaPatch.statusItemRevisions, {
  branch: 4,
  transient: 3,
})

const approvalState = {
  ...frameState,
  blocks: [
    {
      kind: 'toolCall',
      id: 'tool-approval',
      name: 'shell',
      arguments: 'git push',
      text: '',
      status: 'streaming',
    },
  ],
}
const approvalRequestedPatch = reduceConversationDeltas(approvalState, [
  {
    kind: 'toolApprovalRequested',
    approval: {
      callId: 'tool-approval',
      prompt: 'Run shell command?',
      ruleKey: 'shell:write',
    },
  },
])
assert.equal(
  approvalRequestedPatch.blocks?.[0].approval.prompt,
  'Run shell command?'
)
const approvalResolvedPatch = reduceConversationDeltas(
  { ...approvalState, blocks: approvalRequestedPatch.blocks },
  [
    {
      kind: 'toolApprovalResolved',
      callId: 'tool-approval',
      decision: 'allow_once',
    },
  ]
)
assert.equal(approvalResolvedPatch.blocks?.[0].approval, undefined)

const askUserPending = {
  sessionId: 'session-1',
  callId: 'call-1',
  autoSelectAt: 60_000,
  questions: [
    {
      question: 'Which approach?',
      header: 'Approach',
      options: [
        { label: 'A', description: 'First', recommended: true },
        { label: 'B', description: 'Second' },
      ],
    },
  ],
}
const askUserPatch = reduceConversationDeltas(frameState, [
  {
    kind: 'extensionEvent',
    extensionId: 'astrcode-ask-user',
    eventType: 'ask_user.pending',
    schemaVersion: 1,
    payload: askUserPending,
  },
  {
    kind: 'extensionEvent',
    extensionId: 'astrcode-ask-user',
    eventType: 'ask_user.pending',
    schemaVersion: 1,
    payload: askUserPending,
  },
  {
    kind: 'extensionEvent',
    extensionId: 'astrcode-ask-user',
    eventType: 'ask_user.resolved',
    schemaVersion: 1,
    payload: { sessionId: 'session-1', callId: 'call-1' },
  },
  {
    kind: 'extensionEvent',
    extensionId: 'astrcode-ask-user',
    eventType: 'ask_user.pending',
    schemaVersion: 1,
    payload: { ...askUserPending, questions: [...askUserPending.questions] },
  },
])
assert.equal(
  askUserPatch.pendingAskUserQuestions['session-1:call-1']?.callId,
  'call-1'
)
assert.equal(
  askUserPatch.pendingAskUserQuestions['session-1:call-1']?.questions.length,
  1
)
assert.equal(
  askUserPatch.pendingAskUserQuestions['session-1:call-1']?.questions[0]
    .options[0].recommended,
  true,
  'the recommended flag must survive wire decoding'
)
assert.equal(
  askUserPatch.pendingAskUserQuestions['session-1:call-1']?.autoSelectAt,
  60_000,
  'the server auto-select deadline must survive wire decoding'
)
assert.equal(askUserPatch.resolvedAskUserCallIds, undefined)

const collisionPatch = reduceConversationDeltas(frameState, [
  {
    kind: 'extensionEvent',
    extensionId: 'astrcode-ask-user',
    eventType: 'ask_user.pending',
    schemaVersion: 1,
    payload: askUserPending,
  },
  {
    kind: 'extensionEvent',
    extensionId: 'astrcode-ask-user',
    eventType: 'ask_user.pending',
    schemaVersion: 1,
    payload: { ...askUserPending, sessionId: 'session-2' },
  },
  {
    kind: 'extensionEvent',
    extensionId: 'astrcode-ask-user',
    eventType: 'ask_user.resolved',
    schemaVersion: 1,
    payload: { sessionId: 'session-1', callId: 'call-1' },
  },
])
assert.equal(
  collisionPatch.pendingAskUserQuestions['session-2:call-1']?.sessionId,
  'session-2',
  'a callId reused by another session must not be dropped or mis-resolved'
)
assert.equal(
  collisionPatch.pendingAskUserQuestions['session-1:call-1'],
  undefined
)

const resolvedDuringRefreshPatch = reduceConversationDeltas(
  { ...frameState, pendingAskUserRefreshSessionId: 'session-1' },
  [
    {
      kind: 'extensionEvent',
      extensionId: 'astrcode-ask-user',
      eventType: 'ask_user.resolved',
      schemaVersion: 1,
      payload: { sessionId: 'session-1', callId: 'call-during-refresh' },
    },
  ]
)
assert.deepEqual(resolvedDuringRefreshPatch.resolvedAskUserCallIds, {
  'session-1:call-during-refresh': 'session-1',
})

const pendingDuringSnapshot = {
  ...askUserPending,
  callId: 'call-new',
}
const otherSessionPending = {
  ...askUserPending,
  sessionId: 'session-2',
  callId: 'call-other',
}
assert.deepEqual(
  mergePendingAskUserSnapshot(
    {
      'session-1:call-old': askUserPending,
      'session-1:call-new': pendingDuringSnapshot,
      'session-2:call-other': otherSessionPending,
    },
    { 'session-1:call-resolved': 'session-1' },
    [
      { ...askUserPending, callId: 'call-resolved' },
      { ...askUserPending, callId: 'call-snapshot' },
    ],
    'session-1',
    new Set(['session-1:call-old', 'session-2:call-other']),
    true
  ),
  {
    pendingAskUserQuestions: {
      'session-2:call-other': otherSessionPending,
      'session-1:call-snapshot': { ...askUserPending, callId: 'call-snapshot' },
      'session-1:call-new': pendingDuringSnapshot,
    },
    resolvedAskUserCallIds: {},
  },
  'REST recovery must preserve a pending SSE event that arrived during the request, keep entries of other sessions, and ignore resolved tombstones'
)
assert.deepEqual(
  mergePendingAskUserSnapshot(
    {},
    { 'session-1:call-1': 'session-1' },
    [askUserPending],
    'session-1',
    new Set(),
    false
  ),
  {
    pendingAskUserQuestions: { 'session-1:call-1': askUserPending },
    resolvedAskUserCallIds: {},
  },
  'an authoritative REST snapshot must allow a reused call ID'
)

const frameBuffer = new ConversationDeltaFrameBuffer({
  maxDeltas: 4,
  maxTextChars: 12,
})
assert.equal(
  frameBuffer.push(
    { kind: 'patchBlock', blockId: 'assistant-buffered', textDelta: 'hello' },
    '10'
  ),
  false
)
assert.equal(
  frameBuffer.push(
    { kind: 'patchBlock', blockId: 'assistant-buffered', textDelta: ' world' },
    '11'
  ),
  false,
  'adjacent text fragments should occupy one buffered delta'
)
assert.equal(
  frameBuffer.push(
    { kind: 'thinkingDelta', blockId: 'assistant-buffered', delta: '!' },
    '12'
  ),
  true,
  'the text budget should request a lossless early flush'
)

const bufferedFrame = frameBuffer.drain()
assert.equal(bufferedFrame.cursor, '12')
assert.deepEqual(bufferedFrame.deltas, [
  {
    kind: 'patchBlock',
    blockId: 'assistant-buffered',
    textDelta: 'hello',
  },
  {
    kind: 'patchBlock',
    blockId: 'assistant-buffered',
    textDelta: ' world',
  },
  {
    kind: 'thinkingDelta',
    blockId: 'assistant-buffered',
    delta: '!',
  },
])
assert.equal(frameBuffer.isEmpty(), true)

const replacementBuffer = new ConversationDeltaFrameBuffer()
replacementBuffer.push(
  {
    kind: 'patchArguments',
    blockId: 'tool-buffered',
    arguments: '{"command":"old"}',
  },
  '20'
)
replacementBuffer.push(
  {
    kind: 'patchArguments',
    blockId: 'tool-buffered',
    arguments: '{"command":"new"}',
  },
  '21'
)
replacementBuffer.push(
  {
    kind: 'toolOutput',
    callId: 'tool-buffered',
    stream: 'stdout',
    delta: 'out',
  },
  '22'
)
replacementBuffer.push(
  {
    kind: 'toolOutput',
    callId: 'tool-buffered',
    stream: 'stderr',
    delta: 'err',
  },
  '23'
)
assert.deepEqual(coalesceDeltas(replacementBuffer.drain().deltas), [
  {
    kind: 'patchArguments',
    blockId: 'tool-buffered',
    arguments: '{"command":"new"}',
  },
  {
    kind: 'toolOutput',
    callId: 'tool-buffered',
    parts: [
      { stream: 'stdout', delta: 'out' },
      { stream: 'stderr', delta: 'err' },
    ],
  },
])

const reducerFixturePath = path.resolve(
  process.cwd(),
  '..',
  'crates',
  'astrcode-protocol',
  'fixtures',
  'conversation-reducer.json'
)
const reducerFixture = JSON.parse(fs.readFileSync(reducerFixturePath, 'utf8'))
const reducerInitialState = {
  blocks: reducerFixture.initialBlocks,
  control: null,
  cursor: '1',
  phase: 'streaming',
  compactSubmitting: false,
  agentSessions: [],
  statusItems: {},
  statusItemRevisions: {},
  pendingAskUserQuestions: {},
  resolvedAskUserCallIds: {},
  pendingAskUserRefreshSessionId: null,
  askUserEventRevision: 0,
  transientHint: null,
}
const reducerPatch = reduceConversationDeltas(
  reducerInitialState,
  reducerFixture.envelopes.map((envelope) => envelope.delta),
  reducerFixture.envelopes.at(-1).cursor.value
)
assert.deepEqual(
  {
    ...reducerInitialState,
    ...reducerPatch,
    compactSubmitting: undefined,
    statusItemRevisions: undefined,
    pendingAskUserQuestions: undefined,
    resolvedAskUserCallIds: undefined,
    pendingAskUserRefreshSessionId: undefined,
    askUserEventRevision: undefined,
    transientHint: undefined,
  },
  {
    ...reducerFixture.expected,
    compactSubmitting: undefined,
    statusItemRevisions: undefined,
    pendingAskUserQuestions: undefined,
    resolvedAskUserCallIds: undefined,
    pendingAskUserRefreshSessionId: undefined,
    askUserEventRevision: undefined,
    transientHint: undefined,
  },
  'the frontend reducer must converge to the shared wire fixture state'
)

console.log('delta coalescing tests passed')
