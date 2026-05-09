/**
 * Starts Vite dev server and blocks until the HTTP port responds.
 */

import { spawn } from 'node:child_process';
import { request } from 'node:http';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const FRONTEND_DIR = path.resolve(__dirname, '..');

const HOST = process.env.TAURI_DEV_HOST || 'localhost';
const PORT = 5173;
const MAX_RETRIES = 120;
const RETRY_MS = 500;

console.log(`[dev-server] Starting Vite in ${FRONTEND_DIR} (host=${HOST}, port=${PORT})...`);

const vite = spawn('npm', ['run', 'dev'], {
  stdio: 'inherit',
  shell: true,
  cwd: FRONTEND_DIR,
});

function poll(retries = 0) {
  request({ hostname: HOST, port: PORT, path: '/', family: 4 }, (res) => {
    res.resume();
    console.log(`[dev-server] Vite ready on http://${HOST}:${PORT}`);
  })
    .on('error', (err) => {
      if (retries >= MAX_RETRIES) {
        console.error(`[dev-server] Timed out after ${(MAX_RETRIES * RETRY_MS) / 1000}s`);
        console.error(`[dev-server] Last error: ${err.code} — ${err.message}`);
        vite.kill();
        process.exit(1);
      }
      setTimeout(() => poll(retries + 1), RETRY_MS);
    })
    .end();
}

setTimeout(poll, 1000);

vite.on('exit', (code) => process.exit(code ?? 0));
process.on('SIGINT', () => { vite.kill(); });
process.on('SIGTERM', () => { vite.kill(); });
