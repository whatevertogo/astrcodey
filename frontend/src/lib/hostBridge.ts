import { isTauriEnvironment, waitForTauriEnvironment } from './tauri'

export interface HostBridge {
  isDesktopHost: boolean
  canSelectDirectory: boolean
  selectDirectory(): Promise<string | null>
  getServerOrigin(): string | null
}

function desktopBridge(): HostBridge {
  return {
    isDesktopHost: true,
    canSelectDirectory: true,
    async selectDirectory() {
      await waitForTauriEnvironment()
      const { invoke } = await import('@tauri-apps/api/core')
      return invoke<string | null>('select_directory')
    },
    getServerOrigin() {
      return window.__ASTRCODE_BOOTSTRAP__?.serverOrigin ?? null
    },
  }
}

function browserBridge(): HostBridge {
  return {
    isDesktopHost: false,
    canSelectDirectory: false,
    selectDirectory() {
      return Promise.resolve(null)
    },
    getServerOrigin() {
      return window.__ASTRCODE_BOOTSTRAP__?.serverOrigin ?? ''
    },
  }
}

let _bridge: HostBridge | null = null

export function getHostBridge(): HostBridge {
  if (!_bridge) {
    const injectedDesktopFlag = Boolean(
      window.__ASTRCODE_BOOTSTRAP__?.isDesktopHost
    )
    _bridge =
      isTauriEnvironment() || injectedDesktopFlag
        ? desktopBridge()
        : browserBridge()
  }
  return _bridge
}
