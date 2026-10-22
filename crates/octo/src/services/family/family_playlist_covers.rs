//! A family playlist's own picture: what an app uploads with `setFamilyPlaylistCover`, kept as
//! `<config>/covers/<playlist id>.jpg`. The covers folder is where Octo's painted covers look
//! for a picture first, so the playlist's cover is that picture from then on, and the painted
//! one again once it is removed. A family playlist's cover is looked up by its id, never its
//! name, so no other list's picture is taken for it and its picture for no other list.
//!
//! An upload is read whatever its format (JPEG, PNG, WebP, GIF or BMP), turned the way its
//! Exif orientation says (a phone's photo is often stored sideways with a tag), cropped to its
//! center square, made no larger than [`KEPT_SIDE`], and kept as a JPEG: the stored file is
//! always one Octo wrote, never the bytes it was sent.

use std::io::Cursor;
use std::path::Path;

use image::imageops::FilterType;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageReader, Limits};

/// The largest upload read: a phone photo, with room to spare.
pub const MAX_UPLOAD_BYTES: usize = 20 * 1024 * 1024;
/// The side of the square kept. Covers are asked for at up to about 1200 pixels.
pub const KEPT_SIDE: u32 = 1500;
/// The smallest square side taken: anything less would show as a blur.
pub const SMALLEST_SIDE: u32 = 100;
/// The widest or tallest picture read. Past this, a small file can still ask for gigabytes.
const LARGEST_READ: u32 = 16_384;
/// The most memory a picture may take decoded: a 48-megapixel phone photo, with little to
/// spare. A small file that unpacks to more (a PNG of one color, say) is refused.
const LARGEST_DECODED: u64 = 200 * 1024 * 1024;
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
    /// More pixels than Octo reads ([`LARGEST_DECODED`] bytes decoded, or a side past
    /// [`LARGEST_READ`]).
    TooManyPixels,
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
            CoverRefused::TooManyPixels => {
                "That picture has too many pixels; a photo of up to about 48 megapixels works.".to_string()
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

/// The picture as it is kept: the center square of `picture`, the way up its Exif orientation
/// says, at most [`KEPT_SIDE`] on a side, as a JPEG.
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
    limits.max_alloc = Some(LARGEST_DECODED);
    reader.limits(limits.clone());
    let mut decoder = reader.into_decoder().map_err(|_| CoverRefused::Unreadable)?;
    // What `ImageReader::decode` does before decoding, which `into_decoder` leaves to the
    // caller: the decoded picture's size is charged against the allowance first.
    limits
        .reserve(decoder.total_bytes())
        .map_err(|_| CoverRefused::TooManyPixels)?;
    decoder
        .set_limits(limits)
        .map_err(|_| CoverRefused::TooManyPixels)?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let image = DynamicImage::from_decoder(decoder).map_err(|error| match error {
        image::ImageError::Limits(_) => CoverRefused::TooManyPixels,
        _ => CoverRefused::Unreadable,
    })?;
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
    drop(image);
    let mut kept = if side > KEPT_SIDE {
        square.resize_exact(KEPT_SIDE, KEPT_SIDE, FilterType::CatmullRom)
    } else {
        square
    };
    // Turned last, once it is small: the center square is the same region whichever way the
    // picture is turned or flipped, so the order does not change what is kept.
    kept.apply_orientation(orientation);
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

/// Writes `bytes` to `path` whole: to a file of its own beside it first, then renamed over it,
/// so a cover being read is never half written, and two uploads at once never share a file
/// (the later rename wins).
pub fn write_whole(path: &Path, bytes: &[u8]) -> Result<(), CoverRefused> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static WRITES: AtomicU64 = AtomicU64::new(0);
    let folder = path
        .parent()
        .ok_or_else(|| CoverRefused::NotWritten("no folder".to_string()))?;
    std::fs::create_dir_all(folder).map_err(|e| CoverRefused::NotWritten(e.to_string()))?;
    let partial = path.with_extension(format!(
        "{}-{}.partial",
        std::process::id(),
        WRITES.fetch_add(1, Ordering::Relaxed)
    ));
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

    /// A JPEG with an Exif orientation tag, as a phone writes a photo taken sideways: the
    /// pixels as the sensor saw them, and a tag saying which way up they go.
    fn jpeg_turned(width: u32, height: u32, exif_orientation: u8) -> Vec<u8> {
        let mut image = image::RgbImage::from_pixel(width, height, image::Rgb([0, 0, 255]));
        for y in 0..height / 4 {
            for x in 0..width {
                image.put_pixel(x, y, image::Rgb([255, 0, 0]));
            }
        }
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 95)
            .encode_image(&image)
            .expect("encodes");
        // APP1: "Exif\0\0", a little-endian TIFF header, one IFD with one entry (Orientation,
        // SHORT, 1 value), and no next IFD.
        let mut exif = b"Exif\0\0".to_vec();
        exif.extend(b"II*\0");
        exif.extend(8u32.to_le_bytes());
        exif.extend(1u16.to_le_bytes());
        exif.extend(0x0112u16.to_le_bytes());
        exif.extend(3u16.to_le_bytes());
        exif.extend(1u32.to_le_bytes());
        exif.extend(u16::from(exif_orientation).to_le_bytes());
        exif.extend([0u8; 2]);
        exif.extend(0u32.to_le_bytes());
        let length = u16::try_from(exif.len() + 2).expect("short");
        let mut with_tag = jpeg[..2].to_vec();
        with_tag.extend([0xFF, 0xE1]);
        with_tag.extend(length.to_be_bytes());
        with_tag.extend(exif);
        with_tag.extend(&jpeg[2..]);
        with_tag
    }

    #[test]
    fn a_photo_taken_sideways_is_turned_the_way_its_tag_says() {
        // Red along the top as stored; the tag says rotate 90° clockwise, so the red goes to
        // the right side of what is kept.
        let kept = read(&prepared(&jpeg_turned(300, 200, 6)).expect("kept")).to_rgb8();
        assert_eq!((kept.width(), kept.height()), (200, 200));
        let red = |x: u32, y: u32| {
            let [r, _, b] = kept.get_pixel(x, y).0;
            r > 200 && b < 60
        };
        assert!(red(190, 100), "the top went right");
        assert!(!red(10, 100), "the left is not red");
        assert!(!red(100, 190), "the bottom is not red");
        // Untagged, the red stays along the top.
        let plain = read(&prepared(&jpeg_turned(300, 200, 1)).expect("kept")).to_rgb8();
        let [r, _, _] = plain.get_pixel(100, 10).0;
        assert!(r > 200);
    }

    /// A PNG of `width` × `height` black pixels, written by hand so the test never holds the
    /// decoded picture itself: rows of zeros deflate to almost nothing.
    fn black_png(width: u32, height: u32) -> Vec<u8> {
        use std::io::Write;
        fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
            out.extend((data.len() as u32).to_be_bytes());
            out.extend(kind);
            out.extend(data);
            let mut crc = flate2::Crc::new();
            crc.update(kind);
            crc.update(data);
            out.extend(crc.sum().to_be_bytes());
        }
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut header = Vec::new();
        header.extend(width.to_be_bytes());
        header.extend(height.to_be_bytes());
        header.extend([8, 2, 0, 0, 0]);
        chunk(&mut out, b"IHDR", &header);
        let mut deflate = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
        let row = vec![0u8; 1 + width as usize * 3];
        for _ in 0..height {
            deflate.write_all(&row).expect("deflates");
        }
        chunk(&mut out, b"IDAT", &deflate.finish().expect("deflates"));
        chunk(&mut out, b"IEND", &[]);
        out
    }

    #[test]
    fn a_small_file_that_unpacks_to_too_much_is_refused() {
        // 12,000 × 12,000 of one color is a few hundred KB as a PNG and 432 MB decoded.
        let bomb = black_png(12_000, 12_000);
        assert!(bomb.len() < 2 * 1024 * 1024, "{} bytes", bomb.len());
        assert_eq!(prepared(&bomb), Err(CoverRefused::TooManyPixels));
        // The same picture at a size a phone takes reads fine.
        assert!(prepared(&black_png(800, 600)).is_ok());
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
