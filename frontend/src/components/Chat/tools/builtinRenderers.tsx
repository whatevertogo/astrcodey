import {
  boolValue,
  changesLabel,
  compactLine,
  formatBytes,
  numberValue,
  paginationLabel,
  pathFor,
  stringValue,
  truncateMiddle,
} from './helpers'
import {
  FileToolDetails,
  PatchToolDetails,
  ReadToolDetails,
  SearchToolDetails,
  ShellToolDetails,
  TerminalToolDetails,
} from './details'
import { registerToolRenderer } from './registry'
import {
  todoWriteRenderSpec,
  todoWriteSummaryLine,
} from './todoWrite'
import { RenderSpecViewer } from '../RenderSpecViewer'

registerToolRenderer({
  id: 'builtin:read',
  priority: 100,
  match: ({ block }) => block.name === 'read',
  summary: ({ block, meta }) => {
    const path = pathFor(block)
    const shownLines = numberValue(meta, 'shownLines')
    const totalLines = numberValue(meta, 'totalLines')
    const linePart =
      shownLines != null && totalLines != null
        ? `${shownLines}/${totalLines} lines`
        : shownLines != null
          ? `${shownLines} lines`
          : ''
    return compactLine(
      ['read', path && truncateMiddle(path), linePart, paginationLabel(meta)]
        .filter(Boolean)
        .join(' ')
    )
  },
  render: ({ block }) => <ReadToolDetails block={block} />,
})

registerToolRenderer({
  id: 'builtin:grep',
  priority: 100,
  match: ({ block }) => block.name === 'grep',
  summary: ({ args, meta }) => {
    const pattern = stringValue(meta, 'pattern') || stringValue(args, 'pattern')
    const returned = numberValue(meta, 'returned')
    const outputMode = stringValue(meta, 'outputMode') || 'files_with_matches'
    return compactLine(
      [
        'grep',
        pattern && `"${truncateMiddle(pattern, 60)}"`,
        returned != null && `${returned} result${returned === 1 ? '' : 's'}`,
        outputMode,
        paginationLabel(meta),
      ]
        .filter(Boolean)
        .join(' ')
    )
  },
  render: ({ block }) => <SearchToolDetails block={block} />,
})

registerToolRenderer({
  id: 'builtin:find',
  priority: 100,
  match: ({ block }) => block.name === 'find',
  summary: ({ args, meta }) => {
    const pattern = stringValue(meta, 'pattern') || stringValue(args, 'pattern')
    const count = numberValue(meta, 'count')
    const total = numberValue(meta, 'totalMatches')
    const countPart =
      count != null && total != null
        ? `${count}/${total} files`
        : count != null
          ? `${count} files`
          : ''
    return compactLine(
      [
        'find',
        pattern && truncateMiddle(pattern, 64),
        countPart,
        paginationLabel(meta),
      ]
        .filter(Boolean)
        .join(' ')
    )
  },
  render: ({ block }) => <SearchToolDetails block={block} />,
})

registerToolRenderer({
  id: 'builtin:file-change',
  priority: 100,
  match: ({ block }) => block.name === 'write' || block.name === 'edit',
  summary: ({ block, meta }) => {
    if (block.name === 'write') {
      const path = pathFor(block)
      const created = boolValue(meta, 'created')
      const action =
        created === true ? 'create' : created === false ? 'write' : 'write'
      return compactLine(
        [action, path && truncateMiddle(path), changesLabel(meta)]
          .filter(Boolean)
          .join(' ')
      )
    }

    const path = pathFor(block)
    const replacements = numberValue(meta, 'replacements')
    const operations = numberValue(meta, 'operationCount')
    const count =
      replacements != null
        ? `${replacements} replacement${replacements === 1 ? '' : 's'}`
        : operations != null
          ? `${operations} edit${operations === 1 ? '' : 's'}`
          : ''
    return compactLine(
      ['edit', path && truncateMiddle(path), count, changesLabel(meta)]
        .filter(Boolean)
        .join(' ')
    )
  },
  render: ({ block }) => <FileToolDetails block={block} />,
})

registerToolRenderer({
  id: 'builtin:shell',
  priority: 100,
  match: ({ block }) => block.name === 'shell',
  summary: ({ args, meta }) => {
    const command = stringValue(meta, 'command') || stringValue(args, 'command')
    return command ? `$ ${compactLine(command)}` : ''
  },
  render: ({ block }) => <ShellToolDetails block={block} />,
})

registerToolRenderer({
  id: 'builtin:patch',
  priority: 100,
  match: ({ block }) => block.name === 'patch',
  summary: ({ meta }) => {
    const applied = numberValue(meta, 'filesApplied', 'filesChanged') ?? 0
    const failed = numberValue(meta, 'filesFailed') ?? 0
    return compactLine(
      ['patch', `${applied} applied`, failed > 0 && `${failed} failed`]
        .filter(Boolean)
        .join(' ')
    )
  },
  render: ({ block }) => <PatchToolDetails block={block} />,
})

registerToolRenderer({
  id: 'builtin:todoWrite',
  priority: 100,
  match: ({ block }) => block.name === 'todoWrite',
  summary: ({ args, meta }) => todoWriteSummaryLine(args, meta) ?? '',
  render: ({ block, args, meta }) => {
    const spec = todoWriteRenderSpec(args, meta)
    if (spec) return <RenderSpecViewer spec={spec} />
    if (block.status === 'streaming') return null
    return undefined
  },
})

registerToolRenderer({
  id: 'builtin:terminal',
  priority: 100,
  match: ({ block }) => block.name === 'terminal',
  summary: ({ args, meta }) => {
    const action = stringValue(args, 'action')
    const id = stringValue(meta, 'id') || stringValue(args, 'id')
    const command = stringValue(meta, 'command') || stringValue(args, 'command')
    const bytesSent = numberValue(meta, 'bytesSent')
    const alive = boolValue(meta, 'alive')
    const exitCode = numberValue(meta, 'exitCode')
    const count = numberValue(meta, 'count')
    const status =
      alive != null
        ? alive
          ? 'alive'
          : 'exited'
        : exitCode != null
          ? `exit ${exitCode}`
          : ''
    return compactLine(
      [
        'terminal',
        action,
        command && truncateMiddle(command, 72),
        id && truncateMiddle(id, 36),
        bytesSent != null && formatBytes(bytesSent),
        count != null && `${count} active`,
        status,
      ]
        .filter(Boolean)
        .join(' ')
    )
  },
  render: ({ block }) => <TerminalToolDetails block={block} />,
})
