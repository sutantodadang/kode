use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use kode_core::ImageAttachment;

pub(crate) const MAX_IMAGE_BYTES: usize = 7 * 1024 * 1024;
pub(crate) const MAX_IMAGES: usize = 20;
pub(crate) const MAX_TOTAL_IMAGE_BYTES: usize = 20 * 1024 * 1024;

pub(crate) fn looks_like_image_path(value: &str) -> bool {
    let value = value.trim().trim_matches(['"', '\'']);
    Path::new(value)
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp"
            )
        })
}

pub(crate) fn load_image(cwd: &Path, value: &str) -> anyhow::Result<ImageAttachment> {
    let value = value.trim().trim_matches(['"', '\'']);
    if value.is_empty() {
        bail!("image path is required");
    }

    let supplied = PathBuf::from(value);
    let path = if supplied.is_absolute() {
        supplied
    } else {
        cwd.join(supplied)
    };
    let metadata = std::fs::metadata(&path)
        .with_context(|| format!("cannot read image {}", path.display()))?;
    if !metadata.is_file() {
        bail!("image path is not a file: {}", path.display());
    }
    if metadata.len() > MAX_IMAGE_BYTES as u64 {
        bail!(
            "image is too large ({:.1} MiB); maximum is 7 MiB",
            metadata.len() as f64 / 1024.0 / 1024.0
        );
    }

    let bytes =
        std::fs::read(&path).with_context(|| format!("cannot read image {}", path.display()))?;
    let media_type = detect_media_type(&bytes)
        .ok_or_else(|| anyhow::anyhow!("unsupported image; use PNG, JPEG, GIF, or WebP"))?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("image")
        .to_string();

    Ok(ImageAttachment {
        name,
        media_type: media_type.to_string(),
        data: STANDARD.encode(&bytes),
        size_bytes: bytes.len(),
    })
}

fn detect_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_supported_formats_by_content() {
        assert_eq!(detect_media_type(b"\x89PNG\r\n\x1a\n"), Some("image/png"));
        assert_eq!(detect_media_type(b"\xff\xd8\xff\0"), Some("image/jpeg"));
        assert_eq!(detect_media_type(b"GIF89a"), Some("image/gif"));
        assert_eq!(detect_media_type(b"RIFF\0\0\0\0WEBP"), Some("image/webp"));
        assert_eq!(detect_media_type(b"not an image"), None);
    }
}
