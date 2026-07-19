import { useEffect, useMemo, useRef, useState } from 'react'
import type { SlashCommandInfo } from '../../services/types'
import { cn } from '../../lib/utils'

interface CommandSelectorProps {
  visible: boolean
  options: SlashCommandInfo[]
  loading: boolean
  onSelect: (option: SlashCommandInfo) => void
  onClose: () => void
  query: string
}

export default function CommandSelector({
  visible,
  options,
  loading,
  onSelect,
  onClose,
  query,
}: CommandSelectorProps) {
  const [selectedIndex, setSelectedIndex] = useState(0)
  const panelRef = useRef<HTMLDivElement>(null)

  const filteredOptions = useMemo(() => {
    if (!query) return options
    const q = query.toLowerCase()
    return options.filter(
      (opt) =>
        opt.name.toLowerCase().includes(q) ||
        opt.description.toLowerCase().includes(q)
    )
  }, [options, query])

  useEffect(() => {
    if (!visible) return

    const handleKeyDown = (e: KeyboardEvent) => {
      switch (e.key) {
        case 'ArrowDown':
          if (filteredOptions.length === 0) return
          e.preventDefault()
          e.stopPropagation()
          setSelectedIndex((prev) => (prev + 1) % filteredOptions.length)
          break
        case 'ArrowUp':
          if (filteredOptions.length === 0) return
          e.preventDefault()
          e.stopPropagation()
          setSelectedIndex(
            (prev) =>
              (prev - 1 + filteredOptions.length) % filteredOptions.length
          )
          break
        case 'Tab':
        case 'Enter':
          if (filteredOptions.length === 0) return
          if (!e.shiftKey && !e.isComposing) {
            e.preventDefault()
            e.stopPropagation()
            if (filteredOptions[selectedIndex]) {
              onSelect(filteredOptions[selectedIndex])
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
  }, [visible, filteredOptions, selectedIndex, onSelect, onClose])

  useEffect(() => {
    if (!visible || !filteredOptions[selectedIndex]) return
    const target = panelRef.current?.querySelector(
      `[data-index="${selectedIndex}"]`
    )
    target?.scrollIntoView({ block: 'nearest' })
  }, [selectedIndex, visible, filteredOptions])

  if (!visible) return null

  return (
    <div
      className="absolute bottom-[calc(100%+8px)] left-1/2 -translate-x-1/2 w-[calc(100%-24px)] max-w-[760px] max-h-[420px] overflow-y-auto rounded-xl border border-border bg-surface shadow-2xl p-1.5 z-[9999]"
      ref={panelRef}
      onMouseDown={(e) => e.preventDefault()}
      role="listbox"
      aria-label="命令选择"
    >
      {loading ? (
        <div className="flex items-center justify-center py-4 text-xs text-text-muted">
          加载中...
        </div>
      ) : filteredOptions.length === 0 ? (
        <div className="px-3 py-2 text-xs text-text-faint">
          没有找到匹配「{query}」的命令
        </div>
      ) : (
        filteredOptions.map((option, index) => {
          const previousOption = index > 0 ? filteredOptions[index - 1] : null
          const showHeader =
            !previousOption || previousOption.source !== option.source
          const headerText = sourceLabel(option.source)

          return (
            <div key={`${option.source}-${option.name}`}>
              {showHeader && (
                <div className="px-3 py-1.5 mt-1 first:mt-0 text-[11px] font-semibold text-text-muted tracking-wider">
                  {headerText}
                </div>
              )}
              <button
                type="button"
                role="option"
                aria-selected={index === selectedIndex}
                data-index={index}
                onMouseEnter={() => setSelectedIndex(index)}
                onClick={() => onSelect(option)}
                className={cn(
                  'w-full flex items-center justify-start gap-3 h-[34px] text-left transition-all duration-100 ease-out rounded-lg cursor-pointer border',
                  index === selectedIndex
                    ? 'bg-accent-soft text-accent-strong border-l-[3px] border-l-accent-strong pl-[7px] pr-2.5 font-semibold'
                    : 'text-text-secondary border-transparent px-2.5 hover:bg-surface-muted'
                )}
              >
                <span
                  className={cn(
                    'flex items-center justify-center shrink-0 w-4 h-4',
                    index === selectedIndex
                      ? 'text-accent-strong'
                      : 'text-text-muted'
                  )}
                >
                  <CommandIcon
                    source={option.source}
                    selected={index === selectedIndex}
                  />
                </span>
                <div className="flex flex-1 items-center gap-3 min-w-0 overflow-hidden">
                  <span
                    className={cn(
                      'text-[13px] shrink-0 text-inherit leading-normal',
                      index === selectedIndex ? 'font-semibold' : 'font-medium'
                    )}
                  >
                    /{option.name}
                  </span>
                  {option.description && (
                    <span
                      className={cn(
                        'text-[12px] truncate min-w-0 flex-1 leading-normal',
                        index === selectedIndex
                          ? 'text-accent-strong/80'
                          : 'text-text-muted'
                      )}
                      title={option.description}
                    >
                      {option.description}
                    </span>
                  )}
                </div>
              </button>
            </div>
          )
        })
      )}
    </div>
  )
}

function sourceLabel(source: SlashCommandInfo['source']): string {
  switch (source) {
    case 'builtin':
      return '内置命令'
    case 'skill':
      return '技能'
    case 'extension':
      return '插件'
  }
}

function CommandIcon({
  source,
  selected,
}: {
  source: SlashCommandInfo['source']
  selected: boolean
}) {
  if (source === 'skill') {
    return (
      <svg
        className={cn(
          'h-4 w-4',
          selected ? 'text-accent-strong' : 'text-text-muted'
        )}
        viewBox="0 0 24 24"
        aria-hidden="true"
      >
        <path d="M13 10V3L4 14h7v7l9-11h-7z" fill="currentColor" />
      </svg>
    )
  }

  return (
    <svg
      className={cn(
        'h-4 w-4 fill-none',
        selected ? 'stroke-accent-strong' : 'stroke-text-muted'
      )}
      viewBox="0 0 24 24"
      strokeLinecap="round"
      strokeLinejoin="round"
      strokeWidth="2"
      aria-hidden="true"
    >
      <path d="M8 8 4 12l4 4M16 8l4 4-4 4M13 5l-2 14" />
    </svg>
  )
}
