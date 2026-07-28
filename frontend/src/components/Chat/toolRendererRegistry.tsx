import type { ReactNode } from 'react'
import type { ConversationBlock } from '../../services/types'
import type { RenderSpec } from '../../types/render-spec'

export type ToolCallBlockModel = Extract<
  ConversationBlock,
  { kind: 'toolCall' }
>

export type ToolJsonRecord = Record<string, unknown>

export interface ToolRendererContext {
  block: ToolCallBlockModel
  args: ToolJsonRecord
  meta: ToolJsonRecord
  agentSpec?: RenderSpec
}

export interface ToolRenderer {
  id: string
  priority?: number
  match: (context: ToolRendererContext) => boolean
  summary?: (context: ToolRendererContext) => string
  render?: (context: ToolRendererContext) => ReactNode
}

const toolRenderers: ToolRenderer[] = []

export function registerToolRenderer(renderer: ToolRenderer): () => void {
  const existingIndex = toolRenderers.findIndex(
    (item) => item.id === renderer.id
  )
  if (existingIndex >= 0) {
    toolRenderers[existingIndex] = renderer
  } else {
    toolRenderers.push(renderer)
  }
  toolRenderers.sort(
    (left, right) => (right.priority ?? 0) - (left.priority ?? 0)
  )

  return () => {
    const index = toolRenderers.findIndex((item) => item.id === renderer.id)
    if (index >= 0 && toolRenderers[index] === renderer) {
      toolRenderers.splice(index, 1)
    }
  }
}

export function getToolRenderer(
  context: ToolRendererContext
): ToolRenderer | undefined {
  return toolRenderers.find((renderer) => renderer.match(context))
}

export function listToolRenderers(): readonly ToolRenderer[] {
  return toolRenderers
}
