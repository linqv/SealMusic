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

pub fn read_meta_info(path: &Path) -> Result<Track, LoftyError> {
    let tagged = Probe::open(path)?.options(ParseOptions::new().read_cover_art(false)).read()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
    let title = tag
        .and_then(Accessor::title)
        .map(|s| s.into_owned())
        .unwrap_or_else(|| path.file_stem().unwrap_or_default().to_string_lossy().into_owned());
    Ok(Track {
        id: TrackId::local(path),
        origin: TrackOrigin::Local(path.to_owned()),
        title,
        artist: tag
            .and_then(Accessor::artist)
            .map(|s| s.into_owned())
            .unwrap_or_else(|| "unknown".into()),
        duration: tagged.properties().duration(),
    })
}

#[derive(Default)]
pub struct TrackDetails {
    pub cover: Option<image::RgbaImage>,
    pub lyrics: Vec<LyricLine>,
}

pub fn read_details(path: &Path) -> TrackDetails {
    let tagged = lofty::read_from_path(path).ok();
    let tag = tagged.as_ref().and_then(|file| file.primary_tag().or_else(|| file.first_tag()));
    let cover = tag
        .and_then(|tag| {
            tag.get_picture_type(PictureType::CoverFront).or_else(|| tag.pictures().first())
        })
        .and_then(|pic| image::load_from_memory(pic.data()).ok())
        .map(|img| img.into_rgba8());
    let embedded = tag.and_then(|tag| {
        tag.get_string(ItemKey::UnsyncLyrics).or_else(|| tag.get_string(ItemKey::Lyrics))
    });
    TrackDetails {
        cover,
        lyrics: read_lyrics(path, embedded),
    }
}

#[cfg(test)]
mod tests {
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
}
