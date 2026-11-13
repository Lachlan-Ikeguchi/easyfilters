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
use iced::{application, time, Alignment, ContentFit, Element, Length, Size, Subscription, Task, Theme};

use crate::settings::{Mode, Shared, Stats};
use crate::config;

pub fn run(shared: Arc<Shared>, config_path: PathBuf) -> iced::Result {
    application(
        move || (App::new(shared.clone(), config_path.clone()), Task::none()),
        App::update,
        App::view,
    )
    .title("EasyFilters")
    .window_size(Size::new(1024.0, 600.0))
    .theme(Theme::Dark)
    .subscription(App::subscription)
    .run()
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
        }
        if let Err(e) = config::save_current(&self.config_path, &self.shared) {
            self.status = format!("config save failed: {e}");
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Tick => {
                if !self.shared.running() {
                    return iced::exit();
                }
                // New preview frame? (take() so the slot does not grow stale.)
                {
                    let mut slot = self.shared.preview.lock().unwrap();
                    if let Some(p) = slot.take() {
                        if p.seq != self.preview_seq {
                            self.preview = Some(Handle::from_rgba(p.width, p.height, p.rgba));
                            self.preview_seq = p.seq;
                        }
                    }
                }
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
                self.stats = *self.shared.stats.lock().unwrap();
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
        time::every(Duration::from_millis(33)).map(|_| Message::Tick)
    }

    fn view(&self) -> Element<'_, Message> {
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
        let status = text(format!(
            "{}  |  {:.1} fps, {} frames, {} drops  |  decode {:.1} / infer {:.1} / composite {:.1} / encode {:.1} ms",
            self.status, st.fps, st.frames, st.drops, st.decode_ms, st.infer_ms, st.composite_ms, st.encode_ms
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
                slider(0.0..=1.0, self.mask_smoothing, Message::SmoothingChanged),
            ]
            .spacing(4),
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
