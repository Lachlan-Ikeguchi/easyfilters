//! iced GUI (milestone 4): a live preview fed from the pipeline's preview
//! slot (RGBA frames straight out of the compositing stage, not re-read from
//! the loopback device) plus controls for every runtime setting. Every change
//! is applied to `Shared` immediately and persisted to the TOML config.
//!
//! Renderer: tiny-skia (software, pure Rust) — see Cargo.toml. 720p30 preview
//! through a single image widget is comfortable on CPU.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use iced::widget::image::Handle;
use iced::widget::{
    button, column, container, image, pick_list, row, scrollable, slider, text, text_input, toggler,
};
use iced::window;
use iced::{daemon, time, Alignment, ContentFit, Element, Length, Size, Subscription, Task, Theme};

use crate::settings::{Mode, Shared, Stats};
use crate::config;

pub fn run(shared: Arc<Shared>, config_path: PathBuf) -> iced::Result {
    // Milestone 6: run as an iced *daemon*. A daemon does not exit when its
    // last window closes, which is what makes a true "hide to tray"
    // possible: hide = `window::close` (the window is destroyed, not
    // minimized), show = `window::open`. winit 0.30 cannot unmap a window
    // on native Wayland (`set_visible` is a no-op and un-minimize is
    // ignored there), so close/reopen is the only real hide on Wayland —
    // and it works on X11 too, so there is a single code path.
    // `--start-minimized` just skips the initial `window::open` (tray only).
    // Close requests are handled via the `close_requests` subscription so
    // they can hide-to-tray instead of exiting (`exit_on_close_request =
    // false` disables the runtime's default exit).
    let start_minimized = shared.snapshot_settings().start_minimized;
    daemon(
        move || {
            let app = App::new(shared.clone(), config_path.clone());
            let task = if start_minimized {
                Task::none()
            } else {
                window::open(window_settings()).1.map(Message::WindowId)
            };
            (app, task)
        },
        App::update,
        App::view,
    )
    .title("EasyFilters")
    .theme(Theme::Dark)
    .subscription(App::subscription)
    .run()
}

/// Settings for every window this app opens (the initial one and any the
/// tray "Show/Hide window" item re-opens).
fn window_settings() -> window::Settings {
    window::Settings {
        size: Size::new(1024.0, 600.0),
        visible: true,
        exit_on_close_request: false,
        ..Default::default()
    }
}

struct App {
    shared: Arc<Shared>,
    config_path: PathBuf,
    // Control state mirroring the shared settings; re-synced on Tick in case
    // the tray changed something.
    enabled: bool,
    mode: Mode,
    blur_strength: f64,
    mask_smoothing: f32,
    gaze_correction: bool,
    gaze_strength: f32,
    // Mirror the whole feed horizontally.
    mirror: bool,
    // Milestone 6: close the window to the tray instead of quitting.
    close_to_tray: bool,
    // Window bookkeeping for hide/show (tray "Show/Hide window").
    window_id: Option<window::Id>,
    window_visible: bool,
    background_text: String,
    // Device controls (applied via "Apply" -> stream restart).
    input_text: String,
    size_text: String,
    fps_text: String,
    // Preview + status.
    preview: Option<Handle>,
    preview_seq: u64,
    stats: Stats,
    status: String,
}

#[derive(Debug, Clone)]
enum Message {
    Tick,
    Toggled(bool),
    ModePicked(Mode),
    BlurChanged(f64),
    SmoothingChanged(f32),
    GazeToggled(bool),
    GazeStrengthChanged(f32),
    MirrorToggled(bool),
    CloseToTrayToggled(bool),
    /// The window was asked to close (WM close button). Milestone 6:
    /// hide to tray when `close_to_tray`, else quit.
    CloseRequested(window::Id),
    /// A window was opened (or resized) — carries its id so the
    /// tray-driven show/hide on Tick can address it.
    WindowId(window::Id),
    /// A window was closed (by our own hide, or by the runtime) — keeps the
    /// tray's show/hide state in sync.
    WindowClosed(window::Id),
    BackgroundChanged(String),
    BackgroundSubmit,
    InputChanged(String),
    SizeChanged(String),
    FpsChanged(String),
    ApplyDevice,
}

impl App {
    fn new(shared: Arc<Shared>, config_path: PathBuf) -> Self {
        let s = shared.snapshot_settings();
        let d = shared.device.read().unwrap().clone();
        Self {
            shared,
            config_path,
            enabled: s.enabled,
            mode: s.mode,
            blur_strength: s.blur_strength,
            mask_smoothing: s.mask_smoothing,
            gaze_correction: s.gaze_correction,
            gaze_strength: s.gaze_strength,
            mirror: s.mirror,
            close_to_tray: s.close_to_tray,
            window_id: None,
            window_visible: !s.start_minimized,
            background_text: s
                .background
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            input_text: d
                .input
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            size_text: format!("{}x{}", d.width, d.height),
            fps_text: d.fps.to_string(),
            preview: None,
            preview_seq: 0,
            stats: Stats::default(),
            status: "starting…".to_string(),
        }
    }

    /// Push the control state into the shared settings and persist it.
    fn apply_and_save(&mut self) {
        {
            let mut s = self.shared.settings.write().unwrap();
            s.enabled = self.enabled;
            s.mode = self.mode;
            s.blur_strength = self.blur_strength;
            s.mask_smoothing = self.mask_smoothing;
            s.gaze_correction = self.gaze_correction;
            s.gaze_strength = self.gaze_strength;
            s.mirror = self.mirror;
            s.close_to_tray = self.close_to_tray;
        }
        if let Err(e) = config::save_current(&self.config_path, &self.shared) {
            self.status = format!("config save failed: {e}");
        }
    }

    /// Hide the window to the tray: destroy it. A real hide means the window
    /// is gone from the screen and taskbar, not minimized — winit 0.30
    /// cannot unmap a Wayland window (`set_visible` is a no-op there and
    /// un-minimize is ignored), so on every platform we `window::close`;
    /// daemon mode keeps the process (and the pipeline) alive with zero
    /// windows, and `show_task` opens a fresh one.
    fn hide_task(&self, id: window::Id) -> Task<Message> {
        window::close(id)
    }

    /// Show the window again: open a fresh one. The previous window was
    /// destroyed by `hide_task` (window ids are never reused), so there is
    /// nothing to close here.
    fn show_task(&self) -> Task<Message> {
        window::open(window_settings()).1.map(Message::WindowId)
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Tick => {
                if !self.shared.running() {
                    return iced::exit();
                }
                // New preview frame? (take() so the slot does not grow stale.)
                // While hidden there is no window to draw into, so drop the
                // frame instead of copying it into a texture — and tell the
                // pipeline to stop building previews at all.
                {
                    let mut slot = self.shared.preview.lock().unwrap();
                    if let Some(p) = slot.take() {
                        if self.window_visible && p.seq != self.preview_seq {
                            tracing::debug!(seq = p.seq, w = p.width, h = p.height, "new preview frame");
                            self.preview = Some(Handle::from_rgba(p.width, p.height, p.rgba));
                            self.preview_seq = p.seq;
                        }
                    }
                }
                self.shared
                    .preview_on
                    .store(self.window_visible, Ordering::Relaxed);
                // Re-sync controls in case the tray toggled something.
                let s = self.shared.snapshot_settings();
                if s.enabled != self.enabled {
                    self.enabled = s.enabled;
                }
                if s.mode != self.mode {
                    self.mode = s.mode;
                }
                if (s.blur_strength - self.blur_strength).abs() > 1e-9 {
                    self.blur_strength = s.blur_strength;
                }
                if (s.mask_smoothing - self.mask_smoothing).abs() > 1e-6 {
                    self.mask_smoothing = s.mask_smoothing;
                }
                if s.gaze_correction != self.gaze_correction {
                    self.gaze_correction = s.gaze_correction;
                }
                if (s.gaze_strength - self.gaze_strength).abs() > 1e-6 {
                    self.gaze_strength = s.gaze_strength;
                }
                if s.mirror != self.mirror {
                    self.mirror = s.mirror;
                }
                if s.close_to_tray != self.close_to_tray {
                    self.close_to_tray = s.close_to_tray;
                }
                self.stats = *self.shared.stats.lock().unwrap();
                // Milestone 6: tray "Show/Hide window" flips `show_window`;
                // apply it as a hide/show on the edge.
                let want_visible = self.shared.show_window.load(Ordering::Relaxed);
                if want_visible != self.window_visible {
                    self.window_visible = want_visible;
                    // Show opens a fresh window (no id needed); hide closes
                    // the current one — after a hide there is no window, so
                    // there is nothing to close.
                    return if want_visible {
                        self.show_task()
                    } else if let Some(id) = self.window_id {
                        self.hide_task(id)
                    } else {
                        Task::none()
                    };
                }
                Task::none()
            }
            Message::CloseRequested(id) => {
                self.window_id = Some(id);
                if self.close_to_tray {
                    // Milestone 6: hide to the tray, keep filtering; the tray
                    // "Show/Hide window" item brings the window back.
                    self.window_visible = false;
                    self.shared.show_window.store(false, Ordering::Relaxed);
                    self.hide_task(id)
                } else {
                    iced::exit()
                }
            }
            Message::WindowId(id) => {
                self.window_id = Some(id);
                Task::none()
            }
            Message::WindowClosed(id) => {
                // The current window is gone (we closed it via hide, or the
                // runtime did) — keep the tray's show/hide toggle in sync.
                // A stale event for an already-replaced window is ignored.
                if self.window_id == Some(id) {
                    self.window_id = None;
                    self.window_visible = false;
                    self.shared.show_window.store(false, Ordering::Relaxed);
                }
                Task::none()
            }
            Message::CloseToTrayToggled(v) => {
                self.close_to_tray = v;
                self.apply_and_save();
                Task::none()
            }
            Message::Toggled(v) => {
                self.enabled = v;
                self.apply_and_save();
                Task::none()
            }
            Message::ModePicked(m) => {
                self.mode = m;
                self.apply_and_save();
                Task::none()
            }
            Message::BlurChanged(v) => {
                self.blur_strength = v;
                self.apply_and_save();
                Task::none()
            }
            Message::SmoothingChanged(v) => {
                self.mask_smoothing = v;
                self.apply_and_save();
                Task::none()
            }
            Message::GazeToggled(v) => {
                self.gaze_correction = v;
                self.apply_and_save();
                Task::none()
            }
            Message::GazeStrengthChanged(v) => {
                self.gaze_strength = v;
                self.apply_and_save();
                Task::none()
            }
            Message::MirrorToggled(v) => {
                self.mirror = v;
                self.apply_and_save();
                Task::none()
            }
            Message::BackgroundChanged(t) => {
                self.background_text = t;
                Task::none()
            }
            Message::BackgroundSubmit => {
                let path = self.background_text.trim().to_string();
                {
                    let mut s = self.shared.settings.write().unwrap();
                    s.background = if path.is_empty() {
                        None
                    } else {
                        Some(PathBuf::from(&path))
                    };
                }
                // The pipeline reloads the image only when this moves.
                self.shared.bg_generation.fetch_add(1, Ordering::Relaxed);
                match config::save_current(&self.config_path, &self.shared) {
                    Ok(()) => {
                        self.status = if path.is_empty() {
                            "background: #003200".to_string()
                        } else {
                            format!("background: {path}")
                        }
                    }
                    Err(e) => self.status = format!("config save failed: {e}"),
                }
                Task::none()
            }
            Message::InputChanged(t) => {
                self.input_text = t;
                Task::none()
            }
            Message::SizeChanged(t) => {
                self.size_text = t;
                Task::none()
            }
            Message::FpsChanged(t) => {
                self.fps_text = t;
                Task::none()
            }
            Message::ApplyDevice => {
                match parse_size(&self.size_text).and_then(|(w, h)| {
                    self.fps_text
                        .trim()
                        .parse::<u32>()
                        .map(|fps| (w, h, fps))
                        .map_err(|_| format!("bad fps {:?}", self.fps_text))
                }) {
                    Ok((w, h, fps)) => {
                        {
                            let mut d = self.shared.device.write().unwrap();
                            let input = self.input_text.trim();
                            d.input = if input.is_empty() {
                                None
                            } else {
                                Some(PathBuf::from(input))
                            };
                            d.width = w;
                            d.height = h;
                            d.fps = fps;
                        }
                        // The pipeline notices this and restarts the stream.
                        self.shared.device_generation.fetch_add(1, Ordering::Relaxed);
                        match config::save_current(&self.config_path, &self.shared) {
                            Ok(()) => {
                                self.status = format!("stream restarting at {w}x{h}@{fps}")
                            }
                            Err(e) => self.status = format!("config save failed: {e}"),
                        }
                    }
                    Err(e) => self.status = e,
                }
                Task::none()
            }
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            time::every(Duration::from_millis(33)).map(|_| Message::Tick),
            // Milestone 6: close requests (handled above, not auto-exit).
            window::close_requests().map(Message::CloseRequested),
            // Window bookkeeping: capture the id of each opened window (open
            // + resize events) and notice when one is closed, so the
            // tray-driven show/hide on Tick can address it.
            window::open_events().map(Message::WindowId),
            window::resize_events().map(|(id, _)| Message::WindowId(id)),
            window::close_events().map(Message::WindowClosed),
        ])
    }

    fn view(&self, _window: window::Id) -> Element<'_, Message> {
        let preview: Element<'_, Message> = match &self.preview {
            Some(handle) => container(
                image(handle.clone())
                    .content_fit(ContentFit::Contain)
                    .width(Length::Fill)
                    .height(Length::Fill),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .into(),
            None => container(text("starting preview…").size(20))
                .center(Length::Fill)
                .width(Length::Fill)
                .into(),
        };

        let st = self.stats;
        // Milestone 6: show the actually-negotiated stream format (the camera
        // may not support the requested WxH/fps).
        let negotiated = match st.negotiated {
            Some((w, h, fps)) => format!("  |  negotiated {w}x{h}@{fps:.0}"),
            None => String::new(),
        };
        let status = text(format!(
            "{}  |  {:.1} fps, {} frames, {} drops{negotiated}  |  decode {:.1} / infer {:.1} / composite {:.1} / gaze {:.1} / encode {:.1} ms",
            self.status, st.fps, st.frames, st.drops, st.decode_ms, st.infer_ms, st.composite_ms, st.gaze_ms, st.encode_ms
        ))
        .size(12);

        let controls = column![
            text("EasyFilters").size(24),
            toggler(self.enabled)
                .label("Filtering enabled")
                .on_toggle(Message::Toggled),
            row![
                text("Mode").width(Length::Fixed(90.0)),
                pick_list(Mode::ALL.to_vec(), Some(self.mode), Message::ModePicked)
            ]
            .spacing(8)
            .align_y(Alignment::Center),
            column![
                text(format!("Blur strength: {:.0}", self.blur_strength)),
                slider(1.0..=40.0, self.blur_strength, Message::BlurChanged),
            ]
            .spacing(4),
            column![
                text(format!("Mask smoothing: {:.2}", self.mask_smoothing)),
                // iced sliders default to step = 1, which on a 0..=1 range
                // snaps the value to exactly 0 or 1 — set a real step.
                slider(0.0..=1.0, self.mask_smoothing, Message::SmoothingChanged).step(0.01f32),
            ]
            .spacing(4),
            toggler(self.close_to_tray)
                .label("Close window to tray (keep filtering)")
                .on_toggle(Message::CloseToTrayToggled),
            toggler(self.gaze_correction)
                .label("Gaze correction (capped pupil shift)")
                .on_toggle(Message::GazeToggled),
            column![
                text(format!("Gaze strength: {:.2}", self.gaze_strength)),
                slider(0.0..=1.0, self.gaze_strength, Message::GazeStrengthChanged).step(0.01f32),
            ]
            .spacing(4),
            toggler(self.mirror)
                .label("Mirror feed (flip horizontally)")
                .on_toggle(Message::MirrorToggled),
            column![
                text("Background image (empty = #003200), Enter to apply:"),
                text_input("/path/to/bg.jpg", &self.background_text)
                    .on_input(Message::BackgroundChanged)
                    .on_submit(Message::BackgroundSubmit),
            ]
            .spacing(4),
            column![
                text("Input camera (empty = auto):"),
                text_input("/dev/video1", &self.input_text).on_input(Message::InputChanged),
                text("Resolution (WxH) and fps, then Apply:"),
                row![
                    text_input("1280x720", &self.size_text).on_input(Message::SizeChanged),
                    text_input("30", &self.fps_text).on_input(Message::FpsChanged),
                    button("Apply").on_press(Message::ApplyDevice),
                ]
                .spacing(8),
            ]
            .spacing(4),
            status,
        ]
        .spacing(12)
        .width(Length::Fixed(360.0));

        row![preview, scrollable(controls)]
            .spacing(12)
            .padding(12)
            .into()
    }
}

/// Parse "1280x720" (also accepts 'X').
fn parse_size(s: &str) -> Result<(u32, u32), String> {
    let (w, h) = s
        .trim()
        .split_once(['x', 'X'])
        .ok_or_else(|| format!("bad resolution {s:?}, want WxH"))?;
    let w: u32 = w
        .trim()
        .parse()
        .map_err(|_| format!("bad width in {s:?}"))?;
    let h: u32 = h
        .trim()
        .parse()
        .map_err(|_| format!("bad height in {s:?}"))?;
    if w == 0 || h == 0 {
        return Err("width/height must be > 0".to_string());
    }
    Ok((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_accepts_wxh() {
        assert_eq!(parse_size("1280x720").unwrap(), (1280, 720));
        assert_eq!(parse_size(" 640X480 ").unwrap(), (640, 480));
    }

    #[test]
    fn parse_size_rejects_garbage() {
        assert!(parse_size("").is_err());
        assert!(parse_size("1280").is_err());
        assert!(parse_size("ax720").is_err());
        assert!(parse_size("0x720").is_err());
        assert!(parse_size("1280x0").is_err());
    }
}
