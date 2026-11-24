use std::{
    cell::{Cell, OnceCell, RefCell},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use slint::{Model, ToSharedString, winit_030::WinitWindowAccessor};

use crate::{
    config::Config,
    library::{Track, TrackId},
    player::{
        self,
        command::{Controls, PlayerCommand as Cmd},
        state::{PlaybackEvent as Event, SharedState, Status},
    },
    slint_types::*,
};

const SEARCH_DEBOUNCE: Duration = Duration::from_millis(120);

type LyricTimeline = Rc<RefCell<Vec<(f32, f32)>>>;

struct ScrollGuard {
    last_y: Cell<f32>,
    allow_scroll: Cell<Instant>,
}

impl Default for ScrollGuard {
    fn default() -> Self {
        Self {
            last_y: Cell::new(0.),
            allow_scroll: Cell::new(Instant::now()),
        }
    }
}

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

fn find_active_lyric(timeline: &[(f32, f32)], pos: f32) -> i32 {
    let idx = timeline.partition_point(|&(start, _)| start <= pos);
    let Some(i) = idx.checked_sub(1) else {
        return -1;
    };
    let (start, end) = timeline[i];
    if pos >= start && pos < end {
        i as i32
    } else {
        -1
    }
}

fn sync_lyrics_view(
    ui: &MainWindow,
    timeline: &LyricTimeline,
    pos: f32,
    scroll: &ScrollGuard,
    force_scroll: bool,
) {
    let display = ui.global::<DisplayGlobal>();
    let current_y = display.get_lyric_viewport_y();
    if (scroll.last_y.get() - current_y).abs() > 0.5 {
        scroll.allow_scroll.set(Instant::now() + Duration::from_secs(5));
    }
    scroll.last_y.set(current_y);

    let active = find_active_lyric(&timeline.borrow(), pos);
    let prev = display.get_active_lyric_index();
    if active != prev {
        display.set_active_lyric_index(active);
    }
    if active >= 0
        && (active != prev || force_scroll)
        && Instant::now() >= scroll.allow_scroll.get()
    {
        let next_y =
            -((active as usize).saturating_sub(5) as f32) * display.get_lyric_line_height();
        scroll.last_y.set(next_y);
        display.set_lyric_viewport_y(next_y);
    }
}

fn sync_visible_progress(
    ui: &MainWindow,
    state: &SharedState,
    timeline: &LyricTimeline,
    scroll: &ScrollGuard,
    force_scroll: bool,
) {
    let page = ui.get_current_page();
    if page != 0 && page != 2 {
        return;
    }
    let playback = ui.global::<PlaybackGlobal>();
    if playback.get_dragging() {
        return;
    }
    let pos = state.read().unwrap().position_now().as_secs_f32();
    playback.set_progress(pos);
    if page == 2 {
        sync_lyrics_view(ui, timeline, pos, scroll, force_scroll);
    }
}

fn sync_progress_timer(
    ui: &MainWindow,
    timer: &Rc<slint::Timer>,
    state: &SharedState,
    timeline: &LyricTimeline,
    scroll: &Rc<ScrollGuard>,
) {
    let should_run = ui.window().is_visible() && !ui.global::<PlaybackGlobal>().get_paused();
    if !should_run {
        timer.stop();
        return;
    }
    if timer.running() {
        return;
    }
    let weak = ui.as_weak();
    let timer_ref = Rc::clone(timer);
    let state = state.clone();
    let timeline = Rc::clone(timeline);
    let scroll = Rc::clone(scroll);
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(200), move || {
        let Some(ui) = weak.upgrade() else {
            timer_ref.stop();
            return;
        };
        if !ui.window().is_visible() || ui.global::<PlaybackGlobal>().get_paused() {
            timer_ref.stop();
            return;
        }
        sync_visible_progress(&ui, &state, &timeline, &scroll, false);
    });
}

fn song(t: &Track) -> SongInfo {
    let secs = t.duration.as_secs();
    SongInfo {
        id: t.id.0.as_str().into(),
        song_name: t.title.as_str().into(),
        singer: t.artist.as_str().into(),
        duration: format!("{:02}:{:02}", secs / 60, secs % 60).into(),
        is_local: t.local_path().is_some(),
    }
}
fn model(tracks: Vec<Arc<Track>>) -> slint::ModelRc<SongInfo> {
    slint::ModelRc::new(slint::VecModel::from(tracks.iter().map(|t| song(t)).collect::<Vec<_>>()))
}
fn apply(ui: &MainWindow, event: Event, sync: &UiSync) {
    let display = ui.global::<DisplayGlobal>();
    let playback = ui.global::<PlaybackGlobal>();
    match event {
        Event::State(s, seeked) => {
            display.set_current_song(s.track.as_deref().map(song).unwrap_or_default());
            playback.set_can_seek(s.can_seek());
            playback.set_paused(s.status != Status::Playing);
            playback.set_duration(s.duration.as_secs_f32());
            let pos = seeked.unwrap_or_else(|| s.position_now()).as_secs_f32();
            if !playback.get_dragging() {
                playback.set_progress(pos);
            }
            playback.set_volume(s.volume);
            playback.set_loop_mode(s.loop_mode);
            playback.set_shuffle(s.shuffle);
            if ui.get_current_page() == 2 {
                sync_lyrics_view(ui, &sync.timeline, pos, &sync.scroll, seeked.is_some());
            } else {
                display.set_active_lyric_index(find_active_lyric(&sync.timeline.borrow(), pos));
            }
            sync_progress_timer(ui, &sync.timer, &sync.state, &sync.timeline, &sync.scroll);
        }
        Event::Details(details, position) => {
            let cover = details
                .cover
                .map(|rgba| {
                    slint::Image::from_rgba8(slint::SharedPixelBuffer::clone_from_slice(
                        rgba.as_raw(),
                        rgba.width(),
                        rgba.height(),
                    ))
                })
                .unwrap_or_else(default_cover);
            display.set_album_image(cover);
            let mut times = Vec::with_capacity(details.lyrics.len());
            let lyrics: Vec<_> = details
                .lyrics
                .into_iter()
                .map(|l| {
                    times.push((l.time, l.time + l.duration));
                    LyricItem {
                        time: l.time,
                        text: l.text.into(),
                    }
                })
                .collect();
            *sync.timeline.borrow_mut() = times;
            display.set_lyrics(slint::ModelRc::new(slint::VecModel::from(lyrics)));
            sync.scroll.last_y.set(0.);
            sync.scroll.allow_scroll.set(Instant::now());
            display.set_lyric_viewport_y(0.);
            let pos = position.as_secs_f32();
            if ui.get_current_page() == 2 {
                sync_lyrics_view(ui, &sync.timeline, pos, &sync.scroll, true);
            } else {
                display.set_active_lyric_index(find_active_lyric(&sync.timeline.borrow(), pos));
            }
        }
        Event::Library(dir, key, asc, tracks) => {
            display.set_song_dir(dir.display().to_shared_string());
            display.set_sort_key(key);
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
            collections.set_liked_tracks(slint::ModelRc::new(slint::VecModel::from(
                liked.into_iter().map(|id| id.0.into()).collect::<Vec<_>>(),
            )));
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
            let rows: Vec<_> = view
                .rows
                .iter()
                .map(|track| crate::slint_types::CatalogRow {
                    song: song(track),
                    source: match &track.origin {
                        crate::library::TrackOrigin::Online(reference) => reference.source().into(),
                        crate::library::TrackOrigin::Local(_) => Default::default(),
                    },
                })
                .collect();
            lx.set_catalog_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
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
            ui.window().set_minimized(false);
            let _ = ui.show();
            sync.theme.set_visible(true);
            sync_visible_progress(ui, &sync.state, &sync.timeline, &sync.scroll, false);
            sync_progress_timer(ui, &sync.timer, &sync.state, &sync.timeline, &sync.scroll);
        }
        #[cfg(target_os = "linux")]
        Event::Quit => {
            let _ = slint::quit_event_loop();
        }
    }
}

#[cfg(target_os = "linux")]
mod theme_watcher {
    use std::{sync::Mutex, thread};

    use futures_channel::mpsc::{self, UnboundedSender};
    use futures_lite::{StreamExt, future};
    use mpris_server::zbus::{
        self,
        zvariant::{OwnedValue, Value},
    };
    use slint::ComponentHandle;

    use crate::slint_types::{DisplayGlobal, MainWindow};

    enum Cmd {
        CheckNow,
        Shutdown,
    }

    pub struct ThemeWatcher {
        tx: UnboundedSender<Cmd>,
        handle: Mutex<Option<thread::JoinHandle<()>>>,
    }

    impl ThemeWatcher {
        pub fn start(ui_weak: slint::Weak<MainWindow>, check_immediately: bool) -> Self {
            let (tx, mut rx) = mpsc::unbounded::<Cmd>();
            if check_immediately {
                let _ = tx.unbounded_send(Cmd::CheckNow);
            }
            let handle = thread::spawn(move || {
                future::block_on(async move {
                    let Ok(conn) = zbus::Connection::session().await else {
                        log::debug!("Theme watcher unavailable: no D-Bus session bus");
                        return;
                    };
                    let Ok(proxy) = zbus::proxy::Builder::<zbus::Proxy<'_>>::new(&conn)
                        .destination("org.freedesktop.portal.Desktop")
                        .and_then(|b| b.path("/org/freedesktop/portal/desktop"))
                        .and_then(|b| b.interface("org.freedesktop.portal.Settings"))
                        .map(|b| b.cache_properties(zbus::proxy::CacheProperties::No))
                    else {
                        return;
                    };
                    let Ok(proxy) = proxy.build().await else {
                        log::debug!("Theme watcher unavailable: portal Settings proxy failed");
                        return;
                    };
                    let Ok(mut signals) = proxy.receive_signal("SettingChanged").await else {
                        log::debug!(
                            "Theme watcher unavailable: SettingChanged subscription failed"
                        );
                        return;
                    };

                    enum Event {
                        Command(Option<Cmd>),
                        Signal(Option<zbus::Message>),
                    }

                    loop {
                        let next = future::or(async { Event::Command(rx.next().await) }, async {
                            Event::Signal(signals.next().await)
                        })
                        .await;
                        match next {
                            Event::Command(Some(Cmd::CheckNow)) => {
                                if let Some(is_light) = read_portal_light(&proxy).await {
                                    apply_theme(&ui_weak, is_light);
                                }
                            }
                            Event::Command(Some(Cmd::Shutdown)) | Event::Command(None) => break,
                            Event::Signal(Some(msg)) => {
                                if let Ok((ns, key, val)) =
                                    msg.body().deserialize::<(String, String, OwnedValue)>()
                                    && ns == "org.freedesktop.appearance"
                                    && key == "color-scheme"
                                    && let Some(is_light) = parse_color_scheme(&val)
                                {
                                    apply_theme(&ui_weak, is_light);
                                }
                            }
                            Event::Signal(None) => break,
                        }
                    }
                });
            });
            Self {
                tx,
                handle: Mutex::new(Some(handle)),
            }
        }

        pub fn check_now(&self) {
            let _ = self.tx.unbounded_send(Cmd::CheckNow);
        }

        pub fn set_visible(&self, _visible: bool) {}

        pub fn stop(&self) {
            let _ = self.tx.unbounded_send(Cmd::Shutdown);
            if let Some(handle) = self.handle.lock().unwrap().take() {
                let _ = handle.join();
            }
        }
    }

    async fn read_portal_light(proxy: &zbus::Proxy<'_>) -> Option<bool> {
        let value = match proxy
            .call::<_, _, OwnedValue>("ReadOne", &("org.freedesktop.appearance", "color-scheme"))
            .await
        {
            Ok(v) => v,
            Err(_) => proxy
                .call::<_, _, OwnedValue>("Read", &("org.freedesktop.appearance", "color-scheme"))
                .await
                .ok()?,
        };
        parse_color_scheme(&value)
    }

    fn parse_color_scheme(value: &OwnedValue) -> Option<bool> {
        let mut current: &Value<'_> = value;
        while let Value::Value(inner) = current {
            current = inner;
        }
        match current {
            Value::U32(scheme) => Some(*scheme == 2),
            _ => None,
        }
    }

    fn apply_theme(ui_weak: &slint::Weak<MainWindow>, is_light: bool) {
        let weak = ui_weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                let display = ui.global::<DisplayGlobal>();
                if display.get_follow_system_theme() && display.get_light_ui() != is_light {
                    ui.invoke_set_light_theme(is_light);
                    log::info!(
                        "system theme changed to: {}",
                        if is_light {
                            "light"
                        } else {
                            "dark"
                        }
                    );
                }
            }
        });
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

use theme_watcher::ThemeWatcher;

struct UiSync {
    state: SharedState,
    timer: Rc<slint::Timer>,
    timeline: LyricTimeline,
    scroll: Rc<ScrollGuard>,
    theme: Rc<ThemeWatcher>,
}

thread_local! {
    static GUI_SYNC: RefCell<Option<Rc<UiSync>>> = const { RefCell::new(None) };
}

fn register_callbacks(
    ui: &MainWindow,
    controls: &Controls,
    can_run_hidden: Arc<AtomicBool>,
    sync: &UiSync,
) {
    let collections = ui.global::<CollectionsGlobal>();
    collections.on_contains_track(|tracks, id| tracks.iter().any(|track| track == id));
    let tx = controls.clone();
    collections.on_select_collection(move |index| tx.send(Cmd::SelectCollection(index as usize)));
    let tx = controls.clone();
    collections.on_create_collection(move |name| tx.send(Cmd::CreateCollection(name.into())));
    let tx = controls.clone();
    collections.on_toggle_like(move |id| tx.send(Cmd::ToggleLike(TrackId(id.into()))));
    let weak = ui.as_weak();
    collections.on_choose_collection(move |id| {
        if let Some(ui) = weak.upgrade() {
            ui.invoke_show_collection_picker(id);
        }
    });
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
    ui.on_toggle_play(move || tx.send(Cmd::TogglePlayPause));
    let tx = controls.clone();
    ui.on_stop(move || tx.send(Cmd::Stop));
    let tx = controls.clone();
    ui.on_play_next(move || tx.send(Cmd::Next));
    let tx = controls.clone();
    ui.on_play_prev(move || tx.send(Cmd::Previous));
    let tx = controls.clone();
    ui.on_change_progress(move |s| {
        if let Ok(d) = Duration::try_from_secs_f32(s) {
            tx.send(Cmd::SeekTo(None, d));
        }
    });
    let tx = controls.clone();
    ui.on_seek_relative(move |seconds| tx.send(Cmd::SeekRelative(i64::from(seconds) * 1_000_000)));
    let tx = controls.clone();
    ui.on_switch_mode(move |m| tx.send(Cmd::SetLoopMode(m)));
    let tx = controls.clone();
    ui.on_set_shuffle(move |s| tx.send(Cmd::SetShuffle(s)));
    let tx = controls.clone();
    ui.on_change_volume(move |v| tx.send(Cmd::SetVolume(v)));
    let tx = controls.clone();
    ui.on_sort_song_list(move |k, a| tx.send(Cmd::Sort(k, a)));
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
        lx.on_import_script(move || {
            if let Some(path) = rfd::FileDialog::new().add_filter("JavaScript", &["js"]).pick_file()
            {
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
    ui.on_select_song_dir(move || {
        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
            tx.send(Cmd::Refresh(dir));
        }
    });
    ui.on_set_lang(|lang| {
        if let Err(e) = slint::select_bundled_translation(&lang) {
            log::warn!("Language: {e:?}");
        }
    });
    let theme = Rc::clone(&sync.theme);
    ui.on_set_follow_system_theme(move |follow| {
        if follow {
            theme.check_now();
        }
    });
    let weak = ui.as_weak();
    let state = sync.state.clone();
    let timeline = Rc::clone(&sync.timeline);
    let scroll = Rc::clone(&sync.scroll);
    #[cfg(target_os = "linux")]
    let catalog_controls = controls.clone();
    ui.on_page_changed(move |_page| {
        #[cfg(target_os = "linux")]
        if _page != 1 {
            catalog_controls.send(Cmd::AbortCatalogSearch);
        }
        if let Some(ui) = weak.upgrade() {
            sync_visible_progress(&ui, &state, &timeline, &scroll, true);
        }
    });
    ui.on_prev_lyric_time(|p, lines| {
        lines.iter().map(|l| l.time).filter(|t| *t < p - 0.5).last().unwrap_or(0.)
    });
    ui.on_next_lyric_time(|p, lines| {
        lines.iter().map(|l| l.time).find(|t| *t > p + 0.5).unwrap_or(p)
    });
    ui.on_quit(|| {
        let _ = slint::quit_event_loop();
    });
    // window control callbacks
    let ui_weak = ui.as_weak();
    ui.on_window_minimize(move || {
        if let Some(ui) = ui_weak.upgrade() {
            ui.window().set_minimized(true);
        }
    });

    // Position cache for fallback drag — defined before maximize
    // so the handler can reset it when window state changes.
    let init_pos = Rc::new(Cell::new(None::<(i32, i32)>));

    let ui_weak = ui.as_weak();
    let init_pos_max = init_pos.clone();
    ui.on_window_maximize(move || {
        if let Some(ui) = ui_weak.upgrade() {
            let window = ui.window();
            window.set_maximized(!window.is_maximized());
        }
        // Reset position cache — window position changed, so the
        // cached fallback origin from a previous gesture is stale.
        init_pos_max.set(None);
    });
    let weak = ui.as_weak();
    ui.on_window_close(move || {
        if let Some(ui) = weak.upgrade()
            && let Err(e) =
                ui.window().dispatch_event_with_result(slint::platform::WindowEvent::CloseRequested)
        {
            log::warn!("Closing window: {e}");
        }
    });
    let timer = Rc::clone(&sync.timer);
    let theme = Rc::clone(&sync.theme);
    ui.window().on_close_requested(move || {
        timer.stop();
        theme.set_visible(false);
        if !can_run_hidden.load(Ordering::Acquire) {
            let _ = slint::quit_event_loop();
        }
        slint::CloseRequestResponse::HideWindow
    });

    // Window dragging — use native OS move via winit's drag_window()
    let ui_weak = ui.as_weak();
    ui.on_window_drag_delta(move |dx: f32, dy: f32| {
        if let Some(app) = ui_weak.upgrade() {
            // Try native OS drag (works on Wayland, X11, Windows when button is pressed)
            let native_ok =
                app.window().with_winit_window(|w| w.drag_window().is_ok()).unwrap_or(false);

            if native_ok {
                // OS window manager/compositor handles all movement
                init_pos.set(None);
                return;
            }

            // Fallback: manually position the window (non-winit backends)
            let (ix, iy) = init_pos.get().unwrap_or_else(|| {
                let pos = app.window().position();
                init_pos.set(Some((pos.x, pos.y)));
                (pos.x, pos.y)
            });
            app.window().set_position(slint::PhysicalPosition::new(ix + dx as i32, iy + dy as i32));
        }
    });

    // pure callback to format duration string
    ui.on_format_duration(|dura| {
        format!("{:02}:{:02}", (dura as u32) / 60, (dura as u32) % 60).to_shared_string()
    });

    ui.on_open_url_in_browser(|url| {
        if let Err(err) = open::that(&url) {
            log::warn!("Could not open browser: {}", err.kind());
        }
    });
}

/// Center the window on the current monitor's work area (avoids taskbar/dock).
fn center_window(ui: &MainWindow) {
    let result = ui.window().with_winit_window(|w| {
        let window_size = w.inner_size();
        if window_size.width == 0 || window_size.height == 0 {
            return None;
        }
        let monitor = w.current_monitor()?;
        let monitor_size = monitor.size();
        let monitor_pos = monitor.position();
        let x = monitor_pos.x + (monitor_size.width as i32 - window_size.width as i32) / 2;
        let y = monitor_pos.y + (monitor_size.height as i32 - window_size.height as i32) / 2;
        Some((x, y))
    });

    if let Some(Some((x, y))) = result {
        ui.window().set_position(slint::PhysicalPosition::new(x, y));
        log::info!("window centered at ({}, {})", x, y);
    }
}

fn register_tray(ui: &MainWindow, tray: &AppTray, controls: &Controls, sync: &UiSync) {
    let weak = ui.as_weak();
    let state = sync.state.clone();
    let timer = Rc::clone(&sync.timer);
    let timeline = Rc::clone(&sync.timeline);
    let scroll = Rc::clone(&sync.scroll);
    let theme = Rc::clone(&sync.theme);
    tray.on_show_window(move || {
        if let Some(ui) = weak.upgrade() {
            ui.window().set_minimized(false);
            let _ = ui.show();
            theme.set_visible(true);
            sync_visible_progress(&ui, &state, &timeline, &scroll, false);
            sync_progress_timer(&ui, &timer, &state, &timeline, &scroll);
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

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let start = Instant::now();
    let cfg = Config::load();
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
    #[cfg(target_os = "linux")]
    {
        let lx = ui.global::<OnlineGlobal>();
        lx.set_quality(cfg.online_quality.as_str().into());
        lx.set_search_source(cfg.online_search_source.as_str().into());
    }
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
    let theme = Rc::new(ThemeWatcher::start(ui.as_weak(), cfg.follow_system_theme));
    let sync = Rc::new(UiSync {
        state: shared.clone(),
        timer: Rc::new(slint::Timer::default()),
        timeline: Rc::new(RefCell::new(Vec::new())),
        scroll: Rc::new(ScrollGuard::default()),
        theme: Rc::clone(&theme),
    });
    GUI_SYNC.with(|cell| *cell.borrow_mut() = Some(Rc::clone(&sync)));
    register_callbacks(&ui, &controls, can_run_hidden.clone(), &sync);
    if let Some(tray) = &tray {
        register_tray(&ui, tray, &controls, &sync);
    }
    #[cfg(target_os = "linux")]
    let mpris_available = Arc::new(AtomicBool::new(true));
    #[cfg(target_os = "linux")]
    let (desktop, service) = crate::integrations::mpris::start(
        controls.clone(),
        shared.clone(),
        can_run_hidden,
        mpris_available.clone(),
    );
    let weak = ui.as_weak();
    let mut scroll_y = Some(cfg.gallery_viewport_y);
    let worker = player::start(
        cfg,
        shared,
        rx,
        #[cfg(target_os = "linux")]
        mpris_available,
        move |event| {
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
        },
    );
    ui.show()?;
    let weak = ui.as_weak();
    slint::Timer::single_shot(Duration::from_millis(50), move || {
        if let Some(ui) = weak.upgrade() {
            center_window(&ui);
        }
    });
    log::info!("Window ready in {:?}", start.elapsed());
    let run = slint::run_event_loop_until_quit();
    controls.send(Cmd::Shutdown);
    let mut cfg = worker.join().map_err(|_| "Playback worker failed")?;
    #[cfg(target_os = "linux")]
    if service.join().is_err() {
        log::warn!("MPRIS worker failed");
    }
    sync.timer.stop();
    theme.stop();
    GUI_SYNC.with(|cell| cell.borrow_mut().take());
    cfg.lang = display.get_lang().into();
    cfg.light_ui = display.get_light_ui();
    cfg.follow_system_theme = display.get_follow_system_theme();
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
    use super::find_active_lyric;

    #[test]
    fn find_active_lyric_handles_boundaries_gaps_and_bilingual_duplicates() {
        let timeline = vec![(1.0, 1.0), (1.0, 3.0), (3.0, 5.0), (8.0, 108.0)];
        assert_eq!(find_active_lyric(&[], 1.0), -1);
        assert_eq!(find_active_lyric(&timeline, 0.5), -1);
        assert_eq!(find_active_lyric(&timeline, 1.0), 1);
        assert_eq!(find_active_lyric(&timeline, 2.5), 1);
        assert_eq!(find_active_lyric(&timeline, 3.0), 2);
        assert_eq!(find_active_lyric(&timeline, 6.0), -1);
        assert_eq!(find_active_lyric(&timeline, 8.0), 3);
        assert_eq!(find_active_lyric(&timeline, 108.0), -1);
    }
}
