import {
  useElapsedSeconds,
  runningElapsedLabel,
} from '../../../hooks/useElapsedSeconds'
import { cn } from '../../../lib/utils'
import {
  arrayValue,
  boolValue,
  byteChangeLabel,
  changesLabel,
  countLines,
  formatBytes,
  numberValue,
  paginationLabel,
  pathFor,
  pathScopeLabel,
  stringValue,
  toolArgs,
  toolMeta,
  truncateMiddle,
  compactLine,
  asRecord,
  type ToolCall,
} from './helpers'
import { CodePreview, MetaGrid, MetaRow, ReadContentPreview } from './shared'

export function FileToolDetails({ block }: { block: ToolCall }) {
  const args = toolArgs(block)
  const meta = toolMeta(block)
  const path = pathFor(block)
  const diff = stringValue(meta, 'diff')
  const content = stringValue(args, 'content')
  const oldStr = stringValue(args, 'oldStr', 'old_string')
  const newStr = stringValue(args, 'newStr', 'new_string')
  const edits = Array.isArray(args.edits) ? args.edits : []
  const created = boolValue(meta, 'created')
  const oldBytes = numberValue(meta, 'oldBytes')
  const newBytes = numberValue(meta, 'newBytes')
  const operationCount = numberValue(meta, 'operationCount')
  const replacements = numberValue(meta, 'replacements')
  const insertions = numberValue(meta, 'insertions')
  const deletions = numberValue(meta, 'deletions')

  return (
    <div className="min-w-0 divide-y divide-border/70">
      <div className="pb-3">
        <MetaGrid>
          <MetaRow label="path" value={path} />
          <MetaRow
            label="action"
            value={
              block.name === 'write'
                ? created
                  ? 'created'
                  : 'overwritten'
                : 'edited'
            }
          />
          <MetaRow label="size" value={changesLabel(meta)} />
          <MetaRow
            label="ops"
            value={
              block.name === 'edit'
                ? [
                    operationCount != null && `${operationCount} op`,
                    replacements != null && `${replacements} repl`,
                  ]
                    .filter(Boolean)
                    .join(' / ')
                : undefined
            }
          />
          <MetaRow
            label="lines"
            value={
              content
                ? countLines(content)
                : insertions != null || deletions != null
                  ? `+${insertions ?? 0} / -${deletions ?? 0}`
                  : undefined
            }
          />
          <MetaRow
            label="bytes"
            value={
              oldBytes != null || newBytes != null
                ? byteChangeLabel(oldBytes, newBytes)
                : undefined
            }
          />
        </MetaGrid>
      </div>

      {diff ? (
        <CodePreview text={diff} tone="diff" />
      ) : content ? (
        <CodePreview text={content} />
      ) : oldStr || newStr ? (
        <div className="space-y-3 pt-3">
          {oldStr && (
            <div>
              <div className="mb-1 font-mono text-[11px] font-semibold uppercase tracking-wide text-text-muted">
                old
              </div>
              <CodePreview text={oldStr} />
            </div>
          )}
          {newStr && (
            <div>
              <div className="mb-1 font-mono text-[11px] font-semibold uppercase tracking-wide text-text-muted">
                new
              </div>
              <CodePreview text={newStr} />
            </div>
          )}
        </div>
      ) : edits.length > 0 ? (
        <div className="space-y-2 pt-3 font-mono text-[12px] text-code-text">
          {edits.slice(0, 8).map((edit, index) => {
            const item = asRecord(edit)
            const itemOld = stringValue(item, 'oldStr', 'old_string')
            const itemNew = stringValue(item, 'newStr', 'new_string')
            return (
              <div key={index} className="min-w-0">
                <span className="text-text-muted">#{index + 1}</span>{' '}
                <span>{truncateMiddle(compactLine(itemOld), 80)}</span>
                <span className="text-text-muted"> -&gt; </span>
                <span>{truncateMiddle(compactLine(itemNew), 80)}</span>
              </div>
            )
          })}
          {edits.length > 8 && (
            <div className="text-text-muted">
              +{edits.length - 8} more edits
            </div>
          )}
        </div>
      ) : (
        <CodePreview text={block.text || 'No file preview available.'} />
      )}
    </div>
  )
}

export function ShellToolDetails({ block }: { block: ToolCall }) {
  const args = toolArgs(block)
  const meta = toolMeta(block)
  const command = stringValue(meta, 'command') || stringValue(args, 'command')
  const intent = stringValue(meta, 'intent') || stringValue(args, 'intent')
  const cwd = stringValue(meta, 'cwd') || stringValue(args, 'cwd')
  const shell = stringValue(meta, 'shell')
  const exitCode = numberValue(meta, 'exitCode')
  const timeout = numberValue(args, 'timeout')
  const timedOut = boolValue(meta, 'timedOut')
  const stdoutBytes = numberValue(meta, 'stdoutBytes')
  const stderrBytes = numberValue(meta, 'stderrBytes')
  const stdin = stringValue(args, 'stdin')
  const streaming = block.status === 'streaming'
  const elapsed = useElapsedSeconds(streaming && !block.text)
  const output =
    block.text ||
    (streaming
      ? command
        ? runningElapsedLabel(elapsed)
        : 'Waiting for output...'
      : '')

  return (
    <div className="min-w-0 divide-y divide-border/70">
      <div className="pb-3">
        <div className="mb-3 font-mono text-[13px] leading-relaxed text-code-text">
          <span className="select-none text-text-muted">$ </span>
          <span className="wrap-break-word">
            {command || '(empty command)'}
          </span>
        </div>
        <MetaGrid>
          <MetaRow label="cwd" value={cwd} />
          <MetaRow label="shell" value={shell} />
          <MetaRow
            label="exit"
            value={
              timedOut
                ? 'timed out'
                : exitCode != null
                  ? String(exitCode)
                  : undefined
            }
          />
          <MetaRow
            label="timeout"
            value={timeout != null ? `${timeout}s` : undefined}
          />
          <MetaRow
            label="output"
            value={[
              formatBytes(stdoutBytes),
              formatBytes(stderrBytes) && `stderr ${formatBytes(stderrBytes)}`,
            ]
              .filter(Boolean)
              .join(' / ')}
          />
          <MetaRow label="intent" value={intent} />
          <MetaRow
            label="stdin"
            value={stdin ? `${formatBytes(stdin.length)} piped` : undefined}
          />
        </MetaGrid>
      </div>
      <CodePreview
        text={output || '(no output)'}
        tone={block.status === 'error' ? 'stderr' : 'default'}
      />
    </div>
  )
}

export function ReadToolDetails({ block }: { block: ToolCall }) {
  const args = toolArgs(block)
  const meta = toolMeta(block)
  const path = pathFor(block)
  const offset = numberValue(meta, 'offset') ?? numberValue(args, 'offset')
  const shownLines = numberValue(meta, 'shownLines')
  const totalLines = numberValue(meta, 'totalLines')
  const returnedChars = numberValue(meta, 'returnedChars')
  const charOffset =
    numberValue(meta, 'charOffset') ?? numberValue(args, 'charOffset')

  return (
    <div className="min-w-0 divide-y divide-border/70">
      <div className="pb-3">
        <MetaGrid>
          <MetaRow label="path" value={path} />
          <MetaRow
            label="lines"
            value={
              shownLines != null && totalLines != null
                ? `${shownLines}/${totalLines}`
                : shownLines
            }
          />
          <MetaRow label="offset" value={offset} />
          <MetaRow label="chars" value={returnedChars} />
          <MetaRow label="charOffset" value={charOffset} />
          <MetaRow label="next" value={paginationLabel(meta)} />
        </MetaGrid>
      </div>
      <ReadContentPreview text={block.text || '(no content)'} />
    </div>
  )
}

export function SearchToolDetails({ block }: { block: ToolCall }) {
  const args = toolArgs(block)
  const meta = toolMeta(block)
  const isFind = block.name === 'find'
  const pattern = stringValue(meta, 'pattern') || stringValue(args, 'pattern')
  const scope = pathScopeLabel(args, meta)
  const outputMode = stringValue(meta, 'outputMode') || 'files_with_matches'
  const returned = numberValue(meta, 'returned') ?? numberValue(meta, 'count')
  const totalMatches = numberValue(meta, 'totalMatches')
  const skippedFiles = numberValue(meta, 'skippedFiles')
  const glob = stringValue(args, 'glob')
  const fileType = stringValue(args, 'fileType')

  return (
    <div className="min-w-0 divide-y divide-border/70">
      <div className="pb-3">
        <MetaGrid>
          <MetaRow label="pattern" value={pattern} />
          <MetaRow label="scope" value={scope} />
          <MetaRow label="mode" value={isFind ? 'files' : outputMode} />
          <MetaRow
            label="returned"
            value={
              totalMatches != null && isFind
                ? `${returned ?? 0}/${totalMatches}`
                : returned
            }
          />
          <MetaRow label="glob" value={glob} />
          <MetaRow label="type" value={fileType} />
          <MetaRow label="skipped" value={skippedFiles} />
          <MetaRow label="next" value={paginationLabel(meta)} />
        </MetaGrid>
      </div>
      <CodePreview text={block.text || '(no results)'} />
    </div>
  )
}

export function PatchToolDetails({ block }: { block: ToolCall }) {
  const args = toolArgs(block)
  const meta = toolMeta(block)
  const patch = stringValue(args, 'patch')
  const files = arrayValue(meta, 'files')
  const applied = numberValue(meta, 'filesApplied', 'filesChanged')
  const failed = numberValue(meta, 'filesFailed')

  return (
    <div className="min-w-0 divide-y divide-border/70">
      <div className="pb-3">
        <MetaGrid>
          <MetaRow label="applied" value={applied} />
          <MetaRow label="failed" value={failed} />
          <MetaRow label="files" value={files.length || undefined} />
        </MetaGrid>
      </div>
      {files.length > 0 && (
        <div className="space-y-1 py-3 font-mono text-[12px] leading-relaxed">
          {files.slice(0, 12).map((file, index) => {
            const item = asRecord(file)
            const fileApplied = boolValue(item, 'applied')
            const changeType = stringValue(item, 'changeType') || 'changed'
            const path = stringValue(item, 'path')
            const error = stringValue(item, 'error')
            return (
              <div key={index} className="flex min-w-0 items-baseline gap-2">
                <span
                  className={cn(
                    'shrink-0 text-[11px] font-semibold uppercase',
                    fileApplied === false ? 'text-danger' : 'text-success'
                  )}
                >
                  {fileApplied === false ? 'failed' : changeType}
                </span>
                <span className="min-w-0 flex-1 wrap-break-word text-code-text">
                  {path || '(unknown path)'}
                </span>
                {error && (
                  <span className="min-w-0 wrap-break-word text-danger">
                    {error}
                  </span>
                )}
              </div>
            )
          })}
          {files.length > 12 && (
            <div className="text-text-muted">
              +{files.length - 12} more files
            </div>
          )}
        </div>
      )}
      {patch ? (
        <CodePreview text={patch} tone="diff" />
      ) : (
        <CodePreview text={block.text || '(no patch preview)'} />
      )}
    </div>
  )
}

export function TerminalToolDetails({ block }: { block: ToolCall }) {
  const args = toolArgs(block)
  const meta = toolMeta(block)
  const action = stringValue(args, 'action')
  const id = stringValue(meta, 'id') || stringValue(args, 'id')
  const command = stringValue(meta, 'command') || stringValue(args, 'command')
  const cwd = stringValue(args, 'cwd')
  const input = stringValue(args, 'input')
  const waitMs = numberValue(args, 'waitMs')
  const bytesSent = numberValue(meta, 'bytesSent')
  const droppedBytes = numberValue(meta, 'droppedBytes')
  const exitCode = numberValue(meta, 'exitCode')
  const alive = boolValue(meta, 'alive')
  const count = numberValue(meta, 'count')
  const terminals = arrayValue(meta, 'terminals')

  return (
    <div className="min-w-0 divide-y divide-border/70">
      <div className="pb-3">
        <MetaGrid>
          <MetaRow label="action" value={action} />
          <MetaRow label="id" value={id} />
          <MetaRow label="command" value={command} />
          <MetaRow label="cwd" value={cwd} />
          <MetaRow
            label="alive"
            value={alive != null ? (alive ? 'yes' : 'no') : undefined}
          />
          <MetaRow label="exit" value={exitCode} />
          <MetaRow label="sent" value={formatBytes(bytesSent)} />
          <MetaRow label="dropped" value={formatBytes(droppedBytes)} />
          <MetaRow
            label="wait"
            value={waitMs != null ? `${waitMs}ms` : undefined}
          />
          <MetaRow
            label="input"
            value={input ? `${formatBytes(input.length)} piped` : undefined}
          />
          <MetaRow label="count" value={count} />
        </MetaGrid>
      </div>
      {terminals.length > 0 && (
        <div className="space-y-1 py-3 font-mono text-[12px] text-code-text">
          {terminals.slice(0, 10).map((terminal, index) => (
            <div key={index}>{String(terminal)}</div>
          ))}
          {terminals.length > 10 && (
            <div className="text-text-muted">
              +{terminals.length - 10} more terminals
            </div>
          )}
        </div>
      )}
      <CodePreview text={block.text || '(no output)'} />
    </div>
  )
}
