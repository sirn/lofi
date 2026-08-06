//! Image attachment processing: decode, downscale, and re-encode an attached
//! image so the base64 payload that lands in the context window and the
//! durable transcript stays bounded. Mirrors Pi's `autoResizeImages` behavior
//! (on by default there; unconditional here): downscale to fit a bounding box,
//! then sweep JPEG quality down until the encoded payload fits a byte cap.

use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, GenericImageView};
use lofi_error::{Error, Result};
use lofi_types::ImageConfig;

/// JPEG media type produced by [`normalize`]. Re-encoding always targets JPEG
/// (Pi's resize candidate format) regardless of the source format, so the
/// output media type is constant.
pub const OUTPUT_MEDIA_TYPE: &str = "image/jpeg";

/// Quality sweep bounds. Pi iterates JPEG quality downward; we step from 90
/// down to 40, halving dimensions once if the lowest quality still overflows.
const QUALITY_START: u8 = 90;
const QUALITY_MIN: u8 = 40;
const QUALITY_STEP: u8 = 10;

/// Normalize an attached image: decode `bytes`, downscale to fit
/// `cfg.max_width`×`cfg.max_height` (preserving aspect ratio), and re-encode
/// as JPEG no larger than `cfg.max_bytes`. Returns the JPEG bytes and
/// [`OUTPUT_MEDIA_TYPE`].
///
/// # Errors
/// Returns [`Error::Tool`] when `bytes` is not a decodable image in an enabled
/// format, or when no quality/dimension combination fits `cfg.max_bytes`.
pub fn normalize(bytes: &[u8], cfg: &ImageConfig) -> Result<(Vec<u8>, String)> {
    let format = image::guess_format(bytes)
        .map_err(|e| Error::Tool(format!("unrecognized image format: {e}")))?;
    let img = image::load_from_memory_with_format(bytes, format)
        .map_err(|e| Error::Tool(format!("failed to decode {format:?} image: {e}")))?;

    let mut img = fit_within(&img, cfg.max_width, cfg.max_height);

    // Sweep JPEG quality down; if the floor still overflows, halve the frame
    // and sweep again. Pi follows the same quality-then-dimension fallback.
    for round in 0..2 {
        let mut quality = QUALITY_START;
        loop {
            let encoded = encode_jpeg(&img, quality)?;
            if encoded.len() <= cfg.max_bytes {
                return Ok((encoded, OUTPUT_MEDIA_TYPE.to_string()));
            }
            if quality <= QUALITY_MIN {
                break;
            }
            quality = quality.saturating_sub(QUALITY_STEP).max(QUALITY_MIN);
        }
        if round == 0 {
            img = halve(&img);
        }
    }

    Err(Error::Tool(format!(
        "image exceeds {} bytes even after downscaling and re-encoding",
        cfg.max_bytes
    )))
}

/// Downscale `img` to fit within `max_w`×`max_h` preserving aspect ratio.
/// Returns the original image unchanged when it already fits.
fn fit_within(img: &DynamicImage, max_w: u32, max_h: u32) -> DynamicImage {
    let (w, h) = img.dimensions();
    if w <= max_w && h <= max_h {
        return img.clone();
    }
    // `resize` preserves aspect ratio within the bounding box.
    img.resize(max_w, max_h, image::imageops::FilterType::Lanczos3)
}

fn halve(img: &DynamicImage) -> DynamicImage {
    let (w, h) = img.dimensions();
    let nw = (w / 2).max(1);
    let nh = (h / 2).max(1);
    img.resize_exact(nw, nh, image::imageops::FilterType::Lanczos3)
}

fn encode_jpeg(img: &DynamicImage, quality: u8) -> Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut out);
    // JPEG has no alpha channel; flatten onto RGB8 first.
    let rgb = img.to_rgb8();
    let mut encoder = JpegEncoder::new_with_quality(&mut cursor, quality);
    encoder
        .encode_image(&rgb)
        .map_err(|e| Error::Tool(format!("failed to encode image as JPEG: {e}")))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ExtendedColorType, ImageBuffer, ImageEncoder, Rgb};

    fn write_png(img: &DynamicImage) -> Vec<u8> {
        let rgb = img.to_rgb8();
        let (w, h) = rgb.dimensions();
        let mut out = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut out);
        image::codecs::png::PngEncoder::new(&mut cursor)
            .write_image(rgb.as_raw(), w, h, ExtendedColorType::Rgb8)
            .unwrap();
        out
    }

    fn solid_png(w: u32, h: u32) -> Vec<u8> {
        let buf: ImageBuffer<Rgb<u8>, Vec<u8>> =
            ImageBuffer::from_pixel(w, h, Rgb([200u8, 100, 50]));
        write_png(&DynamicImage::ImageRgb8(buf))
    }

    #[test]
    fn rejects_non_image_bytes() {
        let r = normalize(b"not an image", &ImageConfig::default());
        assert!(r.is_err());
    }

    #[test]
    fn small_image_passes_through_as_jpeg() {
        let png = solid_png(100, 80);
        let (bytes, media_type) = normalize(&png, &ImageConfig::default()).unwrap();
        assert_eq!(media_type, OUTPUT_MEDIA_TYPE);
        let img = image::load_from_memory(&bytes).unwrap();
        assert_eq!(img.dimensions(), (100, 80));
    }

    #[test]
    fn oversized_image_is_downscaled_to_fit() {
        let png = solid_png(4000, 3000);
        let cfg = ImageConfig::default();
        let (bytes, _) = normalize(&png, &cfg).unwrap();
        let img = image::load_from_memory(&bytes).unwrap();
        let (w, h) = img.dimensions();
        assert!(w <= cfg.max_width && h <= cfg.max_height, "{w}x{h}");
        // Aspect ratio preserved: 4000x3000 -> 2000x1500.
        assert_eq!((w, h), (2000, 1500));
    }

    #[test]
    fn respects_byte_cap_by_dropping_quality() {
        // A noisy (high-entropy) frame stays large at high quality, so the
        // quality sweep must engage to meet a tight byte cap.
        let mut buf: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::new(800, 800);
        for (x, y, px) in buf.enumerate_pixels_mut() {
            *px = Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8]);
        }
        let png = write_png(&DynamicImage::ImageRgb8(buf));
        let cfg = ImageConfig {
            max_width: 800,
            max_height: 800,
            max_bytes: 64 * 1024,
        };
        let (bytes, _) = normalize(&png, &cfg).unwrap();
        assert!(bytes.len() <= cfg.max_bytes, "{} > {}", bytes.len(), cfg.max_bytes);
    }
}
