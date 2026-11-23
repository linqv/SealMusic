use std::{
    cell::Cell,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use slint::{Model, ToSharedString, winit_030::WinitWindowAccessor};

use crate::{
    config::Config,
    library::Track,
    player::{
        self,
        command::{Controls, PlayerCommand as Cmd},
        state::{PlaybackEvent as Event, SharedState, Status},
    },
    slint_types::*,
};

fn song(t: &Track) -> SongInfo {
    let secs = t.duration.as_secs();
    SongInfo {
        id: t.id.0.as_str().into(),
        song_name: t.title.as_str().into(),
        singer: t.artist.as_str().into(),
        duration: format!("{:02}:{:02}", secs / 60, secs % 60).into(),
    }
}
fn model(tracks: Vec<Arc<Track>>) -> slint::ModelRc<SongInfo> {
    slint::ModelRc::new(slint::VecModel::from(tracks.iter().map(|t| song(t)).collect::<Vec<_>>()))
}
fn apply(ui: &MainWindow, event: Event) {
    let display = ui.global::<DisplayGlobal>();
    let playback = ui.global::<PlaybackGlobal>();
    match event {
        Event::State(s, seeked) => {
            display.set_current_song(s.track.as_deref().map(song).unwrap_or_default());
            playback.set_paused(s.status != Status::Playing);
            playback.set_duration(s.duration.as_secs_f32());
            if !playback.get_dragging() {
                playback.set_progress(seeked.unwrap_or_else(|| s.position_now()).as_secs_f32());
            }
            playback.set_volume(s.volume);
            playback.set_loop_mode(s.loop_mode);
            playback.set_shuffle(s.shuffle);
        }
        Event::Details(details) => {
            let cover = details
                .cover
                .map(|rgba| {
                    slint::Image::from_rgba8(slint::SharedPixelBuffer::clone_from_slice(
                        rgba.as_raw(),
                        rgba.width(),
                        rgba.height(),
                    ))
                })
                .unwrap_or_else(|| {
                    slint::Image::load_from_svg_data(include_bytes!("../ui/cover.svg")).unwrap()
                });
            display.set_album_image(cover);
            let lyrics: Vec<_> = details
                .lyrics
                .into_iter()
                .map(|l| LyricItem {
                    time: l.time,
                    text: l.text.into(),
                    duration: l.duration,
                })
                .collect();
            display.set_lyrics(slint::ModelRc::new(slint::VecModel::from(lyrics)));
            display.set_lyric_viewport_y(0.);
        }
        Event::Library(dir, key, asc, tracks) => {
            display.set_song_dir(dir.display().to_shared_string());
            display.set_sort_key(key);
            display.set_last_sort_key(key);
            display.set_sort_ascending(asc);
            display.set_song_list(model(tracks));
            log::info!("Library ready");
        }
        Event::Results(v) => display.set_search_results(model(v)),
        Event::Error(msg) => {
            ui.set_operation_error_message(msg.into());
            ui.invoke_show_operation_error();
        }
        #[cfg(target_os = "linux")]
        Event::Raise => {
            ui.window().set_minimized(false);
            let _ = ui.show();
        }
        #[cfg(target_os = "linux")]
        Event::Quit => {
            let _ = slint::quit_event_loop();
        }
    }
}
fn is_system_light() -> bool {
    matches!(dark_light::detect(), Ok(dark_light::Mode::Light))
}
fn register_callbacks(ui: &MainWindow, controls: &Controls, can_run_hidden: Arc<AtomicBool>) {
    let tx = controls.clone();
    ui.on_play(move |t| tx.send(Cmd::PlayTrack(crate::library::TrackId(t.id.into()))));
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
    ui.on_search_songs(move |q| tx.send(Cmd::Search(q.into())));
    let tx = controls.clone();
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
    let weak = ui.as_weak();
    ui.on_set_follow_system_theme(move |follow| {
        if follow && let Some(ui) = weak.upgrade() {
            ui.invoke_set_light_theme(is_system_light());
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
    ui.window().on_close_requested(move || {
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

    ui.on_open_url_in_browser(|url| match open::that(&url) {
        Ok(()) => log::info!("Opened '{}' successfully.", url),
        Err(err) => log::warn!("An error occurred when opening '{}': {}", url, err),
    });
}

fn build_theme_timer(ui_weak: slint::Weak<MainWindow>) -> slint::Timer {
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_secs(1), move || {
        if let Some(ui) = ui_weak.upgrade() {
            let display = ui.global::<DisplayGlobal>();
            if display.get_follow_system_theme() {
                let is_light = is_system_light();
                let current = display.get_light_ui();
                if current != is_light {
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
        }
    });
    timer
}

/// Center the window on the current monitor's work area (avoids taskbar/dock).
/// Returns `true` if centering was applied, `false` if the window wasn't ready yet.
fn center_window(ui: &MainWindow) -> bool {
    let result = ui.window().with_winit_window(|w| {
        let window_size = w.inner_size();
        if window_size.width == 0 || window_size.height == 0 {
            return None;
        }
        let monitor = match w.current_monitor() {
            Some(m) => m,
            None => return None,
        };
        let monitor_size = monitor.size();
        let monitor_pos = monitor.position();
        let x = monitor_pos.x + (monitor_size.width as i32 - window_size.width as i32) / 2;
        let y = monitor_pos.y + (monitor_size.height as i32 - window_size.height as i32) / 2;
        Some((x, y))
    });

    if let Some(Some((x, y))) = result {
        ui.window().set_position(slint::PhysicalPosition::new(x, y));
        log::info!("window centered at ({}, {})", x, y);
        true
    } else {
        false
    }
}

fn register_tray(ui: &MainWindow, tray: &AppTray, controls: &Controls) {
    let weak = ui.as_weak();
    tray.on_show_window(move || {
        if let Some(ui) = weak.upgrade() {
            ui.window().set_minimized(false);
            let _ = ui.show();
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

fn progress_timer(ui: &MainWindow, state: SharedState) -> slint::Timer {
    let weak = ui.as_weak();
    let timer = slint::Timer::default();
    let mut last_y = 0.;
    let mut allow_scroll = Instant::now();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(200), move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if !ui.window().is_visible() {
            return;
        }
        let p = ui.global::<PlaybackGlobal>();
        if p.get_paused() || p.get_dragging() {
            return;
        }
        let pos = state.read().unwrap().position_now().as_secs_f32();
        p.set_progress(pos);
        if ui.get_current_page() != 2 {
            return;
        }
        let display = ui.global::<DisplayGlobal>();
        let current_y = display.get_lyric_viewport_y();
        if (last_y - current_y).abs() > 0.5 {
            allow_scroll = Instant::now() + Duration::from_secs(5);
        }
        last_y = current_y;
        if Instant::now() >= allow_scroll
            && let Some((index, _)) = display
                .get_lyrics()
                .iter()
                .enumerate()
                .find(|(_, l)| l.time <= pos && pos - l.time < 0.25)
        {
            last_y = -(index.saturating_sub(5) as f32) * display.get_lyric_line_height();
            display.set_lyric_viewport_y(last_y);
        }
    });
    timer
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
    ui.invoke_set_light_theme(if cfg.follow_system_theme {
        is_system_light()
    } else {
        cfg.light_ui
    });
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
    let (tx, rx) = mpsc::channel();
    let controls = Controls(tx);
    let shared = SharedState::default();
    let can_run_hidden = Arc::new(AtomicBool::new(tray.is_some()));
    register_callbacks(&ui, &controls, can_run_hidden.clone());
    if let Some(tray) = &tray {
        register_tray(&ui, tray, &controls);
    }
    #[cfg(target_os = "linux")]
    let (desktop, service) =
        crate::integrations::mpris::start(controls.clone(), shared.clone(), can_run_hidden);
    let weak = ui.as_weak();
    let mut scroll = Some(cfg.gallery_viewport_y);
    let worker = player::start(cfg, shared.clone(), rx, move |event| {
        #[cfg(target_os = "linux")]
        if let Event::State(s, seeked) = &event {
            let _ = desktop.send((s.clone(), *seeked));
        }
        let restore = if matches!(event, Event::Library(..)) {
            scroll.take()
        } else {
            None
        };
        let weak = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                apply(&ui, event);
                if let Some(y) = restore {
                    ui.global::<DisplayGlobal>().set_gallery_viewport_y(y);
                }
            }
        });
    });
    let _progress = progress_timer(&ui, shared);
    let _theme = build_theme_timer(ui.as_weak());
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
    cfg.lang = display.get_lang().into();
    cfg.light_ui = display.get_light_ui();
    cfg.follow_system_theme = display.get_follow_system_theme();
    cfg.current_page = ui.get_current_page();
    cfg.sidebar_expanded = ui.get_sidebar_expanded();
    cfg.gallery_viewport_y = display.get_gallery_viewport_y();
    cfg.save()?;
    run?;
    Ok(())
}
