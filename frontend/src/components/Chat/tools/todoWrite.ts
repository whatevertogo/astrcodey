import type { RenderSpec } from '../../../types/render-spec'
import {
  extractRenderSpec,
  extractRenderSummary,
} from '../../../types/render-spec'
import { arrayValue, compactLine, type JsonRecord } from './helpers'

type TodoStatus = 'pending' | 'in_progress' | 'completed'

interface TodoItem {
  content: string
  activeForm: string
  status: TodoStatus
}

function todoStatus(value: unknown): TodoStatus | undefined {
  if (value === 'pending' || value === 'in_progress' || value === 'completed') {
    return value
  }
  return undefined
}

function parseTodoItem(value: unknown): TodoItem | undefined {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return undefined
  const record = value as JsonRecord
  const content = typeof record.content === 'string' ? record.content.trim() : ''
  const activeForm =
    typeof record.activeForm === 'string'
      ? record.activeForm.trim()
      : typeof record.active_form === 'string'
        ? record.active_form.trim()
        : ''
  const status = todoStatus(record.status)
  if (!content || !activeForm || !status) return undefined
  return { content, activeForm, status }
}

export function todoItemsFromContext(args: JsonRecord, meta: JsonRecord): TodoItem[] {
  const fromMeta = arrayValue(meta, 'newTodos', 'new_todos')
  const fromArgs = arrayValue(args, 'todos')
  const source = fromMeta.length > 0 ? fromMeta : fromArgs
  return source.flatMap((item) => {
    const parsed = parseTodoItem(item)
    return parsed ? [parsed] : []
  })
}

export function buildTodoSummary(items: TodoItem[]): string {
  if (items.length === 0) return 'todoWrite · no items'

  let pending = 0
  let inProgress = 0
  let completed = 0
  for (const item of items) {
    switch (item.status) {
      case 'pending':
        pending += 1
        break
      case 'in_progress':
        inProgress += 1
        break
      case 'completed':
        completed += 1
        break
    }
  }

  const parts = ['todoWrite']
  if (pending > 0) parts.push(`${pending} pending`)
  if (inProgress > 0) parts.push(`${inProgress} in-progress`)
  if (completed > 0) parts.push(`${completed} done`)
  return parts.join(' · ')
}

export function buildTodoRenderSpec(items: TodoItem[]): RenderSpec {
  let pending = 0
  let inProgress = 0
  let completed = 0
  for (const item of items) {
    switch (item.status) {
      case 'pending':
        pending += 1
        break
      case 'in_progress':
        inProgress += 1
        break
      case 'completed':
        completed += 1
        break
    }
  }

  const progressItems: RenderSpec[] = items.map((item) => {
    switch (item.status) {
      case 'completed':
        return {
          type: 'progress',
          label: item.content,
          status: '已完成',
          value: 1,
          tone: 'success',
        }
      case 'in_progress':
        return {
          type: 'progress',
          label: item.content,
          status: '进行中',
          value: 0.5,
          tone: 'accent',
        }
      case 'pending':
        return {
          type: 'progress',
          label: item.content,
          status: '待处理',
          value: 0,
          tone: 'muted',
        }
    }
  })

  return {
    type: 'box',
    title: 'Todo List',
    children: [
      {
        type: 'key_value',
        entries: [
          { key: '总计', value: String(items.length) },
          { key: '待处理', value: String(pending) },
          { key: '进行中', value: inProgress.toString(), tone: 'accent' },
          { key: '已完成', value: completed.toString(), tone: 'success' },
        ],
      },
      {
        type: 'list',
        ordered: false,
        items: progressItems,
      },
    ],
  }
}

export function todoWriteRenderSpec(
  args: JsonRecord,
  meta: JsonRecord
): RenderSpec | undefined {
  return extractRenderSpec(meta) ?? buildTodoRenderSpecFromContext(args, meta)
}

export function buildTodoRenderSpecFromContext(
  args: JsonRecord,
  meta: JsonRecord
): RenderSpec | undefined {
  const items = todoItemsFromContext(args, meta)
  if (items.length === 0) return undefined
  return buildTodoRenderSpec(items)
}

export function todoWriteSummaryLine(
  args: JsonRecord,
  meta: JsonRecord
): string | undefined {
  const fromMeta = extractRenderSummary(meta)
  if (fromMeta) return compactLine(fromMeta)

  const items = todoItemsFromContext(args, meta)
  if (items.length === 0) return undefined
  return compactLine(buildTodoSummary(items))
}
