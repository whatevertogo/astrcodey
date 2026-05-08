const TAURI_WAIT_TIMEOUT_MS = 8000;
const TAURI_WAIT_INTERVAL_MS = 50;

export function isTauriEnvironment(): boolean {
  if (typeof window === 'undefined') return false;
  const internals = (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__;
  return typeof (internals as Record<string, unknown> | undefined)?.invoke === 'function';
}

export async function waitForTauriEnvironment(timeoutMs = TAURI_WAIT_TIMEOUT_MS): Promise<void> {
  const startedAt = Date.now();
  while (!isTauriEnvironment()) {
    if (Date.now() - startedAt >= timeoutMs) {
      throw new Error('Tauri IPC 不可用');
    }
    await new Promise((r) => setTimeout(r, TAURI_WAIT_INTERVAL_MS));
  }
}
