//! 从系统剪贴板读取图片并编码为 prompt 附件。

use arboard::Clipboard;
use astrcode_core::message_attachment::{MAX_ATTACHMENT_CONTENT_BYTES, MessageAttachment};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{ImageBuffer, Rgba};

/// 若剪贴板含图片，返回 PNG Base64 附件。
pub fn read_image_attachment() -> Option<MessageAttachment> {
    let mut clipboard = Clipboard::new().ok()?;
    let data = clipboard.get_image().ok()?;
    if data.bytes.is_empty() {
        return None;
    }
    if data.bytes.len() > MAX_ATTACHMENT_CONTENT_BYTES {
        tracing::warn!(
            bytes = data.bytes.len(),
            "clipboard image exceeds size limit"
        );
        return None;
    }
    let width = u32::try_from(data.width).ok()?;
    let height = u32::try_from(data.height).ok()?;
    let rgba = data.bytes.into_owned();
    let buffer = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(width, height, rgba)?;
    let mut png = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut png);
    image::DynamicImage::ImageRgba8(buffer)
        .write_to(&mut cursor, image::ImageFormat::Png)
        .ok()?;
    if png.len() > MAX_ATTACHMENT_CONTENT_BYTES {
        return None;
    }
    Some(MessageAttachment::image_png(
        "Pasted image.png",
        STANDARD.encode(png),
    ))
}
