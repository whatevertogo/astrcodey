import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import react, { reactCompilerPreset } from '@vitejs/plugin-react'
import babel from '@rolldown/plugin-babel'
import tailwindcss from '@tailwindcss/vite'
import { defineConfig } from 'vite'

function resolveRunInfo(): { port: number; authToken: string } | undefined {
  const runInfoPath = path.join(os.homedir(), '.astrcode', 'run.json')
  try {
    const raw = fs.readFileSync(runInfoPath, 'utf8')
    const info = JSON.parse(raw)
    if (info?.port) {
      return { port: info.port, authToken: info.authToken ?? '' }
    }
    return undefined
  } catch {
    return undefined
  }
}

const devHost = process.env.TAURI_DEV_HOST
const host = devHost || '127.0.0.1'
const runInfo = resolveRunInfo()

export default defineConfig({
  plugins: [
    react(),
    babel({ presets: [reactCompilerPreset()] }),
    tailwindcss(),
  ],
  clearScreen: false,
  define: runInfo
    ? { 'import.meta.env.VITE_AUTH_TOKEN': JSON.stringify(runInfo.authToken) }
    : undefined,
  server: {
    port: 5173,
    strictPort: true,
    host,
    hmr: devHost ? { protocol: 'ws', host: devHost, port: 5174 } : undefined,
    watch: { ignored: ['**/src-tauri/**'] },
    proxy: runInfo
      ? {
          '/api': {
            target: `http://127.0.0.1:${runInfo.port}`,
            changeOrigin: true,
          },
        }
      : undefined,
  },
})
