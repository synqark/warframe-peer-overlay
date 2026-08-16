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
    menu::{Menu, MenuEvent, MenuItem},
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, TranslateMessage,
};

/// Shows the tray icon until the user picks `Exit`, at which point `on_exit` runs.
///
/// Returns immediately; the icon lives on a background thread that exits with the process.
pub fn spawn(on_exit: impl Fn() + Send + 'static) {
    std::thread::spawn(move || run(on_exit));
}

fn run(on_exit: impl Fn()) {
    let menu = Menu::new();
    let exit = MenuItem::new("Exit", true, None);
    menu.append(&exit).expect("failed to build tray menu");
    let exit_id = exit.id().clone();

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
                if event.id == exit_id {
                    on_exit();
                    return;
                }
            }
        }
    }
}

fn icon() -> Icon {
    // Embedded rather than read at runtime so the release build stays a single portable
    // exe with no assets directory to ship alongside it. The artwork is original — no
    // game asset is redistributed, which is what lets the repository be public domain.
    const ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");
    let image = image::load_from_memory(ICON_PNG)
        .expect("valid tray icon png")
        .into_rgba8();
    let (width, height) = image.dimensions();
    Icon::from_rgba(image.into_raw(), width, height).expect("valid tray icon")
}
