use std::path::Path;
#[cfg(target_os = "linux")]
use std::sync::Arc;

use image::{DynamicImage, GenericImageView, RgbaImage};
use jpeg_decoder::{CodingProcess, Decoder as JpegDecoder, PixelFormat};
use lofty::{
    config::ParseOptions,
    error::LoftyError,
    file::{AudioFile, TaggedFileExt},
    picture::PictureType,
    probe::Probe,
    tag::{Accessor, ItemKey},
};

use super::{
    Track, TrackId, TrackOrigin,
    lyrics::{LyricLine, read_lyrics},
};

const MAX_COVER_DIMENSION: u32 = 320;
const MAX_COVER_SOURCE_DIMENSION: u16 = 8192;
const MAX_COVER_DECODE_BYTES: usize = 64 * 1024 * 1024;
const AMBIENT_COVER_DIMENSION: u32 = 64;
const AMBIENT_BLUR_SOURCE_DIMENSION: u32 = 32;
const AMBIENT_BOX_BLUR_RADIUS: usize = 3;
const AMBIENT_BOX_BLUR_PASSES: usize = 3;
const PALETTE_COVER_DIMENSION: u32 = 384;
const PALETTE_QUALITY: u8 = 5;
const MAX_RAW_COVER_BYTES: usize = 512 * 1024;

pub fn read_meta_info(path: &Path) -> Result<Track, LoftyError> {
    let tagged = Probe::open(path)?.options(ParseOptions::new().read_cover_art(false)).read()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
    let title = tag
        .and_then(Accessor::title)
        .map(|s| s.into_owned())
        .unwrap_or_else(|| path.file_stem().unwrap_or_default().to_string_lossy().into_owned());
    let mut track = Track::new(
        TrackId::local(path),
        TrackOrigin::Local(path.to_owned()),
        title,
        tag.and_then(Accessor::artist).map(|s| s.into_owned()).unwrap_or_else(|| "unknown".into()),
        tagged.properties().duration(),
    );
    track.album = tag.and_then(Accessor::album).map(|s| s.into_owned()).unwrap_or_default();
    Ok(track)
}

pub const MAX_PALETTE_COLORS: usize = 3;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CoverPalette {
    colors: [[u8; 3]; MAX_PALETTE_COLORS],
    len: u8,
}

impl CoverPalette {
    pub fn as_slice(&self) -> &[[u8; 3]] {
        &self.colors[..usize::from(self.len)]
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct CoverSource {
    pub bytes: Box<[u8]>,
    pub extension: &'static str,
    pub palette: Option<CoverPalette>,
}

#[derive(Clone)]
pub struct CoverImage {
    pub pixels: slint::SharedPixelBuffer<slint::Rgba8Pixel>,
}

impl From<RgbaImage> for CoverImage {
    fn from(image: RgbaImage) -> Self {
        Self {
            pixels: slint::SharedPixelBuffer::clone_from_slice(
                image.as_raw(),
                image.width(),
                image.height(),
            ),
        }
    }
}

impl CoverImage {
    #[cfg(test)]
    pub fn dimensions(&self) -> (u32, u32) {
        (self.pixels.width(), self.pixels.height())
    }
}

#[derive(Clone)]
pub struct AmbientCovers {
    pub dark: slint::SharedPixelBuffer<slint::Rgb8Pixel>,
    pub light: slint::SharedPixelBuffer<slint::Rgb8Pixel>,
}

impl AmbientCovers {
    #[cfg(test)]
    pub fn dimensions(&self) -> (u32, u32) {
        (self.dark.width(), self.dark.height())
    }
}

#[derive(Clone, Default)]
pub struct TrackDetails {
    pub cover: Option<CoverImage>,
    pub ambient: Option<AmbientCovers>,
    pub palette: Option<CoverPalette>,
    pub cover_source: Option<Arc<CoverSource>>,
    pub lyrics: Vec<LyricLine>,
}

impl TrackDetails {
    pub(crate) fn from_cover_bytes(bytes: &[u8]) -> Option<Self> {
        let decoded = decode_cover_full(bytes)?;
        let source = CoverSource::new(bytes, &decoded.cover, decoded.palette)?;
        let mut details = Self::from(decoded);
        details.cover_source = Some(Arc::new(source));
        Some(details)
    }
}

impl From<DecodedCover> for TrackDetails {
    fn from(decoded: DecodedCover) -> Self {
        Self {
            cover: Some(decoded.cover),
            ambient: Some(decoded.ambient),
            palette: decoded.palette,
            ..Self::default()
        }
    }
}

impl CoverSource {
    fn new(bytes: &[u8], cover: &CoverImage, palette: Option<CoverPalette>) -> Option<Self> {
        use image::{
            ExtendedColorType, ImageEncoder, ImageFormat,
            codecs::png::{CompressionType, FilterType, PngEncoder},
        };

        if bytes.len() <= MAX_RAW_COVER_BYTES {
            let extension = match image::guess_format(bytes).ok()? {
                ImageFormat::Png => ".png",
                ImageFormat::Jpeg => ".jpg",
                ImageFormat::Gif => ".gif",
                ImageFormat::WebP => ".webp",
                ImageFormat::Bmp => ".bmp",
                _ => ".bin",
            };
            return Some(Self {
                bytes: bytes.into(),
                extension,
                palette,
            });
        }

        let mut bytes = Vec::new();
        PngEncoder::new_with_quality(&mut bytes, CompressionType::Fast, FilterType::Sub)
            .write_image(
                cover.pixels.as_bytes(),
                cover.pixels.width(),
                cover.pixels.height(),
                ExtendedColorType::Rgba8,
            )
            .ok()?;
        Some(Self {
            bytes: bytes.into_boxed_slice(),
            extension: ".png",
            palette,
        })
    }

    pub(crate) fn render(&self) -> TrackDetails {
        decode_scaled_cover(&self.bytes)
            .map(|image| render_cover(image, self.palette).into())
            .unwrap_or_default()
    }
}

struct DecodedCover {
    pub cover: CoverImage,
    pub ambient: AmbientCovers,
    pub palette: Option<CoverPalette>,
}

// GdkPixbuf's nearest-neighbor scaler starts at half a step and uses 16.16 coordinates.
fn nearest_samples(source: u32, target: u32) -> impl Iterator<Item = u32> {
    let step = (u64::from(source) << 16) / u64::from(target);
    (0..target).map(move |index| ((u64::from(index) * step + step / 2) >> 16) as u32)
}

fn is_palette_sample(r: u8, g: u8, b: u8, a: u8) -> bool {
    a >= 125 && !(r > 250 && g > 250 && b > 250)
}

pub(crate) fn extract_palette(img: &DynamicImage) -> Option<CoverPalette> {
    let (source_width, source_height) = img.dimensions();
    if source_width == 0 || source_height == 0 {
        return None;
    }
    let ratio = source_width as f32 / source_height as f32;
    let size = PALETTE_COVER_DIMENSION;
    let (width, height) = if ratio > 1.0 {
        (size, (size as f32 / ratio) as u32)
    } else {
        ((size as f32 * ratio) as u32, size)
    };
    if width == 0 || height == 0 {
        return None;
    }

    let (format, channels) = if img.color().has_alpha() {
        (color_thief::ColorFormat::Rgba, 4)
    } else {
        (color_thief::ColorFormat::Rgb, 3)
    };
    // Match the scaled Pixbuf byte layout that Amberol passes to color-thief.
    let row_bytes = width as usize * channels;
    let stride = row_bytes.next_multiple_of(4);
    let x_samples: Vec<_> = nearest_samples(source_width, width).collect();
    let mut pixels = vec![0; stride * (height as usize - 1) + row_bytes];
    let mut has_color = false;
    for (y, row) in nearest_samples(source_height, height).zip(pixels.chunks_mut(stride)) {
        for (&x, sample) in x_samples.iter().zip(row[..row_bytes].chunks_exact_mut(channels)) {
            let pixel = img.get_pixel(x, y).0;
            has_color = has_color || is_palette_sample(pixel[0], pixel[1], pixel[2], pixel[3]);
            sample.copy_from_slice(&pixel[..channels]);
        }
    }

    // Padding must not create a palette for white artwork. The quantizer can also
    // return synthetic white when none of its sampled pixels pass the filter.
    let has_sample = has_color
        && pixels.chunks_exact(channels).step_by(channels * usize::from(PALETTE_QUALITY)).any(
            |pixel| {
                is_palette_sample(
                    pixel[0],
                    pixel[1],
                    pixel[2],
                    pixel.get(3).copied().unwrap_or(255),
                )
            },
        );
    if !has_sample {
        return None;
    }
    let palette = color_thief::get_palette(&pixels, format, PALETTE_QUALITY, 4).ok()?;
    let mut colors = [[0u8; 3]; MAX_PALETTE_COLORS];
    let mut len = 0usize;
    for c in palette.into_iter().take(MAX_PALETTE_COLORS) {
        colors[len] = [c.r, c.g, c.b];
        len += 1;
    }
    (len > 0).then_some(CoverPalette {
        colors,
        len: len as u8,
    })
}

fn decode_jpeg(bytes: &[u8]) -> Option<DynamicImage> {
    let mut decoder = JpegDecoder::new(bytes);
    decoder.read_info().ok()?;
    let info = decoder.info()?;
    let longest = info.width.max(info.height);
    if info.width == 0
        || info.height == 0
        || longest > MAX_COVER_SOURCE_DIMENSION
        || usize::from(info.width) * usize::from(info.height) * info.pixel_format.pixel_bytes()
            > MAX_COVER_DECODE_BYTES
    {
        return None;
    }
    // Progressive JPEGs still need full-size coefficient buffers, even with a scaled output.
    let (width, height) = if info.coding_process == CodingProcess::DctSequential
        && u32::from(longest) >= 2 * MAX_COVER_DIMENSION
    {
        decoder.scale(MAX_COVER_DIMENSION as u16, MAX_COVER_DIMENSION as u16).ok()?
    } else {
        (info.width, info.height)
    };
    decoder.set_max_decoding_buffer_size(MAX_COVER_DECODE_BYTES);
    let pixels = decoder.decode().ok()?;
    let (width, height) = (u32::from(width), u32::from(height));
    match info.pixel_format {
        PixelFormat::RGB24 => {
            image::RgbImage::from_raw(width, height, pixels).map(DynamicImage::ImageRgb8)
        }
        PixelFormat::L8 => {
            image::GrayImage::from_raw(width, height, pixels).map(DynamicImage::ImageLuma8)
        }
        PixelFormat::L16 => {
            let luma: Vec<u8> = pixels.as_chunks::<2>().0.iter().map(|pair| pair[0]).collect();
            image::GrayImage::from_raw(width, height, luma).map(DynamicImage::ImageLuma8)
        }
        PixelFormat::CMYK32 => {
            let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
            for &[c, m, y, k] in pixels.as_chunks::<4>().0 {
                let k = u16::from(k);
                rgb.push(((u16::from(c) * k) / 255) as u8);
                rgb.push(((u16::from(m) * k) / 255) as u8);
                rgb.push(((u16::from(y) * k) / 255) as u8);
            }
            image::RgbImage::from_raw(width, height, rgb).map(DynamicImage::ImageRgb8)
        }
    }
}

fn decode_scaled_cover(bytes: &[u8]) -> Option<DynamicImage> {
    let img = if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        decode_jpeg(bytes)?
    } else {
        let mut reader =
            image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
        let mut limits = image::Limits::default();
        limits.max_alloc = Some(MAX_COVER_DECODE_BYTES as u64);
        limits.max_image_width = Some(u32::from(MAX_COVER_SOURCE_DIMENSION));
        limits.max_image_height = limits.max_image_width;
        reader.limits(limits);
        reader.decode().ok()?
    };
    Some(scale_cover(img))
}

fn decode_cover_full(bytes: &[u8]) -> Option<DecodedCover> {
    let img = decode_scaled_cover(bytes)?;
    let palette = extract_palette(&img);
    Some(render_cover(img, palette))
}

fn render_cover(img: DynamicImage, palette: Option<CoverPalette>) -> DecodedCover {
    let cover_rgba = img.into_rgba8();
    let ambient = make_ambient_cover(&cover_rgba);
    let cover = cover_rgba.into();
    crate::trim_allocator();
    DecodedCover {
        cover,
        ambient,
        palette,
    }
}

#[cfg(test)]
pub(crate) fn decode_cover(bytes: &[u8]) -> Option<(CoverImage, Option<CoverPalette>)> {
    decode_cover_full(bytes).map(|decoded| (decoded.cover, decoded.palette))
}

fn box_blur_h(src: &[[u32; 4]], dst: &mut [[u32; 4]], size: usize, radius: usize) {
    let diameter = (2 * radius + 1) as u32;
    let half = diameter / 2;
    for y in 0..size {
        let row = &src[y * size..(y + 1) * size];
        let out = &mut dst[y * size..(y + 1) * size];
        let mut acc = [0u32; 4];
        for k in 0..=radius {
            for c in 0..4 {
                acc[c] += row[0][c];
            }
            if k > 0 {
                let rk = k.min(size - 1);
                for c in 0..4 {
                    acc[c] += row[rk][c];
                }
            }
        }
        for (x, pixel) in out.iter_mut().enumerate() {
            for (dst_c, &acc_c) in pixel.iter_mut().zip(&acc) {
                *dst_c = (acc_c + half) / diameter;
            }
            let remove_idx = x.saturating_sub(radius);
            let add_idx = (x + radius + 1).min(size - 1);
            for c in 0..4 {
                acc[c] = acc[c] + row[add_idx][c] - row[remove_idx][c];
            }
        }
    }
}

fn box_blur_v(src: &[[u32; 4]], dst: &mut [[u32; 4]], size: usize, radius: usize) {
    let diameter = (2 * radius + 1) as u32;
    let half = diameter / 2;
    for x in 0..size {
        let mut acc = [0u32; 4];
        for k in 0..=radius {
            for c in 0..4 {
                acc[c] += src[x][c];
            }
            if k > 0 {
                let rk = k.min(size - 1);
                for c in 0..4 {
                    acc[c] += src[rk * size + x][c];
                }
            }
        }
        for y in 0..size {
            for c in 0..4 {
                dst[y * size + x][c] = (acc[c] + half) / diameter;
            }
            let remove_idx = y.saturating_sub(radius);
            let add_idx = (y + radius + 1).min(size - 1);
            for c in 0..4 {
                acc[c] = acc[c] + src[add_idx * size + x][c] - src[remove_idx * size + x][c];
            }
        }
    }
}

fn lerp_stop(a: [u8; 4], b: [u8; 4], t: f32) -> [f32; 4] {
    [
        f32::from(a[0]) + (f32::from(b[0]) - f32::from(a[0])) * t,
        f32::from(a[1]) + (f32::from(b[1]) - f32::from(a[1])) * t,
        f32::from(a[2]) + (f32::from(b[2]) - f32::from(a[2])) * t,
        (f32::from(a[3]) + (f32::from(b[3]) - f32::from(a[3])) * t) / 255.0,
    ]
}

fn sample_scrim(stops: [[u8; 4]; 3], t: f32) -> [f32; 4] {
    if t <= 0.55 {
        lerp_stop(stops[0], stops[1], (t / 0.55).clamp(0.0, 1.0))
    } else {
        lerp_stop(stops[1], stops[2], ((t - 0.55) / 0.45).clamp(0.0, 1.0))
    }
}

fn composite_ambient_pixel(rgba: [f32; 4], base: [u8; 3], scrim: [f32; 4]) -> slint::Rgb8Pixel {
    let cover_a = (rgba[3] / 255.0).clamp(0.0, 1.0);
    let scrim_a = scrim[3];
    let blend = |ch: f32, b: u8, s: f32| -> u8 {
        let over_base = ch * cover_a + f32::from(b) * (1.0 - cover_a);
        (over_base * (1.0 - scrim_a) + s * scrim_a).round().clamp(0.0, 255.0) as u8
    };
    slint::Rgb8Pixel::new(
        blend(rgba[0], base[0], scrim[0]),
        blend(rgba[1], base[1], scrim[1]),
        blend(rgba[2], base[2], scrim[2]),
    )
}

fn make_ambient_cover(img: &RgbaImage) -> AmbientCovers {
    const DARK_BASE: [u8; 3] = [0x18, 0x1c, 0x22];
    const LIGHT_BASE: [u8; 3] = [0xee, 0xf1, 0xf7];
    const DARK_SCRIM: [[u8; 4]; 3] =
        [[0x16, 0x1a, 0x20, 0x94], [0x10, 0x13, 0x19, 0xb0], [0x0c, 0x0e, 0x13, 0xc4]];
    const LIGHT_SCRIM: [[u8; 4]; 3] =
        [[0xf4, 0xf6, 0xfa, 0xb8], [0xee, 0xf1, 0xf7, 0xcc], [0xe8, 0xec, 0xf4, 0xd8]];

    let src_dim = AMBIENT_BLUR_SOURCE_DIMENSION as usize;
    let tiny = image::imageops::resize(
        img,
        AMBIENT_BLUR_SOURCE_DIMENSION,
        AMBIENT_BLUR_SOURCE_DIMENSION,
        image::imageops::FilterType::Triangle,
    );
    let mut buf: Vec<[u32; 4]> = tiny
        .pixels()
        .map(|p| [u32::from(p[0]), u32::from(p[1]), u32::from(p[2]), u32::from(p[3])])
        .collect();
    let mut scratch = vec![[0u32; 4]; src_dim * src_dim];
    for _ in 0..AMBIENT_BOX_BLUR_PASSES {
        box_blur_h(&buf, &mut scratch, src_dim, AMBIENT_BOX_BLUR_RADIUS);
        box_blur_v(&scratch, &mut buf, src_dim, AMBIENT_BOX_BLUR_RADIUS);
    }

    let out_dim = AMBIENT_COVER_DIMENSION;
    let mut dark = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(out_dim, out_dim);
    let mut light = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(out_dim, out_dim);
    let dark_slice = dark.make_mut_slice();
    let light_slice = light.make_mut_slice();

    // 155deg CSS/Slint linear-gradient direction vector: (sin 155°, -cos 155°).
    let angle = 155.0_f32.to_radians();
    let (dx, dy) = (angle.sin(), -angle.cos());
    let span = dx.abs() + dy.abs();
    let max_src = (src_dim - 1) as f32;

    for y in 0..out_dim as usize {
        let v = (y as f32 + 0.5) / out_dim as f32;
        let sy = (v * src_dim as f32 - 0.5).clamp(0.0, max_src);
        let y0 = sy.floor() as usize;
        let y1 = (y0 + 1).min(src_dim - 1);
        let wy = sy - y0 as f32;

        for x in 0..out_dim as usize {
            let u = (x as f32 + 0.5) / out_dim as f32;
            let sx = (u * src_dim as f32 - 0.5).clamp(0.0, max_src);
            let x0 = sx.floor() as usize;
            let x1 = (x0 + 1).min(src_dim - 1);
            let wx = sx - x0 as f32;

            let p00 = buf[y0 * src_dim + x0];
            let p10 = buf[y0 * src_dim + x1];
            let p01 = buf[y1 * src_dim + x0];
            let p11 = buf[y1 * src_dim + x1];
            let mut rgba = [0.0f32; 4];
            for c in 0..4 {
                let top = p00[c] as f32 * (1.0 - wx) + p10[c] as f32 * wx;
                let bot = p01[c] as f32 * (1.0 - wx) + p11[c] as f32 * wx;
                rgba[c] = top * (1.0 - wy) + bot * wy;
            }

            let grad_t = (((u - 0.5) * dx + (v - 0.5) * dy) / span + 0.5).clamp(0.0, 1.0);
            let idx = y * out_dim as usize + x;
            dark_slice[idx] =
                composite_ambient_pixel(rgba, DARK_BASE, sample_scrim(DARK_SCRIM, grad_t));
            light_slice[idx] =
                composite_ambient_pixel(rgba, LIGHT_BASE, sample_scrim(LIGHT_SCRIM, grad_t));
        }
    }

    AmbientCovers {
        dark,
        light,
    }
}

fn scale_cover(img: DynamicImage) -> DynamicImage {
    let longest = img.width().max(img.height());
    if longest >= 2 * MAX_COVER_DIMENSION {
        // Large reductions can average directly into the destination, without a
        // full-width floating-point intermediate. Keep Triangle for small reductions.
        img.thumbnail(MAX_COVER_DIMENSION, MAX_COVER_DIMENSION)
    } else if longest > MAX_COVER_DIMENSION {
        img.resize(MAX_COVER_DIMENSION, MAX_COVER_DIMENSION, image::imageops::FilterType::Triangle)
    } else {
        img
    }
}

fn extract_details(tagged: Option<&lofty::file::TaggedFile>, path: Option<&Path>) -> TrackDetails {
    let tag = tagged.and_then(|file| file.primary_tag().or_else(|| file.first_tag()));
    let picture = tag.and_then(|tag| {
        tag.get_picture_type(PictureType::CoverFront).or_else(|| tag.pictures().first())
    });
    let mut details =
        picture.and_then(|pic| TrackDetails::from_cover_bytes(pic.data())).unwrap_or_default();
    let embedded = tag.and_then(|tag| {
        tag.get_string(ItemKey::UnsyncLyrics).or_else(|| tag.get_string(ItemKey::Lyrics))
    });
    details.lyrics = match path {
        Some(path) => read_lyrics(path, embedded),
        None => super::lyrics::parse_lyrics(embedded.unwrap_or_default()),
    };
    details
}

pub fn read_details(path: &Path) -> TrackDetails {
    let tagged = Probe::open(path)
        .ok()
        .and_then(|probe| probe.options(ParseOptions::new().read_properties(false)).read().ok());
    extract_details(tagged.as_ref(), Some(path))
}

#[cfg(target_os = "linux")]
pub fn read_details_from_reader<R: std::io::Read + std::io::Seek>(reader: R) -> TrackDetails {
    let tagged = Probe::new(reader)
        .guess_file_type()
        .ok()
        .and_then(|probe| probe.options(ParseOptions::new().read_properties(false)).read().ok());
    extract_details(tagged.as_ref(), None)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    #[test]
    fn reads_audio_properties_tags_lyrics_and_cover() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tagged.flac");
        let track = read_meta_info(&path).unwrap();
        assert_eq!(
            (track.title.as_str(), track.artist.as_str(), track.duration.as_secs()),
            ("Fixture title", "Fixture artist", 2)
        );
        let details = read_details(&path);
        assert_eq!(details.cover.unwrap().dimensions(), (2, 3));
        assert_eq!(details.lyrics[0].text, "Embedded line");
    }

    #[test]
    fn decode_cover_downscales_oversized_images() {
        let img = image::RgbaImage::new(1024, 512);
        let mut encoded = Vec::new();
        img.write_to(&mut Cursor::new(&mut encoded), image::ImageFormat::Png).unwrap();
        let (decoded, _) = decode_cover(&encoded).unwrap();
        assert_eq!(decoded.dimensions(), (MAX_COVER_DIMENSION, MAX_COVER_DIMENSION / 2));

        // Craft a minimal PNG IHDR header declaring 9000x9000 (> 8192 limit) so decode_cover
        // rejects it via image::Limits before allocating pixel buffers.
        let mut oversized_png = vec![137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13];
        oversized_png.extend_from_slice(b"IHDR");
        oversized_png.extend_from_slice(&9000u32.to_be_bytes());
        oversized_png.extend_from_slice(&9000u32.to_be_bytes());
        oversized_png.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
        assert!(decode_cover(&oversized_png).is_none());
    }

    #[test]
    fn large_sequential_jpeg_decodes_directly_to_reduced_pixels() {
        let image = image::RgbImage::from_pixel(2048, 1024, image::Rgb([59, 104, 142]));
        let mut jpeg = Cursor::new(Vec::new());
        image.write_to(&mut jpeg, image::ImageFormat::Jpeg).unwrap();

        let thumb = decode_jpeg(jpeg.get_ref()).unwrap();
        assert_eq!(thumb.dimensions(), (512, 256));
        let details = decode_cover_full(jpeg.get_ref()).unwrap();
        assert_eq!(details.cover.dimensions(), (MAX_COVER_DIMENSION, MAX_COVER_DIMENSION / 2));
        assert_eq!(details.ambient.dimensions(), (64, 64));
        assert!(details.palette.is_some());

        let small = image::RgbImage::from_pixel(120, 80, image::Rgb([12, 90, 180]));
        let mut small_jpeg = Cursor::new(Vec::new());
        small.write_to(&mut small_jpeg, image::ImageFormat::Jpeg).unwrap();
        let small_details = decode_cover_full(small_jpeg.get_ref()).unwrap();
        assert_eq!(small_details.cover.dimensions(), (120, 80));
    }

    #[test]
    fn extract_palette_handles_empty_white_and_transparent_covers() {
        let empty = RgbaImage::new(0, 0);
        assert!(extract_palette(&empty.into()).is_none());

        let transparent = RgbaImage::from_pixel(16, 16, image::Rgba([40, 80, 210, 0]));
        assert!(extract_palette(&transparent.into()).is_none());

        let white = image::RgbImage::from_pixel(9, 15, image::Rgb([255, 255, 255]));
        assert!(extract_palette(&white.into()).is_none());
    }

    #[test]
    fn decode_cover_matches_amberol_palettes_for_rgb_and_rgba() {
        let rgb = image::RgbImage::from_fn(9, 15, |x, y| {
            image::Rgb([(x * 22) as u8, (y * 13) as u8, ((x * 27 + y * 11) % 256) as u8])
        });
        let rgb = DynamicImage::from(rgb);
        let rgba = DynamicImage::from(rgb.to_rgba8());
        // Reference outputs from GdkPixbuf nearest-neighbor scaling and color-thief 0.2.2.
        for (image, expected) in [
            (rgb, [[0x56, 0x50, 0x4e], [0x57, 0xcc, 0x72], [0xda, 0x73, 0x5c]]),
            (rgba, [[0x33, 0x58, 0x89], [0x5d, 0x5a, 0x1f], [0x79, 0x61, 0xe6]]),
        ] {
            let mut encoded = Cursor::new(Vec::new());
            image.write_to(&mut encoded, image::ImageFormat::Png).unwrap();
            let (cover, palette) = decode_cover(encoded.get_ref()).unwrap();
            assert_eq!(cover.dimensions(), (9, 15));
            assert_eq!(palette.unwrap().as_slice(), &expected);
        }
    }
}
