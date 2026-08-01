import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import {
  ProtocolDecodeError,
  decodeConversationBlock,
  decodeConversationSnapshot,
  decodeConversationStreamEnvelope,
} from '../../target/frontend-contract/services/protocol.js'

const fixturePath = path.resolve(
  process.cwd(),
  '..',
  'crates',
  'astrcode-protocol',
  'fixtures',
  'conversation-stream.json'
)

const fixture = JSON.parse(fs.readFileSync(fixturePath, 'utf8'))

assert.equal(fixture.length, 5)

const patch = decodeConversationStreamEnvelope(fixture[0])
assert.equal(patch.delta.kind, 'patchBlock')
assert.equal(patch.delta.blockId, 'assistant-1')
assert.equal(patch.delta.textDelta, 'hello')

const finalize = decodeConversationStreamEnvelope(fixture[1])
assert.equal(finalize.delta.kind, 'finalizeBlock')
assert.equal(finalize.delta.block.kind, 'assistant')
assert.equal(finalize.delta.block.text, 'complete answer')
assert.equal(finalize.delta.block.status, 'complete')

const rehydrate = decodeConversationStreamEnvelope(fixture[2])
assert.equal(rehydrate.delta.kind, 'rehydrateRequired')

const continued = decodeConversationStreamEnvelope({
  sessionId: 'parent-session',
  cursor: { value: '7' },
  delta: {
    kind: 'sessionContinued',
    parentSessionId: 'parent-session',
    newSessionId: 'child-session',
    parentCursor: { value: '7' },
  },
})
assert.equal(continued.delta.kind, 'sessionContinued')
assert.equal(continued.delta.parentSessionId, 'parent-session')
assert.equal(continued.delta.newSessionId, 'child-session')
assert.equal(continued.delta.parentCursor.value, '7')

const toolOutput = decodeConversationStreamEnvelope(fixture[3])
assert.equal(toolOutput.delta.kind, 'toolOutput')
assert.equal(toolOutput.delta.callId, 'tool-1')
assert.equal(toolOutput.delta.stream, 'stdout')
assert.equal(toolOutput.delta.delta, 'tool output')

const patchArguments = decodeConversationStreamEnvelope(fixture[4])
assert.equal(patchArguments.delta.kind, 'patchArguments')
assert.equal(patchArguments.delta.blockId, 'tool-1')
assert.equal(patchArguments.delta.arguments, 'Cargo.toml')

const agentSession = decodeConversationStreamEnvelope({
  sessionId: 'parent-session',
  cursor: { value: '8' },
  delta: {
    kind: 'agentSessionUpdated',
    agentSession: {
      childSessionId: 'child-session',
      toolCallId: 'tool-call-1',
      agentName: 'explorer',
      task: 'inspect code',
      status: 'running',
      finalSessionId: 'leaf-session',
      summary: 'done',
      error: 'interrupted',
      phase: 'calling_tool',
      currentTool: 'read',
    },
  },
})
assert.equal(agentSession.delta.kind, 'agentSessionUpdated')
assert.equal(agentSession.delta.agentSession.toolCallId, 'tool-call-1')
assert.equal(agentSession.delta.agentSession.finalSessionId, 'leaf-session')
assert.equal(agentSession.delta.agentSession.summary, 'done')
assert.equal(agentSession.delta.agentSession.error, 'interrupted')
assert.equal(agentSession.delta.agentSession.phase, 'calling_tool')
assert.equal(agentSession.delta.agentSession.currentTool, 'read')

const sparseAgentSession = decodeConversationStreamEnvelope({
  sessionId: 'parent-session',
  cursor: { value: '9' },
  delta: {
    kind: 'agentSessionUpdated',
    agentSession: {
      childSessionId: 'child-session',
      phase: 'thinking',
    },
  },
})
assert.equal(sparseAgentSession.delta.kind, 'agentSessionUpdated')
assert.equal(sparseAgentSession.delta.agentSession.status, undefined)
assert.equal(sparseAgentSession.delta.agentSession.agentName, undefined)
assert.equal(sparseAgentSession.delta.agentSession.task, undefined)
assert.equal(sparseAgentSession.delta.agentSession.currentTool, undefined)

const extensionEvent = decodeConversationStreamEnvelope({
  sessionId: 'session-1',
  cursor: { value: '10' },
  delta: {
    kind: 'extensionEvent',
    extensionId: 'astrcode-ask-user',
    eventType: 'ask_user.pending',
    schemaVersion: 1,
    payload: { sessionId: 'session-1', callId: 'call-1', questions: [] },
  },
})
assert.equal(extensionEvent.delta.kind, 'extensionEvent')
assert.equal(extensionEvent.delta.extensionId, 'astrcode-ask-user')
assert.equal(extensionEvent.delta.payload.callId, 'call-1')

const scalarExtensionEvent = decodeConversationStreamEnvelope({
  sessionId: 'session-1',
  cursor: { value: '10' },
  delta: {
    kind: 'extensionEvent',
    extensionId: 'extension',
    eventType: 'scalar',
    schemaVersion: 1,
    payload: ['valid', 'json'],
  },
})
assert.deepEqual(scalarExtensionEvent.delta.payload, ['valid', 'json'])

const approvalRequested = decodeConversationStreamEnvelope({
  sessionId: 'session-1',
  cursor: { value: '11' },
  delta: {
    kind: 'toolApprovalRequested',
    approval: {
      callId: 'tool-approval',
      prompt: 'Run shell command?',
      ruleKey: 'shell:write',
    },
  },
})
assert.equal(approvalRequested.delta.kind, 'toolApprovalRequested')
assert.equal(approvalRequested.delta.approval.callId, 'tool-approval')
assert.equal(approvalRequested.delta.approval.ruleKey, 'shell:write')

const approvalResolved = decodeConversationStreamEnvelope({
  sessionId: 'session-1',
  cursor: { value: '12' },
  delta: {
    kind: 'toolApprovalResolved',
    callId: 'tool-approval',
    decision: 'allow_once',
  },
})
assert.equal(approvalResolved.delta.kind, 'toolApprovalResolved')
assert.equal(approvalResolved.delta.decision, 'allow_once')

assert.throws(
  () =>
    decodeConversationStreamEnvelope({
      sessionId: 'session-1',
      cursor: { value: '3' },
      delta: { kind: 'patchBlock', blockId: 'assistant-1' },
    }),
  ProtocolDecodeError
)

assert.throws(
  () =>
    decodeConversationStreamEnvelope({
      sessionId: 'session-1',
      cursor: { value: '3' },
      delta: {
        kind: 'appendBlock',
        block: { kind: 'assistant', text: 'missing id', status: 'streaming' },
      },
    }),
  ProtocolDecodeError
)

assert.throws(() => decodeConversationStreamEnvelope(null), ProtocolDecodeError)

assert.throws(
  () =>
    decodeConversationStreamEnvelope({
      sessionId: 's',
      cursor: 'not-an-object',
      delta: fixture[0].delta,
    }),
  ProtocolDecodeError
)

assert.throws(
  () =>
    decodeConversationStreamEnvelope({
      sessionId: 's',
      cursor: { value: '1' },
      delta: { kind: 'not-a-real-delta' },
    }),
  ProtocolDecodeError
)

// User blocks omit `attachments` when empty (serde skip_serializing_if).
const userWithoutAttachments = decodeConversationBlock({
  kind: 'user',
  id: 'u-1',
  text: 'hello',
})
assert.equal(userWithoutAttachments.kind, 'user')
assert.equal(userWithoutAttachments.text, 'hello')
assert.equal(userWithoutAttachments.attachments, undefined)

for (const status of ['failed', 'cancelled']) {
  const toolBlock = decodeConversationBlock({
    kind: 'toolCall',
    id: `tool-${status}`,
    name: 'probe',
    arguments: '{}',
    text: status,
    status,
  })
  assert.equal(toolBlock.kind, 'toolCall')
  assert.equal(toolBlock.status, status)
}

assert.throws(
  () =>
    decodeConversationBlock({
      kind: 'toolCall',
      id: 'tool-error',
      name: 'probe',
      arguments: '{}',
      text: 'error',
      status: 'error',
    }),
  ProtocolDecodeError
)

const toolWithApproval = decodeConversationBlock({
  kind: 'toolCall',
  id: 'tool-approval',
  name: 'shell',
  arguments: 'git push',
  text: '',
  status: 'streaming',
  approval: {
    callId: 'tool-approval',
    prompt: 'Run shell command?',
    ruleKey: 'shell:write',
  },
})
assert.equal(toolWithApproval.kind, 'toolCall')
assert.equal(toolWithApproval.approval?.prompt, 'Run shell command?')

assert.throws(
  () =>
    decodeConversationBlock({
      kind: 'assistant',
      id: 'assistant-invalid-status',
      text: '',
      status: 'failed',
    }),
  ProtocolDecodeError
)

const snapshotWithoutAttachmentFields = decodeConversationSnapshot({
  sessionId: 's-1',
  sessionTitle: 'title',
  cursor: { value: '0' },
  phase: 'idle',
  control: {
    phase: 'idle',
    canSubmitPrompt: true,
    canRequestCompact: false,
    compactPending: false,
    compacting: false,
  },
  blocks: [
    { kind: 'user', id: 'u-1', text: 'prior message' },
    {
      kind: 'assistant',
      id: 'a-1',
      text: 'reply',
      status: 'complete',
    },
  ],
  agentSessions: [],
})
assert.equal(snapshotWithoutAttachmentFields.blocks.length, 2)
