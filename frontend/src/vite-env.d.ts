/// <reference types="vite/client" />

interface Window {
  __TAURI_INTERNALS__?: {
    invoke?: unknown
    transformCallback?: unknown
  }
  __ASTRCODE_BOOTSTRAP__?: {
    isDesktopHost?: boolean
    serverOrigin?: string
  }
}
