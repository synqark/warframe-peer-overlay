//! The loadout window: an ordinary desktop window, unlike the overlay, meant to be moved
//! beside the game or onto another monitor. It stays hidden until the tray menu asks for it,
//! and closing it only hides it again, so it comes back where the user left it.
//!
//! One card per player: ours always on top, then each squad member in the order they joined,
//! whose card goes as soon as they leave the squad. A card is a header line (name, mastery
//! rank, platform, quest progress) over an equipment line (warframe, weapons, companion), and
//! each line keeps its columns aligned from card to card.
//!
//! egui's zoom factor is global, and the overlay drives it from the game's resolution (see
//! `ui_scale`). This window belongs to the desktop instead, so every size here goes through
//! `Scale`, which divides that zoom back out: the window follows its own monitor's scaling
//! whatever the game's resolution.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use directories::ProjectDirs;
use eframe::egui::{
    self, Color32, CornerRadius, Frame, Galley, IconData, Margin, ScrollArea, Sense, Stroke,
    TextFormat, ViewportBuilder, ViewportClass, ViewportCommand, ViewportId,
    text::{LayoutJob, TextWrapping},
};
use serde::{Deserialize, Serialize};
use warframe_peer_overlay::{
    loadout::{Item, Loadout},
    monitor::LoadoutView,
    names, tray,
};

use crate::{flag_icon_bytes, platform_color, text_format};

/// What a window shows and the chrome around it. The two differ in nothing else: both take
/// the same rows and run the same way.
#[derive(Clone, Copy)]
struct Layout {
    /// Tells the viewports apart.
    name: &'static str,
    title: &'static str,
    /// The size the window first opens at; the user is free to resize it from there.
    initial_size: [f32; 2],
    min_size: [f32; 2],
    draw: fn(&mut egui::Ui, &[LoadoutView]),
}

/// A card per player, stacked downwards, with all their gear on one line.
const COMPACT: Layout = Layout {
    name: "loadouts",
    title: "Loadouts - Warframe Peer Overlay",
    initial_size: [980.0, 440.0],
    min_size: [360.0, 160.0],
    draw: show_cards,
};

/// Tall cards side by side, listing the mods on every slot. Four of them fit the width this
/// one opens at.
const FULL: Layout = Layout {
    name: "loadouts-full",
    title: "Loadouts (full) - Warframe Peer Overlay",
    initial_size: [1440.0, 900.0],
    min_size: [420.0, 240.0],
    draw: show_full_cards,
};

const BACKGROUND: Color32 = Color32::from_rgb(10, 14, 20);
const CARD_FILL: Color32 = Color32::from_rgb(28, 35, 46);
/// Our own card is framed in the overlay's gold.
const OWN_STROKE: Color32 = Color32::from_rgb(194, 163, 87);
const GOLD_TEXT: Color32 = Color32::from_rgb(224, 194, 112);
const TEXT: Color32 = Color32::from_rgb(226, 230, 236);
const MUTED: Color32 = Color32::from_rgb(130, 138, 150);
const QUEST_DONE: Color32 = Color32::from_rgb(92, 200, 142);
/// Whoever the squad connects through, in the overlay's `HOST` gold.
const HOST_COLOR: Color32 = Color32::from_rgb(244, 190, 70);
/// Where a peer connects from, at the right end of the header line.
const LOCATION: Color32 = Color32::from_rgb(190, 198, 210);
const FLAG_SIZE: f32 = 13.0;
const MOD_COLOR: Color32 = Color32::from_rgb(168, 178, 192);
/// A tall card's content width: four cards, with their margins, the space between them, the
/// window's own margins and room for the scroll bar, fit the 1440 the full window opens at.
const CARD_WIDTH: f32 = 312.0;
/// How far a mod sits in from the item it is installed on.
const MOD_INDENT: f32 = 10.0;
/// Rows of mods each slot keeps room for on a tall card, two mods to a row, whatever the
/// player has on: fourteen for the warframe, ten on the primary and the secondary, twelve on
/// the melee and the companion. The slots then line up from card to card.
const MOD_ROWS: [usize; 5] = [7, 5, 5, 6, 6];
/// Where mods and arcanes live, for those the export does not name.
const MOD_PATH: &str = "/Lotus/Upgrades/Mods/";
const ARCANE_PATH: &str = "CosmeticEnhancers/";

/// The equipment line's slots, in the order it shows them.
const GEAR_SLOTS: [&str; 5] = [
    "フレーム",
    "プライマリ",
    "セカンダリ",
    "近接",
    "コンパニオン",
];
/// The quests a loadout tells about, as `(short label, full name)`.
const QUESTS: [(&str, &str); 2] = [("New War", "The New War"), ("Old Peace", "The Old Peace")];

pub struct LoadoutWindow {
    layout: Layout,
    /// Raised from the tray thread, taken on the next pass.
    show_request: Arc<AtomicBool>,
    /// Fixed once the window first opens: egui patches a window whose builder changes, and a
    /// size worked out again under another zoom would undo the user's resizing.
    builder: Option<ViewportBuilder>,
    visible: bool,
    rows: Vec<LoadoutView>,
    /// Where the window was on the last pass, and what has been written down for it, so a
    /// drag is stored once it comes to rest rather than at every step.
    seen: Option<Placement>,
    stored: Option<Placement>,
}

/// Where a window was last left, in desktop points (the zoom divided back out), so that it
/// opens there again on the next run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Placement {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

impl LoadoutWindow {
    /// A card per player, each player's gear on a single line.
    pub fn compact() -> Self {
        Self::new(COMPACT)
    }

    /// A tall card per player, side by side, listing the mods on every slot.
    pub fn full() -> Self {
        Self::new(FULL)
    }

    fn new(layout: Layout) -> Self {
        let stored = placements().and_then(|file| read(&file, layout.name));
        Self {
            layout,
            show_request: Arc::default(),
            builder: None,
            visible: false,
            rows: Vec::new(),
            seen: None,
            stored,
        }
    }

    /// The flag that brings the window up, for another thread to raise. The UI thread only
    /// looks at it during a pass, so ask for a repaint of the root viewport along with it.
    pub fn show_request(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.show_request)
    }

    pub fn set_rows(&mut self, rows: Vec<LoadoutView>) {
        self.rows = rows;
    }

    /// Runs the window for one pass of the root viewport. Call it on every pass, shown or
    /// not: egui closes for good a viewport it does not hear about for a pass.
    pub fn show(&mut self, context: &egui::Context) {
        let id = ViewportId::from_hash_of(self.layout.name);
        let draw = self.layout.draw;
        if self.show_request.swap(false, Ordering::Relaxed) {
            if self.builder.is_none() {
                self.builder = Some(builder(self.layout, self.stored, Scale::of(context)));
            } else {
                // Back where the user left it, restored if minimized, and in front.
                for command in [
                    ViewportCommand::Visible(true),
                    ViewportCommand::Minimized(false),
                    ViewportCommand::Focus,
                ] {
                    context.send_viewport_cmd_to(id, command);
                }
            }
            self.visible = true;
        }
        // Never asked for yet: no window at all, rather than a hidden one.
        let Some(builder) = self.builder.clone() else {
            return;
        };
        context.show_viewport_immediate(id, builder, |ui, class| {
            // eframe gives every viewport a window of its own. The embedded fallback would
            // squeeze this one into the click-through overlay, so it is not drawn at all.
            if class != ViewportClass::Immediate {
                return;
            }
            // Closing only hides the window, so it keeps its place for next time.
            if ui.ctx().input(|input| input.viewport().close_requested()) {
                ui.ctx().send_viewport_cmd(ViewportCommand::Visible(false));
                self.visible = false;
            }
            if self.visible {
                draw(ui, &self.rows);
            }
        });
        if self.visible {
            self.remember(context, id, Scale::of(context));
        }
    }

    /// Writes down where the user has put the window, once they have stopped moving it, for
    /// the next run to open it there.
    fn remember(&mut self, context: &egui::Context, id: ViewportId, scale: Scale) {
        let placement = context.input_for(id, |input| {
            let viewport = input.viewport();
            let position = viewport.outer_rect?.min;
            let size = viewport.inner_rect?.size();
            let ([x, y], [width, height]) = (
                scale.unscaled([position.x, position.y]),
                scale.unscaled([size.x, size.y]),
            );
            Some(Placement {
                x,
                y,
                width,
                height,
            })
        });
        let Some(placement) = placement else {
            return;
        };
        if self.seen == Some(placement)
            && self.stored != Some(placement)
            && let Some(file) = placements()
        {
            write(&file, self.layout.name, placement);
            self.stored = Some(placement);
        }
        self.seen = Some(placement);
    }
}

/// Both windows' placements live in one file beside the loadouts, keyed by layout name.
fn placements() -> Option<PathBuf> {
    ProjectDirs::from("com", "synqark", "WarframePeerOverlay")
        .map(|dirs| dirs.data_local_dir().join("windows.json"))
}

fn read(file: &Path, name: &str) -> Option<Placement> {
    let stored: HashMap<String, Placement> = serde_json::from_slice(&fs::read(file).ok()?).ok()?;
    stored.get(name).copied()
}

/// Leaves whatever the other window has written down as it is.
fn write(file: &Path, name: &str, placement: Placement) {
    let mut stored: HashMap<String, Placement> = fs::read(file)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    stored.insert(name.to_owned(), placement);
    if let Some(directory) = file.parent()
        && fs::create_dir_all(directory).is_ok()
        && let Ok(json) = serde_json::to_vec_pretty(&stored)
    {
        let _ = fs::write(file, json);
    }
}

fn builder(layout: Layout, stored: Option<Placement>, scale: Scale) -> ViewportBuilder {
    let (rgba, width, height) = tray::icon_rgba();
    let size = stored.map_or(layout.initial_size, |stored| [stored.width, stored.height]);
    let builder = ViewportBuilder::default()
        .with_title(layout.title)
        .with_inner_size(scale.scaled(size))
        .with_min_inner_size(scale.scaled(layout.min_size))
        .with_icon(IconData {
            rgba,
            width,
            height,
        });
    match stored {
        // Trusted as it stands: a window left on a monitor that is now gone opens off screen,
        // and moving it back (or deleting the file) is the way out.
        Some(stored) => builder.with_position(scale.scaled([stored.x, stored.y])),
        None => builder,
    }
}

/// Turns this file's sizes, authored as desktop points, into egui points under whatever zoom
/// the overlay has set.
#[derive(Clone, Copy)]
struct Scale(f32);

impl Scale {
    fn of(context: &egui::Context) -> Self {
        Self(1.0 / context.zoom_factor())
    }

    fn px(self, points: f32) -> f32 {
        points * self.0
    }

    /// Both halves of a size or a position.
    fn scaled(self, [x, y]: [f32; 2]) -> [f32; 2] {
        [self.px(x), self.px(y)]
    }

    /// The other way about: what egui measured, back in desktop points, to be written down.
    fn unscaled(self, [x, y]: [f32; 2]) -> [f32; 2] {
        [x / self.0, y / self.0]
    }

    fn margin(self, x: f32, y: f32) -> Margin {
        Margin::symmetric(self.px(x).round() as i8, self.px(y).round() as i8)
    }

    fn text(self, size: f32, color: Color32) -> TextFormat {
        text_format(self.px(size), color)
    }
}

/// One player's card, laid out.
struct Card {
    is_local: bool,
    header: Vec<Cell>,
    /// Sits at the right end of the header line; `None` when the location is unknown, as it
    /// is for ourselves.
    geo: Option<Geo>,
    /// `None` while the loadout is not captured.
    gear: Option<Vec<Cell>>,
}

struct Cell {
    galley: Arc<Galley>,
    tooltip: Option<String>,
}

/// Where a peer connects from: the text, and the flag of the country it names.
struct Geo {
    galley: Arc<Galley>,
    flag: Option<(String, Arc<[u8]>)>,
}

fn show_cards(ui: &mut egui::Ui, rows: &[LoadoutView]) {
    let scale = Scale::of(ui.ctx());
    let cards: Vec<Card> = rows.iter().map(|row| lay_out(ui, row, scale)).collect();
    let header_widths = column_widths(cards.iter().map(|card| card.header.as_slice()));
    let gear_widths = column_widths(cards.iter().filter_map(|card| card.gear.as_deref()));
    let missing =
        LayoutJob::single_section("ロードアウト未取得".to_owned(), scale.text(13.0, MUTED));

    Frame::new()
        .fill(BACKGROUND)
        .inner_margin(scale.margin(10.0, 10.0))
        .show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(scale.px(16.0), scale.px(4.0));
                for (card, row) in cards.iter().zip(rows) {
                    let frame = Frame::new()
                        .fill(CARD_FILL)
                        .stroke(if card.is_local {
                            Stroke::new(scale.px(1.0), OWN_STROKE)
                        } else {
                            Stroke::NONE
                        })
                        .corner_radius(CornerRadius::same(scale.px(6.0).round() as u8))
                        .inner_margin(scale.margin(12.0, 8.0))
                        .show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            show_header(ui, card, &header_widths, scale);
                            match &card.gear {
                                Some(gear) => show_line(ui, gear, &gear_widths),
                                None => {
                                    ui.label(missing.clone());
                                }
                            }
                        });
                    copy_on_right_click(ui, frame.response.rect, row);
                    ui.add_space(scale.px(4.0));
                }
            });
        });
}

fn lay_out(ui: &egui::Ui, row: &LoadoutView, scale: Scale) -> Card {
    let cell = |(job, tooltip): (LayoutJob, Option<String>)| Cell {
        galley: ui.fonts_mut(|fonts| fonts.layout_job(job)),
        tooltip,
    };
    let header = header(row, scale).into_iter().map(cell).collect();
    let gear = row.loadout.as_ref().map(|loadout| {
        GEAR_SLOTS
            .into_iter()
            .zip(gear(loadout))
            .map(|(slot, (value, details))| cell((slot_job(slot, &value, scale), details)))
            .collect()
    });

    let location = location(row);
    let flag = flag_icon_bytes(&row.country_code);
    let geo = (!location.is_empty() || flag.is_some()).then(|| Geo {
        galley: ui.fonts_mut(|fonts| fonts.layout_job(single(&location, 13.0, LOCATION, scale))),
        flag,
    });

    Card {
        is_local: row.is_local,
        header,
        geo,
        gear,
    }
}

/// The header both layouts open a card with: who the player is, their mastery rank and
/// platform, and how far they are through the two quests a loadout tells about — each with
/// what to say about it on hover.
fn header(row: &LoadoutView, scale: Scale) -> Vec<(LayoutJob, Option<String>)> {
    let mut name = name_job(row, scale);
    if row.is_local && !row.name.is_empty() {
        name.append("自分", scale.px(8.0), scale.text(11.0, MUTED));
    }
    let mut header = vec![
        (name, None),
        (mastery_job(row, scale), None),
        (platform_job(row, scale), None),
    ];
    header.extend(quest_jobs(row, scale));
    header
}

/// The player's name, in gold when the card is ours.
fn name_job(row: &LoadoutView, scale: Scale) -> LayoutJob {
    let color = if row.is_local { GOLD_TEXT } else { TEXT };
    let name = if row.name.is_empty() {
        "自分"
    } else {
        &row.name
    };
    single(name, 17.0, color, scale)
}

/// The mark that a card is ours, for a layout that keeps it apart from the name.
fn own_mark(scale: Scale) -> LayoutJob {
    single("自分", 11.0, MUTED, scale)
}

fn mastery_job(row: &LoadoutView, scale: Scale) -> LayoutJob {
    let mastery = row
        .loadout
        .as_ref()
        .and_then(|loadout| loadout.mastery_rank)
        .map(|rank| format!("MR {rank}"))
        .unwrap_or_default();
    single(&mastery, 14.0, GOLD_TEXT, scale)
}

fn platform_job(row: &LoadoutView, scale: Scale) -> LayoutJob {
    single(&row.platform, 14.0, platform_color(&row.platform), scale)
}

/// Empty unless the squad connects through this player.
fn host_job(row: &LoadoutView, scale: Scale) -> LayoutJob {
    let host = if row.is_host { "HOST" } else { "" };
    single(host, 13.0, HOST_COLOR, scale)
}

/// How far the player is through the two quests a loadout tells about, each with what to say
/// about it on hover. Empty for a loadout not captured yet.
fn quest_jobs(row: &LoadoutView, scale: Scale) -> Vec<(LayoutJob, Option<String>)> {
    let loadout = row.loadout.as_ref();
    let done = [
        loadout.and_then(|loadout| loadout.post_new_war),
        loadout.and_then(|loadout| loadout.post_old_peace),
    ];
    QUESTS
        .into_iter()
        .zip(done)
        .map(|((label, quest), done)| {
            let job = match done {
                Some(true) => single(&format!("✔ {label}"), 13.0, QUEST_DONE, scale),
                Some(false) => single(&format!("✖ {label}"), 13.0, MUTED, scale),
                None => LayoutJob::default(),
            };
            let tooltip = done.map(|done| {
                format!(
                    "{quest}: {}",
                    if done {
                        "クリア済み"
                    } else {
                        "未クリア"
                    }
                )
            });
            (job, tooltip)
        })
        .collect()
}

fn single(text: &str, size: f32, color: Color32, scale: Scale) -> LayoutJob {
    LayoutJob::single_section(text.to_owned(), scale.text(size, color))
}

/// A slot and what fills it, as `フレーム Volt Prime`.
fn slot_job(slot: &str, value: &str, scale: Scale) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.append(slot, 0.0, scale.text(12.0, MUTED));
    job.append(value, scale.px(6.0), scale.text(16.0, TEXT));
    job
}

/// Where the peer connects from, as the card shows it: `region, country`, leaving out
/// whichever the geo lookup did not give — and, where one already spells out the other (a city
/// state, or a country whose ISO name carries the region), the shorter of the two alone.
fn location(row: &LoadoutView) -> String {
    let (region, country) = (row.region.trim(), row.country.trim());
    match (region, country) {
        ("", country) => country.to_owned(),
        (region, "") => region.to_owned(),
        (region, country) if country.contains(region) => region.to_owned(),
        (region, country) if region.contains(country) => country.to_owned(),
        (region, country) => format!("{region}, {country}"),
    }
}

/// The width of each column: its widest cell on any card.
fn column_widths<'a>(lines: impl Iterator<Item = &'a [Cell]>) -> Vec<f32> {
    let mut widths: Vec<f32> = Vec::new();
    for line in lines {
        for (column, cell) in line.iter().enumerate() {
            let width = cell.galley.size().x;
            match widths.get_mut(column) {
                Some(widest) => *widest = widest.max(width),
                None => widths.push(width),
            }
        }
    }
    widths
}

/// One line of cells, each as wide as its column so the columns line up from card to card. A
/// window too narrow for the line wraps it instead.
fn show_line(ui: &mut egui::Ui, cells: &[Cell], widths: &[f32]) {
    ui.horizontal_wrapped(|ui| show_cells(ui, cells, widths));
}

/// The header line, with where the peer connects from pinned to its right end. It takes
/// whatever room the cells leave: too narrow a window pushes it onto the next line.
fn show_header(ui: &mut egui::Ui, card: &Card, widths: &[f32], scale: Scale) {
    ui.horizontal_wrapped(|ui| {
        show_cells(ui, &card.header, widths);
        let Some(geo) = &card.geo else {
            return;
        };
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if let Some((uri, bytes)) = &geo.flag {
                ui.add(
                    egui::Image::from_bytes(uri.clone(), bytes.clone())
                        .fit_to_exact_size(egui::Vec2::splat(scale.px(FLAG_SIZE))),
                );
            }
            let (rect, _) = ui.allocate_exact_size(geo.galley.size(), Sense::hover());
            ui.painter()
                .galley(rect.left_top(), geo.galley.clone(), LOCATION);
        });
    });
}

fn show_cells(ui: &mut egui::Ui, cells: &[Cell], widths: &[f32]) {
    for (cell, &width) in cells.iter().zip(widths) {
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(width, cell.galley.size().y), Sense::hover());
        ui.painter()
            .galley(rect.left_top(), cell.galley.clone(), TEXT);
        if let Some(tooltip) = &cell.tooltip {
            response.on_hover_text(tooltip);
        }
    }
}

/// The tall cards: one per player, side by side, each listing the mods on every slot. It
/// scrolls both ways, since a long list of mods, or a fifth player, outgrows the window.
fn show_full_cards(ui: &mut egui::Ui, rows: &[LoadoutView]) {
    let scale = Scale::of(ui.ctx());
    Frame::new()
        .fill(BACKGROUND)
        .inner_margin(scale.margin(10.0, 10.0))
        .show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            ScrollArea::both().auto_shrink(false).show(ui, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(scale.px(8.0), scale.px(3.0));
                ui.horizontal_top(|ui| {
                    for row in rows {
                        show_full_card(ui, row, scale);
                    }
                });
            });
        });
}

fn show_full_card(ui: &mut egui::Ui, row: &LoadoutView, scale: Scale) {
    let frame = Frame::new()
        .fill(CARD_FILL)
        .stroke(if row.is_local {
            Stroke::new(scale.px(1.0), OWN_STROKE)
        } else {
            Stroke::NONE
        })
        .corner_radius(CornerRadius::same(scale.px(6.0).round() as u8))
        .inner_margin(scale.margin(10.0, 8.0))
        .show(ui, |ui| {
            ui.set_width(scale.px(CARD_WIDTH));
            ui.vertical(|ui| {
                show_full_identity(ui, row, scale);
                show_full_status(ui, row, scale);
                show_full_location(ui, row, scale);
                let Some(loadout) = &row.loadout else {
                    ui.label(single("ロードアウト未取得", 13.0, MUTED, scale));
                    return;
                };
                let slots = GEAR_SLOTS
                    .into_iter()
                    .zip(gear(loadout))
                    .zip(items(loadout))
                    .zip(MOD_ROWS);
                for (((slot, (value, details)), item), rows) in slots {
                    ui.separator();
                    show_full_slot(ui, slot, &value, details, scale);
                    show_full_mods(ui, item, rows, scale);
                }
            });
        });
    copy_on_right_click(ui, frame.response.rect, row);
}

/// A right-click anywhere on a card puts that player's whole loadout on the clipboard, laid
/// out over several lines. The pointer is read straight from the input rather than through a
/// widget over the card: one would take the hovering away from the names under it, and with
/// it everything they have to say.
fn copy_on_right_click(ui: &egui::Ui, card: egui::Rect, row: &LoadoutView) {
    if row.json.is_empty() {
        return;
    }
    let clicked = ui.input(|input| {
        input.pointer.secondary_clicked()
            && input
                .pointer
                .interact_pos()
                .is_some_and(|pointer| card.contains(pointer))
    });
    if clicked {
        ui.ctx().copy_text(row.json.pretty());
    }
}

/// The first line: whose card this is — the host's, ours — on the left, and whose name it
/// carries on the right.
fn show_full_identity(ui: &mut egui::Ui, row: &LoadoutView, scale: Scale) {
    ui.horizontal(|ui| {
        whole(ui, host_job(row, scale));
        if row.is_local {
            whole(ui, own_mark(scale));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            whole(ui, name_job(row, scale));
        });
    });
}

/// The second line: what the player has reached on the left, how far through the quests they
/// are on the right.
fn show_full_status(ui: &mut egui::Ui, row: &LoadoutView, scale: Scale) {
    ui.horizontal(|ui| {
        whole(ui, mastery_job(row, scale));
        whole(ui, platform_job(row, scale));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // Laid down from the right, so the last quest goes first to leave them in order.
            for (job, tooltip) in quest_jobs(row, scale).into_iter().rev() {
                let quest = whole(ui, job);
                if let Some((quest, tooltip)) = quest.zip(tooltip) {
                    quest.on_hover_text(tooltip);
                }
            }
        });
    });
}

/// The third line: the flag, then where the peer connects from. It keeps its height even on
/// our own card, which has nothing to show there, so the slots below still line up with the
/// card beside it.
fn show_full_location(ui: &mut egui::Ui, row: &LoadoutView, scale: Scale) {
    let location = location(row);
    let flag = flag_icon_bytes(&row.country_code);
    ui.horizontal(|ui| {
        let text = ui.fonts_mut(|fonts| {
            fonts
                .layout_job(single("M", 13.0, LOCATION, scale))
                .size()
                .y
        });
        ui.set_min_height(text.max(scale.px(FLAG_SIZE)));
        if let Some((uri, bytes)) = &flag {
            ui.add(
                egui::Image::from_bytes(uri.clone(), bytes.clone())
                    .fit_to_exact_size(egui::Vec2::splat(scale.px(FLAG_SIZE))),
            );
        }
        whole(ui, single(&location, 13.0, LOCATION, scale));
    });
}

/// A slot: what it is against the left edge, what fills it against the right, cut short with
/// an ellipsis where the label leaves the name too little room to stand in.
fn show_full_slot(
    ui: &mut egui::Ui,
    slot: &str,
    value: &str,
    details: Option<String>,
    scale: Scale,
) {
    ui.horizontal(|ui| {
        let label = ui.fonts_mut(|fonts| fonts.layout_job(single(slot, 12.0, MUTED, scale)));
        let room = (ui.available_width() - label.size().x - ui.spacing().item_spacing.x).max(0.0);
        let mut job = single(value, 16.0, TEXT, scale);
        job.wrap = TextWrapping::truncate_at_width(room);
        let name = ui.fonts_mut(|fonts| fonts.layout_job(job));

        let (rect, _) = ui.allocate_exact_size(label.size(), Sense::hover());
        ui.painter().galley(rect.left_top(), label, MUTED);
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(room, name.size().y), Sense::hover());
        let against_the_edge = egui::pos2(rect.right() - name.size().x, rect.top());
        ui.painter().galley(against_the_edge, name, TEXT);
        if let Some(details) = details {
            response.on_hover_text(details);
        }
    });
}

/// The mods installed on an item, two to a row, in a block `rows` tall whatever the player has
/// on, so the slot after it starts level with the one on the card beside it.
fn show_full_mods(ui: &mut egui::Ui, item: &Option<Item>, rows: usize, scale: Scale) {
    let installed = item.as_ref().map(mods).unwrap_or_default();
    for pair in installed.chunks(2) {
        ui.horizontal_top(|ui| {
            ui.add_space(scale.px(MOD_INDENT));
            let column = (ui.available_width() - ui.spacing().item_spacing.x) / 2.0;
            for (name, path) in pair {
                show_mod(ui, column, name, path, scale);
            }
        });
    }
    let empty = rows.saturating_sub(installed.len().div_ceil(2));
    if empty > 0 {
        let row = ui.fonts_mut(|fonts| {
            fonts
                .layout_job(single("M", 12.0, MOD_COLOR, scale))
                .size()
                .y
        });
        let spacing = ui.spacing().item_spacing.y;
        // A row costs its own height and the space above it; the space above this block is
        // already there.
        ui.add_space(empty as f32 * (row + spacing) - spacing);
    }
}

/// A part of a line, left whole: nothing here is long enough to be worth breaking, and an
/// empty one is left out rather than taking up the space between.
fn whole(ui: &mut egui::Ui, job: LayoutJob) -> Option<egui::Response> {
    (!job.text.is_empty())
        .then(|| ui.add(egui::Label::new(job).wrap_mode(egui::TextWrapMode::Extend)))
}

/// One of the two mods a row holds, in a column of its own so that the second lines up all
/// the way down the card. A name too long for its column wraps inside it.
fn show_mod(ui: &mut egui::Ui, column: f32, name: &str, path: &str, scale: Scale) {
    let mut job = single(name, 12.0, MOD_COLOR, scale);
    // Cut short with an ellipsis: running over would cross into the column beside it, and
    // wrapping would cost the block a row it has kept no room for. The whole of it is a hover
    // away.
    job.wrap = TextWrapping::truncate_at_width(column);
    let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(column, galley.size().y), Sense::hover());
    ui.painter().galley(rect.left_top(), galley, MOD_COLOR);
    response.on_hover_text(format!("{name}\n{path}"));
}

/// The mods and arcanes installed on an item, named as the arsenal names them, in the order
/// the game lists them. The cosmetics sharing that list are left out, and one the export does
/// not name keeps the end of its path. The game lists some of them twice over, but nothing can
/// be installed twice, so each is kept once.
fn mods(item: &Item) -> Vec<(&str, &str)> {
    let mut installed = HashSet::new();
    item.upgrades
        .iter()
        .filter(|path| installed.insert(path.as_str()))
        .filter_map(|path| match names::lookup(path) {
            Some(entry) if matches!(entry.kind, names::Kind::Mod | names::Kind::Arcane) => {
                Some((entry.name, path.as_str()))
            }
            None if path.starts_with(MOD_PATH) || path.contains(ARCANE_PATH) => {
                Some((path_tail(path), path.as_str()))
            }
            _ => None,
        })
        .collect()
}

/// What fills each of `GEAR_SLOTS`, in that order.
fn items(loadout: &Loadout) -> [&Option<Item>; 5] {
    [
        &loadout.warframe,
        &loadout.primary,
        &loadout.secondary,
        &loadout.melee,
        &loadout.companion,
    ]
}

/// What each slot holds, as `(value, hover details)`, with `—` for an empty one.
fn gear(loadout: &Loadout) -> [(String, Option<String>); 5] {
    let mut gear = items(loadout).map(|item| match item {
        Some(item) => (item_label(item).to_owned(), Some(item_details(item))),
        None => ("—".to_owned(), None),
    });
    // A companion goes by the name its owner gave it, unless that is the breed's own name.
    if let Some(item) = &loadout.companion
        && let Some(name) = &loadout.companion_name
        && name != item_label(item)
        && let Some(companion) = gear.last_mut()
    {
        companion.0 = format!("{} ({name})", companion.0);
    }
    gear
}

/// The name the arsenal shows for the item, or the last segment of its internal path when the
/// names table does not know it (or was built without the submodule).
fn item_label(item: &Item) -> &str {
    names::item_name(&item.path, &item.parts).unwrap_or_else(|| path_tail(&item.path))
}

/// Hover details: the name (which the card may have had to cut short), the full internal
/// path, a modular item's parts, then rank and forma where known.
fn item_details(item: &Item) -> String {
    let mut lines = vec![item_label(item).to_owned(), item.path.clone()];
    if !item.parts.is_empty() {
        let parts = item
            .parts
            .iter()
            .map(|part| names::lookup(part).map_or_else(|| path_tail(part), |entry| entry.name))
            .collect::<Vec<_>>();
        lines.push(parts.join(" / "));
    }
    let stats = [
        item.rank.map(|rank| format!("ランク {rank}")),
        item.forma.map(|forma| format!("フォーマ {forma}")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    if !stats.is_empty() {
        lines.push(stats.join(" / "));
    }
    lines.join("\n")
}

fn path_tail(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(path: &str, rank: Option<u64>, forma: Option<u64>) -> Option<Item> {
        Some(Item {
            path: path.to_owned(),
            rank,
            forma,
            parts: Vec::new(),
            upgrades: Vec::new(),
        })
    }

    #[test]
    fn remembers_where_each_window_was_left() {
        // The thread name contains `::`, which Windows rejects in a file name.
        let file = std::env::temp_dir().join(format!(
            "warframe-peer-overlay-windows-{}.json",
            std::process::id()
        ));
        let _ = fs::remove_file(&file);
        assert_eq!(read(&file, "loadouts"), None, "nothing written down yet");

        let compact = Placement {
            x: 10.0,
            y: 20.0,
            width: 980.0,
            height: 440.0,
        };
        let full = Placement {
            x: 30.0,
            y: 40.0,
            width: 1440.0,
            height: 900.0,
        };
        write(&file, "loadouts", compact);
        write(&file, "loadouts-full", full);

        assert_eq!(
            read(&file, "loadouts"),
            Some(compact),
            "one window's placement outlives the other's"
        );
        assert_eq!(read(&file, "loadouts-full"), Some(full));
        fs::remove_file(file).unwrap();
    }

    #[test]
    fn lists_the_mods_and_arcanes_installed_on_an_item() {
        let rifle = Item {
            upgrades: [
                "/Lotus/Upgrades/Skins/Deluxe/AlchemistDeluxeShotgunSkin",
                "/Lotus/Upgrades/Mods/Rifle/WeaponDamageAmountMod",
                "/Lotus/Upgrades/CosmeticEnhancers/Offensive/PrimaryDamageOnKill",
                // The game lists some of them twice; nothing is installed twice.
                "/Lotus/Upgrades/Mods/Rifle/WeaponDamageAmountMod",
                "/Lotus/Upgrades/Mods/Example/NotInTheExport",
            ]
            .map(str::to_owned)
            .to_vec(),
            ..item("/Lotus/Weapons/Tenno/Rifle/Rifle", Some(30), None).unwrap()
        };

        // The cosmetic is left out; one the export does not name shows by the end of its path.
        assert_eq!(
            mods(&rifle)
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>(),
            ["Serration", "Primary Merciless", "NotInTheExport"]
        );
    }

    #[test]
    fn names_an_item_as_the_arsenal_does() {
        let volt = item("/Lotus/Powersuits/Volt/VoltPrime", Some(30), Some(12)).unwrap();
        assert_eq!(item_label(&volt), "Volt Prime");

        let zaw = Item {
            parts: [
                "/Lotus/Weapons/Ostron/Melee/ModularMelee01/Balance/BalanceSpeedIICritI",
                "/Lotus/Weapons/Ostron/Melee/ModularMelee02/Handle/HandleNine",
                "/Lotus/Weapons/Ostron/Melee/ModularMelee02/Tip/TipEleven",
            ]
            .map(str::to_owned)
            .to_vec(),
            ..item(
                "/Lotus/Weapons/Ostron/Melee/LotusModularWeapon",
                Some(30),
                None,
            )
            .unwrap()
        };
        assert_eq!(item_label(&zaw), "Dokrahm");
        assert_eq!(
            item_details(&zaw),
            "Dokrahm\n/Lotus/Weapons/Ostron/Melee/LotusModularWeapon\nVargeet Jai II / Korb / Dokrahm\nランク 30"
        );
    }

    #[test]
    fn falls_back_to_the_end_of_the_path_for_an_item_the_table_lacks() {
        let suit = item("/Example/Suits/ExampleSuit", Some(30), Some(2)).unwrap();
        assert_eq!(item_label(&suit), "ExampleSuit");
        assert_eq!(
            item_details(&suit),
            "ExampleSuit\n/Example/Suits/ExampleSuit\nランク 30 / フォーマ 2"
        );

        let bare = item("/Example/Rifle", None, None).unwrap();
        assert_eq!(item_details(&bare), "Rifle\n/Example/Rifle");
    }

    #[test]
    fn lists_the_gear_in_slot_order_with_the_companions_name() {
        let loadout = Loadout {
            warframe: item("/Example/Suit", Some(30), None),
            primary: item("/Example/Rifle", None, None),
            companion: item("/Example/Pet", None, None),
            companion_name: Some("Pup".to_owned()),
            ..Loadout::default()
        };

        let values = gear(&loadout).map(|(value, _)| value);
        assert_eq!(values, ["Suit", "Rifle", "—", "—", "Pet (Pup)"]);
        // An empty slot has nothing to tell on hover.
        assert_eq!(gear(&loadout)[2].1, None);

        // A companion called after its own breed is not called that twice.
        let named_after_itself = Loadout {
            companion_name: Some("Pet".to_owned()),
            ..loadout
        };
        assert_eq!(gear(&named_after_itself)[4].0, "Pet");
    }

    #[test]
    fn shows_where_a_peer_connects_from() {
        let row = |region: &str, country: &str| LoadoutView {
            region: region.to_owned(),
            country: country.to_owned(),
            ..LoadoutView::default()
        };

        assert_eq!(location(&row("Tokyo", "Japan")), "Tokyo, Japan");
        assert_eq!(location(&row("", "Japan")), "Japan");
        assert_eq!(location(&row("Tokyo", "")), "Tokyo");
        // A city state, and a country whose ISO name carries the region, are said once.
        assert_eq!(location(&row("Hong Kong", "Hong Kong")), "Hong Kong");
        assert_eq!(
            location(&row("Taiwan", "Taiwan, Province of China[a]")),
            "Taiwan"
        );
        // Ours, or any peer whose IP never resolved: nothing to show.
        assert_eq!(location(&row("", "")), "");
    }

    #[test]
    fn leaves_a_name_without_a_companion_out() {
        let loadout = Loadout {
            companion_name: Some("Pup".to_owned()),
            ..Loadout::default()
        };
        assert_eq!(gear(&loadout)[4].0, "—");
    }
}
