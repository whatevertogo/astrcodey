import type { KeybindingInfo } from '../services/types'

/** 与后端/CLI 一致的快捷键 id（如 `shift+tab`）。 */
export function keyIdFromKeyboardEvent(event: KeyboardEvent): string {
  const parts: string[] = []
  if (event.ctrlKey || event.metaKey) parts.push('ctrl')
  if (event.altKey) parts.push('alt')
  if (event.shiftKey) parts.push('shift')

  const raw = event.key
  if (['Control', 'Alt', 'Shift', 'Meta'].includes(raw)) {
    return parts.join('+')
  }

  let key = raw
  if (key === 'Tab') key = 'tab'
  else if (key === ' ') key = 'space'
  else if (key === 'Escape') key = 'escape'
  else if (key.length === 1) key = key.toLowerCase()
  else key = key.toLowerCase()

  parts.push(key)
  return parts.join('+')
}

/** 规范化扩展注册的 key 字段，便于与按键事件比较。 */
export function canonicalBindingKey(key: string): string {
  return key
    .split('+')
    .map((part) => {
      const p = part.trim().toLowerCase()
      if (p === 'meta' || p === 'cmd' || p === 'command') return 'ctrl'
      return p
    })
    .join('+')
}

export function findKeybinding(
  keybindings: KeybindingInfo[],
  pressedId: string
): KeybindingInfo | undefined {
  return keybindings.find((item) => canonicalBindingKey(item.key) === pressedId)
}

export function slashCommandText(binding: KeybindingInfo): string {
  const args = binding.arguments.trim()
  return args ? `/${binding.command} ${args}`.trim() : `/${binding.command}`
}

/** 解析 `/name args`；非斜杠命令返回 null。 */
export function parseSlashCommand(
  text: string
): { name: string; arguments: string } | null {
  const trimmed = text.trim()
  if (!trimmed.startsWith('/')) return null
  const body = trimmed.slice(1).trim()
  if (!body) return { name: '', arguments: '' }
  const space = body.search(/\s/)
  if (space === -1) {
    return { name: body.toLowerCase(), arguments: '' }
  }
  return {
    name: body.slice(0, space).toLowerCase(),
    arguments: body.slice(space).trim(),
  }
}

/** 是否为已注册的扩展/内置斜杠命令（忙碌时不应进入 composer 队列）。 */
export function isRegisteredSlashCommand(
  text: string,
  commands: { name: string }[]
): boolean {
  const parsed = parseSlashCommand(text)
  if (!parsed || !parsed.name) return false
  if (parsed.name === 'compact' || parsed.name === 'model') return true
  return commands.some((c) => c.name.toLowerCase() === parsed.name)
}
