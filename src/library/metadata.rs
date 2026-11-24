use std::path::Path;

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

const MAX_COVER_DIMENSION: u32 = 512;
#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
#[derive(Clone)]
pub struct RawCover {
    pub bytes: Vec<u8>,
    pub extension: &'static str,
}

#[derive(Clone, Default)]
pub struct TrackDetails {
    pub cover: Option<image::RgbaImage>,
    #[cfg(target_os = "linux")]
    pub raw_cover: Option<RawCover>,
    pub lyrics: Vec<LyricLine>,
}

pub(crate) fn decode_cover(bytes: &[u8]) -> Option<image::RgbaImage> {
    let mut reader =
        image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(64 * 1024 * 1024);
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    reader.limits(limits);
    let img = reader.decode().ok()?;
    Some(resize_cover(img))
}

fn resize_cover(img: image::DynamicImage) -> image::RgbaImage {
    let resized = if img.width() > MAX_COVER_DIMENSION || img.height() > MAX_COVER_DIMENSION {
        img.resize(MAX_COVER_DIMENSION, MAX_COVER_DIMENSION, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    resized.into_rgba8()
}

#[cfg(target_os = "linux")]
fn raw_cover_extension(mime: Option<&lofty::picture::MimeType>) -> Option<&'static str> {
    use lofty::picture::MimeType;
    match mime? {
        MimeType::Png => Some(".png"),
        MimeType::Jpeg => Some(".jpg"),
        MimeType::Gif => Some(".gif"),
        MimeType::Unknown(s) if s.eq_ignore_ascii_case("image/webp") => Some(".webp"),
        _ => None,
    }
}

fn extract_details(tagged: Option<&lofty::file::TaggedFile>, path: Option<&Path>) -> TrackDetails {
    let tag = tagged.and_then(|file| file.primary_tag().or_else(|| file.first_tag()));
    let picture = tag.and_then(|tag| {
        tag.get_picture_type(PictureType::CoverFront).or_else(|| tag.pictures().first())
    });
    let cover = picture.and_then(|pic| decode_cover(pic.data()));
    #[cfg(target_os = "linux")]
    let raw_cover = picture.and_then(|pic| {
        if cover.is_none() || pic.data().len() > MAX_RAW_COVER_BYTES {
            return None;
        }
        let extension = raw_cover_extension(pic.mime_type())?;
        Some(RawCover {
            bytes: pic.data().to_vec(),
            extension,
        })
    });
    let embedded = tag.and_then(|tag| {
        tag.get_string(ItemKey::UnsyncLyrics).or_else(|| tag.get_string(ItemKey::Lyrics))
    });
    let lyrics = match path {
        Some(path) => read_lyrics(path, embedded),
        None => super::lyrics::parse_lyrics(embedded.unwrap_or_default()),
    };
    TrackDetails {
        cover,
        #[cfg(target_os = "linux")]
        raw_cover,
        lyrics,
    }
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
        let decoded = decode_cover(&encoded).unwrap();
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
}
