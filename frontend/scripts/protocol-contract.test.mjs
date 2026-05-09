import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import {
  ProtocolDecodeError,
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

const continued = decodeConversationStreamEnvelope(fixture[2])
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
