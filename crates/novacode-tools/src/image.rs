//! Image normalisation at attach time (L2.22): sniff the real format, keep a
//! file that already fits byte-for-byte, otherwise convert and shrink it to
//! fit every provider (2000×2000, 4.5MB of base64, under Anthropic's 5MB).
//! The normalised image is what gets stored, so it's done once.

use base64::Engine;
use novacode_types::Block;
use std::path::{Path, PathBuf};

pub const MAX_SIDE: u32 = 2000;
/// Base64 payload limit.
pub const MAX_B64: usize = 4_718_592;
/// Refuse to even decode anything bigger than this.
#[cfg(feature = "images")]
const MAX_INPUT: usize = 50 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub mime: &'static str,
    /// Base64.
    pub data: String,
    pub width: u32,
    pub height: u32,
    /// Original size, when it was shrunk.
    pub resized_from: Option<(u32, u32)>,
}

impl Image {
    pub fn block(&self) -> Block {
        Block::Image {
            mime_type: self.mime.into(),
            data: self.data.clone(),
        }
    }

    /// `1200×800` or `2000×1500, shrunk from 4000×3000`.
    pub fn describe(&self) -> String {
        match self.resized_from {
            Some((w, h)) => format!("{}×{}, shrunk from {w}×{h}", self.width, self.height),
            None => format!("{}×{}", self.width, self.height),
        }
    }
}

/// The formats every provider takes as-is.
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        _ => None,
    }
}

/// Whether the bytes look like any image (including ones that need converting).
pub fn is_image(bytes: &[u8]) -> bool {
    sniff(bytes).is_some()
        || matches!(
            bytes,
            [b'B', b'M', ..] | [0, 0, 1, 0, ..] | [b'I', b'I', 42, 0, ..] | [b'M', b'M', 0, 42, ..]
        )
        || bytes
            .get(4..12)
            .is_some_and(|b| b.starts_with(b"ftyp") && (b.ends_with(b"heic") || b.ends_with(b"heix")))
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn b64_len(n: usize) -> usize {
    n.div_ceil(3) * 4
}

#[cfg(feature = "images")]
pub fn normalize(bytes: &[u8]) -> Result<Image, String> {
    use image::codecs::jpeg::JpegEncoder;
    use image::imageops::FilterType;
    use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};
    use std::io::Cursor;

    if bytes.len() > MAX_INPUT {
        return Err(format!(
            "image is {} MB; the limit is {} MB",
            bytes.len() >> 20,
            MAX_INPUT >> 20
        ));
    }
    let reader = || {
        ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|e| e.to_string())
    };
    let unsupported = |e: String| {
        if bytes.get(4..8) == Some(b"ftyp") {
            "HEIC images aren't supported; convert to PNG or JPEG first".to_string()
        } else {
            format!("can't read this image: {e}")
        }
    };
    // Fast path: a supported file that fits is sent byte-for-byte (header read only).
    if let Some(mime) = sniff(bytes)
        && let Ok((w, h)) = reader()?.into_dimensions()
        && w <= MAX_SIDE
        && h <= MAX_SIDE
        && b64_len(bytes.len()) <= MAX_B64
    {
        return Ok(Image {
            mime,
            data: b64(bytes),
            width: w,
            height: h,
            resized_from: None,
        });
    }
    let mut decoder = reader()?.into_decoder().map_err(|e| unsupported(e.to_string()))?;
    let orientation = decoder.orientation().ok();
    let mut img = DynamicImage::from_decoder(decoder).map_err(|e| unsupported(e.to_string()))?;
    if let Some(o) = orientation {
        img.apply_orientation(o);
    }
    let (w0, h0) = (img.width(), img.height());
    let encode = |img: &DynamicImage| -> Vec<(&'static str, Vec<u8>)> {
        let mut out = Vec::new();
        let mut png = Vec::new();
        if img.write_to(&mut Cursor::new(&mut png), ImageFormat::Png).is_ok() {
            out.push(("image/png", png));
        }
        // JPEG has no alpha: only offer it for opaque images.
        if !img.color().has_alpha() {
            for q in [80, 70, 55, 40] {
                let mut jpg = Vec::new();
                if JpegEncoder::new_with_quality(&mut jpg, q)
                    .encode_image(&img.to_rgb8())
                    .is_ok()
                {
                    out.push(("image/jpeg", jpg));
                }
            }
        }
        out
    };
    let mut side = MAX_SIDE.min(w0.max(h0));
    loop {
        let scaled = if w0.max(h0) > side {
            img.resize(side, side, FilterType::Lanczos3)
        } else {
            img.clone()
        };
        // The smallest encoding that fits (PNG first when both do, it's lossless).
        let best = encode(&scaled)
            .into_iter()
            .filter(|(_, b)| b64_len(b.len()) <= MAX_B64)
            .min_by_key(|(m, b)| (*m != "image/png", b.len()));
        if let Some((mime, b)) = best {
            let (w, h) = (scaled.width(), scaled.height());
            return Ok(Image {
                mime,
                data: b64(&b),
                width: w,
                height: h,
                resized_from: ((w, h) != (w0, h0)).then_some((w0, h0)),
            });
        }
        if side <= 64 {
            return Err("can't shrink this image under the size limit".into());
        }
        side = side * 3 / 4;
    }
}

/// shortcut: built without the `images` feature, only files that already fit are sent.
#[cfg(not(feature = "images"))]
pub fn normalize(bytes: &[u8]) -> Result<Image, String> {
    let mime = sniff(bytes).ok_or("this image format needs novacode built with the `images` feature")?;
    if b64_len(bytes.len()) > MAX_B64 {
        return Err("image too large (novacode was built without the `images` feature)".into());
    }
    Ok(Image {
        mime,
        data: b64(bytes),
        width: 0,
        height: 0,
        resized_from: None,
    })
}

/// `@path` mentions in a prompt that name image files: (path as written, file).
pub fn mentions(text: &str, cwd: &Path) -> Vec<(String, PathBuf)> {
    let home = novacode_base::platform::home();
    text.split_whitespace()
        .filter_map(|w| w.strip_prefix('@'))
        .map(|w| w.trim_end_matches([',', '.', ';', ':', ')', '!', '?']))
        .filter(|w| !w.is_empty())
        .filter_map(|w| {
            let p = match (w.strip_prefix("~/"), &home) {
                (Some(rest), Some(h)) => h.join(rest),
                _ => cwd.join(w),
            };
            let head = std::fs::File::open(&p).ok().and_then(|mut f| {
                use std::io::Read;
                let mut b = [0u8; 16];
                f.read(&mut b).ok().map(|n| b[..n].to_vec())
            })?;
            is_image(&head).then(|| (w.to_string(), p))
        })
        .collect()
}

/// The prompt as content: its text, then each mentioned image. Problems
/// come back as notes for the user; the text is always sent.
pub fn attach(text: &str, cwd: &Path) -> (Vec<Block>, Vec<String>) {
    let mut blocks = vec![Block::text(text)];
    let mut notes = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for (shown, path) in mentions(text, cwd) {
        if !seen.insert(path.clone()) {
            continue;
        }
        match std::fs::read(&path)
            .map_err(|e| e.to_string())
            .and_then(|b| normalize(&b))
        {
            Ok(img) => {
                notes.push(format!("attached image {shown} ({})", img.describe()));
                blocks.push(img.block());
            }
            Err(e) => notes.push(format!("image {shown} not attached: {e}")),
        }
    }
    (blocks, notes)
}

#[cfg(all(test, feature = "images"))]
mod tests {
    use super::*;
    use image::{DynamicImage, ImageFormat, RgbImage, RgbaImage};
    use std::io::Cursor;

    fn encode(img: DynamicImage, f: ImageFormat) -> Vec<u8> {
        let mut b = Vec::new();
        img.write_to(&mut Cursor::new(&mut b), f).unwrap();
        b
    }

    #[test]
    fn small_supported_image_is_kept_byte_for_byte() {
        let png = encode(DynamicImage::ImageRgb8(RgbImage::new(10, 5)), ImageFormat::Png);
        let i = normalize(&png).unwrap();
        assert_eq!((i.mime, i.width, i.height, i.resized_from), ("image/png", 10, 5, None));
        assert_eq!(i.data, b64(&png));
    }

    #[test]
    fn large_or_foreign_images_are_converted_and_shrunk() {
        let big = encode(DynamicImage::ImageRgba8(RgbaImage::new(4000, 1000)), ImageFormat::Png);
        let i = normalize(&big).unwrap();
        assert_eq!((i.width, i.height, i.resized_from), (2000, 500, Some((4000, 1000))));
        assert_eq!(i.mime, "image/png", "alpha keeps PNG");
        let bmp = encode(DynamicImage::ImageRgb8(RgbImage::new(8, 8)), ImageFormat::Bmp);
        assert!(is_image(&bmp) && sniff(&bmp).is_none());
        let i = normalize(&bmp).unwrap();
        assert!(i.mime == "image/png" || i.mime == "image/jpeg");
        assert!(normalize(b"not an image").is_err());
    }

    #[test]
    fn noisy_photo_gets_under_the_byte_limit() {
        // Random pixels barely compress: PNG is far over the limit, JPEG must win.
        let mut seed = 1u32;
        // 1300² noisy RGB ≈ 5MB as PNG, ≈ 6.8MB base64: over the limit.
        let img = RgbImage::from_fn(1300, 1300, |_, _| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            image::Rgb([(seed >> 24) as u8, (seed >> 16) as u8, (seed >> 8) as u8])
        });
        let i = normalize(&encode(DynamicImage::ImageRgb8(img), ImageFormat::Png)).unwrap();
        assert_eq!(i.mime, "image/jpeg");
        assert!(i.data.len() <= MAX_B64);
    }

    #[test]
    fn prompt_mentions_attach_images() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("shot.png"),
            encode(DynamicImage::ImageRgb8(RgbImage::new(3, 3)), ImageFormat::Png),
        )
        .unwrap();
        std::fs::write(dir.path().join("notes.md"), "text").unwrap();
        let (blocks, notes) = attach("look at @shot.png, and @notes.md and @shot.png", dir.path());
        assert_eq!(blocks.len(), 2, "text + one image (deduped, notes.md is text)");
        assert!(matches!(&blocks[1], Block::Image { mime_type, .. } if mime_type == "image/png"));
        assert_eq!(notes, vec!["attached image shot.png (3×3)"]);
    }
}
