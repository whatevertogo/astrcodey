import { performance } from 'node:perf_hooks'

import { reduceConversationDeltas } from '../../target/frontend-delta/applyDelta.js'

const HISTORY_BLOCKS = 10_000
const STREAM_DELTAS = 4_000
const RUNS = 25

const blocks = Array.from({ length: HISTORY_BLOCKS }, (_, index) => ({
  kind: index % 3 === 0 ? 'user' : 'assistant',
  id: `history-${index}`,
  text: `message ${index}`,
  ...(index % 3 === 0 ? {} : { status: 'complete' }),
}))
blocks.push({
  kind: 'assistant',
  id: 'active-assistant',
  text: '',
  status: 'streaming',
})

const deltas = Array.from({ length: STREAM_DELTAS }, () => ({
  kind: 'patchBlock',
  blockId: 'active-assistant',
  textDelta: 'token ',
}))

const state = {
  blocks,
  control: null,
  cursor: '1',
  phase: 'streaming',
  compactSubmitting: false,
  agentSessions: [],
  statusItems: {},
  statusItemRevisions: {},
  transientHint: null,
}

for (let index = 0; index < 5; index += 1) {
  reduceConversationDeltas(state, deltas, '2')
}

const samples = []
for (let index = 0; index < RUNS; index += 1) {
  const startedAt = performance.now()
  const patch = reduceConversationDeltas(state, deltas, '2')
  samples.push(performance.now() - startedAt)

  const active = patch.blocks?.at(-1)
  if (active?.text.length !== STREAM_DELTAS * 'token '.length) {
    throw new Error('conversation profile produced an invalid active block')
  }
  if (patch.blocks?.[0] !== blocks[0]) {
    throw new Error('conversation profile replaced an unchanged history block')
  }
}

samples.sort((left, right) => left - right)
const percentile = (fraction) =>
  samples[Math.min(samples.length - 1, Math.floor(samples.length * fraction))]

console.log(
  JSON.stringify(
    {
      historyBlocks: HISTORY_BLOCKS,
      streamDeltas: STREAM_DELTAS,
      runs: RUNS,
      medianMs: Number(percentile(0.5).toFixed(3)),
      p95Ms: Number(percentile(0.95).toFixed(3)),
      maxMs: Number(samples.at(-1).toFixed(3)),
    },
    null,
    2
  )
)
