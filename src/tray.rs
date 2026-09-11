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
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, TranslateMessage,
};

/// A choice from the tray menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Bring up the loadout window.
    ShowLoadouts,
    Exit,
}

/// Shows the tray icon and hands each menu choice to `on_command`, until the user picks
/// `Exit`, which is the last one handed over.
///
/// Returns immediately; the icon lives on a background thread that exits with the process.
pub fn spawn(on_command: impl Fn(Command) + Send + 'static) {
    std::thread::spawn(move || run(on_command));
}

fn run(on_command: impl Fn(Command)) {
    let menu = Menu::new();
    let loadouts = MenuItem::new("Loadouts", true, None);
    let exit = MenuItem::new("Exit", true, None);
    menu.append_items(&[&loadouts, &PredefinedMenuItem::separator(), &exit])
        .expect("failed to build tray menu");
    let (loadouts_id, exit_id) = (loadouts.id().clone(), exit.id().clone());

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
                if event.id == loadouts_id {
                    on_command(Command::ShowLoadouts);
                } else if event.id == exit_id {
                    on_command(Command::Exit);
                    return;
                }
            }
        }
    }
}

/// The project's icon as `(rgba, width, height)`: the tray's, and the loadout window's too.
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
