// Release builds are GUI-only: without this the exe opens a console window behind the
// overlay. Debug builds keep the console so `cargo run` can still print panics.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod loadout_window;

use std::{
    env, fs,
    path::PathBuf,
    sync::{Arc, atomic::Ordering, mpsc::Receiver},
    thread,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Frame, RichText,
    Stroke, TextFormat, ViewportBuilder, text::LayoutJob,
};
use loadout_window::LoadoutWindow;
use warframe_peer_overlay::{
    monitor::{self, MonitorSnapshot, PeerView, WindowRect},
    notify,
    single_instance::SingleInstance,
    tray,
};
use winit::platform::windows::WindowExtWindows;

const OVERLAY_HEIGHT: f32 = 50.0;

/// Every size in this file is authored against a 2560x1440 Warframe client area; the
/// whole UI is scaled by the game's client height relative to this reference so the
/// overlay keeps the same proportions at any resolution.
const REFERENCE_WINDOW_HEIGHT: f32 = 1440.0;
const MIN_UI_SCALE: f32 = 0.2;
const MAX_UI_SCALE: f32 = 4.0;

/// Widest the location line is allowed to be before it starts scrolling instead of growing
/// the peer card. Like every other size here it is in reference-scale points, so it is meant
/// to be tuned by hand against a 2560x1440 client area.
const LOCATION_MAX_WIDTH: f32 = 200.0;
const LOCATION_COLOR: Color32 = Color32::from_rgb(190, 198, 210);
/// Reference-scale points the location line travels per second while scrolling.
const MARQUEE_SPEED: f32 = 15.0;
/// Seconds the location line rests at the start and again at the end of its travel.
const MARQUEE_DWELL: f64 = 2.0;
/// The overlay otherwise repaints twice a second, which would turn the marquee into a
/// slideshow; while any line is scrolling it asks for 10 fps instead.
const MARQUEE_FRAME_INTERVAL: Duration = Duration::from_millis(100);

fn main() -> eframe::Result {
    // Held for the whole process: dropping it would free the name for a second instance.
    let Some(_instance) = SingleInstance::acquire("WarframePeerOverlay") else {
        // There is no console in a release build, so a toast is the only way to explain
        // why double-clicking the exe appeared to do nothing.
        let _ = notify::show(
            "Warframe Peer Overlay",
            "すでに起動しています。終了するにはタスクトレイのアイコンからExitを選択してください。",
        );
        // WinRT hands the toast to the notification platform asynchronously, so exiting
        // straight away drops it before it is ever delivered. The overlay is already
        // running, so a short pause here costs the user nothing.
        thread::sleep(Duration::from_secs(1));
        return Ok(());
    };

    let geo_enabled = !env::args().any(|argument| argument == "--no-geo");
    let options = eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_title("Warframe Peer Overlay")
            .with_inner_size([760.0, OVERLAY_HEIGHT])
            .with_resizable(false)
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top()
            .with_visible(false),
        ..Default::default()
    };

    eframe::run_native(
        "Warframe Peer Overlay",
        options,
        Box::new(move |context| {
            egui_extras::install_image_loaders(&context.egui_ctx);
            configure_fonts(&context.egui_ctx);
            configure_style(&context.egui_ctx);
            let loadout_windows = [LoadoutWindow::compact(), LoadoutWindow::full()];
            let [show_loadouts, show_full_loadouts] =
                loadout_windows.each_ref().map(LoadoutWindow::show_request);
            // The tray owns a thread of its own; see `tray` for why it cannot share this one.
            let egui_ctx = context.egui_ctx.clone();
            tray::spawn(move |command| {
                // The root viewport runs its UI even while the overlay is hidden, so the pass
                // this repaint brings on is the one that opens the window asked for.
                let asked_for = match command {
                    tray::Command::ShowLoadouts => &show_loadouts,
                    tray::Command::ShowFullLoadouts => &show_full_loadouts,
                    tray::Command::Exit => {
                        egui_ctx.send_viewport_cmd_to(
                            egui::ViewportId::ROOT,
                            egui::ViewportCommand::Close,
                        );
                        return;
                    }
                };
                asked_for.store(true, Ordering::Relaxed);
                egui_ctx.request_repaint_of(egui::ViewportId::ROOT);
            });
            Ok(Box::new(OverlayApp {
                updates: monitor::spawn(geo_enabled),
                snapshot: None,
                cards: Vec::new(),
                loadout_windows,
                geo_enabled,
                rendered_once: false,
                native_window_configured: false,
                applied_rect: None,
                visible: false,
                startup_notice_pending: true,
            }))
        }),
    )
}

struct OverlayApp {
    updates: Receiver<MonitorSnapshot>,
    snapshot: Option<MonitorSnapshot>,
    /// Rebuilt only when a snapshot arrives: decoding the flag SVGs and building the layout
    /// jobs every frame would be wasteful now that the marquee raises the repaint rate.
    cards: Vec<PeerCard>,
    /// Windows of their own, but run from this viewport's passes (see `loadout_window`).
    loadout_windows: [LoadoutWindow; 2],
    geo_enabled: bool,
    rendered_once: bool,
    native_window_configured: bool,
    /// Geometry already handed to the window, so the per-frame repositioning does not issue a
    /// `SetWindowPos` on every one of the marquee's frames.
    applied_rect: Option<WindowRect>,
    visible: bool,
    /// Cleared once the monitor reports for the first time, so the "waiting for Warframe"
    /// toast fires at most once per launch instead of on every quit-and-relaunch cycle.
    startup_notice_pending: bool,
}

impl eframe::App for OverlayApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        if self.rendered_once && !self.native_window_configured {
            if let Some(window) = frame.winit_window() {
                window.set_skip_taskbar(true);
                window.set_undecorated_shadow(false);
            }
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::MousePassthrough(true));
            self.native_window_configured = true;
        }
        self.rendered_once = true;
        while let Ok(snapshot) = self.updates.try_recv() {
            if self.startup_notice_pending {
                self.startup_notice_pending = false;
                if !snapshot.warframe_running {
                    // The overlay window stays hidden until Warframe has a window, so a
                    // toast is the only feedback that the launch actually worked.
                    let _ = notify::show("Warframe Peer Overlay", "Warframeの起動を待機中");
                }
            }
            self.cards = peer_cards(&snapshot.peers, self.geo_enabled);
            for window in &mut self.loadout_windows {
                window.set_rows(snapshot.loadouts.clone());
            }
            self.snapshot = Some(snapshot);
        }
        let context = ui.ctx().clone();
        // Run on every pass, whether or not the overlay itself is showing.
        for window in &mut self.loadout_windows {
            window.show(&context);
        }
        context.request_repaint_after(Duration::from_millis(500));
        if let Some(window_rect) = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.window_rect)
        {
            // Drive egui's pixels-per-point from Warframe's resolution rather than the
            // monitor DPI, so fonts, margins and the overlay bar all scale together with
            // the game window. Positions below use the same value because that is what
            // egui-winit applies to viewport commands once the new zoom takes effect.
            let scale = ui_scale(window_rect.height);
            context.set_pixels_per_point(scale);
            // egui-winit turns each of these into an unconditional `SetWindowPos`, so send
            // them only when the game window actually moved rather than on every frame.
            if self.applied_rect != Some(window_rect) {
                let width = window_rect.width as f32 / scale;
                let left = window_rect.left as f32 / scale;
                let top = window_rect.bottom as f32 / scale - OVERLAY_HEIGHT;
                context.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                    width,
                    OVERLAY_HEIGHT,
                )));
                context
                    .send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(left, top)));
                self.applied_rect = Some(window_rect);
            }
        }

        // Only show the overlay once Warframe's window position is known, to avoid flashing
        // it at a stale or default location before the game window is ready.
        let should_show = self
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.window_rect.is_some());
        if should_show != self.visible {
            context.send_viewport_cmd(egui::ViewportCommand::Visible(should_show));
            self.visible = should_show;
            // Re-apply the geometry on the next frame: a window that was hidden when the
            // commands above were processed may not have kept them.
            self.applied_rect = None;
        }
        if !should_show {
            return;
        }

        let monitoring = self
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.status == "EE.logを監視中");
        if monitoring {
            if !self.cards.is_empty() {
                show_compact_peer_panel(ui, &self.cards);
            }
            return;
        }

        let panel = Frame::new()
            .fill(Color32::from_rgba_unmultiplied(10, 14, 20, 225))
            .stroke(Stroke::new(1.0, Color32::from_rgb(194, 163, 87)))
            .corner_radius(CornerRadius::same(6))
            .inner_margin(egui::Margin::symmetric(14, 5));
        panel.show(ui, |ui| {
            ui.set_min_size(ui.available_size());

            let running = self
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.warframe_running);
            let header = header_job(
                self.snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.status.as_str())
                    .unwrap_or("監視を開始しています"),
                running,
            );
            show_centered_job(ui, header);
            ui.add_space(2.0);

            if let Some(snapshot) = &self.snapshot {
                if snapshot.peers.is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.label(
                            RichText::new("分隊ピアを待機中")
                                .color(Color32::from_rgb(170, 178, 190)),
                        );
                    });
                } else {
                    show_centered_peers(ui, &self.cards);
                }
            }
        });
    }
}

fn ui_scale(window_height: i32) -> f32 {
    (window_height as f32 / REFERENCE_WINDOW_HEIGHT).clamp(MIN_UI_SCALE, MAX_UI_SCALE)
}

fn header_job(status: &str, running: bool) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.append(
        "WARFRAME PEERS",
        0.0,
        text_format(14.0, Color32::from_rgb(224, 194, 112)),
    );
    job.append(
        "  |  ",
        0.0,
        text_format(14.0, Color32::from_rgb(100, 108, 120)),
    );
    job.append(
        status,
        0.0,
        text_format(
            14.0,
            if running {
                Color32::from_rgb(92, 200, 142)
            } else {
                Color32::from_rgb(160, 168, 180)
            },
        ),
    );
    job
}

fn show_centered_job(ui: &mut egui::Ui, job: LayoutJob) {
    let width = ui.fonts_mut(|fonts| fonts.layout_job(job.clone()).size().x);
    let left_margin = ((ui.available_width() - width) * 0.5).max(0.0);
    ui.horizontal(|ui| {
        ui.add_space(left_margin);
        ui.label(job);
    });
}

const FLAG_ICON_SIZE: f32 = 14.0;

struct PeerCard {
    flag: Option<(String, Arc<[u8]>)>,
    first_line: LayoutJob,
    second_line: LayoutJob,
}

fn show_compact_peer_panel(ui: &mut egui::Ui, cards: &[PeerCard]) {
    let (content_width, content_height) = peer_content_size(ui, cards);
    let panel_width = content_width + 30.0;
    let panel_height = content_height + 6.0;
    let left_margin = ((ui.available_width() - panel_width) * 0.5).max(0.0);
    let top_margin = ((ui.available_height() - panel_height) * 0.5).max(0.0);

    ui.add_space(top_margin);
    ui.horizontal(|ui| {
        ui.add_space(left_margin);
        Frame::new()
            .fill(Color32::from_rgba_unmultiplied(10, 14, 20, 225))
            .stroke(Stroke::new(1.0, Color32::from_rgb(194, 163, 87)))
            .corner_radius(CornerRadius::same(6))
            .inner_margin(egui::Margin::symmetric(14, 2))
            .show(ui, |ui| show_peer_cards(ui, cards));
    });
}

fn show_centered_peers(ui: &mut egui::Ui, cards: &[PeerCard]) {
    let (content_width, content_height) = peer_content_size(ui, cards);
    let top_margin = ((ui.available_height() - content_height) * 0.5).max(0.0);
    let left_margin = ((ui.available_width() - content_width) * 0.5).max(0.0);
    ui.add_space(top_margin);
    ui.horizontal(|ui| {
        ui.add_space(left_margin);
        show_peer_cards(ui, cards);
    });
}

fn peer_cards(peers: &[PeerView], geo_enabled: bool) -> Vec<PeerCard> {
    peers
        .iter()
        .map(|peer| PeerCard {
            flag: flag_icon_bytes(&peer.country_code),
            first_line: peer_first_line_job(peer),
            second_line: peer_second_line_job(peer, geo_enabled),
        })
        .collect()
}

fn peer_content_size(ui: &mut egui::Ui, cards: &[PeerCard]) -> (f32, f32) {
    let sizes = cards
        .iter()
        .map(|card| {
            let first_line = ui.fonts_mut(|fonts| fonts.layout_job(card.first_line.clone()).size());
            let second_line =
                ui.fonts_mut(|fonts| fonts.layout_job(card.second_line.clone()).size());
            let flag_width = if card.flag.is_some() {
                FLAG_ICON_SIZE + ui.spacing().item_spacing.x
            } else {
                0.0
            };
            // The location line never widens the card past its cap: past that it scrolls.
            let width = flag_width + first_line.x.max(second_line.x.min(LOCATION_MAX_WIDTH));
            let height = first_line.y.max(FLAG_ICON_SIZE) + second_line.y;
            egui::vec2(width, height)
        })
        .collect::<Vec<_>>();
    let width = sizes.iter().map(|size| size.x + 20.0).sum::<f32>()
        + ui.spacing().item_spacing.x * cards.len().saturating_sub(1) as f32;
    let height = sizes.iter().map(|size| size.y + 8.0).fold(0.0, f32::max);
    (width, height)
}

fn show_peer_cards(ui: &mut egui::Ui, cards: &[PeerCard]) {
    let time = ui.input(|input| input.time);
    for card in cards {
        Frame::new()
            .fill(Color32::from_rgba_unmultiplied(28, 35, 46, 235))
            .corner_radius(CornerRadius::same(4))
            .inner_margin(egui::Margin::symmetric(10, 4))
            .show(ui, |ui| {
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        if let Some((uri, bytes)) = &card.flag {
                            ui.add(
                                egui::Image::from_bytes(uri.clone(), bytes.clone())
                                    .fit_to_exact_size(egui::vec2(FLAG_ICON_SIZE, FLAG_ICON_SIZE)),
                            );
                        }
                        ui.label(card.first_line.clone());
                    });
                    show_location_line(ui, &card.second_line, time);
                });
            });
    }
}

/// Draws the location line inside a fixed-width window, scrolling it when the text is wider
/// than `LOCATION_MAX_WIDTH`. The overlay is click-through, so there is no hovering or
/// dragging to reveal the rest of the text — it has to reveal itself.
fn show_location_line(ui: &mut egui::Ui, job: &LayoutJob, time: f64) {
    let galley = ui.fonts_mut(|fonts| fonts.layout_job(job.clone()));
    let text_width = galley.size().x;
    let view_width = text_width.min(LOCATION_MAX_WIDTH);
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(view_width, galley.size().y),
        egui::Sense::hover(),
    );
    if text_width > view_width {
        ui.ctx().request_repaint_after(MARQUEE_FRAME_INTERVAL);
    }
    let offset = marquee_offset(time, text_width, view_width);
    ui.painter_at(rect).galley(
        rect.left_top() - egui::vec2(offset, 0.0),
        galley,
        LOCATION_COLOR,
    );
}

/// How far the location line has travelled left at `time` seconds. The text rests at the
/// start for `MARQUEE_DWELL`, travels at `MARQUEE_SPEED` until its end is flush with the
/// right edge, rests there for another `MARQUEE_DWELL`, then jumps back to the start.
fn marquee_offset(time: f64, text_width: f32, view_width: f32) -> f32 {
    let overflow = text_width - view_width;
    if overflow <= 0.0 {
        return 0.0;
    }

    let travel = f64::from(overflow) / f64::from(MARQUEE_SPEED);
    let phase = time.rem_euclid(MARQUEE_DWELL + travel + MARQUEE_DWELL);
    if phase <= MARQUEE_DWELL {
        0.0
    } else if phase >= MARQUEE_DWELL + travel {
        overflow
    } else {
        ((phase - MARQUEE_DWELL) * f64::from(MARQUEE_SPEED)) as f32
    }
}

fn flag_icon_bytes(country_code: &str) -> Option<(String, Arc<[u8]>)> {
    let data_uri = rs_grid_icons::flag_data_uri(country_code)?;
    let encoded = data_uri.strip_prefix("data:image/svg+xml;base64,")?;
    let bytes = STANDARD.decode(encoded).ok()?;
    Some((format!("bytes://flags/{country_code}.svg"), bytes.into()))
}

fn peer_first_line_job(peer: &PeerView) -> LayoutJob {
    let mut job = LayoutJob::default();
    if peer.is_hosting {
        job.append(
            "❓  ",
            0.0,
            text_format(13.0, Color32::from_rgb(244, 190, 70)),
        );
    }
    if peer.is_host {
        job.append(
            "HOST  ",
            0.0,
            text_format(14.0, Color32::from_rgb(244, 190, 70)),
        );
    }
    job.append(
        &peer.platform,
        0.0,
        text_format(14.0, platform_color(&peer.platform)),
    );
    job.append(
        &format!("  {}", peer.name),
        0.0,
        text_format(14.0, Color32::from_rgb(226, 230, 236)),
    );
    job
}

/// Mirrors the Tailwind platform text colors from the original web overlay.
fn platform_color(platform: &str) -> Color32 {
    match platform {
        "PC" => Color32::from_rgb(96, 165, 250),   // text-blue-400
        "Xbox" => Color32::from_rgb(74, 222, 128), // text-green-400
        "PS" => Color32::from_rgb(147, 197, 253),  // text-blue-300
        "NSW" | "NS2" => Color32::from_rgb(248, 113, 113), // text-red-400
        "iOS" => Color32::from_rgb(209, 213, 219), // text-gray-300
        "And." => Color32::from_rgb(134, 239, 172), // text-green-300
        _ => Color32::from_rgb(156, 163, 175),     // text-wf-text-muted
    }
}

fn peer_second_line_job(peer: &PeerView, geo_enabled: bool) -> LayoutJob {
    let mut job = LayoutJob::default();
    let location = if peer.is_local {
        " ".to_owned()
    } else if peer.is_hosting && !peer.country.is_empty() {
        format!("Relayed via {}", peer.country)
    } else {
        match (peer.region.is_empty(), peer.country.is_empty()) {
            (false, false) => format!("{}, {}", peer.region, peer.country),
            (true, false) => peer.country.clone(),
            (false, true) => peer.region.clone(),
            _ if geo_enabled => "地域を解決中".to_owned(),
            _ => "地域取得OFF".to_owned(),
        }
    };
    job.append(&location, 0.0, text_format(13.0, LOCATION_COLOR));
    job
}

fn text_format(size: f32, color: Color32) -> TextFormat {
    TextFormat {
        font_id: FontId::proportional(size),
        color,
        ..Default::default()
    }
}

fn configure_fonts(context: &egui::Context) {
    let windows_dir = env::var_os("WINDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let font_dir = windows_dir.join("Fonts");
    let japanese_font = [
        "NotoSansJP-VF.ttf",
        "YuGothM.ttc",
        "meiryo.ttc",
        "msgothic.ttc",
    ]
    .iter()
    .find_map(|file_name| fs::read(font_dir.join(file_name)).ok());

    let Some(japanese_font) = japanese_font else {
        return;
    };

    let mut fonts = FontDefinitions::default();
    let font_name = "windows_japanese".to_owned();
    fonts.font_data.insert(
        font_name.clone(),
        Arc::new(FontData::from_owned(japanese_font)),
    );
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .push(font_name.clone());
    }
    context.set_fonts(fonts);
}

fn configure_style(context: &egui::Context) {
    // Both windows are designed dark. Left to follow a light Windows theme, egui would drop
    // the style below and draw the loadout window's scroll bar and tooltips light.
    context.set_theme(egui::Theme::Dark);
    let mut style = (*context.style_of(egui::Theme::Dark)).clone();
    style.visuals.dark_mode = true;
    style.visuals.panel_fill = Color32::TRANSPARENT;
    style.visuals.override_text_color = Some(Color32::from_rgb(226, 230, 236));
    style.spacing.item_spacing = egui::vec2(8.0, 5.0);
    context.set_style_of(egui::Theme::Dark, style);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scales_the_ui_against_the_reference_height() {
        let reference = REFERENCE_WINDOW_HEIGHT as i32;
        assert_eq!(ui_scale(reference), 1.0);
        assert_eq!(ui_scale(reference / 2), 0.5);
        assert_eq!(ui_scale(reference * 2), 2.0);
        // Degenerate window sizes must not collapse the UI to an unusable scale.
        assert_eq!(ui_scale(1), MIN_UI_SCALE);
        assert_eq!(ui_scale(i32::MAX), MAX_UI_SCALE);
    }

    #[test]
    fn holds_scrolls_and_restarts_the_location_marquee() {
        let view_width = LOCATION_MAX_WIDTH;
        // One second of travel, so the phases fall on whole seconds.
        let text_width = view_width + MARQUEE_SPEED;
        let cycle = MARQUEE_DWELL + 1.0 + MARQUEE_DWELL;

        // Text that fits never moves, no matter how long the overlay has been up.
        assert_eq!(marquee_offset(0.0, view_width, view_width), 0.0);
        assert_eq!(marquee_offset(97.5, view_width, view_width), 0.0);

        assert_eq!(marquee_offset(0.0, text_width, view_width), 0.0);
        assert_eq!(marquee_offset(MARQUEE_DWELL, text_width, view_width), 0.0);
        assert_eq!(
            marquee_offset(MARQUEE_DWELL + 0.5, text_width, view_width),
            MARQUEE_SPEED * 0.5
        );
        // Once the end of the text is flush with the right edge it stays there, then the
        // cycle restarts at the beginning rather than scrolling back.
        assert_eq!(
            marquee_offset(MARQUEE_DWELL + 1.0, text_width, view_width),
            MARQUEE_SPEED
        );
        assert_eq!(
            marquee_offset(cycle - 0.1, text_width, view_width),
            MARQUEE_SPEED
        );
        assert_eq!(marquee_offset(cycle, text_width, view_width), 0.0);
        assert_eq!(marquee_offset(cycle * 3.0, text_width, view_width), 0.0);
    }

    #[test]
    fn shows_relay_country_for_hosting_providers() {
        let peer = PeerView {
            country: "United Kingdom".to_owned(),
            region: "England".to_owned(),
            is_hosting: true,
            ..PeerView::default()
        };

        assert_eq!(
            peer_second_line_job(&peer, true).text,
            "Relayed via United Kingdom"
        );
        assert!(peer_first_line_job(&peer).text.starts_with("❓  "));
    }
}
