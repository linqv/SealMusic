use std::{fs::File, io::BufReader, path::Path};

use rodio::Decoder;

pub fn decode(path: &Path) -> Result<Decoder<BufReader<File>>, String> {
    let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Decoder::try_from(file).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::{fs, path::Path, time::Duration};

    use rodio::Source;

    use super::decode;
    use crate::library::{metadata, scan};

    #[test]
    fn scans_decodes_and_seeks_aac_lc_and_alac_m4a() {
        let dir = tempfile::tempdir().unwrap();
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        // Both fixtures contain a two-second, 16 kHz mono tone and MP4 tags.
        fs::copy(fixtures.join("aac-lc.m4a"), dir.path().join("aac-lc.M4A")).unwrap();
        fs::copy(fixtures.join("alac.m4a"), dir.path().join("alac.m4a")).unwrap();

        let mut tracks = scan::scan(dir.path()).unwrap();
        tracks.sort_by(|a, b| a.title.cmp(&b.title));
        assert_eq!(tracks.len(), 2);

        for (track, title) in tracks.iter().zip(["AAC-LC", "ALAC"]) {
            assert_eq!(track.title, title);
            assert_eq!(track.artist, "Fixture artist");
            assert_eq!(track.album, "Fixture album");
            // The AAC sample's reported duration can include encoder padding.
            assert!(track.duration.abs_diff(Duration::from_secs(2)) < Duration::from_millis(150));

            let path = track.local_path().unwrap();
            let details = metadata::read_details(path);
            assert_eq!(details.cover.unwrap().dimensions(), (2, 3));
            assert_eq!(details.lyrics[0].text, "Embedded line");

            let mut source = decode(path).unwrap();
            assert!(source.by_ref().take(4_000).any(|sample| sample.abs() > 0.01));
            source.try_seek(Duration::from_secs(1)).unwrap();
            let remaining = source.count();
            assert!((14_000..=18_000).contains(&remaining), "{title}: {remaining} samples");
        }
    }
}
