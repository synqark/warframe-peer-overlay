use tauri_winrt_notification::{Error, Toast};

/// Shows a Windows toast.
///
/// Windows only routes toasts through a registered AppUserModelID, and the overlay is a
/// portable exe with no Start menu entry to register one. Borrowing PowerShell's built-in
/// id is the standard fallback for unregistered desktop apps; the toast then shows under
/// PowerShell's name, but it does show.
///
/// Callers decide what to do on failure — notifications are informational, and a machine
/// with toasts disabled should still get the overlay.
pub fn show(title: &str, body: &str) -> Result<(), Error> {
    Toast::new(Toast::POWERSHELL_APP_ID)
        .title(title)
        .text1(body)
        .show()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ignored by default because passing means a real toast pops up on the desktop.
    /// Run with `cargo test --lib -- --ignored show_` to check toast delivery on a machine.
    #[test]
    #[ignore = "shows a real desktop notification"]
    fn show_delivers_a_toast() {
        show("Warframe Peer Overlay", "テスト通知").expect("toast is delivered");
    }
}
