/**
 * Starts Vite dev server and blocks until the HTTP port responds.
 */

import { spawn } from 'node:child_process'
import { request } from 'node:http'
import { fileURLToPath } from 'node:url'
import path from 'node:path'

const __dirname = path.dirname(fileURLToPath(import.meta.url))
const FRONTEND_DIR = path.resolve(__dirname, '..')
const VITE_CLI = path.join(
  FRONTEND_DIR,
  'node_modules',
  'vite',
  'bin',
  'vite.js'
)
const DEFAULT_HOST = '127.0.0.1'
const DEFAULT_PORT = 5173
const DEFAULT_MAX_RETRIES = 120
const DEFAULT_RETRY_MS = 500

export function resolveDevServerHost(env = process.env) {
  return env.TAURI_DEV_HOST || DEFAULT_HOST
}

export function healthCheckOptions(host, port = DEFAULT_PORT) {
  return { hostname: host, port, path: '/' }
}

function poll({ host, port, maxRetries, retryMs, child }, retries = 0) {
  request(healthCheckOptions(host, port), (res) => {
    res.resume()
    console.log(`[dev-server] Vite ready on http://${host}:${port}`)
  })
    .on('error', (err) => {
      if (retries >= maxRetries) {
        console.error(
          `[dev-server] Timed out after ${(maxRetries * retryMs) / 1000}s`
        )
        console.error(`[dev-server] Last error: ${err.code} — ${err.message}`)
        child.kill()
        process.exit(1)
      }
      setTimeout(
        () => poll({ host, port, maxRetries, retryMs, child }, retries + 1),
        retryMs
      )
    })
    .end()
}

function run() {
  const host = resolveDevServerHost()
  const port = DEFAULT_PORT

  console.log(
    `[dev-server] Starting Vite in ${FRONTEND_DIR} (host=${host}, port=${port})...`
  )

  const vite = spawn(process.execPath, [VITE_CLI], {
    stdio: 'inherit',
    cwd: FRONTEND_DIR,
  })

  setTimeout(
    () =>
      poll({
        host,
        port,
        maxRetries: DEFAULT_MAX_RETRIES,
        retryMs: DEFAULT_RETRY_MS,
        child: vite,
      }),
    1000
  )

  vite.on('exit', (code) => process.exit(code ?? 0))
  process.on('SIGINT', () => {
    vite.kill()
  })
  process.on('SIGTERM', () => {
    vite.kill()
  })
}

if (
  process.argv[1] &&
  path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)
) {
  run()
}
