//! The tray icon and its context menu, running on a dedicated thread.
//!
//! The thread is not an implementation detail — it is the whole point. `tray-icon` shows
//! the context menu with `TrackPopupMenu`, which runs a modal message loop on whichever
//! thread owns the tray window. When that thread is winit's (as it is under eframe), the
//! modal loop never receives mouse or keyboard input: the menu pops up, but no item ever
//! highlights and choosing one does nothing at all. Owning the tray on a separate thread
//! with a plain `GetMessageW` pump makes the menu behave normally.
//!
//! Verified by bisecting: a bare Win32 pump works, the same menu under eframe does not,
//! and moving it to its own thread under eframe works again.

use std::ptr;

use tray_icon::{
    Icon, TrayIconBuilder,
    menu::{
        CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
    },
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, TranslateMessage,
};

use crate::lang::{self, Language};

/// A choice from the tray menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Bring up the loadout window.
    ShowLoadouts,
    /// Bring up the one that lists the mods on every slot.
    ShowFullLoadouts,
    /// Bring up the one that lays six places out in two rows of three.
    ShowGridLoadouts,
    /// Bring up the one that keeps the players met before.
    ShowHistory,
    /// Bring up the one that shows the missions loaded lately, for debugging.
    ShowSession,
    /// Write the chosen language down and start the overlay again in it. Handed over even
    /// when it is the language already in force, so the caller decides what that is worth.
    SetLanguage(Language),
    Exit,
}

/// The windows the menu opens, in the order it offers them.
const WINDOWS: [(&str, Command); 5] = [
    ("Loadouts", Command::ShowLoadouts),
    ("Loadouts (full)", Command::ShowFullLoadouts),
    ("Loadouts (6grid)", Command::ShowGridLoadouts),
    ("History", Command::ShowHistory),
    ("Session (debug)", Command::ShowSession),
];

/// Shows the tray icon and hands each menu choice to `on_command`, until the user picks
/// `Exit`, which is the last one handed over.
///
/// Returns immediately; the icon lives on a background thread that exits with the process.
pub fn spawn(on_command: impl Fn(Command) + Send + 'static) {
    std::thread::spawn(move || run(on_command));
}

/// The menu, and what the pump needs to turn a choice back into a `Command`.
struct TrayMenu {
    menu: Menu,
    /// Which command each plain item stands for. The menu keeps the items themselves alive,
    /// so only their ids are held here.
    items: Vec<(MenuId, Command)>,
    /// The language items, kept so that a choice can set their ticks, in the order of
    /// `Language::ALL`.
    languages: [CheckMenuItem; 2],
}

/// Builds the menu with `chosen` ticked under `Language`. Apart from the pump, so that a test
/// can look at what it holds without an icon in the notification area or a message loop.
fn build_menu(chosen: Language) -> TrayMenu {
    let windows = WINDOWS.map(|(label, command)| (MenuItem::new(label, true, None), command));
    let languages = Language::ALL
        .map(|language| CheckMenuItem::new(language.label(), true, language == chosen, None));
    let language_menu = Submenu::new("Language", true);
    language_menu
        .append_items(&[&languages[0], &languages[1]])
        .expect("failed to build the language menu");
    let exit = MenuItem::new("Exit", true, None);
    // Bound rather than built inline: `append_items` borrows every entry, and a temporary
    // would not live that long.
    let (above, below) = (
        PredefinedMenuItem::separator(),
        PredefinedMenuItem::separator(),
    );

    let mut entries: Vec<&dyn IsMenuItem> = windows
        .iter()
        .map(|(item, _)| item as &dyn IsMenuItem)
        .collect();
    entries.extend([
        &above as &dyn IsMenuItem,
        &language_menu,
        &below,
        &exit as &dyn IsMenuItem,
    ]);
    let menu = Menu::new();
    menu.append_items(&entries)
        .expect("failed to build tray menu");

    let items = windows
        .iter()
        .map(|(item, command)| (item.id().clone(), *command))
        .chain(std::iter::once((exit.id().clone(), Command::Exit)))
        .collect();
    TrayMenu {
        menu,
        items,
        languages,
    }
}

fn run(on_command: impl Fn(Command)) {
    // Ticked against the language in force when the menu is built. A change restarts the
    // overlay, so the process that follows builds the menu again with the other one ticked.
    let in_force = lang::current();
    let TrayMenu {
        menu,
        items,
        languages,
    } = build_menu(in_force);

    // Bound to a name for the lifetime of the pump: dropping the handle would take the
    // icon out of the notification area and leave the overlay with no way to quit.
    let _tray = TrayIconBuilder::new()
        .with_tooltip("Warframe Peer Overlay")
        .with_menu(Box::new(menu))
        .with_icon(icon())
        .build()
        .expect("failed to create tray icon");

    unsafe {
        let mut message: MSG = std::mem::zeroed();
        while GetMessageW(&mut message, ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
            // muda turns the menu selection into a `WM_COMMAND` handled by the dispatch
            // above, so the event channel only ever needs draining right after one.
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                if let Some(index) = languages.iter().position(|item| *item.id() == event.id) {
                    // Windows ticks whichever item was clicked and leaves the other alone,
                    // so both are set from the choice rather than from the click: picking
                    // the language already in force changes nothing, and the menu must not
                    // show it unticked afterwards.
                    let picked = Language::ALL[index];
                    for (item, language) in languages.iter().zip(Language::ALL) {
                        item.set_checked(language == picked);
                    }
                    on_command(Command::SetLanguage(picked));
                    if picked != in_force {
                        // The overlay is closing to come back in the other language. The
                        // process exits without unwinding, so the icon is let go here
                        // rather than left orphaned in the notification area beside the
                        // one the next process adds.
                        return;
                    }
                } else if let Some(command) = items
                    .iter()
                    .find_map(|(id, command)| (*id == event.id).then_some(*command))
                {
                    on_command(command);
                    if command == Command::Exit {
                        return;
                    }
                }
            }
        }
    }
}

/// The project's icon as `(rgba, width, height)`: the tray's, and the loadout windows' too.
pub fn icon_rgba() -> (Vec<u8>, u32, u32) {
    // Embedded rather than read at runtime so the release build stays a single portable
    // exe with no assets directory to ship alongside it. The artwork is original — no
    // game asset is redistributed, which is what lets the repository be public domain.
    const ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");
    let image = image::load_from_memory(ICON_PNG)
        .expect("valid icon png")
        .into_rgba8();
    let (width, height) = image.dimensions();
    (image.into_raw(), width, height)
}

fn icon() -> Icon {
    let (rgba, width, height) = icon_rgba();
    Icon::from_rgba(rgba, width, height).expect("valid tray icon")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_the_menu_with_the_language_in_force_ticked() {
        let built = build_menu(Language::English);

        // Every window, and the way out; the separators and the submenu stand for no
        // command of their own.
        assert_eq!(built.items.len(), WINDOWS.len() + 1);
        assert_eq!(built.items.last().expect("an exit item").1, Command::Exit);
        assert_eq!(
            built.languages.each_ref().map(CheckMenuItem::is_checked),
            [false, true]
        );

        let japanese = build_menu(Language::Japanese);
        assert_eq!(
            japanese.languages.each_ref().map(CheckMenuItem::is_checked),
            [true, false]
        );
    }
}
