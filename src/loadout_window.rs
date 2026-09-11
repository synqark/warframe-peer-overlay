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

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use eframe::egui::{
    self, Color32, CornerRadius, Frame, Galley, IconData, Margin, ScrollArea, Sense, Stroke,
    TextFormat, ViewportBuilder, ViewportClass, ViewportCommand, ViewportId, text::LayoutJob,
};
use warframe_peer_overlay::{
    loadout::{Item, Loadout},
    monitor::LoadoutView,
    names, tray,
};

use crate::{platform_color, text_format};

const TITLE: &str = "Loadouts - Warframe Peer Overlay";
/// The size the window first opens at; the user is free to resize it from there.
const INITIAL_SIZE: [f32; 2] = [980.0, 440.0];
const MIN_SIZE: [f32; 2] = [360.0, 160.0];

const BACKGROUND: Color32 = Color32::from_rgb(10, 14, 20);
const CARD_FILL: Color32 = Color32::from_rgb(28, 35, 46);
/// Our own card is framed in the overlay's gold.
const OWN_STROKE: Color32 = Color32::from_rgb(194, 163, 87);
const GOLD_TEXT: Color32 = Color32::from_rgb(224, 194, 112);
const TEXT: Color32 = Color32::from_rgb(226, 230, 236);
const MUTED: Color32 = Color32::from_rgb(130, 138, 150);
const QUEST_DONE: Color32 = Color32::from_rgb(92, 200, 142);

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

#[derive(Default)]
pub struct LoadoutWindow {
    /// Raised from the tray thread, taken on the next pass.
    show_request: Arc<AtomicBool>,
    /// Fixed once the window first opens: egui patches a window whose builder changes, and a
    /// size worked out again under another zoom would undo the user's resizing.
    builder: Option<ViewportBuilder>,
    visible: bool,
    rows: Vec<LoadoutView>,
}

impl LoadoutWindow {
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
        let id = ViewportId::from_hash_of("loadouts");
        if self.show_request.swap(false, Ordering::Relaxed) {
            if self.builder.is_none() {
                self.builder = Some(builder(Scale::of(context)));
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
                show_cards(ui, &self.rows);
            }
        });
    }
}

fn builder(scale: Scale) -> ViewportBuilder {
    let (rgba, width, height) = tray::icon_rgba();
    ViewportBuilder::default()
        .with_title(TITLE)
        .with_inner_size(scale.size(INITIAL_SIZE))
        .with_min_inner_size(scale.size(MIN_SIZE))
        .with_icon(IconData {
            rgba,
            width,
            height,
        })
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

    fn size(self, [width, height]: [f32; 2]) -> [f32; 2] {
        [self.px(width), self.px(height)]
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
    /// `None` while the loadout is not captured.
    gear: Option<Vec<Cell>>,
}

struct Cell {
    galley: Arc<Galley>,
    tooltip: Option<String>,
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
                for card in &cards {
                    Frame::new()
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
                            show_line(ui, &card.header, &header_widths);
                            match &card.gear {
                                Some(gear) => show_line(ui, gear, &gear_widths),
                                None => {
                                    ui.label(missing.clone());
                                }
                            }
                        });
                    ui.add_space(scale.px(4.0));
                }
            });
        });
}

fn lay_out(ui: &egui::Ui, row: &LoadoutView, scale: Scale) -> Card {
    let cell = |job: LayoutJob, tooltip: Option<String>| Cell {
        galley: ui.fonts_mut(|fonts| fonts.layout_job(job)),
        tooltip,
    };
    let single = |text: &str, size: f32, color: Color32| {
        LayoutJob::single_section(text.to_owned(), scale.text(size, color))
    };
    let loadout = row.loadout.as_ref();

    let mut name = LayoutJob::default();
    if row.name.is_empty() {
        name.append("自分", 0.0, scale.text(15.0, GOLD_TEXT));
    } else {
        let color = if row.is_local { GOLD_TEXT } else { TEXT };
        name.append(&row.name, 0.0, scale.text(15.0, color));
        if row.is_local {
            name.append("自分", scale.px(8.0), scale.text(11.0, MUTED));
        }
    }
    let mastery = loadout
        .and_then(|loadout| loadout.mastery_rank)
        .map(|rank| format!("MR {rank}"))
        .unwrap_or_default();
    let quests = [
        loadout.and_then(|loadout| loadout.post_new_war),
        loadout.and_then(|loadout| loadout.post_old_peace),
    ];
    let mut header = vec![
        cell(name, None),
        cell(single(&mastery, 14.0, GOLD_TEXT), None),
        cell(
            single(&row.platform, 14.0, platform_color(&row.platform)),
            None,
        ),
    ];
    for ((label, quest), done) in QUESTS.into_iter().zip(quests) {
        let job = match done {
            Some(true) => single(&format!("✔ {label}"), 13.0, QUEST_DONE),
            Some(false) => single(&format!("✖ {label}"), 13.0, MUTED),
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
        header.push(cell(job, tooltip));
    }

    let gear = loadout.map(|loadout| {
        gear(loadout)
            .into_iter()
            .zip(GEAR_SLOTS)
            .map(|((value, details), slot)| {
                let mut job = LayoutJob::default();
                job.append(slot, 0.0, scale.text(11.0, MUTED));
                job.append(&value, scale.px(6.0), scale.text(14.0, TEXT));
                cell(job, details)
            })
            .collect()
    });

    Card {
        is_local: row.is_local,
        header,
        gear,
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
    ui.horizontal_wrapped(|ui| {
        for (cell, &width) in cells.iter().zip(widths) {
            let (rect, response) =
                ui.allocate_exact_size(egui::vec2(width, cell.galley.size().y), Sense::hover());
            ui.painter()
                .galley(rect.left_top(), cell.galley.clone(), TEXT);
            if let Some(tooltip) = &cell.tooltip {
                response.on_hover_text(tooltip);
            }
        }
    });
}

/// The equipment line in `GEAR_SLOTS` order, as `(value, hover details)`, with `—` for an
/// empty slot.
fn gear(loadout: &Loadout) -> [(String, Option<String>); 5] {
    let slot = |item: &Option<Item>| match item {
        Some(item) => (item_label(item).to_owned(), Some(item_details(item))),
        None => ("—".to_owned(), None),
    };
    let mut companion = slot(&loadout.companion);
    if loadout.companion.is_some()
        && let Some(name) = &loadout.companion_name
    {
        companion.0 = format!("{} ({name})", companion.0);
    }
    [
        slot(&loadout.warframe),
        slot(&loadout.primary),
        slot(&loadout.secondary),
        slot(&loadout.melee),
        companion,
    ]
}

/// The name the arsenal shows for the item, or the last segment of its internal path when the
/// names table does not know it (or was built without the submodule).
fn item_label(item: &Item) -> &str {
    names::item_name(&item.path, &item.parts).unwrap_or_else(|| path_tail(&item.path))
}

/// Hover details: the full internal path, a modular item's parts, then rank and forma where
/// known.
fn item_details(item: &Item) -> String {
    let mut lines = vec![item.path.clone()];
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
        })
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
            "/Lotus/Weapons/Ostron/Melee/LotusModularWeapon\nVargeet Jai II / Korb / Dokrahm\nランク 30"
        );
    }

    #[test]
    fn falls_back_to_the_end_of_the_path_for_an_item_the_table_lacks() {
        let suit = item("/Example/Suits/ExampleSuit", Some(30), Some(2)).unwrap();
        assert_eq!(item_label(&suit), "ExampleSuit");
        assert_eq!(
            item_details(&suit),
            "/Example/Suits/ExampleSuit\nランク 30 / フォーマ 2"
        );

        let bare = item("/Example/Rifle", None, None).unwrap();
        assert_eq!(item_details(&bare), "/Example/Rifle");
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
