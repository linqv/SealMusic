use std::{
    any::Any,
    cell::{Cell, OnceCell, RefCell},
    collections::HashSet,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use slint::{Model, ToSharedString};

use crate::{
    config::Config,
    library::{
        Track, TrackId,
        lyrics::{LyricLine, group_for_display},
    },
    player::{
        self,
        command::{Controls, PlayerCommand as Cmd},
        state::{PlaybackEvent as Event, SharedState, Status},
    },
    slint_types::*,
};

const SEARCH_DEBOUNCE: Duration = Duration::from_millis(120);
const PROGRESS_TICK_MS: u64 = 250;
const KARAOKE_TICK_MS: u64 = 33;
const LYRIC_MIN_WAIT_MS: u64 = 8;
const LYRIC_MAX_WAIT_MS: u64 = 1000;
const WORD_PROGRESS_STEPS: f32 = 24.0;

#[derive(Clone, Copy, Debug, PartialEq)]
struct WordSpan {
    start: f32,
    end: f32,
}

#[derive(Clone, Debug, PartialEq)]
struct LineTiming {
    start: f32,
    end: f32,
    words: Vec<WordSpan>,
    sweep_end: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct LyricCursor {
    line_index: i32,
    word_index: i32,
    word_progress: f32,
    sweeping: bool,
}

type LyricTimeline = Rc<RefCell<Vec<LineTiming>>>;

fn default_cover() -> slint::Image {
    thread_local! {
        static COVER: OnceCell<slint::Image> = const { OnceCell::new() };
    }
    COVER.with(|cell| {
        cell.get_or_init(|| {
            slint::Image::load_from_svg_data(include_bytes!("../ui/cover.svg")).unwrap()
        })
        .clone()
    })
}

fn find_active_lyric(timeline: &[LineTiming], pos: f32) -> i32 {
    let idx = timeline.partition_point(|line| line.start <= pos);
    let Some(i) = idx.checked_sub(1) else {
        return -1;
    };
    let line = &timeline[i];
    if pos >= line.start && pos < line.end {
        i as i32
    } else {
        -1
    }
}

fn resolve_lyric_cursor(timeline: &[LineTiming], pos: f32) -> LyricCursor {
    let line_index = find_active_lyric(timeline, pos);
    if line_index < 0 {
        return LyricCursor {
            line_index: -1,
            word_index: -1,
            word_progress: 0.0,
            sweeping: false,
        };
    }
    let line = &timeline[line_index as usize];
    if line.words.is_empty() {
        return LyricCursor {
            line_index,
            word_index: -1,
            word_progress: 0.0,
            sweeping: false,
        };
    }
    if pos < line.words[0].start {
        return LyricCursor {
            line_index,
            word_index: -1,
            word_progress: 0.0,
            sweeping: false,
        };
    }
    if pos >= line.sweep_end {
        return LyricCursor {
            line_index,
            word_index: line.words.len() as i32,
            word_progress: 1.0,
            sweeping: false,
        };
    }
    let wi = line.words.partition_point(|w| w.start <= pos).saturating_sub(1);
    let w = line.words[wi];
    if pos >= w.end {
        return LyricCursor {
            line_index,
            word_index: wi as i32,
            word_progress: 1.0,
            sweeping: false,
        };
    }
    let raw = ((pos - w.start) / (w.end - w.start)).clamp(0.0, 1.0);
    let word_progress = (raw * WORD_PROGRESS_STEPS).round() / WORD_PROGRESS_STEPS;
    LyricCursor {
        line_index,
        word_index: wi as i32,
        word_progress,
        sweeping: true,
    }
}

fn next_lyric_delay(timeline: &[LineTiming], pos: f32, cursor: LyricCursor) -> Option<Duration> {
    if timeline.is_empty() {
        return None;
    }
    if cursor.sweeping {
        return Some(Duration::from_millis(KARAOKE_TICK_MS));
    }
    let mut next_ts: Option<f32> = None;
    let mut consider = |ts: f32| {
        if ts.is_finite() && ts > pos {
            next_ts = Some(next_ts.map_or(ts, |cur| cur.min(ts)));
        }
    };
    if cursor.line_index >= 0 {
        let line = &timeline[cursor.line_index as usize];
        if let Some(word) = line.words.iter().find(|w| w.start > pos) {
            consider(word.start);
        }
        consider(line.end);
    }
    let next_idx = timeline.partition_point(|line| line.start <= pos);
    if let Some(next_line) = timeline.get(next_idx) {
        consider(next_line.start);
    }
    let ts = next_ts?;
    let wait_ms = ((f64::from(ts) - f64::from(pos)).max(0.0) * 1000.0).round() as u64;
    Some(Duration::from_millis(wait_ms.clamp(LYRIC_MIN_WAIT_MS, LYRIC_MAX_WAIT_MS)))
}

fn min_progress_step(duration: f32, now_playing_open: bool) -> f32 {
    if !duration.is_finite() || duration <= 0.0 {
        return 0.5;
    }
    let bar_px = if now_playing_open {
        324.0
    } else {
        520.0
    };
    (duration / bar_px).clamp(0.2, 1.0)
}

fn set_progress_exact(playback: &PlaybackGlobal<'_>, pos: f32) {
    playback.set_progress(pos);
    let secs = pos.max(0.0).floor() as i32;
    if secs != playback.get_progress_secs() {
        playback.set_progress_secs(secs);
    }
}

fn sync_progress_quantized(ui: &MainWindow, pos: f32) {
    let playback = ui.global::<PlaybackGlobal>();
    let secs = pos.max(0.0).floor() as i32;
    let secs_changed = secs != playback.get_progress_secs();
    if secs_changed {
        playback.set_progress_secs(secs);
    }
    let now_playing_open = ui.get_lyrics_visible() || ui.get_queue_visible();
    let step = min_progress_step(playback.get_duration(), now_playing_open);
    let prev = playback.get_progress();
    if secs_changed || pos < prev || (pos - prev) >= step {
        playback.set_progress(pos);
    }
}

fn sync_active_lyric(ui: &MainWindow, timeline: &LyricTimeline, pos: f32) -> LyricCursor {
    let display = ui.global::<DisplayGlobal>();
    let cursor = resolve_lyric_cursor(&timeline.borrow(), pos);
    if cursor.line_index != display.get_active_lyric_index() {
        display.set_active_lyric_index(cursor.line_index);
    }
    if cursor.word_index != display.get_active_word_index() {
        display.set_active_word_index(cursor.word_index);
    }
    if (cursor.word_progress - display.get_active_word_progress()).abs() > f32::EPSILON {
        display.set_active_word_progress(cursor.word_progress);
    }
    cursor
}

fn sync_visible_progress(ui: &MainWindow, state: &SharedState, timeline: &LyricTimeline) {
    let playback = ui.global::<PlaybackGlobal>();
    let pos = state.read().unwrap().position_now().as_secs_f32();
    set_progress_exact(&playback, pos);
    if ui.get_lyrics_visible() {
        sync_active_lyric(ui, timeline, pos);
    }
}

fn schedule_lyric_timer(ui: &MainWindow, sync: &Rc<UiSync>) {
    let should_run = ui.window().is_visible()
        && !ui.global::<PlaybackGlobal>().get_paused()
        && ui.get_lyrics_visible()
        && !ui.get_now_playing_sliding()
        && !sync.timeline.borrow().is_empty();
    if !should_run {
        sync.lyric_timer.stop();
        return;
    }
    let pos = sync.state.read().unwrap().position_now().as_secs_f32();
    let cursor = sync_active_lyric(ui, &sync.timeline, pos);
    let Some(delay) = next_lyric_delay(&sync.timeline.borrow(), pos, cursor) else {
        sync.lyric_timer.stop();
        return;
    };
    let weak = ui.as_weak();
    let sync_ref = Rc::clone(sync);
    sync.lyric_timer.start(slint::TimerMode::SingleShot, delay, move || {
        let Some(ui) = weak.upgrade() else {
            sync_ref.lyric_timer.stop();
            return;
        };
        schedule_lyric_timer(&ui, &sync_ref);
    });
}

fn sync_timers(ui: &MainWindow, sync: &Rc<UiSync>) {
    let should_run = ui.window().is_visible() && !ui.global::<PlaybackGlobal>().get_paused();
    if !should_run {
        sync.progress_timer.stop();
        sync.lyric_timer.stop();
        return;
    }
    if !sync.progress_timer.running() {
        let weak = ui.as_weak();
        let sync_ref = Rc::clone(sync);
        sync.progress_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(PROGRESS_TICK_MS),
            move || {
                let Some(ui) = weak.upgrade() else {
                    sync_ref.progress_timer.stop();
                    return;
                };
                if !ui.window().is_visible() || ui.global::<PlaybackGlobal>().get_paused() {
                    sync_ref.progress_timer.stop();
                    sync_ref.lyric_timer.stop();
                    return;
                }
                if ui.get_now_playing_sliding() {
                    return;
                }
                let pos = sync_ref.state.read().unwrap().position_now().as_secs_f32();
                sync_progress_quantized(&ui, pos);
            },
        );
    }
    schedule_lyric_timer(ui, sync);
}

struct ImmutableTracker;

impl slint::ModelTracker for ImmutableTracker {
    fn attach_peer(&self, _peer: slint::ModelPeer) {}
    fn track_row_count_changes(&self) {}
    fn track_row_data_changes(&self, _row: usize) {}
}

static IMMUTABLE_TRACKER: ImmutableTracker = ImmutableTracker;

fn format_mmss(duration: Duration) -> slint::SharedString {
    let secs = duration.as_secs();
    format!("{:02}:{:02}", secs / 60, secs % 60).into()
}

fn song(t: &Track) -> SongInfo {
    SongInfo {
        id: t.id.0.as_str().into(),
        song_name: t.title.as_str().into(),
        singer: t.artist.as_str().into(),
        album: t.album.as_str().into(),
        duration: format_mmss(t.duration),
        is_local: t.local_path().is_some(),
    }
}

fn song_row(t: &Track) -> SongInfo {
    SongInfo {
        id: t.id.0.as_str().into(),
        song_name: t.title.as_str().into(),
        singer: t.artist.as_str().into(),
        album: slint::SharedString::default(),
        duration: format_mmss(t.duration),
        is_local: t.local_path().is_some(),
    }
}

struct TrackListModel(Vec<Arc<Track>>);

impl Model for TrackListModel {
    type Data = SongInfo;

    fn row_count(&self) -> usize {
        self.0.len()
    }

    fn row_data(&self, row: usize) -> Option<SongInfo> {
        self.0.get(row).map(|track| song_row(track))
    }

    fn model_tracker(&self) -> &dyn slint::ModelTracker {
        &IMMUTABLE_TRACKER
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct LikedTracksModel {
    ids: Vec<Arc<str>>,
    set: HashSet<Arc<str>>,
}

impl LikedTracksModel {
    fn new(liked: Vec<TrackId>) -> Self {
        let ids: Vec<Arc<str>> = liked.into_iter().map(|id| Arc::<str>::from(id.0)).collect();
        let set: HashSet<Arc<str>> = ids.iter().cloned().collect();
        Self {
            ids,
            set,
        }
    }
}

impl Model for LikedTracksModel {
    type Data = slint::SharedString;

    fn row_count(&self) -> usize {
        self.ids.len()
    }

    fn row_data(&self, row: usize) -> Option<slint::SharedString> {
        self.ids.get(row).map(|id| id.as_ref().into())
    }

    fn model_tracker(&self) -> &dyn slint::ModelTracker {
        &IMMUTABLE_TRACKER
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(target_os = "linux")]
struct CatalogRowModel(Vec<Arc<Track>>);

#[cfg(target_os = "linux")]
impl Model for CatalogRowModel {
    type Data = CatalogRow;

    fn row_count(&self) -> usize {
        self.0.len()
    }

    fn row_data(&self, row: usize) -> Option<CatalogRow> {
        self.0.get(row).map(|track| CatalogRow {
            song: song_row(track),
            source: match &track.origin {
                crate::library::TrackOrigin::Online(reference) => reference.source().into(),
                crate::library::TrackOrigin::Local(_) => Default::default(),
            },
        })
    }

    fn model_tracker(&self) -> &dyn slint::ModelTracker {
        &IMMUTABLE_TRACKER
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn model(tracks: Vec<Arc<Track>>) -> slint::ModelRc<SongInfo> {
    slint::ModelRc::new(TrackListModel(tracks))
}

fn lyrics_should_project(ui: &MainWindow) -> bool {
    ui.window().is_visible()
        && (ui.get_lyrics_visible() || (ui.get_now_playing_visible() && !ui.get_queue_visible()))
}

fn project_lyrics(ui: &MainWindow, sync: &UiSync, pos: f32) {
    let track_duration = sync.state.read().unwrap().duration.as_secs_f32();
    let grouped = group_for_display(&sync.raw_lyrics.borrow(), track_duration);
    let mut times = Vec::with_capacity(grouped.len());
    let lyrics: Vec<_> = grouped
        .into_iter()
        .map(|line| {
            let word_spans: Vec<_> = line
                .words
                .iter()
                .map(|w| WordSpan {
                    start: w.time,
                    end: (w.time + w.duration).max(w.time + 0.001),
                })
                .collect();
            let sweep_end = word_spans.iter().map(|w| w.end).fold(line.time, f32::max);
            times.push(LineTiming {
                start: line.time,
                end: line.end_time,
                words: word_spans,
                sweep_end,
            });
            let words: Vec<_> = line
                .words
                .into_iter()
                .map(|w| LyricWord {
                    text: w.text.into(),
                    space_after: w.space_after,
                })
                .collect();
            LyricItem {
                time: line.time,
                text: line.text.into(),
                translation: line.translation.into(),
                words: slint::ModelRc::new(slint::VecModel::from(words)),
            }
        })
        .collect();
    *sync.timeline.borrow_mut() = times;
    sync.lyrics_projected.set(true);
    sync_active_lyric(ui, &sync.timeline, pos);
    ui.global::<DisplayGlobal>().set_lyrics(slint::ModelRc::new(slint::VecModel::from(lyrics)));
}

fn clear_lyrics_projection(ui: &MainWindow, sync: &UiSync) {
    if !sync.lyrics_projected.replace(false) {
        return;
    }
    sync.timeline.borrow_mut().clear();
    let display = ui.global::<DisplayGlobal>();
    display.set_lyrics(Default::default());
    display.set_active_lyric_index(-1);
    display.set_active_word_index(-1);
    display.set_active_word_progress(0.0);
}

fn sync_lyrics_projection(ui: &MainWindow, sync: &UiSync, pos: Option<f32>) {
    if lyrics_should_project(ui) {
        if !sync.lyrics_projected.get() {
            let pos =
                pos.unwrap_or_else(|| sync.state.read().unwrap().position_now().as_secs_f32());
            project_lyrics(ui, sync, pos);
        }
    } else {
        clear_lyrics_projection(ui, sync);
    }
}

fn set_queue_projection(ui: &MainWindow, controls: &Controls, visible: bool) {
    if !visible {
        ui.global::<DisplayGlobal>().set_upcoming(Default::default());
    }
    controls.send(Cmd::SetQueueVisible(visible));
}

fn apply_cover_palette(ui: &MainWindow, palette: Option<crate::library::metadata::CoverPalette>) {
    let theme = ui.global::<Theme>();
    let colors = palette.as_ref().map_or(&[][..], |p| p.as_slice());
    if colors.is_empty() {
        theme.set_has_cover_palette(false);
        theme.set_cover_color_count(0);
        return;
    }
    if let Some(&[r, g, b]) = colors.first() {
        theme.set_cover_color_0(slint::Color::from_rgb_u8(r, g, b));
    }
    if let Some(&[r, g, b]) = colors.get(1) {
        theme.set_cover_color_1(slint::Color::from_rgb_u8(r, g, b));
    }
    if let Some(&[r, g, b]) = colors.get(2) {
        theme.set_cover_color_2(slint::Color::from_rgb_u8(r, g, b));
    }
    theme.set_cover_color_count(colors.len() as i32);
    theme.set_has_cover_palette(true);
}

fn clear_hidden_details(ui: &MainWindow, sync: &UiSync) {
    sync.window_hidden.set(true);
    sync.details_cleared.set(true);
    *sync.raw_lyrics.borrow_mut() = Arc::from([]);
    let display = ui.global::<DisplayGlobal>();
    display.set_album_image(slint::Image::default());
    display.set_ambient_image(slint::Image::default());
    display.set_has_ambient_image(false);
}

fn restore_window_details(controls: &Controls, sync: &UiSync) {
    sync.window_hidden.set(false);
    if sync.details_cleared.replace(false) {
        controls.send(Cmd::ReloadDetails);
    }
}

fn apply(ui: &MainWindow, event: Event, sync: &Rc<UiSync>) {
    let display = ui.global::<DisplayGlobal>();
    let playback = ui.global::<PlaybackGlobal>();
    match event {
        Event::State(s, seeked) => {
            display.set_current_song(s.track.as_deref().map(song).unwrap_or_default());
            playback.set_can_seek(s.can_seek());
            playback.set_paused(s.status != Status::Playing);
            playback.set_duration(s.duration.as_secs_f32());
            let pos = seeked.unwrap_or_else(|| s.position_now()).as_secs_f32();
            set_progress_exact(&playback, pos);
            playback.set_volume(s.volume);
            playback.set_loop_mode(s.loop_mode.into());
            playback.set_shuffle(s.shuffle);
            if sync.lyrics_projected.get() {
                sync_active_lyric(ui, &sync.timeline, pos);
            }
            if ui.get_lyrics_visible() && seeked.is_some() {
                ui.invoke_follow_lyrics();
            }
            sync_timers(ui, sync);
        }
        Event::Upcoming(tracks) => {
            if ui.get_queue_visible() && ui.window().is_visible() {
                display.set_upcoming(model(tracks));
            }
        }
        Event::Details {
            details,
            lyrics,
            position,
            cover_changed,
        } => {
            if sync.window_hidden.get() {
                sync.details_cleared.set(true);
                return;
            }
            if cover_changed {
                let cover = details
                    .cover
                    .map(|cover| slint::Image::from_rgba8(cover.pixels))
                    .unwrap_or_else(default_cover);
                display.set_album_image(cover);
                if let Some(ambient) = details.ambient {
                    display.set_ambient_image(slint::Image::from_rgba8(ambient.pixels));
                    display.set_has_ambient_image(true);
                } else {
                    display.set_ambient_image(slint::Image::default());
                    display.set_has_ambient_image(false);
                }
                apply_cover_palette(ui, details.palette);
            }
            if let Some(lyrics) = lyrics {
                *sync.raw_lyrics.borrow_mut() = lyrics;
                if lyrics_should_project(ui) {
                    project_lyrics(ui, sync, position.as_secs_f32());
                } else {
                    clear_lyrics_projection(ui, sync);
                }
            }
            sync_timers(ui, sync);
        }
        Event::Library(dir, key, asc, tracks) => {
            display.set_song_dir(dir.display().to_shared_string());
            display.set_sort_key(key.into());
            display.set_sort_ascending(asc);
            display.set_library_songs(model(tracks));
            log::info!("Library ready");
        }
        Event::Collections(selected, names, tracks, liked) => {
            let collections = ui.global::<CollectionsGlobal>();
            if collections.get_selected() != selected as i32 {
                display.set_gallery_viewport_y(0.);
            }
            collections.set_names(slint::ModelRc::new(slint::VecModel::from(
                std::iter::once(slint::SharedString::default())
                    .chain(names.into_iter().map(Into::into))
                    .collect::<Vec<_>>(),
            )));
            collections.set_selected(selected as i32);
            collections.set_songs(model(tracks));
            collections.set_liked_tracks(slint::ModelRc::new(LikedTracksModel::new(liked)));
        }
        Event::CollectionSaveFailed => ui.invoke_show_collection_save_error(),
        Event::Results(v) => display.set_search_results(model(v)),
        #[cfg(target_os = "linux")]
        Event::Catalog(view) => {
            let lx = ui.global::<OnlineGlobal>();
            lx.set_catalog_source(view.source.into());
            lx.set_catalog_query(view.query.into());
            lx.set_catalog_page(view.page as i32);
            lx.set_catalog_busy(view.busy);
            lx.set_catalog_failed(view.failed);
            lx.set_catalog_next(view.has_next);
            lx.set_catalog_partial_failure(view.partial_failure);
            lx.set_catalog_rows(slint::ModelRc::new(CatalogRowModel(view.rows)));
        }
        #[cfg(target_os = "linux")]
        Event::Script(script, importing) => {
            let lx = ui.global::<OnlineGlobal>();
            lx.set_installed(script.is_some());
            lx.set_enabled(script.as_ref().is_some_and(|s| s.enabled));
            lx.set_importing(importing);
            lx.set_local_playback(script.as_ref().is_some_and(|s| s.supports_local("musicUrl")));
            lx.set_script_summary(script.map(|s| s.summary()).unwrap_or_default().into());
        }
        #[cfg(target_os = "linux")]
        Event::Link(query, title, issue, track) => {
            let lx = ui.global::<OnlineGlobal>();
            lx.set_preview_query(query.into());
            lx.set_preview_detail(title.into());
            lx.set_preview_issue(issue);
            lx.set_preview_track(track.as_ref().map(|t| t.id.0.as_str()).unwrap_or("").into());
            display.set_search_results(Default::default());
        }
        #[cfg(target_os = "linux")]
        Event::Loading(label, paused) => {
            let lx = ui.global::<OnlineGlobal>();
            lx.set_loading(label.into());
            lx.set_loading_paused(paused);
        }
        #[cfg(target_os = "linux")]
        Event::UpdateAlert(alert) => {
            let url = alert.url.unwrap_or_default();
            let message = if url.is_empty() {
                alert.log
            } else {
                format!("{}\n\n{url}", alert.log)
            };
            ui.set_update_message(message.into());
            ui.set_update_url(url.into());
            ui.invoke_show_source_update();
        }
        #[cfg(target_os = "linux")]
        Event::OnlineFailure((issue, detail)) => {
            ui.set_operation_error_message(
                ui.global::<OnlineGlobal>().invoke_issue_text(issue, detail.into()),
            );
            ui.invoke_show_operation_error();
        }
        Event::Error(msg) => {
            ui.set_operation_error_message(msg.into());
            ui.invoke_show_operation_error();
        }
        #[cfg(target_os = "linux")]
        Event::Raise => {
            let _ = ui.show();
            restore_window_details(&sync.controls, sync);
            set_queue_projection(
                ui,
                &sync.controls,
                ui.get_queue_visible() && ui.window().is_visible(),
            );
            sync_lyrics_projection(ui, sync, None);
            sync_visible_progress(ui, &sync.state, &sync.timeline);
            if ui.get_lyrics_visible() {
                ui.invoke_follow_lyrics();
            }
            sync_timers(ui, sync);
        }
        #[cfg(target_os = "linux")]
        Event::Quit => {
            let _ = slint::quit_event_loop();
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod theme_watcher {
    use std::{
        sync::{Arc, Condvar, Mutex},
        thread,
        time::Duration,
    };

    use slint::ComponentHandle;

    use crate::slint_types::{DisplayGlobal, MainWindow};

    struct State {
        visible: bool,
        check_now: bool,
        shutdown: bool,
    }

    pub struct ThemeWatcher {
        state: Arc<(Mutex<State>, Condvar)>,
        handle: Mutex<Option<thread::JoinHandle<()>>>,
    }

    impl ThemeWatcher {
        pub fn start(ui_weak: slint::Weak<MainWindow>, check_immediately: bool) -> Self {
            let state = Arc::new((
                Mutex::new(State {
                    visible: true,
                    check_now: check_immediately,
                    shutdown: false,
                }),
                Condvar::new(),
            ));
            let shared = state.clone();
            let handle = thread::spawn(move || {
                let (lock, cv) = &*shared;
                loop {
                    {
                        let mut guard = lock.lock().unwrap();
                        while !guard.shutdown && !guard.visible && !guard.check_now {
                            guard = cv.wait(guard).unwrap();
                        }
                        if guard.shutdown {
                            break;
                        }
                        if guard.check_now {
                            guard.check_now = false;
                        } else {
                            let (g, _) = cv.wait_timeout(guard, Duration::from_secs(2)).unwrap();
                            if g.shutdown {
                                break;
                            }
                            if !g.visible && !g.check_now {
                                continue;
                            }
                        }
                    }
                    let is_light = matches!(dark_light::detect(), Ok(dark_light::Mode::Light));
                    let weak = ui_weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            let display = ui.global::<DisplayGlobal>();
                            if display.get_follow_system_theme()
                                && display.get_light_ui() != is_light
                            {
                                ui.invoke_set_light_theme(is_light);
                            }
                        }
                    });
                }
            });
            Self {
                state,
                handle: Mutex::new(Some(handle)),
            }
        }

        pub fn check_now(&self) {
            let (lock, cv) = &*self.state;
            lock.lock().unwrap().check_now = true;
            cv.notify_one();
        }

        pub fn set_visible(&self, visible: bool) {
            let (lock, cv) = &*self.state;
            lock.lock().unwrap().visible = visible;
            cv.notify_one();
        }

        pub fn stop(&self) {
            let (lock, cv) = &*self.state;
            lock.lock().unwrap().shutdown = true;
            cv.notify_one();
            if let Some(handle) = self.handle.lock().unwrap().take() {
                let _ = handle.join();
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
use theme_watcher::ThemeWatcher;

struct UiSync {
    #[cfg(target_os = "linux")]
    controls: Controls,
    state: SharedState,
    progress_timer: Rc<slint::Timer>,
    lyric_timer: Rc<slint::Timer>,
    timeline: LyricTimeline,
    raw_lyrics: RefCell<Arc<[LyricLine]>>,
    lyrics_projected: Cell<bool>,
    window_hidden: Cell<bool>,
    details_cleared: Cell<bool>,
    #[cfg(not(target_os = "linux"))]
    theme: Rc<ThemeWatcher>,
}

thread_local! {
    static GUI_SYNC: RefCell<Option<Rc<UiSync>>> = const { RefCell::new(None) };
}

fn register_callbacks(
    ui: &MainWindow,
    controls: &Controls,
    can_run_hidden: Arc<AtomicBool>,
    sync: &Rc<UiSync>,
) {
    let collections = ui.global::<CollectionsGlobal>();
    collections.on_contains_track(|tracks, id| {
        if let Some(liked) = tracks.as_any().downcast_ref::<LikedTracksModel>() {
            liked.set.contains(id.as_str())
        } else {
            tracks.iter().any(|track| track == id)
        }
    });
    let tx = controls.clone();
    collections.on_select_collection(move |index| tx.send(Cmd::SelectCollection(index as usize)));
    let tx = controls.clone();
    collections.on_create_collection(move |name| tx.send(Cmd::CreateCollection(name.into())));
    let tx = controls.clone();
    collections.on_toggle_like(move |id| tx.send(Cmd::ToggleLike(TrackId(id.into()))));
    let tx = controls.clone();
    collections.on_add_to_collection(move |index, id| {
        tx.send(Cmd::AddToCollection(index as usize, TrackId(id.into())));
    });
    let tx = controls.clone();
    collections.on_remove_from_collection(move |index, id| {
        tx.send(Cmd::RemoveFromCollection(index as usize, TrackId(id.into())));
    });
    let tx = controls.clone();
    collections.on_play_collection_track(move |index, id, via_lx| {
        tx.send(Cmd::PlayCollectionTrack(index as usize, TrackId(id.into()), via_lx));
    });
    let tx = controls.clone();
    ui.on_play(move |t| tx.send(Cmd::PlayTrack(TrackId(t.id.into()))));
    let tx = controls.clone();
    ui.on_play_queued(move |id| tx.send(Cmd::PlayQueuedTrack(TrackId(id.into()))));
    let tx = controls.clone();
    ui.on_toggle_play(move || tx.send(Cmd::TogglePlayPause));
    let tx = controls.clone();
    ui.on_play_next(move || tx.send(Cmd::Next));
    let tx = controls.clone();
    ui.on_play_prev(move || tx.send(Cmd::Previous));
    let tx = controls.clone();
    let weak = ui.as_weak();
    ui.on_change_progress(move |seconds| {
        if let (Some(ui), Ok(position)) = (weak.upgrade(), Duration::try_from_secs_f32(seconds)) {
            let track = ui.global::<DisplayGlobal>().get_current_song().id;
            if !track.is_empty() {
                tx.send(Cmd::SeekTo(Some(TrackId(track.into())), position));
            }
        }
    });
    let tx = controls.clone();
    ui.on_seek_relative(move |seconds| tx.send(Cmd::SeekRelative(i64::from(seconds) * 1_000_000)));
    let tx = controls.clone();
    ui.on_switch_mode(move |m| tx.send(Cmd::SetLoopMode(m.into())));
    let tx = controls.clone();
    ui.on_set_shuffle(move |s| tx.send(Cmd::SetShuffle(s)));
    let tx = controls.clone();
    ui.on_change_volume(move |v| tx.send(Cmd::SetVolume(v)));
    let tx = controls.clone();
    ui.on_sort_song_list(move |k, a| tx.send(Cmd::Sort(k.into(), a)));
    let tx = controls.clone();
    let search_timer = Rc::new(slint::Timer::default());
    let pending_query = Rc::new(RefCell::new(String::new()));
    ui.on_search_songs(move |q| {
        let query: String = q.into();
        if query.is_empty() {
            search_timer.stop();
            pending_query.borrow_mut().clear();
            tx.send(Cmd::Search(query));
            return;
        }
        *pending_query.borrow_mut() = query;
        let tx = tx.clone();
        let pending = Rc::clone(&pending_query);
        search_timer.start(slint::TimerMode::SingleShot, SEARCH_DEBOUNCE, move || {
            let q = pending.borrow().clone();
            tx.send(Cmd::Search(q));
        });
    });
    let tx = controls.clone();
    #[cfg(target_os = "linux")]
    {
        let lx = ui.global::<OnlineGlobal>();
        lx.set_available(true);
        lx.on_is_link(|q| crate::online::is_link(q.as_str()));
        lx.on_is_blank(|q| q.trim().is_empty());
        let tx = controls.clone();
        lx.on_search_catalog(move |source, q, page| {
            if page > 0 {
                tx.send(Cmd::SearchCatalog(source.into(), q.into(), page as u32));
            }
        });
        let tx = controls.clone();
        lx.on_cancel_catalog(move || tx.send(Cmd::CancelCatalog));
        let tx = controls.clone();
        lx.on_play_catalog(move |id| tx.send(Cmd::PlayCatalog(crate::library::TrackId(id.into()))));
        let tx = controls.clone();
        let weak = ui.as_weak();
        lx.on_import_script(move || {
            let mut dialog = rfd::FileDialog::new().add_filter("JavaScript", &["js"]);
            if let Some(ui) = weak.upgrade() {
                dialog = dialog.set_parent(&ui.window().window_handle());
            }
            if let Some(path) = dialog.pick_file() {
                tx.send(Cmd::ImportScript(path));
            }
        });
        let tx = controls.clone();
        lx.on_play_local_source(move |id| {
            tx.send(Cmd::PlayLocalSource(crate::library::TrackId(id.into())))
        });
        let tx = controls.clone();
        lx.on_enable_script(move |enabled| tx.send(Cmd::EnableScript(enabled)));
        let tx = controls.clone();
        lx.on_play_link(move |link| tx.send(Cmd::PlayLink(link.into())));
        let tx = controls.clone();
        lx.on_set_quality(move |quality| tx.send(Cmd::SetOnlineQuality(quality.into())));
    }
    let weak = ui.as_weak();
    ui.on_select_song_dir(move || {
        let mut dialog = rfd::FileDialog::new();
        if let Some(ui) = weak.upgrade() {
            dialog = dialog.set_parent(&ui.window().window_handle());
        }
        if let Some(dir) = dialog.pick_folder() {
            tx.send(Cmd::Refresh(dir));
        }
    });
    ui.on_set_lang(|lang| {
        if let Err(e) = slint::select_bundled_translation(&lang) {
            log::warn!("Language: {e:?}");
        }
    });
    #[cfg(target_os = "linux")]
    {
        let weak = ui.as_weak();
        ui.on_set_follow_system_theme(move |follow| {
            if let Some(ui) = weak.upgrade() {
                if follow {
                    ui.invoke_follow_system_palette();
                } else {
                    ui.invoke_set_light_theme(ui.global::<DisplayGlobal>().get_light_ui());
                }
            }
        });
    }
    #[cfg(not(target_os = "linux"))]
    {
        let theme = Rc::clone(&sync.theme);
        ui.on_set_follow_system_theme(move |follow| {
            if follow {
                theme.check_now();
            }
        });
    }
    #[cfg(target_os = "linux")]
    let catalog_controls = controls.clone();
    ui.on_page_changed(move |_page| {
        #[cfg(target_os = "linux")]
        if _page != 1 {
            catalog_controls.send(Cmd::AbortCatalogSearch);
        }
    });
    let weak = ui.as_weak();
    let sync_ref = Rc::clone(sync);
    ui.on_lyrics_visibility_changed(move || {
        if let Some(ui) = weak.upgrade() {
            sync_lyrics_projection(&ui, &sync_ref, None);
            if ui.get_lyrics_visible() {
                sync_visible_progress(&ui, &sync_ref.state, &sync_ref.timeline);
                ui.invoke_follow_lyrics();
            }
            sync_timers(&ui, &sync_ref);
        }
    });
    let weak = ui.as_weak();
    let tx = controls.clone();
    ui.on_queue_visibility_changed(move || {
        if let Some(ui) = weak.upgrade() {
            set_queue_projection(&ui, &tx, ui.get_queue_visible() && ui.window().is_visible());
        }
    });
    let weak = ui.as_weak();
    let sync_ref = Rc::clone(sync);
    ui.on_now_playing_visibility_changed(move || {
        if let Some(ui) = weak.upgrade() {
            sync_lyrics_projection(&ui, &sync_ref, None);
            sync_timers(&ui, &sync_ref);
            if !ui.get_now_playing_visible() {
                slint::Timer::single_shot(Duration::from_millis(50), crate::trim_allocator);
            }
        }
    });
    let weak = ui.as_weak();
    let sync_ref = Rc::clone(sync);
    ui.on_now_playing_sliding_changed(move || {
        if let Some(ui) = weak.upgrade() {
            if !ui.get_now_playing_sliding() {
                sync_visible_progress(&ui, &sync_ref.state, &sync_ref.timeline);
            }
            sync_timers(&ui, &sync_ref);
        }
    });
    let state = sync.state.clone();
    let timeline = Rc::clone(&sync.timeline);
    ui.on_prev_lyric_time(move |_lines| {
        let p = state.read().unwrap().position_now().as_secs_f32();
        timeline.borrow().iter().map(|l| l.start).rfind(|t| *t < p - 0.5).unwrap_or(0.)
    });
    let state = sync.state.clone();
    let timeline = Rc::clone(&sync.timeline);
    ui.on_next_lyric_time(move |_lines| {
        let p = state.read().unwrap().position_now().as_secs_f32();
        timeline.borrow().iter().map(|l| l.start).find(|t| *t > p + 0.5).unwrap_or(p)
    });
    ui.on_quit(|| {
        let _ = slint::quit_event_loop();
    });
    let sync_ref = Rc::clone(sync);
    let weak = ui.as_weak();
    let tx = controls.clone();
    ui.window().on_close_requested(move || {
        sync_ref.progress_timer.stop();
        sync_ref.lyric_timer.stop();
        #[cfg(not(target_os = "linux"))]
        sync_ref.theme.set_visible(false);
        if let Some(ui) = weak.upgrade() {
            set_queue_projection(&ui, &tx, false);
            clear_lyrics_projection(&ui, &sync_ref);
        }
        if !can_run_hidden.load(Ordering::Acquire) {
            let _ = slint::quit_event_loop();
        } else {
            if let Some(ui) = weak.upgrade() {
                clear_hidden_details(&ui, &sync_ref);
            }
            slint::Timer::single_shot(Duration::from_millis(150), crate::trim_allocator);
        }
        slint::CloseRequestResponse::HideWindow
    });

    // pure callback to format duration string
    ui.on_format_duration(|dura| {
        format!("{}:{:02}", (dura as u32) / 60, (dura as u32) % 60).to_shared_string()
    });

    ui.on_open_url_in_browser(|url| {
        if let Err(err) = open::that(&url) {
            log::warn!("Could not open browser: {}", err.kind());
        }
    });
}

fn register_tray(ui: &MainWindow, tray: &AppTray, controls: &Controls, sync: &Rc<UiSync>) {
    let weak = ui.as_weak();
    let tx = controls.clone();
    let sync_ref = Rc::clone(sync);
    tray.on_show_window(move || {
        if let Some(ui) = weak.upgrade() {
            #[cfg(not(target_os = "linux"))]
            ui.window().set_minimized(false);
            let _ = ui.show();
            restore_window_details(&tx, &sync_ref);
            set_queue_projection(&ui, &tx, ui.get_queue_visible() && ui.window().is_visible());
            #[cfg(not(target_os = "linux"))]
            sync_ref.theme.set_visible(true);
            sync_lyrics_projection(&ui, &sync_ref, None);
            sync_visible_progress(&ui, &sync_ref.state, &sync_ref.timeline);
            if ui.get_lyrics_visible() {
                ui.invoke_follow_lyrics();
            }
            sync_timers(&ui, &sync_ref);
        }
    });
    let tx = controls.clone();
    tray.on_toggle_play(move || tx.send(Cmd::TogglePlayPause));
    let tx = controls.clone();
    tray.on_stop(move || tx.send(Cmd::Stop));
    let tx = controls.clone();
    tray.on_next(move || tx.send(Cmd::Next));
    let tx = controls.clone();
    tray.on_previous(move || tx.send(Cmd::Previous));
    let tx = controls.clone();
    let weak = ui.as_weak();
    tray.on_rescan_songs(move || {
        if let Some(ui) = weak.upgrade() {
            tx.send(Cmd::Refresh(ui.global::<DisplayGlobal>().get_song_dir().as_str().into()));
        }
    });
    tray.on_exit_app(|| {
        let _ = slint::quit_event_loop();
    });
}

#[cfg(target_os = "linux")]
fn select_renderer() -> Result<(), slint::PlatformError> {
    let backend = std::env::var("SLINT_BACKEND").unwrap_or_default();
    let winit = || slint::BackendSelector::new().backend_name("winit".into());
    match backend.to_ascii_lowercase().as_str() {
        "" | "winit" | "femtovg" | "winit-femtovg" => {
            winit().renderer_name("femtovg".into()).require_opengl_es().select()
        }
        "sw" | "software" | "winit-sw" | "winit-software" => {
            winit().renderer_name("software".into()).select()
        }
        _ => Err(format!(
            "Unsupported SLINT_BACKEND value {backend:?}. Use winit-femtovg or winit-software."
        )
        .into()),
    }
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let start = Instant::now();
    let cfg = Config::load();
    #[cfg(target_os = "linux")]
    select_renderer()?;
    let ui = MainWindow::new()?;
    #[cfg(target_os = "linux")]
    slint::set_xdg_app_id(crate::APP_ID)?;
    ui.set_version(env!("CARGO_PKG_VERSION").into());
    ui.set_current_page(cfg.current_page);
    ui.set_sidebar_expanded(cfg.sidebar_expanded);
    let display = ui.global::<DisplayGlobal>();
    display.set_lang(cfg.lang.as_str().into());
    let _ = slint::select_bundled_translation(&cfg.lang);
    display.set_song_dir(cfg.song_dir.display().to_shared_string());
    display.set_follow_system_theme(cfg.follow_system_theme);
    ui.global::<Theme>().set_cover_recoloring(cfg.cover_recoloring);
    #[cfg(target_os = "linux")]
    {
        let lx = ui.global::<OnlineGlobal>();
        lx.set_quality(cfg.online_quality.as_str().into());
        lx.set_search_source(cfg.online_search_source.as_str().into());
    }
    #[cfg(target_os = "linux")]
    if cfg.follow_system_theme {
        ui.invoke_follow_system_palette();
    } else {
        ui.invoke_set_light_theme(cfg.light_ui);
    }
    #[cfg(not(target_os = "linux"))]
    ui.invoke_set_light_theme(cfg.light_ui);
    display.set_app_font_family(
        if cfg!(windows) {
            "Microsoft YaHei UI"
        } else if cfg!(target_os = "macos") {
            "PingFang SC"
        } else {
            "Noto Sans CJK SC"
        }
        .into(),
    );
    ui.global::<PlaybackGlobal>().set_paused(true);
    let tray = AppTray::new().map_err(|e| log::warn!("Tray unavailable: {e}")).ok();
    let (controls, rx) = Controls::channel();
    let shared = SharedState::default();
    let can_run_hidden = Arc::new(AtomicBool::new(tray.is_some()));
    #[cfg(not(target_os = "linux"))]
    let theme = Rc::new(ThemeWatcher::start(ui.as_weak(), cfg.follow_system_theme));
    let sync = Rc::new(UiSync {
        #[cfg(target_os = "linux")]
        controls: controls.clone(),
        state: shared.clone(),
        progress_timer: Rc::new(slint::Timer::default()),
        lyric_timer: Rc::new(slint::Timer::default()),
        timeline: Rc::new(RefCell::new(Vec::new())),
        raw_lyrics: RefCell::new(Arc::from([])),
        lyrics_projected: Cell::new(false),
        window_hidden: Cell::new(false),
        details_cleared: Cell::new(false),
        #[cfg(not(target_os = "linux"))]
        theme: Rc::clone(&theme),
    });
    GUI_SYNC.with(|cell| *cell.borrow_mut() = Some(Rc::clone(&sync)));
    register_callbacks(&ui, &controls, can_run_hidden.clone(), &sync);
    if let Some(tray) = &tray {
        register_tray(&ui, tray, &controls, &sync);
    }
    #[cfg(target_os = "linux")]
    let (desktop, service) =
        crate::integrations::mpris::start(controls.clone(), shared.clone(), can_run_hidden);
    let weak = ui.as_weak();
    let mut scroll_y = Some(cfg.gallery_viewport_y);
    let worker = player::start(cfg, shared, rx, move |event| {
        #[cfg(target_os = "linux")]
        if let Event::State(s, seeked) = &event {
            let _ = desktop.send((Arc::clone(s), *seeked));
        }
        let restore = if matches!(event, Event::Collections(..)) {
            scroll_y.take()
        } else {
            None
        };
        let weak = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let sync = GUI_SYNC.with(|cell| cell.borrow().clone());
            if let Some(sync) = sync
                && let Some(ui) = weak.upgrade()
            {
                apply(&ui, event, &sync);
                if let Some(y) = restore {
                    ui.global::<DisplayGlobal>().set_gallery_viewport_y(y);
                }
            }
        });
    });
    ui.show()?;
    log::info!("Window ready in {:?}", start.elapsed());
    let run = slint::run_event_loop_until_quit();
    if let Err(err) = ui.hide() {
        log::warn!("Hiding window on exit: {err}");
    }
    controls.send(Cmd::Shutdown);
    let mut cfg = worker.join().map_err(|_| "Playback worker failed")?;
    #[cfg(target_os = "linux")]
    if service.join().is_err() {
        log::warn!("MPRIS worker failed");
    }
    sync.progress_timer.stop();
    sync.lyric_timer.stop();
    #[cfg(not(target_os = "linux"))]
    theme.stop();
    GUI_SYNC.with(|cell| cell.borrow_mut().take());
    cfg.lang = display.get_lang().into();
    cfg.light_ui = display.get_light_ui();
    cfg.follow_system_theme = display.get_follow_system_theme();
    cfg.cover_recoloring = ui.global::<Theme>().get_cover_recoloring();
    cfg.current_page = ui.get_current_page();
    cfg.sidebar_expanded = ui.get_sidebar_expanded();
    cfg.gallery_viewport_y = display.get_gallery_viewport_y();
    #[cfg(target_os = "linux")]
    {
        let lx = ui.global::<OnlineGlobal>();
        let quality: String = lx.get_quality().into();
        if crate::online::is_quality(&quality) {
            cfg.online_quality = quality;
        }
        let source: String = lx.get_search_source().into();
        if crate::online::is_search_source(&source) {
            cfg.online_search_source = source;
        }
    }
    cfg.save()?;
    run?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        KARAOKE_TICK_MS, LYRIC_MAX_WAIT_MS, LineTiming, LyricCursor, WordSpan, find_active_lyric,
        min_progress_step, next_lyric_delay, resolve_lyric_cursor,
    };

    fn plain_line(start: f32, end: f32) -> LineTiming {
        LineTiming {
            start,
            end,
            words: Vec::new(),
            sweep_end: start,
        }
    }

    fn timed_line(start: f32, end: f32, words: &[(f32, f32)]) -> LineTiming {
        let words: Vec<_> = words
            .iter()
            .map(|&(s, e)| WordSpan {
                start: s,
                end: e,
            })
            .collect();
        let sweep_end = words.iter().map(|w| w.end).fold(start, f32::max);
        LineTiming {
            start,
            end,
            words,
            sweep_end,
        }
    }

    #[test]
    fn find_active_lyric_handles_boundaries_gaps_and_bilingual_duplicates() {
        let timeline = vec![
            plain_line(1.0, 1.0),
            plain_line(1.0, 3.0),
            plain_line(3.0, 5.0),
            plain_line(8.0, 108.0),
        ];
        assert_eq!(find_active_lyric(&[], 1.0), -1);
        assert_eq!(find_active_lyric(&timeline, 0.5), -1);
        assert_eq!(find_active_lyric(&timeline, 1.0), 1);
        assert_eq!(find_active_lyric(&timeline, 2.5), 1);
        assert_eq!(find_active_lyric(&timeline, 3.0), 2);
        assert_eq!(find_active_lyric(&timeline, 6.0), -1);
        assert_eq!(find_active_lyric(&timeline, 8.0), 3);
        assert_eq!(find_active_lyric(&timeline, 108.0), -1);
    }

    #[test]
    fn resolve_lyric_cursor_and_next_delay_sleep_between_plain_lines_and_sweep_only_active_word() {
        let timeline = vec![plain_line(1.0, 4.0), timed_line(5.0, 9.0, &[(5.2, 5.6), (5.8, 6.2)])];

        // Intro before first line sleeps until 1.0s (capped to LYRIC_MAX_WAIT_MS).
        let intro = resolve_lyric_cursor(&timeline, 0.5);
        assert_eq!(
            intro,
            LyricCursor {
                line_index: -1,
                word_index: -1,
                word_progress: 0.0,
                sweeping: false
            }
        );
        assert_eq!(next_lyric_delay(&timeline, 0.5, intro), Some(Duration::from_millis(500)));

        // Plain LRC line never enters karaoke sweep mode.
        let plain = resolve_lyric_cursor(&timeline, 1.2);
        assert_eq!(
            plain,
            LyricCursor {
                line_index: 0,
                word_index: -1,
                word_progress: 0.0,
                sweeping: false
            }
        );
        assert_eq!(
            next_lyric_delay(&timeline, 1.2, plain),
            Some(Duration::from_millis(LYRIC_MAX_WAIT_MS))
        );
        assert_eq!(next_lyric_delay(&timeline, 3.7, plain), Some(Duration::from_millis(300)));

        // Timed line before first word sleeps until first word starts at 5.2s.
        let pre_word = resolve_lyric_cursor(&timeline, 5.0);
        assert_eq!(
            pre_word,
            LyricCursor {
                line_index: 1,
                word_index: -1,
                word_progress: 0.0,
                sweeping: false
            }
        );
        assert_eq!(next_lyric_delay(&timeline, 5.0, pre_word), Some(Duration::from_millis(200)));

        // Active word sweep ticks at KARAOKE_TICK_MS.
        let mid_word = resolve_lyric_cursor(&timeline, 5.4);
        assert_eq!(mid_word.line_index, 1);
        assert_eq!(mid_word.word_index, 0);
        assert!((mid_word.word_progress - 0.5).abs() < 1e-4);
        assert!(mid_word.sweeping);
        assert_eq!(
            next_lyric_delay(&timeline, 5.4, mid_word),
            Some(Duration::from_millis(KARAOKE_TICK_MS))
        );

        // Gap between words stops sweeping and sleeps until the next word at 5.8s.
        let between_words = resolve_lyric_cursor(&timeline, 5.65);
        assert_eq!(
            between_words,
            LyricCursor {
                line_index: 1,
                word_index: 0,
                word_progress: 1.0,
                sweeping: false
            }
        );
        assert_eq!(
            next_lyric_delay(&timeline, 5.65, between_words),
            Some(Duration::from_millis(150))
        );

        // After the last word finishes sweeping, marks all words sung and sleeps until line end.
        let post_sweep = resolve_lyric_cursor(&timeline, 6.5);
        assert_eq!(
            post_sweep,
            LyricCursor {
                line_index: 1,
                word_index: 2,
                word_progress: 1.0,
                sweeping: false
            }
        );
        assert_eq!(
            next_lyric_delay(&timeline, 6.5, post_sweep),
            Some(Duration::from_millis(LYRIC_MAX_WAIT_MS))
        );

        // Past the final line end: no timer needed.
        let past_end = resolve_lyric_cursor(&timeline, 9.5);
        assert_eq!(next_lyric_delay(&timeline, 9.5, past_end), None);
    }

    #[test]
    fn min_progress_step_scales_with_duration_and_bar_width() {
        assert!((min_progress_step(240.0, true) - (240.0 / 324.0)).abs() < 1e-4);
        assert!((min_progress_step(240.0, false) - (240.0 / 520.0)).abs() < 1e-4);
        assert!((min_progress_step(30.0, true) - 0.2).abs() < 1e-4);
        assert!((min_progress_step(600.0, true) - 1.0).abs() < 1e-4);
    }

    #[test]
    fn proxy_models_project_rows_lazily_and_lookup_likes_in_constant_time() {
        use std::{path::PathBuf, sync::Arc};

        use slint::Model;

        use super::{LikedTracksModel, TrackListModel, song};
        use crate::library::{Track, TrackId, TrackOrigin};

        let mut t = Track::new(
            TrackId("/dev/sealmusic/track/local_1".into()),
            TrackOrigin::Local(PathBuf::from("/music/a.flac")),
            "Title".into(),
            "Artist".into(),
            Duration::from_secs(125),
        );
        t.album = "Album".into();
        let track = Arc::new(t);

        let full = song(&track);
        assert_eq!(full.album.as_str(), "Album");
        assert_eq!(full.duration.as_str(), "02:05");

        let list = TrackListModel(vec![track.clone()]);
        assert_eq!(list.row_count(), 1);
        let row = list.row_data(0).unwrap();
        assert_eq!(row.id.as_str(), "/dev/sealmusic/track/local_1");
        assert_eq!(row.song_name.as_str(), "Title");
        assert_eq!(row.singer.as_str(), "Artist");
        assert_eq!(row.album.as_str(), "");
        assert_eq!(row.duration.as_str(), "02:05");
        assert!(row.is_local);

        let liked = LikedTracksModel::new(vec![track.id.clone()]);
        assert_eq!(liked.row_count(), 1);
        assert_eq!(liked.row_data(0).unwrap().as_str(), track.id.0.as_str());
        assert!(liked.set.contains(track.id.0.as_str()));
        assert!(!liked.set.contains("missing"));
    }
}
