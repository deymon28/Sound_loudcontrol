//! Hidden top-level window that receives system-wide messages eframe does not surface while the
//! UI is hidden: `WM_ENDSESSION` (sign-out / shutdown) and `WM_CLOSE` (e.g. `taskkill`, installers).

use std::sync::OnceLock;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, MSG, RegisterClassW, TranslateMessage,
    WINDOW_EX_STYLE, WM_CLOSE, WM_ENDSESSION, WM_QUERYENDSESSION, WNDCLASSW, WS_OVERLAPPED,
};
use windows::core::w;

struct Hooks {
    quit: Box<dyn Fn() + Send + Sync>,
    session_end: Box<dyn Fn() + Send + Sync>,
}

static HOOKS: OnceLock<Hooks> = OnceLock::new();

/// `quit` runs when the program is asked to close; `session_end` runs synchronously when Windows
/// is signing out or shutting down and must finish restoring state before returning.
pub fn spawn_system_window(quit: impl Fn() + Send + Sync + 'static, session_end: impl Fn() + Send + Sync + 'static) {
    if HOOKS.set(Hooks { quit: Box::new(quit), session_end: Box::new(session_end) }).is_err() {
        return;
    }
    let spawned = std::thread::Builder::new().name("system-window".into()).spawn(|| unsafe {
        let instance = match GetModuleHandleW(None) {
            Ok(h) => h.into(),
            Err(e) => {
                log::warn!("system window unavailable: {e}");
                return;
            }
        };
        let class = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance,
            lpszClassName: w!("LoudControl.System"),
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            log::warn!("cannot register system window class");
            return;
        }
        // A top-level window (never shown) so that session-end broadcasts reach it.
        let created = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("LoudControl.System"),
            w!("LoudControl system"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        );
        if let Err(e) = created {
            log::warn!("cannot create system window: {e}");
            return;
        }
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    });
    if let Err(e) = spawned {
        log::warn!("cannot start system window thread: {e}");
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_QUERYENDSESSION => LRESULT(1),
        WM_ENDSESSION => {
            if wparam.0 != 0 {
                log::info!("Windows session is ending; restoring volumes");
                if let Some(h) = HOOKS.get() {
                    (h.session_end)();
                }
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            log::info!("close requested by the system");
            if let Some(h) = HOOKS.get() {
                (h.quit)();
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
