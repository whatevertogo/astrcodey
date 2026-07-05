import { useEffect, useState, useMemo, useRef } from 'react'
import type { AvailableModel, CurrentModelInfo } from '../../services/types'
import { cn } from '../../lib/utils'

interface ModelSelectorProps {
  refreshKey: number
  getCurrentModel: () => Promise<CurrentModelInfo>
  listAvailableModels: () => Promise<AvailableModel[]>
  setModel: (profileName: string, model: string) => Promise<void>
}

function wireFormatLabel(value: AvailableModel['wireFormat']): string {
  switch (value) {
    case 'openai_chat_completions':
      return 'OpenAI Chat'
    case 'openai_responses':
      return 'OpenAI Responses'
    case 'anthropic_messages':
      return 'Anthropic Messages'
    case 'google_genai':
      return 'Google GenAI'
  }
}

export default function ModelSelector({
  refreshKey,
  getCurrentModel,
  listAvailableModels,
  setModel,
}: ModelSelectorProps) {
  const wrapperRef = useRef<HTMLDivElement>(null)
  const [currentModel, setCurrentModel] = useState<CurrentModelInfo | null>(
    null
  )
  const [options, setOptions] = useState<AvailableModel[]>([])
  const [loading, setLoading] = useState(true)
  const [open, setOpen] = useState(false)
  const [searchQuery, setSearchQuery] = useState('')
  const searchInputRef = useRef<HTMLInputElement>(null)

  useEffect(() => {
    let cancelled = false
    const load = async () => {
      setLoading(true)
      try {
        const [nextOptions, nextCurrent] = await Promise.all([
          listAvailableModels(),
          getCurrentModel(),
        ])
        if (cancelled) return
        setOptions(nextOptions)
        setCurrentModel(nextCurrent)
      } catch {
        if (!cancelled) setOptions([])
      } finally {
        if (!cancelled) setLoading(false)
      }
    }
    void load()
    return () => {
      cancelled = true
    }
  }, [getCurrentModel, listAvailableModels, refreshKey])

  useEffect(() => {
    if (!open) return
    const handlePointerDown = (e: PointerEvent) => {
      if (
        wrapperRef.current &&
        !wrapperRef.current.contains(e.target as Node)
      ) {
        setOpen(false)
        setSearchQuery('')
      }
    }
    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        setOpen(false)
        setSearchQuery('')
      }
    }
    document.addEventListener('pointerdown', handlePointerDown)
    window.addEventListener('keydown', handleKeyDown)
    requestAnimationFrame(() => searchInputRef.current?.focus())
    return () => {
      document.removeEventListener('pointerdown', handlePointerDown)
      window.removeEventListener('keydown', handleKeyDown)
    }
  }, [open])

  const groupedOptions = useMemo(() => {
    const q = searchQuery.toLowerCase()
    const groups = new Map<string, AvailableModel[]>()
    for (const opt of options) {
      if (
        q &&
        !opt.modelId.toLowerCase().includes(q) &&
        !opt.profileName.toLowerCase().includes(q)
      )
        continue
      const group = groups.get(opt.profileName) ?? []
      group.push(opt)
      groups.set(opt.profileName, group)
    }
    return Array.from(groups.entries())
  }, [options, searchQuery])

  const handleSelect = async (profileName: string, modelId: string) => {
    setOpen(false)
    setSearchQuery('')
    try {
      await setModel(profileName, modelId)
      const refreshed = await getCurrentModel()
      setCurrentModel(refreshed)
    } catch {
      /* error silently */
    }
  }

  return (
    <div ref={wrapperRef} className="relative">
      <button
        type="button"
        className={cn(
          'flex h-9 items-center gap-1.5 rounded-full px-2.5 text-[13px] transition-colors duration-150 ease-out disabled:cursor-not-allowed disabled:opacity-60',
          open
            ? 'bg-surface-muted text-text-primary'
            : 'text-text-secondary hover:bg-surface-muted hover:text-text-primary'
        )}
        onClick={() => {
          if (!loading) {
            setOpen((v) => {
              if (v) setSearchQuery('')
              return !v
            })
          }
        }}
        disabled={loading}
        aria-label="选择模型"
      >
        <span className="max-w-[140px] truncate font-medium">
          {currentModel?.modelId || (loading ? '加载中...' : '未选择')}
        </span>
        <svg
          className={`w-3.5 h-3.5 transition-transform duration-200 ${open ? 'rotate-180' : ''}`}
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth="2"
          strokeLinecap="round"
          strokeLinejoin="round"
        >
          <polyline points="6 9 12 15 18 9" />
        </svg>
      </button>

      {open && (
        <div className="absolute bottom-[calc(100%+8px)] left-0 z-[9999] flex w-[240px] origin-bottom-left flex-col rounded-2xl border border-border bg-surface shadow-soft animate-in fade-in zoom-in-95 duration-100">
          <div className="p-1.5 border-b border-border">
            <input
              ref={searchInputRef}
              type="text"
              placeholder="搜索模型..."
              className="w-full bg-transparent border-none py-1.5 pl-2 pr-2 text-[13px] text-text-primary focus:outline-none placeholder:text-text-muted"
              value={searchQuery}
              onChange={(e) => setSearchQuery(e.target.value)}
            />
          </div>
          <div className="overflow-y-auto p-1.5 max-h-[240px]">
            {groupedOptions.length === 0 ? (
              <div className="px-3 py-6 text-center text-[12px] text-text-muted">
                {options.length === 0 ? '未配置模型' : '无结果'}
              </div>
            ) : (
              groupedOptions.map(([profileName, groupOpts], gi) => (
                <div key={profileName} className={gi > 0 ? 'mt-1.5' : ''}>
                  <div className="px-4 py-1.5 text-[11px] font-semibold text-text-muted tracking-wider uppercase select-none">
                    {profileName} · {wireFormatLabel(groupOpts[0].wireFormat)}
                  </div>
                  <div className="flex flex-col gap-0.5 px-1.5">
                    {groupOpts.map((opt) => {
                      const isActive =
                        currentModel?.profileName === opt.profileName &&
                        currentModel?.modelId === opt.modelId
                      return (
                        <button
                          key={`${opt.profileName}-${opt.modelId}`}
                          type="button"
                          className={cn(
                            'w-full flex items-center justify-between px-3 h-[34px] text-left rounded-lg text-[13px] font-medium transition-all duration-100 ease-out',
                            isActive
                              ? 'bg-accent-soft text-accent-strong border-l-[3px] border-l-accent-strong pl-[9px]'
                              : 'text-text-primary hover:bg-surface-muted'
                          )}
                          onClick={() =>
                            void handleSelect(opt.profileName, opt.modelId)
                          }
                        >
                          <span className="truncate">{opt.modelId}</span>
                          {isActive && (
                            <svg
                              className="w-4 h-4 text-accent-strong shrink-0"
                              viewBox="0 0 24 24"
                              fill="none"
                              stroke="currentColor"
                              strokeWidth="2.5"
                              strokeLinecap="round"
                              strokeLinejoin="round"
                            >
                              <polyline points="20 6 9 17 4 12" />
                            </svg>
                          )}
                        </button>
                      )
                    })}
                  </div>
                </div>
              ))
            )}
          </div>
        </div>
      )}
    </div>
  )
}
