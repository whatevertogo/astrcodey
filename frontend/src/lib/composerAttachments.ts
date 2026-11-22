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

const IMAGE_SIGNATURE_BYTES = 12

async function sniffImageMediaType(file: File): Promise<string | null> {
  const head = new Uint8Array(
    await file.slice(0, IMAGE_SIGNATURE_BYTES).arrayBuffer()
  )
  const ascii = (start: number, end: number) =>
    String.fromCharCode(...head.slice(start, end))

  if (
    head.length >= 8 &&
    head[0] === 0x89 &&
    head[1] === 0x50 &&
    head[2] === 0x4e &&
    head[3] === 0x47
  ) {
    return 'image/png'
  }
  if (
    head.length >= 3 &&
    head[0] === 0xff &&
    head[1] === 0xd8 &&
    head[2] === 0xff
  ) {
    return 'image/jpeg'
  }
  if (head.length >= 6 && ascii(0, 4) === 'GIF8') {
    return 'image/gif'
  }
  if (head.length >= 12 && ascii(0, 4) === 'RIFF' && ascii(8, 12) === 'WEBP') {
    return 'image/webp'
  }
  return null
}

export async function readImageFiles(
  files: FileList | File[],
  capacity = MAX_ATTACHMENTS
): Promise<ImageFilesReadResult> {
  const attachments: PromptAttachment[] = []
  const rejected: string[] = []

  for (const file of Array.from(files)) {
    const name = file.name || 'Pasted image.png'
    // 显式非图片类型直接忽略；空类型交给文件头嗅探，避免把任意文件当成 PNG。
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
    const mediaType = file.type || (await sniffImageMediaType(file))
    if (!mediaType) {
      rejected.push(`${name}：无法识别的图片格式`)
      continue
    }
    attachments.push({
      id: crypto.randomUUID(),
      file,
      filename: name,
      mediaType,
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
