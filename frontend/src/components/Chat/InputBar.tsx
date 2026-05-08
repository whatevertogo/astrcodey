import { useRef, useState, useCallback } from 'react'
import { useAppStore } from '../../store/conversation'
import {
  composerShell,
  composerSubmitButton,
  composerInterruptButton,
} from '../../lib/styles'
import { cn } from '../../lib/utils'
import ModelSelector from './ModelSelector'
import * as api from '../../services/api'

function isExecutionPhase(phase: string): boolean {
  return (
    phase === 'thinking' || phase === 'streaming' || phase === 'calling_tool'
  )
}

export default function InputBar() {
  const submitPrompt = useAppStore((s) => s.submitPrompt)
  const abortCurrentTurn = useAppStore((s) => s.abortCurrentTurn)
  const phase = useAppStore((s) => s.phase)
  const control = useAppStore((s) => s.control)
  const workingDir = useAppStore((s) => s.workingDir)
  const activeSessionId = useAppStore((s) => s.activeSessionId)
  const modelRefreshKey = useAppStore((s) => s.modelRefreshKey)
  const bumpModelRefreshKey = useAppStore((s) => s.bumpModelRefreshKey)

  const [value, setValue] = useState('')
  const [isComposing, setIsComposing] = useState(false)
  const textareaRef = useRef<HTMLTextAreaElement>(null)
  const isBusy = isExecutionPhase(phase)
  const canSubmit = control?.canSubmitPrompt ?? false

  const handleInput = useCallback(
    (event: React.ChangeEvent<HTMLTextAreaElement>) => {
      setValue(event.target.value)
      const textarea = textareaRef.current
      if (!textarea) return
      textarea.style.height = 'auto'
      textarea.style.height = `${Math.min(textarea.scrollHeight, 200)}px`
    },
    []
  )

  const submit = useCallback(async () => {
    const trimmed = value.trim()
    if (!trimmed || !activeSessionId || !canSubmit) return
    const accepted = await submitPrompt(trimmed)
    if (!accepted) return
    setValue('')
    if (textareaRef.current) {
      textareaRef.current.style.height = 'auto'
    }
  }, [value, activeSessionId, canSubmit, submitPrompt])

  const handleKeyDown = useCallback(
    (event: React.KeyboardEvent<HTMLTextAreaElement>) => {
      if (event.key === 'Enter' && !event.shiftKey && !isComposing) {
        event.preventDefault()
        submit().catch((err) => console.error('submit failed:', err))
      }
    },
    [submit, isComposing]
  )

  return (
    <div className="flex-shrink-0 bg-panel-bg px-[var(--chat-content-horizontal-padding)] pb-[18px] pt-4">
      <div className="mx-auto w-full max-w-[var(--chat-composer-max-width)] translate-x-[var(--chat-assistant-center-shift)]">
        <div className="relative w-full">
          <div className={composerShell}>
            {workingDir && (
              <div
                className="flex items-center gap-2 rounded-t-[23px] border-b border-border bg-white/40 px-4 py-2.5 text-text-secondary"
                title={workingDir}
              >
                <span
                  className="inline-flex h-3.5 w-3.5 flex-shrink-0 items-center justify-center"
                  aria-hidden="true"
                >
                  <svg className="h-3.5 w-3.5" viewBox="0 0 20 20">
                    <path
                      d="M2.5 5.75A1.75 1.75 0 0 1 4.25 4h4.03c.46 0 .9.18 1.23.5l1.02 1c.32.3.74.47 1.18.47h4.04A1.75 1.75 0 0 1 17.5 7.72v6.53A1.75 1.75 0 0 1 15.75 16H4.25A1.75 1.75 0 0 1 2.5 14.25V5.75Z"
                      fill="none"
                      stroke="currentColor"
                      strokeLinejoin="round"
                      strokeWidth="1.4"
                    />
                  </svg>
                </span>
                <div className="overflow-hidden text-ellipsis whitespace-nowrap font-mono text-xs">
                  {workingDir}
                </div>
              </div>
            )}
            <div className="relative">
              <div className="flex flex-col px-[var(--chat-composer-shell-padding-x)] py-3">
                <textarea
                  ref={textareaRef}
                  className="mb-3 max-h-[240px] min-h-[50px] w-full resize-none overflow-y-auto border-0 bg-transparent p-0 text-[15px] leading-[1.75] text-text-primary placeholder:text-text-muted focus:outline-none disabled:cursor-not-allowed disabled:opacity-60"
                  placeholder="向 AstrCode 提问..."
                  value={value}
                  rows={1}
                  onChange={handleInput}
                  onKeyDown={handleKeyDown}
                  onCompositionStart={() => setIsComposing(true)}
                  onCompositionEnd={() => setIsComposing(false)}
                  disabled={!activeSessionId || (!canSubmit && !isBusy)}
                />
                <div className="flex items-center justify-between">
                  <div className="flex flex-shrink-0 items-center gap-2">
                    <ModelSelector
                      refreshKey={modelRefreshKey}
                      getCurrentModel={api.getCurrentModel}
                      listAvailableModels={api.listModels}
                      setModel={async (profileName, model) => {
                        await api.updateActiveSelection(profileName, model)
                        bumpModelRefreshKey()
                      }}
                    />
                  </div>
                  <div className="flex flex-shrink-0 items-center gap-2">
                    {isBusy ? (
                      <button
                        className={composerInterruptButton}
                        type="button"
                        onClick={() => void abortCurrentTurn()}
                      >
                        中断
                      </button>
                    ) : (
                      <button
                        className={cn(composerSubmitButton)}
                        type="button"
                        onClick={() => void submit()}
                        disabled={
                          !value.trim() || !activeSessionId || !canSubmit
                        }
                        aria-label="发送消息"
                        title="发送消息"
                      >
                        <svg
                          viewBox="0 0 24 24"
                          fill="none"
                          stroke="currentColor"
                          strokeWidth="2.5"
                          strokeLinecap="round"
                          strokeLinejoin="round"
                        >
                          <line x1="12" y1="19" x2="12" y2="5"></line>
                          <polyline points="5 12 12 5 19 12"></polyline>
                        </svg>
                      </button>
                    )}
                  </div>
                </div>
              </div>
            </div>
          </div>
        </div>
      </div>
      <div className="mx-auto mt-2.5 w-full max-w-[var(--chat-composer-max-width)] text-center text-xs text-text-muted">
        AI 可能会产生误导性信息，请核实重要内容
      </div>
    </div>
  )
}
