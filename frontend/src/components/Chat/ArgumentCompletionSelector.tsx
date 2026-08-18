import { useEffect, useRef, useState } from 'react'
import type { CommandCompletionItem } from '../../services/types'
import { cn } from '../../lib/utils'

interface ArgumentCompletionSelectorProps {
  visible: boolean
  items: CommandCompletionItem[]
  loading: boolean
  truncated: boolean
  onSelect: (item: CommandCompletionItem) => void
  onClose: () => void
}

/**
 * 斜杠命令参数补全面板。
 *
 * 展示 server 端 `argument_completions` 返回的候选；Tab/Enter 插入，
 * 上下键导航，Escape 关闭。键处理与 CommandSelector 相同，用 window
 * capture 监听避免与 textarea 的 Enter 提交冲突。
 */
export default function ArgumentCompletionSelector({
  visible,
  items,
  loading,
  truncated,
  onSelect,
  onClose,
}: ArgumentCompletionSelectorProps) {
  const [selectedIndex, setSelectedIndex] = useState(0)
  const panelRef = useRef<HTMLDivElement>(null)

  // 候选列表变化时重置选中项（adjusting-state-during-render 模式，
  // 避免在 effect 里同步 setState）。
  const [prevItems, setPrevItems] = useState(items)
  if (prevItems !== items) {
    setPrevItems(items)
    setSelectedIndex(0)
  }

  useEffect(() => {
    if (!visible) return

    const handleKeyDown = (e: KeyboardEvent) => {
      switch (e.key) {
        case 'ArrowDown':
          if (items.length === 0) return
          e.preventDefault()
          e.stopPropagation()
          setSelectedIndex((prev) => (prev + 1) % items.length)
          break
        case 'ArrowUp':
          if (items.length === 0) return
          e.preventDefault()
          e.stopPropagation()
          setSelectedIndex((prev) => (prev - 1 + items.length) % items.length)
          break
        case 'Tab':
        case 'Enter':
          if (items.length === 0) return
          if (!e.shiftKey && !e.isComposing) {
            e.preventDefault()
            e.stopPropagation()
            if (items[selectedIndex]) {
              onSelect(items[selectedIndex])
            }
          }
          break
        case 'Escape':
          e.preventDefault()
          e.stopPropagation()
          onClose()
          break
      }
    }

    window.addEventListener('keydown', handleKeyDown, { capture: true })
    return () =>
      window.removeEventListener('keydown', handleKeyDown, { capture: true })
  }, [visible, items, selectedIndex, onSelect, onClose])

  useEffect(() => {
    if (!visible || !items[selectedIndex]) return
    const target = panelRef.current?.querySelector(
      `[data-index="${selectedIndex}"]`
    )
    target?.scrollIntoView({ block: 'nearest' })
  }, [selectedIndex, visible, items])

  if (!visible) return null

  return (
    <div
      className="absolute bottom-[calc(100%+8px)] left-1/2 -translate-x-1/2 w-[calc(100%-24px)] max-w-[760px] max-h-[300px] overflow-y-auto rounded-xl border border-border bg-surface shadow-2xl p-1.5 z-[9999]"
      ref={panelRef}
      onMouseDown={(e) => e.preventDefault()}
      role="listbox"
      aria-label="参数补全"
    >
      {loading ? (
        <div className="flex items-center justify-center py-4 text-xs text-text-muted">
          加载中...
        </div>
      ) : items.length === 0 ? (
        <div className="px-3 py-2 text-xs text-text-faint">无补全建议</div>
      ) : (
        <>
          {items.map((item, index) => (
            <button
              key={`${item.insertText}-${index}`}
              type="button"
              role="option"
              aria-selected={index === selectedIndex}
              data-index={index}
              onMouseEnter={() => setSelectedIndex(index)}
              onClick={() => onSelect(item)}
              className={cn(
                'w-full flex items-center justify-between gap-3 h-[34px] text-left transition-all duration-100 ease-out rounded-lg cursor-pointer border',
                index === selectedIndex
                  ? 'bg-accent-soft text-accent-strong border-l-[3px] border-l-accent-strong pl-[7px] pr-2.5 font-semibold'
                  : 'text-text-secondary border-transparent px-2.5 hover:bg-surface-muted'
              )}
            >
              <span className="text-[13px] truncate leading-normal">
                {item.label}
              </span>
              {item.detail && (
                <span className="text-[11px] shrink-0 max-w-[45%] truncate text-text-muted">
                  {item.detail}
                </span>
              )}
            </button>
          ))}
          {truncated && (
            <div className="px-3 py-1.5 text-[11px] text-text-faint">
              结果过多，已截断
            </div>
          )}
        </>
      )}
    </div>
  )
}
