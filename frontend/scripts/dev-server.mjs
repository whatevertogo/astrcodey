/**
 * Wraps `vite` so it only "reports ready" after the HTTP server actually
 * accepts connections.  Tauriʼs `beforeDevCommand` polls `devUrl` in
 * parallel — this guarantees the port is open before polling starts, avoiding
 * ERR_CONNECTION_REFUSED on slow Windows cold-starts.
 */

import { spawn } from 'node:child_process';
import { request } from 'node:http';

const DEV_URL = new URL(process.env.DEV_URL ?? 'http://localhost:5173');
const MAX_RETRIES = 60;
const RETRY_MS = 500;

console.log(`[dev-server] Starting Vite on ${DEV_URL.host}...`);

const vite = spawn('npx', ['vite'], {
  stdio: 'inherit',
  shell: true,
});

let retries = 0;

function poll() {
  const req = request(
    { hostname: DEV_URL.hostname, port: DEV_URL.port, path: '/' },
    (res) => {
      res.resume();
      console.log(`[dev-server] Vite ready — ${DEV_URL.href}`);
    },
  );
  req.on('error', () => {
    if (++retries >= MAX_RETRIES) {
      console.error(`[dev-server] Vite did not start within ${(MAX_RETRIES * RETRY_MS) / 1000}s`);
      vite.kill();
      process.exit(1);
    }
    setTimeout(poll, RETRY_MS);
  });
  req.end();
}

setTimeout(poll, 1000);

// Mirror viteʼs exit code so Tauri knows if something went wrong
vite.on('exit', (code) => process.exit(code ?? 0));
process.on('SIGINT', () => { vite.kill(); });
process.on('SIGTERM', () => { vite.kill(); });
