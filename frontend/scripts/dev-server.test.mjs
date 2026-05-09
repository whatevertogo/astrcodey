import assert from 'node:assert/strict'

import { healthCheckOptions, resolveDevServerHost } from './dev-server.mjs'

assert.equal(resolveDevServerHost({}), '127.0.0.1')
assert.equal(
  resolveDevServerHost({ TAURI_DEV_HOST: '192.168.1.10' }),
  '192.168.1.10'
)

assert.deepEqual(healthCheckOptions('127.0.0.1', 5173), {
  hostname: '127.0.0.1',
  port: 5173,
  path: '/',
})
assert.equal('family' in healthCheckOptions('127.0.0.1', 5173), false)
