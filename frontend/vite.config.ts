import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';
import { defineConfig } from 'vite';

function resolveApiProxyTarget(): string | undefined {
  const runInfoPath = path.join(os.homedir(), '.astrcode', 'run.json');
  try {
    const raw = fs.readFileSync(runInfoPath, 'utf8');
    const info = JSON.parse(raw);
    return info?.port ? `http://127.0.0.1:${info.port}` : undefined;
  } catch {
    return undefined;
  }
}

const host = process.env.TAURI_DEV_HOST;
const apiProxyTarget = resolveApiProxyTarget();

export default defineConfig({
  plugins: [react(), tailwindcss()],
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
    host: host || false,
    hmr: host ? { protocol: 'ws', host, port: 5174 } : undefined,
    watch: { ignored: ['**/src-tauri/**'] },
    proxy: apiProxyTarget
      ? { '/api': { target: apiProxyTarget, changeOrigin: true } }
      : undefined,
  },
});
