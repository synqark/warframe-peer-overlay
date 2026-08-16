use std::ptr;

use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE},
    System::Threading::CreateMutexW,
};

/// Ownership of a named Windows mutex, used to keep a second overlay from attaching
/// itself to the same game window. Windows keeps the name reserved only while a handle
/// to it is open, so this guard must stay alive for as long as the process runs.
pub struct SingleInstance {
    handle: HANDLE,
}

impl SingleInstance {
    /// Claims `name` for this process, or returns `None` when another instance holds it.
    ///
    /// The name is created in the `Local\` namespace so it is scoped to the current
    /// logon session: two users switched via fast user switching each get their own
    /// overlay, while one user cannot start the overlay twice.
    pub fn acquire(name: &str) -> Option<Self> {
        let wide = format!("Local\\{name}")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<u16>>();
        let handle = unsafe { CreateMutexW(ptr::null(), 0, wide.as_ptr()) };
        if handle.is_null() {
            // Without a mutex there is no way to tell instances apart, so let the launch
            // through rather than refusing to start for an unexplained reason.
            return Some(Self {
                handle: ptr::null_mut(),
            });
        }
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe { CloseHandle(handle) };
            return None;
        }
        Some(Self { handle })
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { CloseHandle(self.handle) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquisition_is_refused_until_the_first_is_dropped() {
        let name = format!("WarframePeerOverlayTest-{}", std::process::id());
        let first = SingleInstance::acquire(&name).expect("first instance claims the name");
        assert!(SingleInstance::acquire(&name).is_none());
        drop(first);
        assert!(SingleInstance::acquire(&name).is_some());
    }
}
