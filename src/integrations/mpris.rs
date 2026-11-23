use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use futures_lite::future::block_on;
use mpris_server::{
    LoopStatus, Metadata, PlaybackRate, PlaybackStatus, PlayerInterface, Property, RootInterface,
    Server, Signal, Time, TrackId, Volume,
    zbus::{Result, fdo},
};

use crate::{
    library,
    player::{
        command::{Controls, PlayerCommand as Cmd},
        state::{PlaybackState, SharedState, Status},
    },
    slint_types::LoopMode,
};

pub fn cache_cover(
    image: &image::RgbaImage,
) -> std::result::Result<tempfile::NamedTempFile, Box<dyn std::error::Error>> {
    let dir = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home::home_dir().map(|p| p.join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("sealmusic/art");
    fs::create_dir_all(&dir)?;
    let mut f = tempfile::Builder::new().suffix(".png").tempfile_in(dir)?;
    image.write_to(f.as_file_mut(), image::ImageFormat::Png)?;
    Ok(f)
}

struct Desktop {
    controls: Controls,
    state: SharedState,
}
impl Desktop {
    async fn send(&self, cmd: Cmd) -> fdo::Result<()> {
        self.controls.execute(cmd).await.map_err(fdo::Error::Failed)
    }
    fn snapshot(&self) -> PlaybackState {
        self.state.read().unwrap().clone()
    }
}

fn time(duration: Duration) -> Time {
    Time::from_micros(duration.as_micros().min(i64::MAX as u128) as i64)
}
fn status(s: Status) -> PlaybackStatus {
    match s {
        Status::Stopped => PlaybackStatus::Stopped,
        Status::Playing => PlaybackStatus::Playing,
        Status::Paused => PlaybackStatus::Paused,
    }
}
fn looping(m: LoopMode) -> LoopStatus {
    match m {
        LoopMode::None => LoopStatus::None,
        LoopMode::Track => LoopStatus::Track,
        LoopMode::Playlist => LoopStatus::Playlist,
    }
}
fn metadata(state: &PlaybackState) -> Metadata {
    let Some(t) = &state.track else {
        return Metadata::default();
    };
    let Ok(id) = TrackId::try_from(t.id.0.as_str()) else {
        return Metadata::default();
    };
    let mut m = Metadata::builder()
        .trackid(id)
        .length(time(state.duration))
        .title(t.title.clone())
        .artist([t.artist.clone()]);
    if let Some(url) = &state.art_url {
        m = m.art_url(url.clone());
    }
    m.build()
}

impl RootInterface for Desktop {
    async fn raise(&self) -> fdo::Result<()> {
        self.send(Cmd::Raise).await
    }
    async fn quit(&self) -> fdo::Result<()> {
        self.send(Cmd::Quit).await
    }
    async fn can_quit(&self) -> fdo::Result<bool> {
        Ok(true)
    }
    async fn fullscreen(&self) -> fdo::Result<bool> {
        Ok(false)
    }
    async fn set_fullscreen(&self, _: bool) -> Result<()> {
        Err(fdo::Error::NotSupported("Fullscreen".into()).into())
    }
    async fn can_set_fullscreen(&self) -> fdo::Result<bool> {
        Ok(false)
    }
    async fn can_raise(&self) -> fdo::Result<bool> {
        Ok(true)
    }
    async fn has_track_list(&self) -> fdo::Result<bool> {
        Ok(false)
    }
    async fn identity(&self) -> fdo::Result<String> {
        Ok("SealMusic".into())
    }
    async fn desktop_entry(&self) -> fdo::Result<String> {
        Ok(crate::APP_ID.into())
    }
    async fn supported_uri_schemes(&self) -> fdo::Result<Vec<String>> {
        Ok(vec![])
    }
    async fn supported_mime_types(&self) -> fdo::Result<Vec<String>> {
        Ok(vec![])
    }
}
impl PlayerInterface for Desktop {
    async fn next(&self) -> fdo::Result<()> {
        self.send(Cmd::Next).await
    }
    async fn previous(&self) -> fdo::Result<()> {
        self.send(Cmd::Previous).await
    }
    async fn pause(&self) -> fdo::Result<()> {
        self.send(Cmd::Pause).await
    }
    async fn play_pause(&self) -> fdo::Result<()> {
        self.send(Cmd::TogglePlayPause).await
    }
    async fn stop(&self) -> fdo::Result<()> {
        self.send(Cmd::Stop).await
    }
    async fn play(&self) -> fdo::Result<()> {
        self.send(Cmd::Play).await
    }
    async fn seek(&self, offset: Time) -> fdo::Result<()> {
        self.send(Cmd::SeekRelative(offset.as_micros())).await
    }
    async fn set_position(&self, id: TrackId, pos: Time) -> fdo::Result<()> {
        if let Ok(us) = u64::try_from(pos.as_micros()) {
            self.send(Cmd::SeekTo(
                Some(library::TrackId(id.to_string())),
                Duration::from_micros(us),
            ))
            .await?;
        }
        Ok(())
    }
    async fn open_uri(&self, _: String) -> fdo::Result<()> {
        Err(fdo::Error::NotSupported("OpenUri".into()))
    }
    async fn playback_status(&self) -> fdo::Result<PlaybackStatus> {
        Ok(status(self.snapshot().status))
    }
    async fn loop_status(&self) -> fdo::Result<LoopStatus> {
        Ok(looping(self.snapshot().loop_mode))
    }
    async fn set_loop_status(&self, m: LoopStatus) -> Result<()> {
        self.send(Cmd::SetLoopMode(match m {
            LoopStatus::None => LoopMode::None,
            LoopStatus::Track => LoopMode::Track,
            LoopStatus::Playlist => LoopMode::Playlist,
        }))
        .await?;
        Ok(())
    }
    async fn rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(1.)
    }
    async fn set_rate(&self, rate: PlaybackRate) -> Result<()> {
        if rate == 0. {
            self.send(Cmd::Pause).await?;
        } else if rate != 1. {
            return Err(fdo::Error::NotSupported("Rate".into()).into());
        }
        Ok(())
    }
    async fn shuffle(&self) -> fdo::Result<bool> {
        Ok(self.snapshot().shuffle)
    }
    async fn set_shuffle(&self, s: bool) -> Result<()> {
        Ok(self.send(Cmd::SetShuffle(s)).await?)
    }
    async fn metadata(&self) -> fdo::Result<Metadata> {
        Ok(metadata(&self.snapshot()))
    }
    async fn volume(&self) -> fdo::Result<Volume> {
        Ok(self.snapshot().volume.into())
    }
    async fn set_volume(&self, v: Volume) -> Result<()> {
        Ok(self.send(Cmd::SetVolume(v.clamp(0., 1.) as f32)).await?)
    }
    async fn position(&self) -> fdo::Result<Time> {
        Ok(time(self.snapshot().position_now()))
    }
    async fn minimum_rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(1.)
    }
    async fn maximum_rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(1.)
    }
    async fn can_go_next(&self) -> fdo::Result<bool> {
        Ok(self.snapshot().can_next)
    }
    async fn can_go_previous(&self) -> fdo::Result<bool> {
        Ok(self.snapshot().can_previous)
    }
    async fn can_play(&self) -> fdo::Result<bool> {
        Ok(self.snapshot().can_play)
    }
    async fn can_pause(&self) -> fdo::Result<bool> {
        Ok(self.snapshot().can_play)
    }
    async fn can_seek(&self) -> fdo::Result<bool> {
        Ok(self.snapshot().can_seek())
    }
    async fn can_control(&self) -> fdo::Result<bool> {
        Ok(true)
    }
}

fn changes(old: &PlaybackState, new: &PlaybackState) -> Vec<Property> {
    let mut properties = Vec::new();
    if old.status != new.status {
        properties.push(Property::PlaybackStatus(status(new.status)));
    }
    if old.track != new.track || old.art_url != new.art_url || old.duration != new.duration {
        properties.push(Property::Metadata(metadata(new)));
    }
    if old.volume != new.volume {
        properties.push(Property::Volume(new.volume.into()));
    }
    if old.loop_mode != new.loop_mode {
        properties.push(Property::LoopStatus(looping(new.loop_mode)));
    }
    if old.shuffle != new.shuffle {
        properties.push(Property::Shuffle(new.shuffle));
    }
    if old.can_next != new.can_next {
        properties.push(Property::CanGoNext(new.can_next));
    }
    if old.can_previous != new.can_previous {
        properties.push(Property::CanGoPrevious(new.can_previous));
    }
    if old.can_seek() != new.can_seek() {
        properties.push(Property::CanSeek(new.can_seek()));
    }
    if old.can_play != new.can_play {
        properties.extend([Property::CanPlay(new.can_play), Property::CanPause(new.can_play)]);
    }
    properties
}

type Update = (PlaybackState, Option<Duration>);

pub fn start(
    controls: Controls,
    state: SharedState,
    can_run_hidden: Arc<AtomicBool>,
) -> (mpsc::Sender<Update>, thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<Update>();
    let handle = thread::spawn(move || {
        let server = match block_on(Server::new(
            crate::APP_ID,
            Desktop {
                controls,
                state,
            },
        )) {
            Ok(s) => s,
            Err(e) => {
                // The Linux tray also needs a session bus. Avoid hiding an unreachable window.
                can_run_hidden.store(false, Ordering::Release);
                log::warn!("MPRIS unavailable: {e}");
                return;
            }
        };
        can_run_hidden.store(true, Ordering::Release);
        let mut previous = PlaybackState::default();
        for (current, seeked) in rx {
            let properties = changes(&previous, &current);
            let result = block_on(async {
                if !properties.is_empty() {
                    server.properties_changed(properties).await?;
                }
                if let Some(pos) = seeked {
                    server
                        .emit(Signal::Seeked {
                            position: time(pos),
                        })
                        .await?;
                }
                Ok::<_, mpris_server::zbus::Error>(())
            });
            if let Err(e) = result {
                log::warn!("MPRIS notification: {e}");
                break;
            }
            previous = current;
        }
        if let Err(e) = block_on(server.release_bus_name()) {
            log::warn!("Releasing MPRIS: {e}");
        }
    });
    (tx, handle)
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, path::Path, time::Instant};

    use mpris_server::zbus::{
        self, fdo,
        names::BusName,
        zvariant::{OwnedObjectPath, OwnedValue},
    };

    use super::*;
    use crate::{
        config::Config,
        library::{Track, TrackOrigin},
        player::{command::PlayerCommand as Cmd, state::PlaybackEvent as Event},
    };

    fn sample_track(name: &str) -> Arc<Track> {
        let path = PathBuf::from(format!("/music/{name}.flac"));
        Arc::new(Track {
            id: library::TrackId::local(&path),
            origin: TrackOrigin::Local(path),
            title: "Sample Title".into(),
            artist: "Sample Artist".into(),
            duration: Duration::from_secs(125),
        })
    }

    #[test]
    fn metadata_maps_track_fields_and_roundtrips_track_id() {
        let empty = metadata(&PlaybackState::default());
        assert!(empty.trackid().is_none());

        let track = sample_track("track_1");
        let state = PlaybackState {
            track: Some(track.clone()),
            duration: track.duration,
            art_url: Some("file:///tmp/cover.png".into()),
            ..Default::default()
        };
        let meta = metadata(&state);
        assert_eq!(meta.trackid().unwrap().as_str(), track.id.0);
        assert_eq!(TrackId::try_from(track.id.0.as_str()).unwrap().to_string(), track.id.0);
        assert_eq!(meta.length().unwrap().as_micros(), 125_000_000);
        assert_eq!(meta.title(), Some("Sample Title"));
        assert_eq!(meta.artist(), Some(vec!["Sample Artist".into()]));
        assert_eq!(meta.art_url().unwrap().as_str(), "file:///tmp/cover.png");
    }

    #[test]
    fn changes_only_emits_modified_properties() {
        let track_a = sample_track("a");
        let track_b = sample_track("b");
        let base = PlaybackState {
            track: Some(track_a),
            duration: Duration::from_secs(10),
            position: Duration::from_secs(1),
            status: Status::Paused,
            volume: 0.8,
            loop_mode: LoopMode::Playlist,
            shuffle: false,
            can_next: true,
            can_previous: true,
            can_play: true,
            art_url: Some("file:///tmp/a.png".into()),
            ..Default::default()
        };

        let mut position_only = base.clone();
        position_only.position = Duration::from_secs(4);
        position_only.sampled_at = Instant::now() + Duration::from_secs(3);
        assert!(changes(&base, &position_only).is_empty());

        let updated = PlaybackState {
            track: Some(track_b),
            duration: Duration::from_secs(20),
            status: Status::Playing,
            volume: 0.5,
            loop_mode: LoopMode::Track,
            shuffle: true,
            can_next: false,
            can_previous: false,
            can_play: false,
            art_url: Some("file:///tmp/b.png".into()),
            ..base.clone()
        };
        let diff = changes(&base, &updated);
        assert!(diff.contains(&Property::PlaybackStatus(PlaybackStatus::Playing)));
        assert!(diff.contains(&Property::Metadata(metadata(&updated))));
        assert!(diff.contains(&Property::Volume(0.5)));
        assert!(diff.contains(&Property::LoopStatus(LoopStatus::Track)));
        assert!(diff.contains(&Property::Shuffle(true)));
        assert!(diff.contains(&Property::CanGoNext(false)));
        assert!(diff.contains(&Property::CanGoPrevious(false)));
        assert!(diff.contains(&Property::CanPlay(false)));
        assert!(diff.contains(&Property::CanPause(false)));

        let stopped = PlaybackState {
            status: Status::Stopped,
            ..base.clone()
        };
        assert!(changes(&base, &stopped).contains(&Property::CanSeek(false)));
    }

    #[test]
    fn cache_cover_writes_png_and_cleans_up_on_drop() {
        let mut img = image::RgbaImage::new(2, 3);
        img.put_pixel(0, 0, image::Rgba([255, 0, 0, 255]));
        let temp = cache_cover(&img).unwrap();
        let path = temp.path().to_path_buf();
        assert!(path.exists());
        let decoded = image::open(&path).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (2, 3));
        drop(temp);
        assert!(!path.exists());
    }

    #[test]
    #[ignore = "run under dbus-run-session"]
    fn mpris_dbus_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tagged.flac");
        let first = dir.path().join("01_first.flac");
        let second = dir.path().join("02_second.flac");
        std::fs::copy(&fixture, &first).unwrap();
        std::fs::copy(&fixture, &second).unwrap();

        let (tx, rx) = mpsc::channel();
        let controls = Controls(tx);
        let shared = SharedState::default();
        let can_run_hidden = Arc::new(AtomicBool::new(false));
        let (desktop, service) = start(controls.clone(), shared.clone(), can_run_hidden.clone());
        let cfg = Config {
            song_dir: dir.path().to_path_buf(),
            ..Default::default()
        };
        let worker = crate::player::spawn_test_worker(cfg, shared, rx, move |event| {
            if let Event::State(s, seeked) = &event {
                let _ = desktop.send((s.clone(), *seeked));
            }
        });

        let cached_art_path = block_on(async {
            let conn = zbus::Connection::session().await.unwrap();
            let bus_name =
                BusName::try_from(format!("org.mpris.MediaPlayer2.{}", crate::APP_ID)).unwrap();
            let dbus = fdo::DBusProxy::new(&conn).await.unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !dbus.name_has_owner(bus_name.clone()).await.unwrap() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "MPRIS bus name did not register in time"
                );
                thread::sleep(Duration::from_millis(20));
            }
            assert!(can_run_hidden.load(Ordering::Acquire));

            async fn uncached_proxy<'a>(
                conn: &'a zbus::Connection,
                bus_name: &'a str,
                interface: &'a str,
            ) -> zbus::Proxy<'a> {
                zbus::proxy::Builder::<zbus::Proxy<'a>>::new(conn)
                    .destination(bus_name)
                    .unwrap()
                    .path("/org/mpris/MediaPlayer2")
                    .unwrap()
                    .interface(interface)
                    .unwrap()
                    .cache_properties(zbus::proxy::CacheProperties::No)
                    .build()
                    .await
                    .unwrap()
            }

            let root = uncached_proxy(&conn, bus_name.as_str(), "org.mpris.MediaPlayer2").await;
            assert_eq!(root.get_property::<String>("Identity").await.unwrap(), "SealMusic");
            assert_eq!(root.get_property::<String>("DesktopEntry").await.unwrap(), crate::APP_ID);
            assert!(!root.get_property::<bool>("HasTrackList").await.unwrap());
            assert!(root.get_property::<bool>("CanRaise").await.unwrap());
            assert!(root.get_property::<bool>("CanQuit").await.unwrap());
            assert!(!root.get_property::<bool>("CanSetFullscreen").await.unwrap());
            assert!(
                root.get_property::<Vec<String>>("SupportedUriSchemes").await.unwrap().is_empty()
            );
            assert!(
                root.get_property::<Vec<String>>("SupportedMimeTypes").await.unwrap().is_empty()
            );

            let player =
                uncached_proxy(&conn, bus_name.as_str(), "org.mpris.MediaPlayer2.Player").await;

            while !player.get_property::<bool>("CanPlay").await.unwrap() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "Player library did not become ready in time"
                );
                thread::sleep(Duration::from_millis(20));
            }
            assert!(player.get_property::<bool>("CanControl").await.unwrap());
            assert!(player.get_property::<bool>("CanPause").await.unwrap());
            assert!(player.get_property::<bool>("CanGoNext").await.unwrap());
            assert!(player.get_property::<bool>("CanGoPrevious").await.unwrap());

            let meta: HashMap<String, OwnedValue> = player.get_property("Metadata").await.unwrap();
            let track_id_1 =
                OwnedObjectPath::try_from(meta["mpris:trackid"].try_clone().unwrap()).unwrap();
            assert!(track_id_1.as_str().starts_with("/dev/sealmusic/track/local_"));
            let length = i64::try_from(&*meta["mpris:length"]).unwrap();
            assert!(length > 0);
            assert_eq!(<&str>::try_from(&*meta["xesam:title"]).unwrap(), "Fixture title");
            let artists =
                Vec::<String>::try_from(meta["xesam:artist"].try_clone().unwrap()).unwrap();
            assert_eq!(artists, vec!["Fixture artist"]);
            let art_url = String::try_from(meta["mpris:artUrl"].try_clone().unwrap()).unwrap();
            let art_path = url::Url::parse(&art_url).unwrap().to_file_path().unwrap();
            assert!(art_path.exists());

            player.call::<_, _, ()>("Play", &()).await.unwrap();
            assert_eq!(player.get_property::<String>("PlaybackStatus").await.unwrap(), "Playing");
            player.call::<_, _, ()>("Pause", &()).await.unwrap();
            assert_eq!(player.get_property::<String>("PlaybackStatus").await.unwrap(), "Paused");
            player.call::<_, _, ()>("PlayPause", &()).await.unwrap();
            assert_eq!(player.get_property::<String>("PlaybackStatus").await.unwrap(), "Playing");
            player.call::<_, _, ()>("PlayPause", &()).await.unwrap();
            assert_eq!(player.get_property::<String>("PlaybackStatus").await.unwrap(), "Paused");

            player
                .call::<_, _, ()>("SetPosition", &(track_id_1.as_ref(), 500_000_i64))
                .await
                .unwrap();
            assert_eq!(player.get_property::<i64>("Position").await.unwrap(), 500_000);
            let stale_id = TrackId::try_from("/dev/sealmusic/track/stale").unwrap();
            player.call::<_, _, ()>("SetPosition", &(stale_id.into_inner(), 0_i64)).await.unwrap();
            assert_eq!(player.get_property::<i64>("Position").await.unwrap(), 500_000);
            player.call::<_, _, ()>("Seek", &(250_000_i64)).await.unwrap();
            assert_eq!(player.get_property::<i64>("Position").await.unwrap(), 750_000);

            player.call::<_, _, ()>("Next", &()).await.unwrap();
            let meta_2: HashMap<String, OwnedValue> =
                player.get_property("Metadata").await.unwrap();
            let track_id_2 =
                OwnedObjectPath::try_from(meta_2["mpris:trackid"].try_clone().unwrap()).unwrap();
            assert_ne!(track_id_2, track_id_1);
            player.call::<_, _, ()>("Previous", &()).await.unwrap();
            let meta_back: HashMap<String, OwnedValue> =
                player.get_property("Metadata").await.unwrap();
            let track_id_back =
                OwnedObjectPath::try_from(meta_back["mpris:trackid"].try_clone().unwrap()).unwrap();
            assert_eq!(track_id_back, track_id_1);

            player.set_property("Volume", 0.42_f64).await.unwrap();
            assert!((player.get_property::<f64>("Volume").await.unwrap() - 0.42).abs() < 1e-4);
            player.set_property("Shuffle", true).await.unwrap();
            assert!(player.get_property::<bool>("Shuffle").await.unwrap());
            for loop_status in ["Track", "None", "Playlist"] {
                player.set_property("LoopStatus", loop_status).await.unwrap();
                assert_eq!(player.get_property::<String>("LoopStatus").await.unwrap(), loop_status);
            }

            assert!(player.call::<_, _, ()>("OpenUri", &("file:///tmp/x.flac",)).await.is_err());

            player.call::<_, _, ()>("Stop", &()).await.unwrap();
            assert_eq!(player.get_property::<String>("PlaybackStatus").await.unwrap(), "Stopped");
            assert!(!player.get_property::<bool>("CanSeek").await.unwrap());

            if let Ok(proc_status) = fs::read_to_string("/proc/self/status") {
                for line in proc_status
                    .lines()
                    .filter(|l| l.starts_with("Threads:") || l.starts_with("VmRSS:"))
                {
                    println!("mpris_roundtrip {line}");
                }
            }

            art_path
        });

        controls.send(Cmd::Shutdown);
        worker.join().unwrap();
        service.join().unwrap();

        block_on(async {
            let conn = zbus::Connection::session().await.unwrap();
            let dbus = fdo::DBusProxy::new(&conn).await.unwrap();
            let bus_name =
                BusName::try_from(format!("org.mpris.MediaPlayer2.{}", crate::APP_ID)).unwrap();
            assert!(!dbus.name_has_owner(bus_name).await.unwrap());
        });
        assert!(!cached_art_path.exists());
    }

    #[test]
    #[ignore = "run with invalid DBUS_SESSION_BUS_ADDRESS"]
    fn mpris_degrades_without_session_bus() {
        let (tx, _rx) = mpsc::channel();
        let can_run_hidden = Arc::new(AtomicBool::new(true));
        let (desktop, service) =
            start(Controls(tx), SharedState::default(), can_run_hidden.clone());
        drop(desktop);
        service.join().unwrap();
        assert!(!can_run_hidden.load(Ordering::Acquire));
    }
}
