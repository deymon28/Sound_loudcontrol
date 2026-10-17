//! System tray icon and the global pause hotkey. Both deliver events through a queue
//! that the UI drains in `App::logic`, which runs even while the window is hidden.

use std::str::FromStr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use eframe::egui;
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::icon_art::{IconState, render_icon};

/// Named event a second launch of the program signals to open the running instance's window.
pub const SHOW_EVENT_NAME: windows::core::PCWSTR = windows::core::w!("Local\\LoudControl.Show");

/// Listens for [`SHOW_EVENT_NAME`] on a background thread.
pub fn spawn_show_listener(queue: EventQueue) {
    use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
    let event = match unsafe { CreateEventW(None, false, false, SHOW_EVENT_NAME) } {
        // HANDLE is not Send; pass the raw value to the thread, which owns it for the process lifetime.
        Ok(h) => h.0 as isize,
        Err(e) => {
            log::warn!("cannot create activation event: {e}");
            return;
        }
    };
    let spawned = std::thread::Builder::new().name("activation".into()).spawn(move || {
        let handle = windows::Win32::Foundation::HANDLE(event as *mut core::ffi::c_void);
        loop {
            if unsafe { WaitForSingleObject(handle, INFINITE) }.0 != 0 {
                return;
            }
            queue.push(UiEvent::Show);
        }
    });
    if let Err(e) = spawned {
        log::warn!("cannot start activation listener: {e}");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiEvent {
    Show,
    TogglePause,
    Quit,
}

#[derive(Clone)]
pub struct EventQueue {
    events: Arc<Mutex<Vec<UiEvent>>>,
    ctx: egui::Context,
}

impl EventQueue {
    pub fn new(ctx: egui::Context) -> Self {
        Self { events: Arc::new(Mutex::new(Vec::new())), ctx }
    }

    pub fn push(&self, e: UiEvent) {
        self.events.lock().unwrap().push(e);
        self.ctx.request_repaint();
    }

    pub fn drain(&self) -> Vec<UiEvent> {
        std::mem::take(&mut *self.events.lock().unwrap())
    }
}

pub struct Tray {
    icon: TrayIcon,
    pause_item: CheckMenuItem,
    state: IconState,
}

fn make_icon(state: IconState) -> Option<Icon> {
    Icon::from_rgba(render_icon(32, state), 32, 32).ok()
}

impl Tray {
    pub fn new(queue: EventQueue, paused: bool) -> Result<Self, String> {
        let show = MenuItem::new("Open LoudControl", true, None);
        let pause_item = CheckMenuItem::new("Pause ducking", true, paused, None);
        let quit = MenuItem::new("Quit", true, None);
        let menu = Menu::new();
        menu.append_items(&[&show, &pause_item, &PredefinedMenuItem::separator(), &quit])
            .map_err(|e| e.to_string())?;

        let state = if paused { IconState::Paused } else { IconState::Normal };
        let mut builder = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip(tooltip(state));
        if let Some(icon) = make_icon(state) {
            builder = builder.with_icon(icon);
        }
        let icon = builder.build().map_err(|e| e.to_string())?;

        let (show_id, pause_id, quit_id) = (show.id().clone(), pause_item.id().clone(), quit.id().clone());
        let q = queue.clone();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            if e.id == show_id {
                q.push(UiEvent::Show);
            } else if e.id == pause_id {
                q.push(UiEvent::TogglePause);
            } else if e.id == quit_id {
                q.push(UiEvent::Quit);
            }
        }));
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| match e {
            TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
            | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. } => queue.push(UiEvent::Show),
            _ => {}
        }));

        Ok(Self { icon, pause_item, state })
    }

    pub fn set_state(&mut self, state: IconState) {
        if state == self.state {
            return;
        }
        self.state = state;
        if let Err(e) = self.icon.set_icon(make_icon(state)) {
            log::warn!("cannot update tray icon: {e}");
        }
        let _ = self.icon.set_tooltip(Some(tooltip(state)));
        self.pause_item.set_checked(state == IconState::Paused);
    }
}

fn tooltip(state: IconState) -> &'static str {
    match state {
        IconState::Normal => "LoudControl — listening",
        IconState::Ducking => "LoudControl — ducking",
        IconState::Paused => "LoudControl — paused",
    }
}

/// Global hotkey that toggles pause.
pub struct PauseHotkey {
    manager: Option<GlobalHotKeyManager>,
    current: Option<HotKey>,
    active_id: Arc<AtomicU32>,
}

impl PauseHotkey {
    pub fn new(queue: EventQueue) -> Self {
        let manager = match GlobalHotKeyManager::new() {
            Ok(m) => Some(m),
            Err(e) => {
                log::warn!("global hotkeys unavailable: {e}");
                None
            }
        };
        let active_id = Arc::new(AtomicU32::new(0));
        let id = active_id.clone();
        GlobalHotKeyEvent::set_event_handler(Some(move |e: GlobalHotKeyEvent| {
            if e.state == HotKeyState::Pressed && e.id == id.load(Ordering::Relaxed) {
                queue.push(UiEvent::TogglePause);
            }
        }));
        Self { manager, current: None, active_id }
    }

    /// Registers `text` (e.g. `Ctrl+Alt+M`) replacing the previous hotkey. Empty text disables it.
    pub fn set(&mut self, text: &str) -> Result<(), String> {
        let Some(manager) = &self.manager else { return Err("global hotkeys are unavailable".into()) };
        if let Some(old) = self.current.take() {
            let _ = manager.unregister(old);
            self.active_id.store(0, Ordering::Relaxed);
        }
        let text = text.trim();
        if text.is_empty() {
            return Ok(());
        }
        let hotkey = HotKey::from_str(text).map_err(|e| e.to_string())?;
        manager
            .register(hotkey)
            .map_err(|e| format!("cannot register {text}: {e} (maybe used by another app)"))?;
        self.active_id.store(hotkey.id(), Ordering::Relaxed);
        self.current = Some(hotkey);
        log::info!("pause hotkey set to {text}");
        Ok(())
    }
}
