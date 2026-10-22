//! A family playlist's own picture: what an app uploads with `setFamilyPlaylistCover`, kept as
//! `<config>/covers/<playlist id>.jpg`. The covers folder is where Octo's painted covers look
//! for a picture first, so the playlist's cover is that picture from then on, and the painted
//! one again once it is removed. A family playlist's cover is looked up by its id, never its
//! name, so no other list's picture is taken for it and its picture for no other list.
//!
//! An upload is read whatever its format (JPEG, PNG, WebP, GIF or BMP), cropped to its center
//! square, made no larger than [`KEPT_SIDE`], and kept as a JPEG: the stored file is always one
//! Octo wrote, never the bytes it was sent.

use std::io::Cursor;
use std::path::Path;

use image::imageops::FilterType;
use image::{DynamicImage, ImageReader, Limits};

/// The largest upload read: a phone photo, with room to spare.
pub const MAX_UPLOAD_BYTES: usize = 20 * 1024 * 1024;
/// The side of the square kept. Covers are asked for at up to about 1200 pixels.
pub const KEPT_SIDE: u32 = 1500;
/// The smallest square side taken: anything less would show as a blur.
pub const SMALLEST_SIDE: u32 = 100;
/// The widest or tallest picture read. Past this, a small file can still ask for gigabytes.
const LARGEST_READ: u32 = 16_384;
const JPEG_QUALITY: u8 = 90;

/// Why a picture was not kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoverRefused {
    /// No such playlist.
    NotFound,
    /// Octo keeps no files (no config folder).
    NowhereToKeep,
    /// Nothing was sent.
    Empty,
    /// Larger than [`MAX_UPLOAD_BYTES`].
    TooLarge,
    /// Not a picture Octo can read.
    Unreadable,
    /// Its shorter side is under [`SMALLEST_SIDE`].
    TooSmall,
    /// The file could not be written.
    NotWritten(String),
}

impl CoverRefused {
    /// The words an app shows.
    pub fn message(&self) -> String {
        match self {
            CoverRefused::NotFound => "Playlist not found.".to_string(),
            CoverRefused::NowhereToKeep => {
                "This server keeps no files, so it cannot keep a cover.".to_string()
            }
            CoverRefused::Empty => "Choose a picture first.".to_string(),
            CoverRefused::TooLarge => format!(
                "That picture is larger than {} MB.",
                MAX_UPLOAD_BYTES / (1024 * 1024)
            ),
            CoverRefused::Unreadable => {
                "That is not a picture Octo can read (JPEG, PNG, WebP, GIF or BMP).".to_string()
            }
            CoverRefused::TooSmall => {
                format!(
                    "That picture is too small; it needs to be at least {SMALLEST_SIDE} pixels on each side."
                )
            }
            CoverRefused::NotWritten(why) => format!("The cover could not be saved: {why}"),
        }
    }
}

/// The picture as it is kept: the center square of `picture`, at most [`KEPT_SIDE`] on a side,
/// as a JPEG.
pub fn prepared(picture: &[u8]) -> Result<Vec<u8>, CoverRefused> {
    if picture.is_empty() {
        return Err(CoverRefused::Empty);
    }
    if picture.len() > MAX_UPLOAD_BYTES {
        return Err(CoverRefused::TooLarge);
    }
    let mut reader = ImageReader::new(Cursor::new(picture))
        .with_guessed_format()
        .map_err(|_| CoverRefused::Unreadable)?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(LARGEST_READ);
    limits.max_image_height = Some(LARGEST_READ);
    limits.max_alloc = Some(512 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode().map_err(|_| CoverRefused::Unreadable)?;
    let side = image.width().min(image.height());
    if side < SMALLEST_SIDE {
        return Err(CoverRefused::TooSmall);
    }
    let square = image.crop_imm(
        (image.width() - side) / 2,
        (image.height() - side) / 2,
        side,
        side,
    );
    let kept = if side > KEPT_SIDE {
        square.resize_exact(KEPT_SIDE, KEPT_SIDE, FilterType::CatmullRom)
    } else {
        square
    };
    // JPEG has no transparency: a see-through picture goes onto white, as it shows in a browser.
    let rgb = flattened(&kept);
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY)
        .encode_image(&rgb)
        .map_err(|e| CoverRefused::NotWritten(e.to_string()))?;
    Ok(out)
}

fn flattened(image: &DynamicImage) -> image::RgbImage {
    if !image.color().has_alpha() {
        return image.to_rgb8();
    }
    let rgba = image.to_rgba8();
    image::RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let [r, g, b, a] = rgba.get_pixel(x, y).0;
        let over = |c: u8| -> u8 {
            let (c, a) = (u32::from(c), u32::from(a));
            u8::try_from((c * a + 255 * (255 - a) + 127) / 255).unwrap_or(255)
        };
        image::Rgb([over(r), over(g), over(b)])
    })
}

/// Writes `bytes` to `path` whole: to a file beside it first, then renamed over it, so a cover
/// being read is never half written.
pub fn write_whole(path: &Path, bytes: &[u8]) -> Result<(), CoverRefused> {
    let folder = path
        .parent()
        .ok_or_else(|| CoverRefused::NotWritten("no folder".to_string()))?;
    std::fs::create_dir_all(folder).map_err(|e| CoverRefused::NotWritten(e.to_string()))?;
    let partial = path.with_extension("jpg.partial");
    std::fs::write(&partial, bytes).map_err(|e| CoverRefused::NotWritten(e.to_string()))?;
    std::fs::rename(&partial, path).map_err(|e| {
        let _ = std::fs::remove_file(&partial);
        CoverRefused::NotWritten(e.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgba, RgbaImage};

    fn png(width: u32, height: u32, pixel: [u8; 4]) -> Vec<u8> {
        let image = RgbaImage::from_pixel(width, height, Rgba(pixel));
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image)
            .write_to(&mut out, ImageFormat::Png)
            .expect("encodes");
        out.into_inner()
    }

    fn read(jpeg: &[u8]) -> DynamicImage {
        image::load_from_memory_with_format(jpeg, ImageFormat::Jpeg).expect("a JPEG")
    }

    #[test]
    fn a_picture_is_kept_as_its_center_square_no_larger_than_the_kept_side() {
        let kept = read(&prepared(&png(2400, 1800, [200, 10, 10, 255])).expect("kept"));
        assert_eq!((kept.width(), kept.height()), (KEPT_SIDE, KEPT_SIDE));
        let small = read(&prepared(&png(300, 500, [10, 200, 10, 255])).expect("kept"));
        assert_eq!((small.width(), small.height()), (300, 300));
    }

    #[test]
    fn a_see_through_picture_goes_onto_white() {
        let kept = read(&prepared(&png(120, 120, [0, 0, 0, 0])).expect("kept")).to_rgb8();
        let [r, g, b] = kept.get_pixel(60, 60).0;
        assert!(r > 245 && g > 245 && b > 245, "{r} {g} {b}");
    }

    #[test]
    fn what_is_not_a_usable_picture_is_refused() {
        assert_eq!(prepared(b""), Err(CoverRefused::Empty));
        assert_eq!(prepared(b"not a picture"), Err(CoverRefused::Unreadable));
        assert_eq!(
            prepared(&png(99, 400, [1, 2, 3, 255])),
            Err(CoverRefused::TooSmall)
        );
        assert_eq!(
            prepared(&vec![0u8; MAX_UPLOAD_BYTES + 1]),
            Err(CoverRefused::TooLarge)
        );
    }
}
