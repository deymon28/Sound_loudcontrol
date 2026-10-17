#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod autostart;
mod config;
mod dsp;
mod engine;
mod icon_art;
mod logger;
mod system;
mod timer;
mod tray;
mod ui;

use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError};
use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
use windows::Win32::System::Threading::{CreateMutexW, EVENT_MODIFY_STATE, OpenEventW, SetEvent};
use windows::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow};
use windows::core::w;

use crate::config::Config;
use crate::engine::{EngineHandle, Shared};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--list") {
        attach_console();
        list_sessions();
        return;
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        attach_console();
        println!("LoudControl — per-app audio ducking\n\n  --minimized   start in the tray\n  --list        print audio sessions and their levels, then exit");
        return;
    }

    if !acquire_single_instance() {
        return;
    }

    let dir = config::app_dir();
    logger::init(&dir.join("loudcontrol.log"), cfg!(debug_assertions));
    std::panic::set_hook(Box::new(|info| log::error!("panic: {info}")));
    log::info!("LoudControl {} starting", env!("CARGO_PKG_VERSION"));

    let (cfg, notice) = load_config();
    let start_hidden = args.iter().any(|a| a == "--minimized") || cfg.settings.start_minimized;
    let shared = Shared::new(cfg);
    let mut engine = EngineHandle::start(shared.clone());

    if let Err(e) = ui::run(shared.clone(), start_hidden, notice) {
        log::error!("UI failed: {e}");
    }

    // Restores every ducked volume before the process exits.
    engine.stop();
    if let Err(e) = shared.config_snapshot().save(&config::config_path()) {
        log::error!("cannot save config on exit: {e}");
    }
    log::info!("LoudControl exited");
}

fn load_config() -> (Config, Option<String>) {
    let path = config::config_path();
    if !path.exists() {
        let cfg = Config::default();
        if let Err(e) = cfg.save(&path) {
            log::error!("cannot write default config: {e}");
        }
        log::info!("created default config at {}", path.display());
        return (cfg, None);
    }
    match Config::load(&path) {
        Ok(cfg) => {
            log::info!("loaded {} rule(s) from {}", cfg.rules.len(), path.display());
            (cfg, None)
        }
        Err(e) => {
            // Keep the broken file for the user instead of overwriting it.
            let backup = path.with_extension("broken.toml");
            let _ = std::fs::copy(&path, &backup);
            log::error!("config error, using defaults (backup at {}): {e}", backup.display());
            (Config::default(), Some(format!("Config file was invalid and has been backed up to {}: {e}", backup.display())))
        }
    }
}

/// Returns false if another instance is running, after asking it to open its window.
fn acquire_single_instance() -> bool {
    unsafe {
        // The handle is intentionally leaked: the mutex must live as long as the process.
        let _ = CreateMutexW(None, true, w!("Local\\LoudControl.SingleInstance"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = AllowSetForegroundWindow(ASFW_ANY);
            if let Ok(event) = OpenEventW(EVENT_MODIFY_STATE, false, tray::SHOW_EVENT_NAME) {
                let _ = SetEvent(event);
                let _ = CloseHandle(event);
            }
            return false;
        }
    }
    true
}

fn attach_console() {
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

/// Diagnostic listing: every session with its exe name, device, volume and peak level.
fn list_sessions() {
    if let Err(e) = audio::init_com() {
        eprintln!("COM init failed: {e}");
        return;
    }
    let mut sys = match audio::AudioSystem::new() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("audio system unavailable: {e}");
            return;
        }
    };
    sys.refresh(true);
    let mut peaks: std::collections::HashMap<String, f32> = Default::default();
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(1500) {
        for (k, s) in &sys.sessions {
            let p = peaks.entry(k.clone()).or_default();
            *p = p.max(s.peak());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut rows: Vec<_> = sys.sessions.iter().collect();
    rows.sort_by_key(|(_, s)| s.label().to_lowercase());
    println!("{:<28} {:>7} {:>8} {:>8}  device", "app", "pid", "volume", "peak dB");
    for (k, s) in rows {
        let vol = s.volume().map(|v| format!("{:.0}%", v * 100.0)).unwrap_or_else(|_| "?".into());
        let peak = dsp::lin_to_db(peaks.get(k).copied().unwrap_or(0.0));
        println!("{:<28} {:>7} {:>8} {:>8.1}  {}", s.label(), s.pid, vol, peak, s.device_name);
    }
}
