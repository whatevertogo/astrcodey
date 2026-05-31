import { useEffect } from 'react'
import { useAppStore } from '../store/conversation'
import { findKeybinding, keyIdFromKeyboardEvent } from '../lib/keybindings'

/**
 * 扩展快捷键（与 CLI 一致）：从服务端 keybindings 表分发为扩展命令执行。
 * 使用 capture 阶段，以便在输入框内也能触发（如 Shift+Tab 切换 mode）。
 */
export function useKeybindings() {
  const keybindings = useAppStore((s) => s.keybindings)
  const activeSessionId = useAppStore((s) => s.activeSessionId)
  const executeExtensionCommand = useAppStore((s) => s.executeExtensionCommand)

  useEffect(() => {
    if (!activeSessionId || keybindings.length === 0) return

    const handler = (event: KeyboardEvent) => {
      const pressed = keyIdFromKeyboardEvent(event)
      const binding = findKeybinding(keybindings, pressed)
      if (!binding) return

      event.preventDefault()
      event.stopPropagation()
      void executeExtensionCommand(binding.command, binding.arguments)
    }

    window.addEventListener('keydown', handler, true)
    return () => window.removeEventListener('keydown', handler, true)
  }, [activeSessionId, keybindings, executeExtensionCommand])
}
