import assert from 'node:assert/strict'
import { renderToStaticMarkup } from 'react-dom/server'

import '../src/components/Chat/tools/builtinRenderers.tsx'
import { getToolRenderer } from '../src/components/Chat/toolRendererRegistry.tsx'

function toolContext(name, args, meta, text = '') {
  return {
    block: {
      kind: 'toolCall',
      id: `${name}-call`,
      name,
      arguments: JSON.stringify(args),
      argumentsJson: args,
      text,
      status: 'complete',
      metadata: meta,
    },
    args,
    meta,
  }
}

const resultContext = toolContext(
  'read_tool_result',
  { artifactId: 'result-abc.txt', byteOffset: 0 },
  {
    artifactId: 'result-abc.txt',
    bytes: 100,
    byteOffset: 20,
    returnedBytes: 20,
    hasMore: true,
    nextByteOffset: 40,
  },
  'artifact page'
)
const resultRenderer = getToolRenderer(resultContext)
assert.equal(resultRenderer?.id, 'builtin:read-tool-result')
assert.equal(
  resultRenderer.summary?.(resultContext),
  'read tool result result-abc.txt 20 B/100 B more at byte 40'
)

const readContext = toolContext('read', { path: 'src/lib.rs' }, {})
assert.equal(getToolRenderer(readContext)?.id, 'builtin:read')

const shellContext = toolContext(
  'shell',
  { command: 'echo ok', timeout: 30 },
  { command: 'echo ok', timeoutSecs: 180 }
)
const shellRenderer = getToolRenderer(shellContext)
assert.equal(shellRenderer?.id, 'builtin:shell')
const shellDetails = renderToStaticMarkup(shellRenderer.render?.(shellContext))
assert.match(shellDetails, /timeout.*180s/)
assert.doesNotMatch(shellDetails, /timeout.*30s/)

// Presentation intent: an extension tool (no name-matched renderer) declares
// metadata.presentation and is dispatched to the mapped built-in renderer.
const intentContext = toolContext(
  'acme_deploy',
  { target: 'prod' },
  { presentation: 'terminal', command: 'deploy --prod', exitCode: 0 },
  'deployed'
)
const intentRenderer = getToolRenderer(intentContext)
assert.equal(intentRenderer?.id, 'builtin:presentation-intent')
const intentDetails = renderToStaticMarkup(
  intentRenderer.render?.(intentContext)
)
assert.match(intentDetails, /deploy --prod/)
assert.match(intentDetails, /deployed/)

// Unknown intent values fall through to the generic rendering path.
const unknownIntentContext = toolContext(
  'acme_deploy',
  { target: 'prod' },
  { presentation: 'hologram' },
  'done'
)
assert.equal(getToolRenderer(unknownIntentContext), undefined)

// Name-matched built-in renderers still win over a declared intent.
const namedWithIntent = toolContext(
  'read',
  { path: 'src/lib.rs' },
  { presentation: 'terminal' },
  'content'
)
assert.equal(getToolRenderer(namedWithIntent)?.id, 'builtin:read')
