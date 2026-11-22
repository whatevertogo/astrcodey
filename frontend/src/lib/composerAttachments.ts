import type { PromptAttachment } from '../services/types'

export const MAX_ATTACHMENTS = 4
export const MAX_ATTACHMENT_CONTENT_BYTES = 10 * 1024 * 1024

export interface ImageFilesReadResult {
  attachments: PromptAttachment[]
  /** 被跳过的文件及原因，供 composer 显示，避免图片静默消失。 */
  rejected: string[]
}

function formatMib(bytes: number): string {
  return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`
}

export function readImageFiles(
  files: FileList | File[],
  capacity = MAX_ATTACHMENTS
): ImageFilesReadResult {
  const attachments: PromptAttachment[] = []
  const rejected: string[] = []

  for (const file of Array.from(files)) {
    const name = file.name || 'Pasted image.png'
    // 部分 WebView/剪贴板来源的图片 File.type 为空，不能据此丢弃。
    if (file.type && !file.type.startsWith('image/')) continue
    if (attachments.length >= capacity) {
      rejected.push(`${name}：最多添加 ${MAX_ATTACHMENTS} 张图片`)
      continue
    }
    if (file.size > MAX_ATTACHMENT_CONTENT_BYTES) {
      rejected.push(
        `${name} 体积 ${formatMib(file.size)}，超过 ${formatMib(MAX_ATTACHMENT_CONTENT_BYTES)} 上限`
      )
      continue
    }
    attachments.push({
      id: crypto.randomUUID(),
      file,
      filename: name,
      mediaType: file.type || 'image/png',
      previewUrl: URL.createObjectURL(file),
    })
  }

  return { attachments, rejected }
}

function fileToBase64(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader()
    reader.onload = () => {
      const result = reader.result
      if (typeof result !== 'string') {
        reject(new Error('Failed to read attachment as data URL'))
        return
      }
      const comma = result.indexOf(',')
      resolve(comma >= 0 ? result.slice(comma + 1) : result)
    }
    reader.onerror = () =>
      reject(reader.error ?? new Error('Failed to read attachment'))
    reader.readAsDataURL(file)
  })
}

export async function attachmentToWire(
  attachment: PromptAttachment
): Promise<{ filename: string; content: string; mediaType: string }> {
  return {
    filename: attachment.filename,
    content: await fileToBase64(attachment.file),
    mediaType: attachment.mediaType,
  }
}

export function revokeAttachmentPreviews(attachments: PromptAttachment[]) {
  for (const attachment of attachments) {
    URL.revokeObjectURL(attachment.previewUrl)
  }
}
