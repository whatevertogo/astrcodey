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
  const shouldUseDesktop =
    isTauriEnvironment() ||
    Boolean(window.__ASTRCODE_BOOTSTRAP__?.isDesktopHost)

  if (_bridge?.isDesktopHost) {
    return _bridge
  }

  if (_bridge && _bridge.isDesktopHost === shouldUseDesktop) {
    return _bridge
  }

  if (_bridge && !_bridge.isDesktopHost && !shouldUseDesktop) {
    return _bridge
  }

  _bridge = shouldUseDesktop ? desktopBridge() : browserBridge()
  return _bridge
}

export async function resolveHostBridge(): Promise<HostBridge> {
  if (isTauriEnvironment()) {
    return getHostBridge()
  }

  try {
    await waitForTauriEnvironment(1000)
  } catch {
    // Browser/dev mode has no Tauri IPC; fall back to the regular bridge.
  }

  return getHostBridge()
}
