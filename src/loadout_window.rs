//! The loadout windows: ordinary desktop windows, unlike the overlay, meant to be moved beside
//! the game or onto another monitor. Each stays hidden until the tray menu asks for it, and
//! closing one only hides it again, so it comes back where the user left it.
//!
//! Every window shows a card per player, ours first, then each squad member in the order they
//! joined, whose card goes as soon as they leave the squad. They differ in how a card is laid
//! out (`Layout`): a line of gear per player (`COMPACT`), a tall card listing every slot's mods
//! (`FULL`), six places in two rows of three (`GRID`), and everyone met before beside what they
//! add up to, one of them laid over that as a grid card (`HISTORY`).
//!
//! egui's zoom factor is global, and the overlay drives it from the game's resolution (see
//! `ui_scale`). These windows belong to the desktop instead, so every size here goes through
//! `Scale`, which divides that zoom back out: a window follows its own monitor's scaling
//! whatever the game's resolution.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    f32::consts::{FRAC_PI_2, TAU},
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
    mission::{Ending, Mission},
    monitor::LoadoutView,
    names, tray,
};
use windows_sys::Win32::{
    Foundation::RECT,
    Graphics::Gdi::{MONITOR_DEFAULTTONULL, MonitorFromRect},
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
    /// Which of the history the statistics add up.
    scope: Scope,
    /// What the statistics show, with the history it was added up from and, when it is only
    /// what the list shows, the filter that sifted it.
    tally: Option<(Arc<[HistoryEntry]>, Option<Filter>, Tally)>,
    /// What the history's list is narrowed to.
    filter: Filter,
    /// The entries the filter let through, by their place in the history, with the history
    /// and the filter they were sifted by.
    listed: Option<(Arc<[HistoryEntry]>, Filter, Vec<usize>)>,
    /// The missions loaded lately, the newest last.
    missions: Vec<Mission>,
    /// Everyone in the squad bar ourselves, with the mission each is tied to.
    squad_ties: Vec<(String, Option<Mission>)>,
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

/// Everyone met before down one side, what they add up to on the other, and whichever of them
/// is chosen laid over that. It opens wide enough for four columns of statistics.
const HISTORY: Layout = Layout {
    name: "loadouts-history",
    title: "History - Warframe Peer Overlay",
    initial_size: [1600.0, 900.0],
    min_size: [960.0, 520.0],
    draw: show_history,
};

/// A debugging aid while EE.log's missions are looked into: the one loaded last, whether it
/// ended, and those before it.
const SESSION: Layout = Layout {
    name: "session",
    title: "Session (debug) - Warframe Peer Overlay",
    initial_size: [760.0, 460.0],
    min_size: [420.0, 240.0],
    draw: show_session,
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
/// How much of the room it has a grid card's name is drawn at: a little under it, so that a
/// name standing as tall as the two lines beside it does not crowd them.
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
/// The history's list: as wide as its columns, which are as wide as they need to be but for
/// the mission's node and type, whose longest names are cut short. A row's cells are set in
/// by `ROW_MARGIN`, and the filters over them by as much.
const LIST_WIDTH: f32 = 720.0;
const ROW_MARGIN: [f32; 2] = [6.0, 3.0];
const NAME_WIDTH: f32 = 150.0;
const MASTERY_WIDTH: f32 = 52.0;
const PLATFORM_WIDTH: f32 = 42.0;
const NODE_WIDTH: f32 = 170.0;
const MISSION_TYPE_WIDTH: f32 = 130.0;
/// The history's statistics: cells in a grid `STATS_COLUMNS` wide and `STATS_ROWS` tall, each
/// at `[column, row]`, with `STATS_GAP` between them and the list. They stand in the order of
/// `Statistic::index`.
const STATS_COLUMNS: usize = 4;
const STATS_ROWS: usize = 2;
const STATS_GAP: f32 = 8.0;
const STATS: [(Statistic, [usize; 2]); 8] = [
    (Statistic::Platforms, [0, 0]),
    (Statistic::Countries, [1, 0]),
    (Statistic::Focus, [2, 0]),
    (Statistic::Slot(0), [3, 0]),
    (Statistic::Slot(1), [0, 1]),
    (Statistic::Slot(2), [1, 1]),
    (Statistic::Slot(3), [2, 1]),
    (Statistic::Slot(4), [3, 1]),
];
/// How many kinds of statistic there are.
const KINDS: usize = STATS.len();
/// A pie's diameter at most and at least, the room between it and its legend, and how many
/// triangles a whole pie is made of.
const PIE_MAX: f32 = 220.0;
const PIE_MIN: f32 = 48.0;
const PIE_GAP: f32 = 8.0;
const PIE_STEPS: f32 = 120.0;
/// A line of a legend or a ranking: the room its rank or swatch takes on the left, and that
/// of how many and what share against the right edge.
const RANK_WIDTH: f32 = 30.0;
const COUNT_WIDTH: f32 = 36.0;
const SHARE_WIDTH: f32 = 52.0;
/// Behind a line of a ranking, the overlay's gold, faint.
const BAR: Color32 = Color32::from_rgba_unmultiplied_const(194, 163, 87, 36);
/// Behind a line of a legend or a ranking under the pointer.
const HOVERED: Color32 = Color32::from_rgba_unmultiplied_const(255, 255, 255, 14);
/// How much of its colour a slice keeps while others are picked and it is not.
const UNPICKED: f32 = 0.3;
/// Laid over the statistics beneath the chosen player's card.
const VEIL: Color32 = Color32::from_rgba_unmultiplied_const(10, 14, 20, 215);
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

/// What a cell of the history's statistics counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Statistic {
    Platforms,
    /// The country a player connected from; the region is not counted.
    Countries,
    Focus,
    /// The gear in one of `GEAR_SLOTS`.
    Slot(usize),
}

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
    /// Where a window just opened is still to be put, in desktop pixels, and how many more
    /// moves it is given to get there.
    placing: Option<([f32; 2], u8)>,
}

/// Where a window was last left, so that it opens there again on the next run: its outer
/// corner in desktop pixels, the one measure every monitor shares, and its inner size in
/// desktop points (the zoom divided back out) at the scale of the monitor it was on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Placement {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

/// How many times a window just opened is moved before it is left wherever it has got to.
const PLACING_MOVES: u8 = 3;
/// The strip along a window's top, in desktop pixels, that must be on a monitor for a stored
/// placement to be used: enough of the title bar to take hold of the window by.
const TITLE_BAR: f32 = 32.0;

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

    /// The missions loaded lately, for debugging.
    pub fn session() -> Self {
        Self::new(SESSION)
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
            placing: None,
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

    pub fn set_session(
        &mut self,
        missions: Vec<Mission>,
        squad_ties: Vec<(String, Option<Mission>)>,
    ) {
        self.shown.missions = missions;
        self.shown.squad_ties = squad_ties;
    }

    /// Runs the window for one pass of the root viewport. Call it on every pass, shown or
    /// not: egui closes for good a viewport it does not hear about for a pass.
    pub fn show(&mut self, context: &egui::Context) {
        let id = ViewportId::from_hash_of(self.layout.name);
        let draw = self.layout.draw;
        if self.show_request.swap(false, Ordering::Relaxed) {
            if self.builder.is_none() {
                // A window left where no monitor shows it now opens where Windows puts a new
                // one, keeping only its size.
                let at = self.stored.filter(|stored| on_screen(*stored));
                // Pixels per point of the monitor the window is made on, which Windows picks
                // only once it is made: the overlay's is the likeliest guess, and `place`
                // puts right whatever it gets wrong.
                let native = context
                    .input(|input| input.viewport().native_pixels_per_point)
                    .unwrap_or(1.0);
                self.builder = Some(builder(
                    self.layout,
                    self.stored,
                    at,
                    Scale::of(context),
                    native,
                ));
                self.placing = at.map(|at| ([at.x, at.y], PLACING_MOVES));
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
        if self.placing.is_some() {
            self.place(context, id, Scale::of(context));
        } else if self.visible {
            self.remember(context, id, Scale::of(context));
        }
    }

    /// Moves a window just opened to where it was left, now that it is there to say what
    /// scale its monitor has. winit makes a position given in points into pixels at the
    /// scale of the monitor the window is made on, not the one it is going to, so a window
    /// left on a monitor at 100% beside a primary at 150% would open half as far out again,
    /// and off every monitor. Crossing onto a monitor of another scale also lets Windows
    /// suggest a corner of its own, so the window is looked at again on the next pass.
    fn place(&mut self, context: &egui::Context, id: ViewportId, scale: Scale) {
        let Some((target, moves)) = self.placing else {
            return;
        };
        let Some((corner, native)) = context.input_for(id, |input| {
            let viewport = input.viewport();
            Some((viewport.outer_rect?.min, viewport.native_pixels_per_point?))
        }) else {
            // Not made yet.
            return;
        };
        let [x, y] = scale.to_pixels([corner.x, corner.y], native);
        if moves == 0 || ((x - target[0]).abs() < 1.0 && (y - target[1]).abs() < 1.0) {
            self.placing = None;
            return;
        }
        let [x, y] = scale.to_points(target, native);
        context.send_viewport_cmd_to(id, ViewportCommand::OuterPosition(egui::pos2(x, y)));
        self.placing = Some((target, moves - 1));
    }

    /// Writes down where the user has put the window, once they have stopped moving it, for
    /// the next run to open it there.
    fn remember(&mut self, context: &egui::Context, id: ViewportId, scale: Scale) {
        let placement = context.input_for(id, |input| {
            let viewport = input.viewport();
            let corner = viewport.outer_rect?.min;
            let size = viewport.inner_rect?.size();
            let ([x, y], [width, height]) = (
                scale.to_pixels([corner.x, corner.y], viewport.native_pixels_per_point?),
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

/// Opens the window at the size it was left at, and at `at` for a monitor of `native`
/// pixels per point.
fn builder(
    layout: Layout,
    stored: Option<Placement>,
    at: Option<Placement>,
    scale: Scale,
    native: f32,
) -> ViewportBuilder {
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
    match at {
        Some(at) => builder.with_position(scale.to_points([at.x, at.y], native)),
        None => builder,
    }
}

/// Whether a window put at `placement` would have its title bar on a monitor, for the user
/// to take hold of it by. One left on a monitor that is gone since would open out of reach.
fn on_screen(placement: Placement) -> bool {
    let strip = RECT {
        left: placement.x as i32,
        top: placement.y as i32,
        right: (placement.x + placement.width) as i32,
        bottom: (placement.y + TITLE_BAR) as i32,
    };
    // SAFETY: reads the rectangle and nothing else.
    !unsafe { MonitorFromRect(&strip, MONITOR_DEFAULTTONULL) }.is_null()
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

    /// A position egui measured on a window of `native` pixels per point, in desktop pixels.
    /// Points are not the same size on every monitor, so only pixels say where a window is.
    fn to_pixels(self, position: [f32; 2], native: f32) -> [f32; 2] {
        self.unscaled(position).map(|v| (v * native).round())
    }

    /// The position to hand egui for a window of `native` pixels per point to stand at
    /// `pixels`.
    fn to_points(self, [x, y]: [f32; 2], native: f32) -> [f32; 2] {
        self.scaled([x / native, y / native])
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

/// The name on a grid card: `GRID_NAME_SCALE` of the room its line has.
fn grid_name_job(row: &LoadoutView, room: f32, scale: Scale) -> LayoutJob {
    name_job_at(row, room * GRID_NAME_SCALE, scale)
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
    fit(size, grid_designed(ui, scale))
}

/// How big a grid card stands at its designed size, margins and all (see `grid_fit`).
fn grid_designed(ui: &mut egui::Ui, scale: Scale) -> egui::Vec2 {
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
    egui::vec2(
        scale.px(GRID_WIDTH + 2.0 * margin_x),
        sizing.min_rect().height() + scale.px(2.0 * margin_y),
    )
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

/// The history: everyone met before down the left, and what they add up to on the right
/// (`show_statistics`). Choosing one of them lays their card over the statistics, as the grid
/// lays a card out; a click anywhere but on the card, their row again or Esc puts it away, and
/// another row puts that player's card in its place.
fn show_history(ui: &mut egui::Ui, shown: &mut Shown) {
    let scale = Scale::of(ui.ctx());
    let history = Arc::clone(&shown.history);
    let filter = shown.filter.clone();
    refresh_listed(shown, &history);
    refresh_tally(shown, &history);
    Frame::new()
        .fill(BACKGROUND)
        .inner_margin(scale.margin(10.0, 10.0))
        .show(ui, |ui| {
            let area = ui.available_rect_before_wrap();
            let list = egui::Rect::from_min_size(
                area.min,
                egui::vec2(scale.px(LIST_WIDTH), area.height()),
            );
            let statistics = egui::Rect::from_min_max(
                egui::pos2(list.right() + scale.px(STATS_GAP), area.top()),
                area.max,
            );
            // The statistics first: what is picked there narrows the list in the same pass.
            let room = statistics.width() > 0.0;
            if room {
                // A line as tall as the list's filters, to choose what the statistics add up.
                let bar = egui::Rect::from_min_size(
                    statistics.min,
                    egui::vec2(statistics.width(), filter_height(ui, scale)),
                );
                let cells = egui::Rect::from_min_max(
                    egui::pos2(statistics.left(), bar.bottom() + scale.px(STATS_GAP)),
                    statistics.max,
                );
                show_scope(ui, bar, &mut shown.scope, scale);
                refresh_tally(shown, &history);
                if let Some((_, _, tally)) = &shown.tally {
                    show_statistics(ui, cells, tally, &mut shown.filter, scale);
                }
            }
            let mut left = ui.new_child(
                UiBuilder::new()
                    .id_salt("history-list")
                    .max_rect(list)
                    .layout(egui::Layout::top_down(egui::Align::LEFT)),
            );
            let clicked = show_history_list(&mut left, &history, shown, scale);
            let card = shown
                .chosen
                .as_ref()
                .filter(|_| room)
                .map(|chosen| show_chosen(ui, statistics, &chosen.view, scale));
            ui.advance_cursor_after_rect(area);

            match clicked {
                Some(entry) => {
                    let again = shown.chosen.as_ref().is_some_and(|open| same(open, entry));
                    shown.chosen = (!again).then(|| entry.clone());
                }
                None => {
                    let away = ui.input(|input| {
                        input.key_pressed(egui::Key::Escape)
                            || input.pointer.primary_clicked()
                                && input.pointer.interact_pos().is_some_and(|pointer| {
                                    !card.is_some_and(|card: egui::Rect| card.contains(pointer))
                                })
                    });
                    if away {
                        shown.chosen = None;
                    }
                }
            }
        });
    // The statistics were drawn before the filter changed, and while they add up only what
    // the list shows, they are behind it until the next pass: ask for one now.
    if shown.scope == Scope::Listed && shown.filter != filter {
        ui.ctx().request_repaint_of(egui::ViewportId::ROOT);
    }
}

/// Which of the history the statistics add up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Scope {
    /// Every entry.
    #[default]
    All,
    /// Only the entries the list shows, as the filters narrow it.
    Listed,
}

/// Sifts the history again, when it or the filter has changed since it was last.
fn refresh_listed(shown: &mut Shown, history: &Arc<[HistoryEntry]>) {
    if !shown
        .listed
        .as_ref()
        .is_some_and(|(of, filter, _)| Arc::ptr_eq(of, history) && *filter == shown.filter)
    {
        let listed = shown.filter.apply(history);
        shown.listed = Some((Arc::clone(history), shown.filter.clone(), listed));
    }
}

/// Adds the history up again, when it, what the statistics add up, or — while that is only
/// what the list shows — the filter has changed since it was last. A snapshot hands over the
/// same history until there is more of it.
fn refresh_tally(shown: &mut Shown, history: &Arc<[HistoryEntry]>) {
    let sifted = (shown.scope == Scope::Listed).then(|| shown.filter.clone());
    if shown
        .tally
        .as_ref()
        .is_some_and(|(of, by, _)| Arc::ptr_eq(of, history) && *by == sifted)
    {
        return;
    }
    let added_up = if sifted.is_some() {
        refresh_listed(shown, history);
        let listed = shown
            .listed
            .as_ref()
            .map_or(&[][..], |(_, _, listed)| listed.as_slice());
        tally(listed.iter().map(|&index| &history[index]))
    } else {
        tally(history.iter())
    };
    shown.tally = Some((Arc::clone(history), sifted, added_up));
}

/// How tall a line of the list's filters stands: a box to type into, of the size they are.
fn filter_height(ui: &egui::Ui, scale: Scale) -> f32 {
    let line = ui.fonts_mut(|fonts| fonts.row_height(&egui::FontId::proportional(scale.px(13.0))));
    line + scale.margin(6.0, 3.0).sum().y
}

/// The line over the statistics: against its right edge, which of the history they add up,
/// every entry or only those the list shows.
fn show_scope(ui: &mut egui::Ui, bar: egui::Rect, scope: &mut Scope, scale: Scale) {
    let mut line = ui.new_child(
        UiBuilder::new()
            .id_salt("statistics-scope")
            .max_rect(bar)
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    line.spacing_mut().button_padding = egui::vec2(scale.px(8.0), scale.px(3.0));
    line.spacing_mut().item_spacing.x = scale.px(4.0);
    // Laid down from the right, so the last choice goes first to leave them in order.
    line.selectable_value(
        scope,
        Scope::Listed,
        single("検索データのみ", 12.0, TEXT, scale),
    );
    line.selectable_value(scope, Scope::All, single("全データ", 12.0, TEXT, scale));
    line.add_space(scale.px(4.0));
    line.label(single("統計表示対象データ：", 12.0, MUTED, scale));
}

/// The session window: the mission loaded last set out at length and whether its session
/// ended, the mission each squad member is tied to, and every mission loaded lately, the
/// newest at the top. Everything as the log writes it: types and locations by the game's own
/// names, times by the log's clock.
fn show_session(ui: &mut egui::Ui, shown: &mut Shown) {
    let scale = Scale::of(ui.ctx());
    Frame::new()
        .fill(BACKGROUND)
        .inner_margin(scale.margin(12.0, 10.0))
        .show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            ui.spacing_mut().item_spacing = egui::vec2(scale.px(16.0), scale.px(4.0));
            let heading = |ui: &mut egui::Ui, text: &str| {
                ui.label(single(text, 14.0, GOLD_TEXT, scale));
            };
            let note = |ui: &mut egui::Ui, text: &str| {
                ui.label(single(text, 13.0, MUTED, scale));
            };

            heading(ui, "最後にロードしたミッション");
            match shown.missions.last() {
                Some(latest) => show_latest_mission(ui, latest, scale),
                None => note(ui, "まだ SolNode のミッションのロードを見ていません。"),
            }
            ui.add_space(scale.px(6.0));
            ui.separator();

            heading(ui, "分隊メンバーの紐付け（抜けたときに History へ記録）");
            if shown.squad_ties.is_empty() {
                note(ui, "分隊にメンバーはいません。");
            } else {
                egui::Grid::new("session-ties")
                    .striped(true)
                    .spacing(egui::vec2(scale.px(20.0), scale.px(3.0)))
                    .show(ui, |ui| {
                        for column in ["メンバー", "location", "missionType", "ロード (秒)"]
                        {
                            ui.label(single(column, 12.0, MUTED, scale));
                        }
                        ui.end_row();
                        for (member, tie) in &shown.squad_ties {
                            ui.label(single(member, 13.0, TEXT, scale));
                            match tie {
                                Some(mission) => {
                                    for value in [
                                        &mission.location,
                                        &mission.mission_type,
                                        &mission.loaded_at,
                                    ] {
                                        ui.label(single(value, 13.0, TEXT, scale));
                                    }
                                }
                                None => {
                                    ui.label(single(
                                        "なし（抜けても記録しない）",
                                        13.0,
                                        HOST_COLOR,
                                        scale,
                                    ));
                                }
                            }
                            ui.end_row();
                        }
                    });
            }
            ui.add_space(scale.px(6.0));
            ui.separator();

            heading(ui, "直近のミッション（新しい順）");
            ScrollArea::both()
                .id_salt("session-recent")
                .auto_shrink(false)
                .show(ui, |ui| {
                    egui::Grid::new("session-recent")
                        .striped(true)
                        .spacing(egui::vec2(scale.px(20.0), scale.px(3.0)))
                        .show(ui, |ui| {
                            for column in [
                                "ロード (秒)",
                                "役割",
                                "location",
                                "missionType",
                                "ノード",
                                "終了",
                            ] {
                                ui.label(single(column, 12.0, MUTED, scale));
                            }
                            ui.end_row();
                            for mission in shown.missions.iter().rev() {
                                let (ended, colour) = mission_end(mission);
                                for (value, colour) in [
                                    (mission.loaded_at.clone(), TEXT),
                                    (mission_role(mission).to_owned(), TEXT),
                                    (or_dash(&mission.location), TEXT),
                                    (or_dash(&mission.mission_type), TEXT),
                                    (or_dash(&mission.node), TEXT),
                                    (ended, colour),
                                ] {
                                    ui.label(single(&value, 13.0, colour, scale));
                                }
                                ui.end_row();
                            }
                        });
                });
        });
}

/// The mission loaded last, a line for each of what is known of it.
fn show_latest_mission(ui: &mut egui::Ui, latest: &Mission, scale: Scale) {
    let (ended, ended_colour) = mission_end(latest);
    egui::Grid::new("session-latest")
        .spacing(egui::vec2(scale.px(24.0), scale.px(4.0)))
        .show(ui, |ui| {
            for (label, value, colour) in [
                (
                    "location",
                    with_name(&latest.location, names::node_name(&latest.location)),
                    TEXT,
                ),
                (
                    "missionType",
                    with_name(
                        &latest.mission_type,
                        names::mission_type_name(&latest.mission_type),
                    ),
                    TEXT,
                ),
                ("ノード", or_dash(&latest.node), TEXT),
                (
                    "ロード",
                    format!("{} 秒 ({})", latest.loaded_at, mission_role(latest)),
                    TEXT,
                ),
                ("セッション終了", ended, ended_colour),
            ] {
                ui.label(single(label, 12.0, MUTED, scale));
                ui.label(single(&value, 16.0, colour, scale));
                ui.end_row();
            }
        });
}

/// Whether we loaded the mission as its host or joined it.
fn mission_role(mission: &Mission) -> &'static str {
    if mission.host {
        "ホスト"
    } else {
        "クライアント"
    }
}

/// Whether the mission's session ended, and in what colour to say so: done, by the `EOM` or
/// the abort the log showed and when, or not yet.
fn mission_end(mission: &Mission) -> (String, Color32) {
    match &mission.ended {
        Some(ended) => {
            let by = match ended.by {
                Ending::Eom => "EOM",
                Ending::Abort => "Abort",
            };
            (format!("済 ({by} {} 秒)", ended.at), QUEST_DONE)
        }
        None => ("未".to_owned(), HOST_COLOR),
    }
}

/// An id as the log gives it, followed by the name the export gives it where it does.
fn with_name(id: &str, name: Option<&str>) -> String {
    match name {
        Some(name) => format!("{id} → {name}"),
        None => or_dash(id),
    }
}

/// The text as it is, or `—` for none.
fn or_dash(text: &str) -> String {
    if text.is_empty() {
        "—".to_owned()
    } else {
        text.to_owned()
    }
}

/// Whether two entries of the history are the same player's departure.
fn same(one: &HistoryEntry, other: &HistoryEntry) -> bool {
    one.at == other.at && one.view.name == other.view.name
}

/// Everyone met before whom the filter lets through, newest first, under the box to search
/// them by name; and whichever of them was clicked. Only the rows on screen are laid out:
/// there can be thousands of them.
fn show_history_list<'a>(
    ui: &mut egui::Ui,
    history: &'a Arc<[HistoryEntry]>,
    shown: &mut Shown,
    scale: Scale,
) -> Option<&'a HistoryEntry> {
    if history.is_empty() {
        ui.label(single(
            "まだ記録がありません。分隊のメンバーが抜けたときに記録します。",
            13.0,
            MUTED,
            scale,
        ));
        return None;
    }
    show_list_header(ui, &mut shown.filter, scale);
    refresh_listed(shown, history);
    let listed = shown
        .listed
        .as_ref()
        .map_or(&[][..], |(_, _, listed)| listed.as_slice());
    let count = if shown.filter.is_empty() {
        format!("{}件", history.len())
    } else {
        format!("{} / {}件", listed.len(), history.len())
    };
    ui.label(single(&count, 12.0, MUTED, scale));
    if listed.is_empty() {
        ui.label(single("条件に合う記録がありません。", 13.0, MUTED, scale));
        return None;
    }
    let row = ui.fonts_mut(|fonts| fonts.layout_job(single("M", 14.0, TEXT, scale)).size().y)
        + scale.px(6.0);
    let chosen = shown.chosen.as_ref();
    let mut clicked = None;
    ScrollArea::vertical()
        .id_salt("history")
        .auto_shrink(false)
        .show_rows(ui, row, listed.len(), |ui, range| {
            for &index in &listed[range] {
                let entry = &history[index];
                if show_history_row(ui, entry, chosen, scale) {
                    clicked = Some(entry);
                }
            }
        });
    clicked
}

/// The list's header: over each column with a filter, that filter, lined up with the column's
/// cells - a box to search by name over the columns that say who a player was, one to search
/// by the mission's node over its column, and a menu of the mission types over theirs - and,
/// against the right edge, a button that lets every filter go, the picks in the statistics
/// included.
fn show_list_header(ui: &mut egui::Ui, filter: &mut Filter, scale: Scale) {
    ui.horizontal(|ui| {
        ui.spacing_mut().button_padding = egui::vec2(scale.px(8.0), scale.px(3.0));
        let gap = ui.spacing().item_spacing.x;
        // In from the edge as far as a row's margin sets its cells.
        ui.add_space(f32::from(scale.margin(ROW_MARGIN[0], ROW_MARGIN[1]).left));
        let who = scale.px(NAME_WIDTH + MASTERY_WIDTH + PLATFORM_WIDTH) + 2.0 * gap;
        search_box(
            ui,
            &mut filter.name,
            "history-name",
            "名前で検索",
            who,
            scale,
        );
        let node = scale.px(NODE_WIDTH);
        search_box(
            ui,
            &mut filter.node,
            "history-node",
            "ノードで検索",
            node,
            scale,
        );
        mission_type_menu(ui, &mut filter.mission_type, scale);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let clear = egui::Button::new(single("絞り込み解除", 12.0, TEXT, scale));
            if ui.add_enabled(!filter.is_empty(), clear).clicked() {
                *filter = Filter::default();
            }
        });
    });
    ui.add_space(scale.px(2.0));
}

/// A box to type a search into, `width` wide.
fn search_box(
    ui: &mut egui::Ui,
    text: &mut String,
    id: &str,
    hint: &str,
    width: f32,
    scale: Scale,
) {
    ui.add(
        egui::TextEdit::singleline(text)
            .id_salt(id)
            .hint_text(single(hint, 13.0, MUTED, scale))
            .font(egui::FontId::proportional(scale.px(13.0)))
            .margin(scale.margin(6.0, 3.0))
            .desired_width(width),
    );
}

/// The menu of every mission type the export names, in the order of their names, over the
/// column of mission types and as wide as it; its list grows as wide as the longest name.
fn mission_type_menu(ui: &mut egui::Ui, picked: &mut Option<String>, scale: Scale) {
    let width = scale.px(MISSION_TYPE_WIDTH);
    let selected = match picked.as_deref() {
        Some(id) => single(
            names::mission_type_name(id).unwrap_or(id),
            13.0,
            TEXT,
            scale,
        ),
        None => single("タイプ", 13.0, MUTED, scale),
    };
    // Held to the column's width, which a long name is cut short to rather than widen it.
    ui.allocate_ui_with_layout(
        egui::vec2(width, ui.available_height()),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_max_width(width);
            egui::ComboBox::from_id_salt("history-mission-type")
                .width(width)
                .height(scale.px(480.0))
                .wrap_mode(egui::TextWrapMode::Truncate)
                .selected_text(selected)
                .show_ui(ui, |ui| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                    ui.selectable_value(picked, None, single("すべて", 13.0, MUTED, scale));
                    for (id, name) in names::mission_types() {
                        let option = single(name, 13.0, TEXT, scale);
                        ui.selectable_value(picked, Some((*id).to_owned()), option);
                    }
                });
        },
    );
}

/// The chosen player's card, laid over the statistics on a veil that dims them, drawn as a
/// grid card at the largest step that fits and no larger than it is designed at. Being in an
/// area of its own, above the window's contents, it keeps the pointer from what it covers.
/// Returns where the card is.
fn show_chosen(ui: &mut egui::Ui, over: egui::Rect, row: &LoadoutView, scale: Scale) -> egui::Rect {
    let designed = grid_designed(ui, scale);
    let room = over.shrink(scale.px(STATS_GAP)).size();
    let fitted = fit(room, designed).min(1.0);
    let card = egui::Rect::from_center_size(over.center(), designed * fitted);
    let context = ui.ctx().clone();
    egui::Area::new(ui.id().with("chosen"))
        .order(egui::Order::Middle)
        .fixed_pos(over.min)
        .constrain(false)
        .fade_in(false)
        .show(&context, |ui| {
            ui.painter()
                .rect_filled(over, CornerRadius::same(scale.px(6.0).round() as u8), VEIL);
            ui.set_min_size(over.size());
            let mut inside = ui.new_child(
                UiBuilder::new()
                    .id_salt("chosen-card")
                    .max_rect(card)
                    .layout(egui::Layout::top_down(egui::Align::LEFT)),
            );
            inside.set_clip_rect(card);
            show_grid_card(&mut inside, row, card, scale.times(fitted));
        });
    card
}

/// What the history adds up to, in a grid of cells (`STATS`): platforms and focus schools as
/// pies, countries and each slot's gear as rankings, most first. It always adds up the whole
/// history, whatever the list is narrowed to. Every slice and line is a toggle: anything picked
/// in a cell narrows the list to the players who match one of the picks (`Filter`).
fn show_statistics(
    ui: &mut egui::Ui,
    area: egui::Rect,
    tally: &Tally,
    filter: &mut Filter,
    scale: Scale,
) {
    let gap = scale.px(STATS_GAP);
    let cell = egui::vec2(
        (area.width() - gap * (STATS_COLUMNS - 1) as f32) / STATS_COLUMNS as f32,
        (area.height() - gap * (STATS_ROWS - 1) as f32) / STATS_ROWS as f32,
    )
    .max(egui::Vec2::ZERO);
    for (statistic, [column, row]) in STATS {
        let rect = egui::Rect::from_min_size(
            area.min + egui::vec2(column as f32 * (cell.x + gap), row as f32 * (cell.y + gap)),
            cell,
        );
        let mut inside = ui.new_child(
            UiBuilder::new()
                .id_salt(("statistic", column, row))
                .max_rect(rect)
                .layout(egui::Layout::top_down(egui::Align::LEFT)),
        );
        inside.set_clip_rect(rect.intersect(ui.clip_rect()));
        let counted = &tally.kinds[statistic.index()];
        let picked = &mut filter.picked[statistic.index()];
        Frame::new()
            .fill(CARD_FILL)
            .corner_radius(CornerRadius::same(scale.px(6.0).round() as u8))
            .inner_margin(scale.margin(10.0, 8.0))
            .show(&mut inside, |ui| {
                ui.set_min_size(ui.available_size());
                ui.spacing_mut().item_spacing = egui::vec2(scale.px(8.0), scale.px(2.0));
                match statistic {
                    Statistic::Platforms | Statistic::Focus => {
                        show_pie(ui, statistic, counted, picked, scale);
                    }
                    Statistic::Countries => {
                        show_ranking(ui, statistic, counted, counted.players, picked, scale);
                    }
                    Statistic::Slot(_) => {
                        show_ranking(ui, statistic, counted, tally.loadouts, picked, scale);
                    }
                }
            });
    }
}

/// A cell's title, with a note on how much it counts against the right edge.
fn show_cell_title(ui: &mut egui::Ui, title: &str, note: &str, scale: Scale) {
    ui.horizontal(|ui| {
        whole(ui, single(title, 14.0, GOLD_TEXT, scale));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            whole(ui, single(note, 12.0, MUTED, scale));
        });
    });
    ui.add_space(scale.px(4.0));
}

/// A pie as large as the cell leaves room for above its legend, which scrolls when even the
/// smallest pie leaves it too little. Hovering a slice says what it is. A click on a slice, or
/// on its line in the legend, picks it; while anything is picked, the rest are dimmed.
fn show_pie(
    ui: &mut egui::Ui,
    statistic: Statistic,
    counted: &Counted,
    picked: &mut BTreeSet<String>,
    scale: Scale,
) {
    show_cell_title(
        ui,
        statistic.title(),
        &format!("{}人", counted.players),
        scale,
    );
    let shares = &counted.shares;
    if counted.players == 0 {
        ui.label(single("記録なし", 13.0, MUTED, scale));
        return;
    }
    let colours = shares
        .iter()
        .map(|share| {
            let colour = slice_color(statistic, &share.key);
            if picked.is_empty() || picked.contains(&share.key) {
                colour
            } else {
                colour.gamma_multiply(UNPICKED)
            }
        })
        .collect::<Vec<_>>();
    let height = share_line_height(ui, scale);
    let legend = shares.len() as f32 * (height + ui.spacing().item_spacing.y);
    let diameter = (ui.available_height() - legend - scale.px(PIE_GAP))
        .min(ui.available_width())
        .min(scale.px(PIE_MAX))
        .max(scale.px(PIE_MIN));
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), diameter), Sense::click());
    let (center, radius) = (rect.center(), diameter / 2.0);
    paint_pie(ui.painter(), center, radius, shares, &colours, scale);
    let hovered = response
        .hover_pos()
        .and_then(|pointer| slice_at(shares, pointer - center, radius));
    if let Some(index) = hovered {
        let share = &shares[index];
        let response = response
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text_at_pointer(format!(
                "{}\n{}人 ({})",
                share.label,
                share.count,
                percent(share.count, counted.players)
            ));
        if response.clicked() {
            toggle(picked, &share.key);
        }
    }
    ui.add_space(scale.px(PIE_GAP));
    ScrollArea::vertical()
        .id_salt("legend")
        .auto_shrink(false)
        .show(ui, |ui| {
            for (share, colour) in shares.iter().zip(colours) {
                let line = Line {
                    share,
                    out_of: counted.players,
                    lead: Lead::Swatch(colour),
                    flag: false,
                    bar: None,
                    picked: picked.contains(&share.key),
                };
                if show_share_line(ui, line, height, scale) {
                    toggle(picked, &share.key);
                }
            }
        });
}

/// The slices as a mesh of triangles fanned out from the centre, clockwise from the top, each
/// in its colour of `colours`. A mesh is not smoothed at its edges, so lines in the cell's
/// colour part the slices and trim the rim.
fn paint_pie(
    painter: &egui::Painter,
    center: egui::Pos2,
    radius: f32,
    shares: &[Share],
    colours: &[Color32],
    scale: Scale,
) {
    let total: usize = shares.iter().map(|share| share.count).sum();
    let point = |angle: f32| center + radius * egui::vec2(angle.cos(), angle.sin());
    let mut mesh = egui::Mesh::default();
    let mut edges = Vec::with_capacity(shares.len());
    let mut from = -FRAC_PI_2;
    for (share, colour) in shares.iter().zip(colours) {
        let sweep = TAU * share.count as f32 / total as f32;
        let steps = ((sweep / TAU * PIE_STEPS).ceil() as u32).max(1);
        let base = mesh.vertices.len() as u32;
        mesh.colored_vertex(center, *colour);
        for step in 0..=steps {
            mesh.colored_vertex(point(from + sweep * step as f32 / steps as f32), *colour);
        }
        for step in 0..steps {
            mesh.add_triangle(base, base + 1 + step, base + 2 + step);
        }
        edges.push(from);
        from += sweep;
    }
    painter.add(egui::Shape::mesh(mesh));
    let stroke = Stroke::new(scale.px(1.5), CARD_FILL);
    if shares.len() > 1 {
        for angle in edges {
            painter.line_segment([center, point(angle)], stroke);
        }
    }
    painter.circle_stroke(center, radius, stroke);
}

/// Which of the slices lies `offset` from the pie's centre, if any.
fn slice_at(shares: &[Share], offset: egui::Vec2, radius: f32) -> Option<usize> {
    let total: usize = shares.iter().map(|share| share.count).sum();
    if total == 0 || offset.length() > radius {
        return None;
    }
    // Round from the top, clockwise, as the slices are laid.
    let turned = (offset.y.atan2(offset.x) + FRAC_PI_2).rem_euclid(TAU) / TAU;
    let mut until = 0;
    shares.iter().position(|share| {
        until += share.count;
        turned < until as f32 / total as f32
    })
}

/// A ranking, most first, each line with its rank (and a country's flag), how many players it
/// counts and what share of `out_of` that is, over a bar as long as its share of the first's. A
/// click on a line picks it. Only the lines on screen are laid out.
fn show_ranking(
    ui: &mut egui::Ui,
    statistic: Statistic,
    counted: &Counted,
    out_of: usize,
    picked: &mut BTreeSet<String>,
    scale: Scale,
) {
    let shares = &counted.shares;
    let note = match statistic {
        Statistic::Countries => format!("{}か国", shares.len()),
        _ => format!("{}種", shares.len()),
    };
    show_cell_title(ui, statistic.title(), &note, scale);
    let Some(first) = shares.first() else {
        ui.label(single("記録なし", 13.0, MUTED, scale));
        return;
    };
    let most = first.count as f32;
    let height = share_line_height(ui, scale);
    ScrollArea::vertical()
        .id_salt("ranking")
        .auto_shrink(false)
        .show_rows(ui, height, shares.len(), |ui, range| {
            for share in &shares[range] {
                let line = Line {
                    share,
                    out_of,
                    // Those counted as often share a rank.
                    lead: Lead::Rank(shares.partition_point(|other| other.count > share.count) + 1),
                    flag: statistic == Statistic::Countries,
                    bar: Some(share.count as f32 / most),
                    picked: picked.contains(&share.key),
                };
                if show_share_line(ui, line, height, scale) {
                    toggle(picked, &share.key);
                }
            }
        });
}

/// How tall a line of a legend or a ranking stands.
fn share_line_height(ui: &egui::Ui, scale: Scale) -> f32 {
    ui.fonts_mut(|fonts| fonts.layout_job(single("M", 13.0, TEXT, scale)).size().y) + scale.px(6.0)
}

/// What a line of a legend or a ranking opens with.
enum Lead {
    Swatch(Color32),
    Rank(usize),
}

/// One line of a legend or a ranking.
struct Line<'a> {
    share: &'a Share,
    /// What its share is a share of.
    out_of: usize,
    lead: Lead,
    /// Whether a flag stands before the label, the share's key then being a country's code.
    flag: bool,
    /// How far a bar behind the line runs, as a share of the line's length.
    bar: Option<f32>,
    /// Whether it is picked to narrow the list by.
    picked: bool,
}

/// One line of a legend or a ranking: its lead, the label, then how many and what share
/// against the right edge. A label too long for its room is cut short, the whole of it then a
/// hover away, and a picked line is outlined in gold. Says whether it was clicked.
fn show_share_line(ui: &mut egui::Ui, line: Line, height: f32, scale: Scale) -> bool {
    let Line {
        share,
        out_of,
        lead,
        flag,
        bar,
        picked,
    } = line;
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), height), Sense::click());
    let layout = |job: LayoutJob| ui.fonts_mut(|fonts| fonts.layout_job(job));
    let lead_width = scale.px(RANK_WIDTH);
    // Every line of a ranking of countries keeps room for a flag, one the icons lack included,
    // so the names line up.
    let flag_width = if flag { scale.px(FLAG_SIZE + 6.0) } else { 0.0 };
    let share_right = rect.right() - scale.px(4.0);
    let count_right = share_right - scale.px(SHARE_WIDTH);
    let label_left = rect.left() + lead_width + flag_width;
    let colour = if picked { GOLD_TEXT } else { TEXT };
    let proportion = layout(single(&percent(share.count, out_of), 12.0, MUTED, scale));
    let count = layout(single(&share.count.to_string(), 13.0, colour, scale));
    let mut job = single(&share.label, 13.0, colour, scale);
    job.wrap = TextWrapping::truncate_at_width(
        (count_right - scale.px(COUNT_WIDTH) - label_left).max(0.0),
    );
    let label = layout(job);
    let elided = label.elided;
    let rank = match lead {
        Lead::Rank(rank) => Some(layout(single(&rank.to_string(), 12.0, MUTED, scale))),
        Lead::Swatch(_) => None,
    };

    let painter = ui.painter();
    let middle =
        |size: egui::Vec2, right: f32| egui::pos2(right - size.x, rect.center().y - size.y / 2.0);
    let corner = CornerRadius::same(scale.px(3.0).round() as u8);
    if let Some(bar) = bar {
        painter.rect_filled(
            egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * bar, rect.height())),
            corner,
            BAR,
        );
    }
    if picked {
        painter.rect_stroke(
            rect,
            corner,
            Stroke::new(scale.px(1.0), OWN_STROKE),
            StrokeKind::Inside,
        );
    } else if response.hovered() {
        painter.rect_filled(rect, corner, HOVERED);
    }
    match (lead, rank) {
        (Lead::Swatch(colour), _) => {
            let swatch = egui::Rect::from_center_size(
                egui::pos2(rect.left() + lead_width / 2.0, rect.center().y),
                egui::Vec2::splat(scale.px(10.0)),
            );
            painter.rect_filled(
                swatch,
                CornerRadius::same(scale.px(2.0).round() as u8),
                colour,
            );
        }
        (Lead::Rank(_), Some(rank)) => {
            let at = middle(rank.size(), rect.left() + lead_width - scale.px(8.0));
            painter.galley(at, rank, MUTED);
        }
        (Lead::Rank(_), None) => {}
    }
    if flag && let Some((uri, bytes)) = flag_icon_bytes(&share.key) {
        let at = egui::Rect::from_min_size(
            egui::pos2(
                rect.left() + lead_width,
                rect.center().y - scale.px(FLAG_SIZE) / 2.0,
            ),
            egui::Vec2::splat(scale.px(FLAG_SIZE)),
        );
        egui::Image::from_bytes(uri, bytes).paint_at(ui, at);
    }
    painter.galley(
        middle(label.size(), label_left + label.size().x),
        label,
        colour,
    );
    painter.galley(middle(count.size(), count_right), count, colour);
    painter.galley(middle(proportion.size(), share_right), proportion, MUTED);
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    let clicked = response.clicked();
    if elided {
        response.on_hover_text(&share.label);
    }
    clicked
}

/// `count` as a share of `out_of`, to a tenth of a percent.
fn percent(count: usize, out_of: usize) -> String {
    format!("{:.1}%", count as f64 * 100.0 / out_of.max(1) as f64)
}

/// What the history adds up to, for the statistics beside its list. Everyone it keeps is
/// counted as often as they were met.
#[derive(Debug, Default)]
struct Tally {
    /// Players whose loadout the history keeps: what the gear's shares are out of.
    loadouts: usize,
    /// Each kind of statistic, added up, in the order of `STATS` (`Statistic::index`).
    kinds: [Counted; KINDS],
}

/// One kind of statistic, added up.
#[derive(Debug, Default)]
struct Counted {
    /// The players it knows something of: every one for the platforms, those whose country
    /// is known for the countries, and so on.
    players: usize,
    /// Most first, and those counted as often in the order of their labels.
    shares: Vec<Share>,
}

/// How many players something was counted for.
#[derive(Debug, PartialEq, Eq)]
struct Share {
    /// What a player is matched on (`Statistic::key`).
    key: String,
    label: String,
    count: usize,
}

fn tally<'a>(entries: impl IntoIterator<Item = &'a HistoryEntry>) -> Tally {
    // Each key's count, and the first player counted under it, for its label.
    let mut counts: [HashMap<&str, (usize, &HistoryEntry)>; KINDS] = Default::default();
    let mut loadouts = 0;
    for entry in entries {
        loadouts += usize::from(entry.view.loadout.is_some());
        for (statistic, _) in STATS {
            if let Some(key) = statistic.key(entry) {
                counts[statistic.index()].entry(key).or_insert((0, entry)).0 += 1;
            }
        }
    }
    let kinds = std::array::from_fn(|index| {
        let (statistic, _) = STATS[index];
        let mut shares = std::mem::take(&mut counts[index])
            .into_iter()
            .map(|(key, (count, entry))| Share {
                key: key.to_owned(),
                label: statistic.label(key, entry),
                count,
            })
            .collect::<Vec<_>>();
        shares.sort_by(most_first);
        Counted {
            players: shares.iter().map(|share| share.count).sum(),
            shares,
        }
    });
    Tally { loadouts, kinds }
}

/// Most counted first, and those counted as often in the order of their labels.
fn most_first(one: &Share, other: &Share) -> std::cmp::Ordering {
    other
        .count
        .cmp(&one.count)
        .then_with(|| one.label.cmp(&other.label))
}

impl Statistic {
    /// Where it stands in `STATS`, and so in `Tally::kinds` and `Filter::picked`.
    fn index(self) -> usize {
        match self {
            Statistic::Platforms => 0,
            Statistic::Countries => 1,
            Statistic::Focus => 2,
            Statistic::Slot(slot) => 3 + slot,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Statistic::Platforms => "プラットフォーム",
            Statistic::Countries => "国",
            Statistic::Focus => "フォーカス",
            Statistic::Slot(slot) => GEAR_SLOTS[slot],
        }
    }

    /// What a player is counted under, if anything: their platform; their country's code, or
    /// its name where the code is unknown; the path of their focus school; or the name the
    /// arsenal gives what fills the slot.
    fn key(self, entry: &HistoryEntry) -> Option<&str> {
        let view = &entry.view;
        match self {
            Statistic::Platforms => Some(view.platform.as_str()).filter(|key| !key.is_empty()),
            Statistic::Countries => [&view.country_code, &view.country]
                .into_iter()
                .map(String::as_str)
                .find(|key| !key.is_empty()),
            Statistic::Focus => view.loadout.as_ref()?.operator.as_ref()?.focus.as_deref(),
            Statistic::Slot(slot) => items(view.loadout.as_ref()?)[slot].as_ref().map(item_label),
        }
    }

    /// What a key is shown as, `entry` being a player counted under it: a country by its name
    /// and a focus school by the table's, anything else as it is.
    fn label(self, key: &str, entry: &HistoryEntry) -> String {
        match self {
            Statistic::Countries if !entry.view.country.is_empty() => entry.view.country.clone(),
            Statistic::Focus => dictionary_name(key).to_owned(),
            _ => key.to_owned(),
        }
    }
}

/// A slice's colour: a platform in the colour the overlay writes it in, a focus school in one
/// of its own.
fn slice_color(statistic: Statistic, key: &str) -> Color32 {
    match statistic {
        Statistic::Platforms => platform_color(key),
        Statistic::Focus => focus_color(key),
        _ => MUTED,
    }
}

/// Each focus school in a colour of its own, the school told by the folder its path runs
/// through.
fn focus_color(path: &str) -> Color32 {
    match path.rsplit('/').nth(1) {
        Some("Attack") => Color32::from_rgb(232, 104, 72), // Madurai
        Some("Defense") => Color32::from_rgb(76, 190, 200), // Vazarin
        Some("Power") => Color32::from_rgb(132, 128, 240), // Zenurik
        Some("Tactic") => Color32::from_rgb(232, 196, 84), // Naramon
        Some("Ward") => Color32::from_rgb(176, 140, 100),  // Unairu
        _ => MUTED,
    }
}

/// What the history's list is narrowed to: the players whose name holds what is typed over
/// its column, and whose mission's node holds what is typed over that one - by the node's name
/// or its id, `SolNode228`, either way ignoring case - whose mission was of the type picked
/// over its column, and who, in every kind of statistic with anything picked, match one of the
/// picks. With nothing typed or picked, everyone.
#[derive(Clone, Debug, Default, PartialEq)]
struct Filter {
    name: String,
    node: String,
    /// By id, `MT_LANDSCAPE`.
    mission_type: Option<String>,
    /// The keys picked in each kind of statistic (`Statistic::index`).
    picked: [BTreeSet<String>; KINDS],
}

impl Filter {
    fn is_empty(&self) -> bool {
        self.name.trim().is_empty()
            && self.node.trim().is_empty()
            && self.mission_type.is_none()
            && self.picked.iter().all(BTreeSet::is_empty)
    }

    /// The entries of the history it lets through, by their place in it.
    fn apply(&self, history: &[HistoryEntry]) -> Vec<usize> {
        let needle = |typed: &str| typed.trim().to_lowercase();
        let (name, node) = (needle(&self.name), needle(&self.node));
        let holds = |text: &str, needle: &str| text.to_lowercase().contains(needle);
        history
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                (name.is_empty() || holds(&entry.view.name, &name))
                    && (node.is_empty()
                        || !entry.location.is_empty()
                            && (holds(&entry.location, &node)
                                || names::node_name(&entry.location)
                                    .is_some_and(|named| holds(named, &node))))
                    && self
                        .mission_type
                        .as_ref()
                        .is_none_or(|picked| entry.mission_type == *picked)
                    && STATS.iter().all(|(statistic, _)| {
                        let picked = &self.picked[statistic.index()];
                        picked.is_empty()
                            || statistic.key(entry).is_some_and(|key| picked.contains(key))
                    })
            })
            .map(|(index, _)| index)
            .collect()
    }
}

/// Picks `key`, or lets it go if it was picked.
fn toggle(picked: &mut BTreeSet<String>, key: &str) {
    if !picked.remove(key) {
        picked.insert(key.to_owned());
    }
}

/// One player of the history: who they were, the mission they were tied to, and when the squad
/// came apart. Says whether it has just been asked for.
fn show_history_row(
    ui: &mut egui::Ui,
    entry: &HistoryEntry,
    chosen: Option<&HistoryEntry>,
    scale: Scale,
) -> bool {
    let open = chosen.is_some_and(|chosen| same(chosen, entry));
    let mut cut_short = false;
    let row = Frame::new()
        .fill(if open {
            CARD_FILL
        } else {
            Color32::TRANSPARENT
        })
        .corner_radius(CornerRadius::same(scale.px(4.0).round() as u8))
        .inner_margin(scale.margin(ROW_MARGIN[0], ROW_MARGIN[1]))
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
                let [node, mission_type] = mission_labels(entry);
                let colour = if entry.location.is_empty() {
                    MUTED
                } else {
                    LOCATION
                };
                cut_short |= column(ui, single(&node, 13.0, colour, scale), scale.px(NODE_WIDTH));
                cut_short |= column(
                    ui,
                    single(&mission_type, 13.0, colour, scale),
                    scale.px(MISSION_TYPE_WIDTH),
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
    let response = ui
        .interact(
            row.response.rect,
            ui.id().with((entry.at, entry.view.name.as_str())),
            Sense::click(),
        )
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let clicked = response.clicked();
    // The mission's names in full, and the ids the log gave, where a column cut them short.
    if cut_short {
        let [node, mission_type] = mission_labels(entry);
        response.on_hover_text(format!(
            "{node}
{mission_type}
{} / {}",
            entry.location, entry.mission_type
        ));
    }
    clicked
}

/// The node and the type of the mission a player of the history was tied to, as the list
/// names them: as the export does, by the id EE.log gave where the export lacks it, and `—`
/// where they were tied to none.
fn mission_labels(entry: &HistoryEntry) -> [String; 2] {
    let label = |id: &str, name: Option<&str>| {
        if id.is_empty() {
            "—".to_owned()
        } else {
            name.unwrap_or(id).to_owned()
        }
    };
    [
        label(&entry.location, names::node_name(&entry.location)),
        label(
            &entry.mission_type,
            names::mission_type_name(&entry.mission_type),
        ),
    ]
}

/// A cell of the history's list, cut short where it does not fit its column. Says whether it
/// was.
fn column(ui: &mut egui::Ui, mut job: LayoutJob, width: f32) -> bool {
    job.wrap = TextWrapping::truncate_at_width(width);
    let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
    let elided = galley.elided;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, galley.size().y), Sense::hover());
    ui.painter().galley(rect.left_top(), galley, TEXT);
    elided
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
    fn opens_a_window_at_the_pixels_it_was_left_at_whatever_the_monitors_scale() {
        // The overlay has zoomed out for a game at 1920x1080. The window was left at 100%,
        // on a monitor to the right of a primary at 150%.
        let scale = Scale(1.0 / 0.75);
        let left_at = [3948.0, 641.0];
        let measured = scale.to_points(left_at, 1.0);
        let stored = scale.to_pixels(measured, 1.0);
        assert_eq!(stored, left_at);

        // egui-winit turns the position asked for into pixels at zoom times the scale of
        // the monitor the window is on, here still the primary at 150%, and winit rounds.
        let asked = scale.to_points(stored, 1.5);
        assert_eq!(asked.map(|v| (v * 0.75 * 1.5).round()), left_at);
    }

    const MADURAI: &str = "/Lotus/Upgrades/Focus/Attack/AttackFocusAbility";
    const ZENURIK: &str = "/Lotus/Upgrades/Focus/Power/PowerFocusAbility";
    const SARYN: &str = "/Lotus/Powersuits/Saryn/Saryn";
    const MESA: &str = "/Lotus/Powersuits/Cowgirl/Cowgirl";
    const KAVAT: &str = "/Lotus/Types/Game/CatbrowPet/CheshireCatbrowPetPowerSuit";

    fn met(
        name: &str,
        platform: &str,
        [country_code, country]: [&str; 2],
        focus: Option<&str>,
        warframe: &str,
        companion: Option<&str>,
    ) -> HistoryEntry {
        HistoryEntry {
            at: 0,
            when: String::new(),
            file: String::new(),
            location: String::new(),
            mission_type: String::new(),
            view: LoadoutView {
                name: name.to_owned(),
                platform: platform.to_owned(),
                country_code: country_code.to_owned(),
                country: country.to_owned(),
                loadout: Some(Loadout {
                    warframe: item(warframe, None, None),
                    companion: companion.and_then(|path| item(path, None, None)),
                    operator: focus.map(|path| Operator {
                        drifter: true,
                        focus: Some(path.to_owned()),
                    }),
                    ..Loadout::default()
                }),
                ..LoadoutView::default()
            },
        }
    }

    /// Five departures, one player among them met twice.
    fn departures() -> [HistoryEntry; 5] {
        const US: [&str; 2] = ["US", "United States of America"];
        const JP: [&str; 2] = ["JP", "Japan"];
        [
            met("Ordis", "PS", JP, Some(ZENURIK), MESA, None),
            met("Tenno", "PC", US, Some(MADURAI), SARYN, Some(KAVAT)),
            met("Lotus", "PC", JP, Some(ZENURIK), SARYN, None),
            // Written down before the operator and the country were: counted for neither.
            met("Teshin", "Xbox", ["", ""], None, MESA, None),
            met("Tenno", "PC", US, Some(MADURAI), SARYN, None),
        ]
    }

    #[test]
    fn adds_the_history_up_most_first() {
        let tally = tally(&departures());

        let counts = |statistic: Statistic| {
            let counted = &tally.kinds[statistic.index()];
            let shares = counted
                .shares
                .iter()
                .map(|share| (share.key.clone(), share.count))
                .collect::<Vec<_>>();
            (counted.players, shares)
        };
        let owned = |shares: &[(&str, usize)]| {
            shares
                .iter()
                .map(|(key, count)| (key.to_string(), *count))
                .collect::<Vec<_>>()
        };
        assert_eq!(tally.loadouts, 5);
        assert_eq!(
            counts(Statistic::Platforms),
            (5, owned(&[("PC", 3), ("PS", 1), ("Xbox", 1)])),
            "most first, and those met as often in the order of their names"
        );
        assert_eq!(
            counts(Statistic::Countries),
            (4, owned(&[("JP", 2), ("US", 2)])),
            "counted by code, and only where it is known"
        );
        assert_eq!(
            tally.kinds[Statistic::Countries.index()].shares[1].label,
            "United States of America",
            "shown by name"
        );
        assert_eq!(
            counts(Statistic::Focus),
            (4, owned(&[(MADURAI, 2), (ZENURIK, 2)]))
        );
        let (players, warframes) = counts(Statistic::Slot(0));
        assert_eq!(players, 5);
        assert_eq!(
            warframes
                .iter()
                .map(|(_, count)| *count)
                .collect::<Vec<_>>(),
            [3, 2],
            "Saryn three times, Mesa twice"
        );
        assert_eq!(
            counts(Statistic::Slot(4)).0,
            1,
            "an empty companion slot is not counted"
        );
    }

    #[test]
    fn keeps_the_statistics_in_the_order_they_are_indexed_by() {
        for (index, (statistic, _)) in STATS.iter().enumerate() {
            assert_eq!(statistic.index(), index, "{statistic:?}");
        }
    }

    #[test]
    fn narrows_the_list_to_the_picks_and_the_name() {
        let history = departures();
        let platforms = Statistic::Platforms.index();
        let mut filter = Filter::default();
        assert!(filter.is_empty());
        assert_eq!(
            filter.apply(&history),
            [0, 1, 2, 3, 4],
            "nothing picked lets everyone through"
        );

        toggle(&mut filter.picked[platforms], "PC");
        toggle(&mut filter.picked[platforms], "PS");
        assert_eq!(filter.apply(&history), [0, 1, 2, 4], "either platform");

        toggle(&mut filter.picked[Statistic::Countries.index()], "JP");
        assert_eq!(filter.apply(&history), [0, 2], "and from Japan too");

        filter.name = " LOT ".to_owned();
        assert_eq!(
            filter.apply(&history),
            [2],
            "and a name holding what is typed, whatever its case"
        );

        toggle(&mut filter.picked[platforms], "PC");
        assert!(
            filter.apply(&history).is_empty(),
            "a toggle picked again lets it go: Lotus plays on PC"
        );
        assert!(!filter.is_empty());

        let mut filter = Filter::default();
        toggle(&mut filter.picked[Statistic::Focus.index()], MADURAI);
        assert_eq!(filter.apply(&history), [1, 4]);

        let mut filter = Filter::default();
        let mesa = Statistic::Slot(0).key(&history[0]).unwrap();
        toggle(&mut filter.picked[Statistic::Slot(0).index()], mesa);
        assert_eq!(filter.apply(&history), [0, 3]);
    }

    #[test]
    fn adds_up_only_what_the_list_shows_when_asked() {
        let history: Arc<[HistoryEntry]> = Arc::from(departures());
        let mut shown = Shown {
            history: Arc::clone(&history),
            ..Shown::default()
        };
        let platforms = Statistic::Platforms.index();
        let players = |shown: &Shown| {
            let (_, _, tally) = shown.tally.as_ref().unwrap();
            (tally.kinds[platforms].players, tally.loadouts)
        };
        toggle(&mut shown.filter.picked[platforms], "PC");

        refresh_tally(&mut shown, &history);
        assert_eq!(
            players(&shown),
            (5, 5),
            "every entry, whatever the list shows"
        );

        shown.scope = Scope::Listed;
        refresh_tally(&mut shown, &history);
        assert_eq!(players(&shown), (3, 3), "only the three on PC");

        toggle(&mut shown.filter.picked[platforms], "PS");
        refresh_tally(&mut shown, &history);
        assert_eq!(
            players(&shown),
            (4, 4),
            "added up again as the filter changes"
        );
    }

    #[test]
    fn narrows_the_list_to_a_node_and_a_mission_type() {
        let mut history = departures();
        let mut tie = |index: usize, location: &str, mission_type: &str| {
            history[index].location = location.to_owned();
            history[index].mission_type = mission_type.to_owned();
        };
        tie(0, "SolNode228", "MT_LANDSCAPE");
        tie(1, "SolNode27", "MT_EXTERMINATION");
        tie(2, "EventNode12", "MT_SURVIVAL");
        // The last two were written down before missions were.

        let mut filter = Filter::default();
        for (typed, expected, why) in [
            (" eidolon ", &[0][..], "by the node's name, ignoring case"),
            ("EARTH", &[0, 1][..], "the system is in the name"),
            ("solnode27", &[1][..], "or by its id"),
            ("eventnode", &[2][..], "a node the export lacks, by its id"),
        ] {
            filter.node = typed.to_owned();
            assert_eq!(filter.apply(&history), expected, "{why}");
        }

        filter.node.clear();
        filter.mission_type = Some("MT_EXTERMINATION".to_owned());
        assert_eq!(filter.apply(&history), [1]);
        assert!(!filter.is_empty());
    }

    #[test]
    fn finds_the_slice_under_the_pointer() {
        let share = |key: &str, count| Share {
            key: key.to_owned(),
            label: key.to_owned(),
            count,
        };
        let shares = [share("PC", 3), share("PS", 1)];
        // Laid clockwise from the top: the first three quarters, then the last.
        assert_eq!(slice_at(&shares, egui::vec2(1.0, -10.0), 20.0), Some(0));
        assert_eq!(slice_at(&shares, egui::vec2(10.0, 1.0), 20.0), Some(0));
        assert_eq!(slice_at(&shares, egui::vec2(-1.0, 10.0), 20.0), Some(0));
        assert_eq!(slice_at(&shares, egui::vec2(-10.0, -1.0), 20.0), Some(1));
        assert_eq!(
            slice_at(&shares, egui::vec2(30.0, 0.0), 20.0),
            None,
            "off the pie"
        );
    }

    #[test]
    fn drops_a_placement_no_monitor_shows() {
        let placement = |x, y| Placement {
            x,
            y,
            width: 820.0,
            height: 900.0,
        };
        // The primary monitor's corner is where the desktop's pixels count from.
        assert!(on_screen(placement(0.0, 0.0)));
        assert!(!on_screen(placement(-100_000.0, -100_000.0)));
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
    fn writes_a_grid_cards_name_a_little_smaller_than_its_room() {
        let row = LoadoutView {
            name: "Tenno".to_owned(),
            ..LoadoutView::default()
        };
        let job = grid_name_job(&row, 30.0, Scale(1.0));

        assert_eq!(job.sections[0].format.font_id.size, 27.0);
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
