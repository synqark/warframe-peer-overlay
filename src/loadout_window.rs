//! The loadout windows: ordinary desktop windows, unlike the overlay, meant to be moved beside
//! the game or onto another monitor. Each stays hidden until the tray menu asks for it, and
//! closing one only hides it again, so it comes back where the user left it.
//!
//! Every window shows a card per player, ours first, then each squad member in the order they
//! joined, whose card goes as soon as they leave the squad. They differ in how a card is laid
//! out (`Layout`): a line of gear per player (`COMPACT`), a tall card listing every slot's mods
//! (`FULL`), six places in two rows of three (`GRID`), and everyone met before, one of them
//! laid out as the tall card (`HISTORY`).
//!
//! egui's zoom factor is global, and the overlay drives it from the game's resolution (see
//! `ui_scale`). These windows belong to the desktop instead, so every size here goes through
//! `Scale`, which divides that zoom back out: a window follows its own monitor's scaling
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
    StrokeKind, TextFormat, UiBuilder, ViewportBuilder, ViewportClass, ViewportCommand, ViewportId,
    text::{LayoutJob, TextWrapping},
};
use serde::{Deserialize, Serialize};
use warframe_peer_overlay::{
    history::HistoryEntry,
    loadout::{Item, Loadout, Operator},
    monitor::LoadoutView,
    names, tray,
};

use crate::{flag_icon_bytes, platform_color, text_format};

/// What a window shows and the chrome around it. The windows differ in nothing else: all take
/// the same rows and run the same way.
#[derive(Clone, Copy)]
struct Layout {
    /// Tells the viewports apart.
    name: &'static str,
    title: &'static str,
    /// The size the window first opens at; the user is free to resize it from there.
    initial_size: [f32; 2],
    min_size: [f32; 2],
    draw: fn(&mut egui::Ui, &mut Shown),
}

/// What a window has to draw with.
#[derive(Default)]
struct Shown {
    /// The squad as it stands.
    rows: Vec<LoadoutView>,
    /// Players from squads gone by, newest first.
    history: Arc<[HistoryEntry]>,
    /// Which of those the history window has open.
    chosen: Option<HistoryEntry>,
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

/// Six places in two rows of three, for a squad of six: cards as big as their share of the
/// window, whatever its size.
const GRID: Layout = Layout {
    name: "loadouts-grid",
    title: "Loadouts (6grid) - Warframe Peer Overlay",
    initial_size: [1600.0, 900.0],
    min_size: [640.0, 360.0],
    draw: show_grid_cards,
};

/// Everyone met before down one side, whichever of them is chosen laid out on the other.
const HISTORY: Layout = Layout {
    name: "loadouts-history",
    title: "History - Warframe Peer Overlay",
    initial_size: [820.0, 760.0],
    min_size: [420.0, 240.0],
    draw: show_history,
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
/// The header's text sizes: the name, `HOST`, the mark that a card is ours, and mastery rank
/// and platform.
const NAME_SIZE: f32 = 17.0;
const HOST_SIZE: f32 = 13.0;
const OWN_MARK: &str = "自分";
const OWN_MARK_SIZE: f32 = 11.0;
const STANDING_SIZE: f32 = 14.0;
/// How much of the room it has a grid card's name is drawn at: a little under it, and in
/// italics, so that a name standing as tall as the two lines beside it does not crowd them.
const GRID_NAME_SCALE: f32 = 0.9;
const MOD_COLOR: Color32 = Color32::from_rgb(168, 178, 192);
/// A tall card's content width: four cards, with their margins, the space between them, the
/// window's own margins and room for the scroll bar, fit the 1440 the full window opens at.
const CARD_WIDTH: f32 = 312.0;
/// How far a mod sits in from the item it is installed on.
const MOD_INDENT: f32 = 10.0;
/// Rows of mods each slot keeps room for on a tall card, two mods to a row, whatever the
/// player has on: fourteen for the warframe and twelve for every other slot, a primary or a
/// secondary carrying twelve as often as a melee does. The slots then line up from card to
/// card.
const MOD_ROWS: [usize; 5] = [7, 6, 6, 6, 6];
/// The history's list: as wide as its columns, which are as wide as they need to be.
const LIST_WIDTH: f32 = 400.0;
const NAME_WIDTH: f32 = 150.0;
const MASTERY_WIDTH: f32 = 52.0;
const PLATFORM_WIDTH: f32 = 42.0;
/// The grid's places, `GRID_COLUMNS` to a row.
const GRID_PLACES: usize = 6;
const GRID_COLUMNS: usize = 3;
/// Room between the grid's cards, which stays as it is whatever size the cards are drawn at.
const GRID_GAP: f32 = 8.0;
/// A grid card's margin, and the width of what is inside it, at the size the card is designed
/// at: two columns of gear, each a little narrower than a tall card, with room between. Six
/// cards of it fill a Full HD window, a maximized one included.
const GRID_MARGIN: [f32; 2] = [10.0, 8.0];
const GRID_WIDTH: f32 = 608.0;
const GRID_COLUMN_GAP: f32 = 16.0;
/// The grid header's right-hand column: how wide a line of where the peer connects from may
/// run before it is cut short, and the size it is written at.
const GRID_PLACE_WIDTH: f32 = 160.0;
const PLACE_SIZE: f32 = 13.0;
/// What a grid card shows beneath its header, a row at a time: the column each of
/// `GEAR_SLOTS` goes in, and `None` for the column of auras, operator and quests beside the
/// companion (`show_grid_extras`).
const GRID_GEAR: [[Option<usize>; 2]; 3] =
    [[Some(0), Some(1)], [Some(2), Some(3)], [Some(4), None]];
/// A grid card is drawn at a size that moves in twentieths as the window is resized, so text
/// is not laid out afresh at every size the window passes through, and never below
/// `FIT_MIN`, where it would be past reading anyway.
const FIT_STEPS: f32 = 20.0;
const FIT_MIN: f32 = 0.3;
/// The outline of a place in the grid nobody has taken.
const VACANT_STROKE: Color32 = Color32::from_rgb(52, 60, 72);
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
    shown: Shown,
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

    /// Six places in two rows of three, each card filling its share of the window.
    pub fn grid() -> Self {
        Self::new(GRID)
    }

    /// Everyone we have shared a squad with, and what they brought to it.
    pub fn history() -> Self {
        Self::new(HISTORY)
    }

    fn new(layout: Layout) -> Self {
        let stored = placements().and_then(|file| read(&file, layout.name));
        Self {
            layout,
            show_request: Arc::default(),
            builder: None,
            visible: false,
            shown: Shown::default(),
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
        self.shown.rows = rows;
    }

    pub fn set_history(&mut self, history: Arc<[HistoryEntry]>) {
        self.shown.history = history;
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
                draw(ui, &mut self.shown);
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

/// Every window's placement lives in one file beside the loadouts, keyed by layout name.
fn placements() -> Option<PathBuf> {
    ProjectDirs::from("com", "synqark", "WarframePeerOverlay")
        .map(|dirs| dirs.data_local_dir().join("windows.json"))
}

fn read(file: &Path, name: &str) -> Option<Placement> {
    let stored: HashMap<String, Placement> = serde_json::from_slice(&fs::read(file).ok()?).ok()?;
    stored.get(name).copied()
}

/// Leaves whatever the other windows have written down as it is.
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

    /// The same, for something drawn `factor` times its designed size.
    fn times(self, factor: f32) -> Self {
        Self(self.0 * factor)
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

fn show_cards(ui: &mut egui::Ui, shown: &mut Shown) {
    let scale = Scale::of(ui.ctx());
    let rows = &shown.rows;
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
        name.append(OWN_MARK, scale.px(8.0), scale.text(OWN_MARK_SIZE, MUTED));
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
    name_job_at(row, NAME_SIZE, scale)
}

/// The name on a grid card: `GRID_NAME_SCALE` of the room its line has, and in italics.
fn grid_name_job(row: &LoadoutView, room: f32, scale: Scale) -> LayoutJob {
    let mut job = name_job_at(row, room * GRID_NAME_SCALE, scale);
    for section in &mut job.sections {
        section.format.italics = true;
    }
    job
}

fn name_job_at(row: &LoadoutView, size: f32, scale: Scale) -> LayoutJob {
    let color = if row.is_local { GOLD_TEXT } else { TEXT };
    let name = if row.name.is_empty() {
        "自分"
    } else {
        &row.name
    };
    single(name, size, color, scale)
}

/// The mark that a card is ours, for a layout that keeps it apart from the name.
fn own_mark(scale: Scale) -> LayoutJob {
    single(OWN_MARK, OWN_MARK_SIZE, MUTED, scale)
}

fn mastery_job(row: &LoadoutView, scale: Scale) -> LayoutJob {
    let mastery = row
        .loadout
        .as_ref()
        .and_then(|loadout| loadout.mastery_rank)
        .map(|rank| format!("MR {rank}"))
        .unwrap_or_default();
    single(&mastery, STANDING_SIZE, GOLD_TEXT, scale)
}

fn platform_job(row: &LoadoutView, scale: Scale) -> LayoutJob {
    single(
        &row.platform,
        STANDING_SIZE,
        platform_color(&row.platform),
        scale,
    )
}

/// Empty unless the squad connects through this player.
fn host_job(row: &LoadoutView, scale: Scale) -> LayoutJob {
    let host = if row.is_host { "HOST" } else { "" };
    single(host, HOST_SIZE, HOST_COLOR, scale)
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
fn show_full_cards(ui: &mut egui::Ui, shown: &mut Shown) {
    let scale = Scale::of(ui.ctx());
    let rows = &shown.rows;
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

/// The grid: six places in two rows of three, filled as a squad of six would fill them — ours
/// first, then each member in the order they joined — with a place nobody has taken outlined
/// as vacant. Every card takes its share of the window, and what is on it is drawn at whatever
/// size fills that share (`grid_fit`), so the window can be any size and still show all six.
fn show_grid_cards(ui: &mut egui::Ui, shown: &mut Shown) {
    let scale = Scale::of(ui.ctx());
    Frame::new()
        .fill(BACKGROUND)
        .inner_margin(scale.margin(10.0, 10.0))
        .show(ui, |ui| {
            let area = ui.available_rect_before_wrap();
            let gap = scale.px(GRID_GAP);
            let rows = GRID_PLACES.div_ceil(GRID_COLUMNS);
            let size = egui::vec2(
                (area.width() - gap * (GRID_COLUMNS - 1) as f32) / GRID_COLUMNS as f32,
                (area.height() - gap * (rows - 1) as f32) / rows as f32,
            )
            .max(egui::Vec2::ZERO);
            let fitted = scale.times(grid_fit(ui, size, scale));
            for place in 0..GRID_PLACES {
                let at = egui::vec2((place % GRID_COLUMNS) as f32, (place / GRID_COLUMNS) as f32);
                let rect = egui::Rect::from_min_size(
                    area.min + at * (size + egui::Vec2::splat(gap)),
                    size,
                );
                let mut card = ui.new_child(
                    UiBuilder::new()
                        .id_salt(("grid", place))
                        .max_rect(rect)
                        .layout(egui::Layout::top_down(egui::Align::LEFT)),
                );
                // Whatever does not fit is cut off at the card's edge rather than drawn over
                // the card beneath it.
                card.set_clip_rect(rect.intersect(ui.clip_rect()));
                match shown.rows.get(place) {
                    Some(row) => show_grid_card(&mut card, row, rect, fitted),
                    None => show_vacant_place(&card, rect, fitted),
                }
            }
            ui.advance_cursor_after_rect(area);
        });
}

/// How many times its designed size a grid card is drawn at for it to fill `size`. The width
/// is `GRID_WIDTH`; the height is found by laying out a card out of sight, one with every line
/// a captured loadout can have — its mod blocks keep their height whatever is installed, and
/// the sample carries two auras and an operator, the most the column beside the companion
/// holds, so no captured card stands taller.
fn grid_fit(ui: &mut egui::Ui, size: egui::Vec2, scale: Scale) -> f32 {
    let sample = LoadoutView {
        name: "Tenno".to_owned(),
        platform: "PC".to_owned(),
        is_local: true,
        is_host: true,
        loadout: Some(Loadout {
            mastery_rank: Some(30),
            post_new_war: Some(true),
            post_old_peace: Some(true),
            auras: vec![
                "/Sample/AuraName".to_owned(),
                "/Sample/ExtraAuraName".to_owned(),
            ],
            operator: Some(Operator {
                drifter: true,
                focus: Some("/Sample/FocusAbility".to_owned()),
            }),
            ..Loadout::default()
        }),
        ..LoadoutView::default()
    };
    let mut sizing = ui.new_child(
        UiBuilder::new()
            .id_salt("grid-sizing")
            .max_rect(egui::Rect::from_min_size(
                ui.max_rect().min,
                egui::vec2(scale.px(GRID_WIDTH), 10_000.0),
            ))
            .layout(egui::Layout::top_down(egui::Align::LEFT))
            .invisible(),
    );
    // Out of sight and out of reach: nothing in it can be hovered either.
    sizing.set_clip_rect(egui::Rect::NOTHING);
    show_grid_card_contents(&mut sizing, &sample, scale);
    let [margin_x, margin_y] = GRID_MARGIN;
    let designed = egui::vec2(
        scale.px(GRID_WIDTH + 2.0 * margin_x),
        sizing.min_rect().height() + scale.px(2.0 * margin_y),
    );
    fit(size, designed)
}

/// The largest step of `FIT_STEPS` at which `designed` still fits in `size`.
fn fit(size: egui::Vec2, designed: egui::Vec2) -> f32 {
    let fit = (size / designed).min_elem();
    // A hair over, so a ratio that is exactly a step, less rounding, still lands on it.
    ((fit * FIT_STEPS + 1e-3).floor() / FIT_STEPS).max(FIT_MIN)
}

/// One player's place in the grid, their card filling it.
fn show_grid_card(ui: &mut egui::Ui, row: &LoadoutView, rect: egui::Rect, scale: Scale) {
    let [margin_x, margin_y] = GRID_MARGIN;
    // Every card has a stroke as wide as our gold one, most of them unseen: a frame sets its
    // contents in by its stroke's width, and without it the other cards' rows would stand a
    // pixel higher than ours.
    let stroke = if row.is_local {
        OWN_STROKE
    } else {
        Color32::TRANSPARENT
    };
    Frame::new()
        .fill(CARD_FILL)
        .stroke(Stroke::new(scale.px(1.0), stroke))
        .corner_radius(CornerRadius::same(scale.px(6.0).round() as u8))
        .inner_margin(scale.margin(margin_x, margin_y))
        .show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            show_grid_card_contents(ui, row, scale);
        });
    copy_on_right_click(ui, rect, row);
}

/// A grid card's contents: its header across the whole of it, then its gear in two columns
/// split down the middle, each slot under a rule with its mods beneath.
fn show_grid_card_contents(ui: &mut egui::Ui, row: &LoadoutView, scale: Scale) {
    ui.spacing_mut().item_spacing = egui::vec2(scale.px(8.0), scale.px(3.0));
    show_grid_header(ui, row, scale);
    let Some(loadout) = &row.loadout else {
        ui.label(single("ロードアウト未取得", 13.0, MUTED, scale));
        return;
    };
    let mut gear = gear(loadout);
    let items = items(loadout);
    for slots in GRID_GEAR {
        ui.spacing_mut().item_spacing.x = scale.px(GRID_COLUMN_GAP);
        ui.columns(2, |columns| {
            for (column, slot) in columns.iter_mut().zip(slots) {
                column.spacing_mut().item_spacing.x = scale.px(8.0);
                column.add(egui::Separator::default().spacing(scale.px(6.0)));
                match slot {
                    Some(slot) => {
                        let (value, details) = std::mem::take(&mut gear[slot]);
                        show_full_slot(column, GEAR_SLOTS[slot], &value, details, scale);
                        show_full_mods(column, items[slot], MOD_ROWS[slot], scale);
                    }
                    None => show_grid_extras(column, row, loadout, scale),
                }
            }
        });
    }
}

/// A place in the grid nobody has taken: its outline, and a word to say so.
fn show_vacant_place(ui: &egui::Ui, rect: egui::Rect, scale: Scale) {
    let painter = ui.painter();
    painter.rect(
        rect,
        CornerRadius::same(scale.px(6.0).round() as u8),
        Color32::TRANSPARENT,
        Stroke::new(scale.px(1.0).max(1.0), VACANT_STROKE),
        StrokeKind::Inside,
    );
    let label = ui.fonts_mut(|fonts| fonts.layout_job(single("未参加", 16.0, MUTED, scale)));
    painter.galley(rect.center() - label.size() / 2.0, label, MUTED);
}

/// The history: everyone met before down the left, and whichever of them is chosen laid out
/// on the right, exactly as the full window lays out a card.
fn show_history(ui: &mut egui::Ui, shown: &mut Shown) {
    let scale = Scale::of(ui.ctx());
    let history = Arc::clone(&shown.history);
    Frame::new()
        .fill(BACKGROUND)
        .inner_margin(scale.margin(10.0, 10.0))
        .show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            ui.horizontal_top(|ui| {
                let height = ui.available_height();
                ui.allocate_ui_with_layout(
                    egui::vec2(scale.px(LIST_WIDTH), height),
                    egui::Layout::top_down(egui::Align::LEFT),
                    |ui| {
                        ui.set_width(scale.px(LIST_WIDTH));
                        show_history_list(ui, &history, shown, scale);
                    },
                );
                if let Some(chosen) = shown.chosen.clone() {
                    ScrollArea::vertical()
                        .id_salt("chosen")
                        .auto_shrink(false)
                        .show(ui, |ui| show_full_card(ui, &chosen.view, scale));
                }
            });
        });
}

/// Everyone met before, newest first. Only the rows on screen are laid out: there can be a
/// thousand of them.
fn show_history_list(ui: &mut egui::Ui, history: &[HistoryEntry], shown: &mut Shown, scale: Scale) {
    if history.is_empty() {
        ui.label(single(
            "まだ記録がありません。分隊のメンバーが抜けたときに記録します。",
            13.0,
            MUTED,
            scale,
        ));
        return;
    }
    let row = ui.fonts_mut(|fonts| fonts.layout_job(single("M", 14.0, TEXT, scale)).size().y)
        + scale.px(6.0);
    ScrollArea::vertical()
        .id_salt("history")
        .auto_shrink(false)
        .show_rows(ui, row, history.len(), |ui, range| {
            for entry in &history[range] {
                if show_history_row(ui, entry, shown.chosen.as_ref(), scale) {
                    shown.chosen = Some(entry.clone());
                }
            }
        });
}

/// One player of the history: who they were, and when the squad came apart. Says whether it
/// has just been asked for.
fn show_history_row(
    ui: &mut egui::Ui,
    entry: &HistoryEntry,
    chosen: Option<&HistoryEntry>,
    scale: Scale,
) -> bool {
    let open =
        chosen.is_some_and(|chosen| chosen.at == entry.at && chosen.view.name == entry.view.name);
    let row = Frame::new()
        .fill(if open {
            CARD_FILL
        } else {
            Color32::TRANSPARENT
        })
        .corner_radius(CornerRadius::same(scale.px(4.0).round() as u8))
        .inner_margin(scale.margin(6.0, 3.0))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                column(
                    ui,
                    single(&entry.view.name, 14.0, TEXT, scale),
                    scale.px(NAME_WIDTH),
                );
                column(ui, mastery_job(&entry.view, scale), scale.px(MASTERY_WIDTH));
                column(
                    ui,
                    platform_job(&entry.view, scale),
                    scale.px(PLATFORM_WIDTH),
                );
                match flag_icon_bytes(&entry.view.country_code) {
                    Some((uri, bytes)) => {
                        ui.add(
                            egui::Image::from_bytes(uri, bytes)
                                .fit_to_exact_size(egui::Vec2::splat(scale.px(FLAG_SIZE))),
                        );
                    }
                    None => ui.add_space(scale.px(FLAG_SIZE)),
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    whole(ui, single(&entry.when, 12.0, MUTED, scale));
                });
            });
        });
    ui.interact(
        row.response.rect,
        ui.id().with((entry.at, entry.view.name.as_str())),
        Sense::click(),
    )
    .on_hover_cursor(egui::CursorIcon::PointingHand)
    .clicked()
}

/// A cell of the history's list, cut short where it does not fit its column.
fn column(ui: &mut egui::Ui, mut job: LayoutJob, width: f32) {
    job.wrap = TextWrapping::truncate_at_width(width);
    let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, galley.size().y), Sense::hover());
    ui.painter().galley(rect.left_top(), galley, TEXT);
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
        show_marks(ui, row, scale);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            whole(ui, name_job(row, scale));
        });
    });
}

/// The second line: what the player has reached on the left, how far through the quests they
/// are on the right.
fn show_full_status(ui: &mut egui::Ui, row: &LoadoutView, scale: Scale) {
    ui.horizontal(|ui| {
        show_standing(ui, row, scale);
        show_quests(ui, row, scale);
    });
}

/// `HOST`, and the mark that a card is ours.
fn show_marks(ui: &mut egui::Ui, row: &LoadoutView, scale: Scale) {
    whole(ui, host_job(row, scale));
    if row.is_local {
        whole(ui, own_mark(scale));
    }
}

/// Mastery rank and platform.
fn show_standing(ui: &mut egui::Ui, row: &LoadoutView, scale: Scale) {
    whole(ui, mastery_job(row, scale));
    whole(ui, platform_job(row, scale));
}

/// How far through the quests the player is, against the right edge of a horizontal line.
fn show_quests(ui: &mut egui::Ui, row: &LoadoutView, scale: Scale) {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        // Laid down from the right, so the last quest goes first to leave them in order.
        for (job, tooltip) in quest_jobs(row, scale).into_iter().rev() {
            let quest = whole(ui, job);
            if let Some((quest, tooltip)) = quest.zip(tooltip) {
                quest.on_hover_text(tooltip);
            }
        }
    });
}

/// A grid card's header, two lines tall and in three columns: `HOST` and the mark that the
/// card is ours over mastery rank and platform on the left; where the peer connects from on
/// the right, the region over the country and its flag (`place_lines`), which our own card
/// has none of; and between them the name, one line of it as tall as the two beside it,
/// centred on the card as far as the sides leave it room. The full card's location line and
/// quests are not here: the one is this header's right-hand column, the other is down beside
/// the companion (`show_grid_extras`). Each line keeps its height with nothing on it, so the
/// name stands as tall on every card.
fn show_grid_header(ui: &mut egui::Ui, row: &LoadoutView, scale: Scale) {
    let height = |text: &str, size: f32| {
        ui.fonts_mut(|fonts| fonts.layout_job(single(text, size, TEXT, scale)).size().y)
    };
    let marks = height("HOST", HOST_SIZE).max(height(OWN_MARK, OWN_MARK_SIZE));
    let standing = height("M", STANDING_SIZE);
    // Text stands about as tall as its size, so the size that makes a line `tall` is found
    // from how tall a line comes out at the header's usual one.
    let tall = marks + ui.spacing().item_spacing.y + standing;
    let name_size = NAME_SIZE * tall / height("M", NAME_SIZE);

    let (header, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), tall), Sense::hover());
    let upper = egui::Rect::from_min_size(header.min, egui::vec2(header.width(), marks));
    let lower = egui::Rect::from_min_max(
        egui::pos2(header.min.x, header.max.y - standing),
        header.max,
    );
    let (region, country) = place_lines(row);
    let flag = flag_icon_bytes(&row.country_code);
    // Each side is laid into its lines from its own edge, and measured, so that the name
    // knows how much room is left between them.
    let mut side = |line: egui::Rect, from_the_right: bool, show: &dyn Fn(&mut egui::Ui)| {
        let layout = if from_the_right {
            egui::Layout::right_to_left(egui::Align::Center)
        } else {
            egui::Layout::left_to_right(egui::Align::Center)
        };
        let mut column = ui.new_child(UiBuilder::new().max_rect(line).layout(layout));
        show(&mut column);
        column.min_rect()
    };
    let left = [
        side(upper, false, &|ui| show_marks(ui, row, scale)),
        side(lower, false, &|ui| show_standing(ui, row, scale)),
    ];
    let right = [
        side(upper, true, &|ui| show_place(ui, region, scale)),
        side(lower, true, &|ui| {
            if let Some((uri, bytes)) = &flag {
                ui.add(
                    egui::Image::from_bytes(uri.clone(), bytes.clone())
                        .fit_to_exact_size(egui::Vec2::splat(scale.px(FLAG_SIZE))),
                );
            }
            show_place(ui, country, scale);
        }),
    ];

    let gap = scale.px(GRID_COLUMN_GAP);
    let from = left[0].right().max(left[1].right()) + gap;
    let to = right[0].left().min(right[1].left()) - gap;
    let mut job = grid_name_job(row, name_size, scale);
    job.wrap = TextWrapping::truncate_at_width((to - from).max(0.0));
    let name = ui.fonts_mut(|fonts| fonts.layout_job(job));
    // Centred on the card, unless that would run it into either side.
    let x = (header.center().x - name.size().x / 2.0)
        .min(to - name.size().x)
        .max(from);
    let y = header.center().y - name.size().y / 2.0;
    ui.painter().galley(egui::pos2(x, y), name, TEXT);
}

/// One line of where the peer connects from, cut short where it is too long for the header's
/// right-hand column, the whole of it then a hover away.
fn show_place(ui: &mut egui::Ui, text: &str, scale: Scale) {
    if text.is_empty() {
        return;
    }
    let mut job = single(text, PLACE_SIZE, LOCATION, scale);
    job.wrap = TextWrapping::truncate_at_width(scale.px(GRID_PLACE_WIDTH));
    let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
    let elided = galley.elided;
    let (rect, response) = ui.allocate_exact_size(galley.size(), Sense::hover());
    ui.painter().galley(rect.min, galley, LOCATION);
    if elided {
        response.on_hover_text(text);
    }
}

/// Where the peer connects from, as the grid's header puts it on two lines: `(region,
/// country)`, either empty when the geo lookup did not give it. A region that is the country
/// itself (a city state) is said once, on the country's line beside the flag. One that only
/// shares a word with it (Mexico City, Mexico) keeps its line: two lines have room for both.
fn place_lines(row: &LoadoutView) -> (&str, &str) {
    let (region, country) = (row.region.trim(), row.country.trim());
    if region.eq_ignore_ascii_case(country) {
        return ("", country);
    }
    (region, country)
}

/// The column a grid card keeps beside the companion, in two blocks. The auras on the warframe
/// come first, labelled and named as a slot is, a second one on a line of its own beneath the
/// first, and left out whole when there is none. The operator or drifter follows, with the
/// focus school they have on, and then how far through the quests the player is, against the
/// right edge — the quests belong to that block, so no rule comes between them. A rule parts
/// the blocks only when there are auras above to part it from.
fn show_grid_extras(ui: &mut egui::Ui, row: &LoadoutView, loadout: &Loadout, scale: Scale) {
    let auras = auras(loadout);
    let any_auras = !auras.is_empty();
    for (index, (name, details)) in auras.into_iter().enumerate() {
        let label = if index == 0 { "オーラ" } else { "" };
        show_full_slot(ui, label, name, Some(details), scale);
    }
    if let Some(operator) = &loadout.operator {
        if any_auras {
            ui.add(egui::Separator::default().spacing(scale.px(6.0)));
        }
        let (focus, details) = focus(operator);
        show_full_slot(ui, operator_label(operator), &focus, details, scale);
    }
    ui.horizontal(|ui| show_quests(ui, row, scale));
}

/// The auras on the warframe, each as `(name, hover details)`, named as the arsenal names
/// them, the second slot's after the first.
fn auras(loadout: &Loadout) -> Vec<(&str, String)> {
    loadout
        .auras
        .iter()
        .map(|key| {
            let name = dictionary_name(key);
            (name, format!("{name}\n{key}"))
        })
        .collect()
}

/// Who stands behind the warframe, as the label the line goes by.
fn operator_label(operator: &Operator) -> &'static str {
    if operator.drifter {
        "漂流者"
    } else {
        "オペレーター"
    }
}

/// The focus school the operator or drifter has on, as `(name, hover details)`, with `—` for
/// none.
fn focus(operator: &Operator) -> (String, Option<String>) {
    match &operator.focus {
        Some(path) => {
            let name = dictionary_name(path);
            (name.to_owned(), Some(format!("{name}\n{path}")))
        }
        None => ("—".to_owned(), None),
    }
}

/// The name the table gives a key or path, or the end of it when the table does not know it.
fn dictionary_name(key: &str) -> &str {
    names::lookup(key).map_or_else(|| path_tail(key), |entry| entry.name)
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
    fn puts_where_a_peer_connects_from_on_two_lines_for_the_grid() {
        let row = |region: &str, country: &str| LoadoutView {
            region: region.to_owned(),
            country: country.to_owned(),
            ..LoadoutView::default()
        };

        assert_eq!(place_lines(&row("Tokyo", "Japan")), ("Tokyo", "Japan"));
        assert_eq!(place_lines(&row("", "Japan")), ("", "Japan"));
        assert_eq!(place_lines(&row("Tokyo", "")), ("Tokyo", ""));
        // A city state is said once, on the country's line beside the flag.
        assert_eq!(
            place_lines(&row("Hong Kong", "Hong Kong")),
            ("", "Hong Kong")
        );
        // A region that only shares a word with its country is a place of its own.
        assert_eq!(
            place_lines(&row("Mexico City", "Mexico")),
            ("Mexico City", "Mexico")
        );
        assert_eq!(place_lines(&row("", "")), ("", ""));
    }

    #[test]
    fn leaves_a_name_without_a_companion_out() {
        let loadout = Loadout {
            companion_name: Some("Pup".to_owned()),
            ..Loadout::default()
        };
        assert_eq!(gear(&loadout)[4].0, "—");
    }

    #[test]
    fn names_each_aura_for_a_line_of_its_own() {
        assert!(auras(&Loadout::default()).is_empty());

        // A second aura slot's follows the first; a key the table lacks keeps its end.
        let two = Loadout {
            auras: vec![
                "/Lotus/Language/Mods/CritToAbilityAuraName".to_owned(),
                "/Example/NewAuraName".to_owned(),
            ],
            ..Loadout::default()
        };
        assert_eq!(
            auras(&two),
            [
                (
                    "Growing Power",
                    "Growing Power\n/Lotus/Language/Mods/CritToAbilityAuraName".to_owned()
                ),
                (
                    "NewAuraName",
                    "NewAuraName\n/Example/NewAuraName".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn labels_the_operator_or_drifter_by_the_focus_they_have_on() {
        let drifter = Operator {
            drifter: true,
            focus: Some("/Lotus/Upgrades/Focus/Power/PowerFocusAbility".to_owned()),
        };
        assert_eq!(operator_label(&drifter), "漂流者");
        assert_eq!(
            focus(&drifter),
            (
                "Zenurik".to_owned(),
                Some("Zenurik\n/Lotus/Upgrades/Focus/Power/PowerFocusAbility".to_owned())
            )
        );

        let operator = Operator {
            drifter: false,
            focus: None,
        };
        assert_eq!(operator_label(&operator), "オペレーター");
        assert_eq!(focus(&operator), ("—".to_owned(), None));
    }

    #[test]
    fn writes_a_grid_cards_name_a_little_smaller_and_slanted() {
        let row = LoadoutView {
            name: "Tenno".to_owned(),
            ..LoadoutView::default()
        };
        let job = grid_name_job(&row, 30.0, Scale(1.0));
        let format = &job.sections[0].format;

        assert_eq!(format.font_id.size, 27.0);
        assert!(format.italics);
    }

    #[test]
    fn puts_every_slot_in_the_grid_once() {
        let mut slots = GRID_GEAR
            .into_iter()
            .flatten()
            .flatten()
            .collect::<Vec<_>>();
        slots.sort_unstable();
        assert_eq!(slots, (0..GEAR_SLOTS.len()).collect::<Vec<_>>());
    }

    #[test]
    fn draws_a_grid_card_at_the_largest_step_that_fits() {
        let designed = egui::vec2(600.0, 500.0);
        assert_eq!(
            fit(egui::vec2(600.0, 500.0), designed),
            1.0,
            "a perfect fit"
        );
        assert_eq!(
            fit(egui::vec2(1200.0, 1000.0), designed),
            2.0,
            "twice the room"
        );
        // The tighter of the two decides, and a size between steps goes down to the one below.
        assert_eq!(fit(egui::vec2(590.0, 1000.0), designed), 0.95);
        assert_eq!(fit(egui::vec2(1200.0, 440.0), designed), 0.85);
        assert_eq!(
            fit(egui::vec2(60.0, 50.0), designed),
            FIT_MIN,
            "past reading"
        );
    }
}
