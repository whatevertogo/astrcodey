//! 在附件进入 session 前把图片归一化为 provider 友好的尺寸与体积。
//!
//! 策略对齐 Codex 的 prompt image policy：最长边 2048、32px patch 预算 2500；
//! 尺寸和体积都合格时保留原始字节，只有超预算才缩放并重编码。

use std::io::Cursor;

use astrcode_core::message_attachment::MessageAttachment;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use image::{
    DynamicImage, GenericImageView, ImageDecoder, ImageEncoder, ImageReader, Limits,
    codecs::{jpeg::JpegEncoder, png::PngEncoder},
    imageops::FilterType,
};
use tracing::warn;

const PROMPT_IMAGE_PATCH_SIZE: u32 = 32;
const MAX_PROMPT_IMAGE_DIMENSION: u32 = 2048;
const MAX_PROMPT_IMAGE_PATCHES: u64 = 2_500;

/// provider 友好目标：3 MiB 原始字节，Base64 后约 4 MiB。
const TARGET_IMAGE_BYTES: usize = 3 * 1024 * 1024;
const MIN_IMAGE_DIMENSION: u32 = 320;
const SHRINK_FACTOR: f64 = 0.85;
const MAX_DECODE_DIMENSION: u32 = 12_000;
const MAX_DECODE_ALLOC_BYTES: u64 = 256 * 1024 * 1024;

/// 批量归一化图片附件；非图片附件原样保留。
pub(crate) async fn normalize_image_attachments(attachments: &mut [MessageAttachment]) {
    let jobs: Vec<_> = attachments
        .iter()
        .enumerate()
        .filter(|(_, attachment)| attachment.media_type.starts_with("image/"))
        .map(|(index, attachment)| {
            (
                index,
                attachment.content.clone(),
                attachment.media_type.clone(),
            )
        })
        .collect();
    if jobs.is_empty() {
        return;
    }

    let normalized = tokio::task::spawn_blocking(move || {
        jobs.into_iter()
            .map(|(index, content, media_type)| (index, normalize_one(&content, &media_type)))
            .collect::<Vec<_>>()
    })
    .await;
    match normalized {
        Ok(images) => {
            for (index, image) in images {
                attachments[index].content = image.content;
                attachments[index].media_type = image.media_type;
            }
        },
        Err(error) => {
            warn!(%error, "image normalization task failed; keeping original attachments");
        },
    }
}

struct NormalizedImage {
    content: String,
    media_type: String,
}

fn normalize_one(content: &str, media_type: &str) -> NormalizedImage {
    match try_normalize(content, media_type) {
        Ok((bytes, mime)) => NormalizedImage {
            content: BASE64.encode(bytes),
            media_type: mime,
        },
        Err(error) => {
            warn!(%error, "failed to normalize prompt image; keeping original");
            NormalizedImage {
                content: content.to_string(),
                media_type: media_type.to_string(),
            }
        },
    }
}

fn try_normalize(
    content: &str,
    media_type: &str,
) -> Result<(Vec<u8>, String), ImageNormalizationError> {
    let raw = BASE64.decode(content)?;
    let source = decode_image(&raw)?;
    let (source_width, source_height) = source.dimensions();
    let (target_width, target_height) = prompt_image_dimensions(source_width, source_height);

    if (target_width, target_height) == (source_width, source_height)
        && raw.len() <= TARGET_IMAGE_BYTES
    {
        return Ok((raw, media_type.to_string()));
    }

    let mut image = if (target_width, target_height) == (source_width, source_height) {
        source
    } else {
        source.resize_exact(target_width, target_height, FilterType::Triangle)
    };
    let mut format = OutputFormat::preferred_for(media_type);
    let mut attempt = 0;

    loop {
        let bytes = match format {
            OutputFormat::Png => encode_png(&image)?,
            OutputFormat::Jpeg => encode_jpeg(&image)?,
        };
        if bytes.len() <= TARGET_IMAGE_BYTES || attempt >= 5 {
            return Ok((bytes, format.mime().to_string()));
        }
        // 不透明图片优先尝试 JPEG：同尺寸下通常比 PNG 小得多。
        if format == OutputFormat::Png && !image.color().has_alpha() {
            format = OutputFormat::Jpeg;
            continue;
        }
        let (width, height) = image.dimensions();
        if width <= MIN_IMAGE_DIMENSION && height <= MIN_IMAGE_DIMENSION {
            return Ok((bytes, format.mime().to_string()));
        }
        image = image.resize_exact(
            (f64::from(width) * SHRINK_FACTOR).max(1.0) as u32,
            (f64::from(height) * SHRINK_FACTOR).max(1.0) as u32,
            FilterType::Triangle,
        );
        attempt += 1;
    }
}

fn decode_image(bytes: &[u8]) -> Result<DynamicImage, image::ImageError> {
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DECODE_DIMENSION);
    limits.max_image_height = Some(MAX_DECODE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC_BYTES);
    reader.limits(limits.clone());
    let mut decoder = reader.into_decoder()?;
    // ImageReader::decode 在分配像素缓冲前先做同样的预留检查。
    limits.reserve(decoder.total_bytes())?;
    decoder.set_limits(limits)?;
    let orientation = decoder.orientation().ok();
    let mut image = DynamicImage::from_decoder(decoder)?;
    if let Some(orientation) = orientation {
        image.apply_orientation(orientation);
    }
    Ok(image)
}

/// 返回满足最长边与 patch 预算的目标尺寸；无需缩放时返回原尺寸。
fn prompt_image_dimensions(width: u32, height: u32) -> (u32, u32) {
    let width = width.max(1);
    let height = height.max(1);
    if dimensions_fit(width, height) {
        return (width, height);
    }

    let scale = (f64::from(MAX_PROMPT_IMAGE_DIMENSION) / f64::from(width.max(height))).min(1.0);
    let scaled_width = ((f64::from(width) * scale).round() as u32).max(1);
    let scaled_height = ((f64::from(height) * scale).round() as u32).max(1);
    if dimensions_fit(scaled_width, scaled_height) {
        return (scaled_width, scaled_height);
    }

    let patch = f64::from(PROMPT_IMAGE_PATCH_SIZE);
    let mut patch_scale = (patch * patch * MAX_PROMPT_IMAGE_PATCHES as f64
        / f64::from(scaled_width)
        / f64::from(scaled_height))
    .sqrt();
    let patches_wide = f64::from(scaled_width) * patch_scale / patch;
    let patches_high = f64::from(scaled_height) * patch_scale / patch;
    patch_scale *= (patches_wide.floor() / patches_wide).min(patches_high.floor() / patches_high);

    (
        ((f64::from(scaled_width) * patch_scale).floor() as u32).max(1),
        ((f64::from(scaled_height) * patch_scale).floor() as u32).max(1),
    )
}

fn dimensions_fit(width: u32, height: u32) -> bool {
    let patches_wide = width.div_ceil(PROMPT_IMAGE_PATCH_SIZE);
    let patches_high = height.div_ceil(PROMPT_IMAGE_PATCH_SIZE);
    u64::from(patches_wide) * u64::from(patches_high) <= MAX_PROMPT_IMAGE_PATCHES
        && width <= MAX_PROMPT_IMAGE_DIMENSION
        && height <= MAX_PROMPT_IMAGE_DIMENSION
}

fn encode_png(image: &DynamicImage) -> Result<Vec<u8>, image::ImageError> {
    let rgba = image.to_rgba8();
    let mut buffer = Vec::new();
    PngEncoder::new(&mut buffer).write_image(
        rgba.as_raw(),
        image.width(),
        image.height(),
        image::ExtendedColorType::Rgba8,
    )?;
    Ok(buffer)
}

fn encode_jpeg(image: &DynamicImage) -> Result<Vec<u8>, image::ImageError> {
    let mut buffer = Vec::new();
    JpegEncoder::new_with_quality(&mut buffer, 86).encode_image(image)?;
    Ok(buffer)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputFormat {
    Png,
    Jpeg,
}

impl OutputFormat {
    fn preferred_for(media_type: &str) -> Self {
        if media_type.eq_ignore_ascii_case("image/jpeg")
            || media_type.eq_ignore_ascii_case("image/jpg")
        {
            Self::Jpeg
        } else {
            Self::Png
        }
    }

    fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum ImageNormalizationError {
    #[error("invalid base64 image content: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("image decode/encode failed: {0}")]
    Image(#[from] image::ImageError),
}

#[cfg(test)]
mod tests {
    use image::{ImageFormat, Rgba, RgbaImage};

    use super::*;

    fn png_base64(width: u32, height: u32) -> String {
        let image = RgbaImage::from_pixel(width, height, Rgba([12, 34, 56, 255]));
        let mut bytes = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        BASE64.encode(bytes)
    }

    fn decode_attachment(content: &str) -> DynamicImage {
        image::load_from_memory(&BASE64.decode(content).unwrap()).unwrap()
    }

    #[test]
    fn keeps_small_image_bytes_unchanged() {
        let content = png_base64(640, 480);
        let normalized = normalize_one(&content, "image/png");

        assert_eq!(normalized.content, content);
        assert_eq!(normalized.media_type, "image/png");
    }

    #[test]
    fn resizes_large_image_to_prompt_budget() {
        let normalized = normalize_one(&png_base64(4_000, 3_000), "image/png");
        let image = decode_attachment(&normalized.content);

        assert!(dimensions_fit(image.width(), image.height()));
        assert!(BASE64.decode(&normalized.content).unwrap().len() <= TARGET_IMAGE_BYTES);
    }

    #[test]
    fn patch_budget_shrinks_square_images_below_dimension_limit() {
        let (width, height) = prompt_image_dimensions(2_048, 2_048);

        assert!(dimensions_fit(width, height));
        assert!(width < MAX_PROMPT_IMAGE_DIMENSION);
        assert!(height < MAX_PROMPT_IMAGE_DIMENSION);
    }

    #[test]
    fn undecodable_image_keeps_original_attachment() {
        let content = BASE64.encode(b"not an image");
        let normalized = normalize_one(&content, "image/png");

        assert_eq!(normalized.content, content);
        assert_eq!(normalized.media_type, "image/png");
    }
}
