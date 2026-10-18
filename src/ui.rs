//! egui front end: rule list, rule editor with live graph, live session list, settings.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Layout, Margin, Pos2, Rect, RichText, Sense, Stroke,
    StrokeKind, Ui, pos2, vec2,
};

use crate::autostart;
use crate::config::{self, AppMatcher, Config, DuckMode, FadeCurve, Rule};
use crate::dsp;
use crate::engine::{RuleView, SessionView, Shared, Telemetry};
use crate::icon_art::{IconState, render_icon};
use crate::tray::{EventQueue, PauseHotkey, Tray, UiEvent};

// Palette.
const BG: Color32 = Color32::from_rgb(0x0f, 0x11, 0x16);
const PANEL: Color32 = Color32::from_rgb(0x14, 0x17, 0x1f);
const CARD: Color32 = Color32::from_rgb(0x1a, 0x1e, 0x28);
const CARD_HI: Color32 = Color32::from_rgb(0x22, 0x28, 0x36);
const BORDER: Color32 = Color32::from_rgb(0x28, 0x2e, 0x3d);
const TEXT: Color32 = Color32::from_rgb(0xe6, 0xe9, 0xef);
const MUTED: Color32 = Color32::from_rgb(0x8b, 0x93, 0xa7);
const FAINT: Color32 = Color32::from_rgb(0x4a, 0x51, 0x63);
const ACCENT: Color32 = Color32::from_rgb(0x4f, 0xd8, 0xc8);
const DUCK: Color32 = Color32::from_rgb(0xff, 0xb3, 0x47);
const DANGER: Color32 = Color32::from_rgb(0xff, 0x6b, 0x6b);

// Segoe Fluent Icons / Segoe MDL2 Assets code points.
const ICON_PAUSE: &str = "\u{E769}";
const ICON_PLAY: &str = "\u{E768}";
const ICON_DELETE: &str = "\u{E74D}";
const ICON_COPY: &str = "\u{E8C8}";
const ICON_SETTINGS: &str = "\u{E713}";
const ICON_CLOSE: &str = "\u{E711}";
const ICON_ADD: &str = "\u{E710}";
const ICON_CHECK: &str = "\u{E73E}";

const METER_MIN_DB: f32 = -66.0;
const HISTORY_SECS: f64 = 8.0;
const SAVE_DEBOUNCE: Duration = Duration::from_millis(600);
const UNTITLED_RULE: &str = "Untitled rule";
/// Narrowest the middle column (with its margins) may get before the session panel shrinks.
const CENTER_MIN_WIDTH: f32 = 420.0;
const SESSIONS_MIN_WIDTH: f32 = 220.0;
/// Below this content width, parameter labels go above their controls.
const NARROW_ROW_WIDTH: f32 = 400.0;
/// Below this card width, the trigger and target lists are stacked instead of side by side.
const APP_COLUMNS_MIN_WIDTH: f32 = 440.0;

pub fn run(shared: Arc<Shared>, start_hidden: bool, notice: Option<String>) -> eframe::Result {
    let icon = egui::IconData { rgba: render_icon(64, IconState::Normal), width: 64, height: 64 };
    let viewport = egui::ViewportBuilder::default()
        .with_title("LoudControl")
        .with_app_id("LoudControl")
        .with_inner_size([1120.0, 720.0])
        .with_min_inner_size([860.0, 560.0])
        .with_icon(icon)
        .with_visible(!start_hidden);
    // eframe always shows the window after its first frame. When starting in the tray, create it
    // off-screen so that frame is never seen; it is hidden right away and centered when opened.
    let viewport = if start_hidden { viewport.with_position([-20000.0, -20000.0]).with_active(false) } else { viewport };
    // Debug aid: render offscreen, save a screenshot and quit.
    #[cfg(debug_assertions)]
    let viewport = if std::env::var_os("LOUDCONTROL_SCREENSHOT").is_some() {
        // Optional `LOUDCONTROL_SCREENSHOT_SIZE=860x560` checks other window sizes.
        let size = std::env::var("LOUDCONTROL_SCREENSHOT_SIZE").ok().and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some([w.trim().parse::<f32>().ok()?, h.trim().parse::<f32>().ok()?])
        });
        let viewport = viewport.with_position([-4000.0, 40.0]).with_active(false).with_visible(true);
        match size {
            Some(size) => viewport.with_inner_size(size),
            None => viewport,
        }
    } else {
        viewport
    };
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native(
        "LoudControl",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, shared, start_hidden, notice)))),
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Rule(u64),
    Settings,
}

#[derive(Clone, Copy)]
struct Sample {
    t: f64,
    level_db: f32,
    gain: f32,
}

struct App {
    shared: Arc<Shared>,
    cfg: Config,
    page: Page,
    queue: EventQueue,
    tray: Option<Tray>,
    hotkey: PauseHotkey,
    hotkey_text: String,
    hotkey_status: Result<(), String>,
    autostart: bool,
    history: HashMap<u64, VecDeque<Sample>>,
    dirty_since: Option<Instant>,
    quitting: bool,
    visible: bool,
    /// Started in the tray: keep re-hiding during the first frames, center on first show.
    hidden_start_frames: u32,
    needs_center: bool,
    notice: Option<(String, bool)>,
    manual_entry: String,
    confirm_delete: bool,
    telemetry: Telemetry,
    #[cfg(debug_assertions)]
    screenshot: Option<(std::path::PathBuf, u32)>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, shared: Arc<Shared>, start_hidden: bool, notice: Option<String>) -> Self {
        apply_style(&cc.egui_ctx);
        let cfg = shared.config_snapshot();
        let queue = EventQueue::new(cc.egui_ctx.clone());
        crate::tray::spawn_show_listener(queue.clone());
        let quit_queue = queue.clone();
        let end_shared = shared.clone();
        crate::system::spawn_system_window(
            move || quit_queue.push(UiEvent::Quit),
            move || {
                // Windows may terminate the process as soon as this returns.
                if !end_shared.stop_engine_and_wait(Duration::from_secs(3)) {
                    log::warn!("engine did not stop in time during session end");
                }
                if let Err(e) = end_shared.config_snapshot().save(&config::config_path()) {
                    log::error!("cannot save config at session end: {e}");
                }
                log::logger().flush();
            },
        );

        let ctx = cc.egui_ctx.clone();
        *shared.notify.lock().unwrap() = Some(Box::new(move || ctx.request_repaint()));

        let tray = match Tray::new(queue.clone(), cfg.settings.paused) {
            Ok(t) => Some(t),
            Err(e) => {
                log::warn!("tray icon unavailable: {e}");
                None
            }
        };
        let mut hotkey = PauseHotkey::new(queue.clone());
        let hotkey_status = hotkey.set(&cfg.settings.pause_hotkey);
        if let Err(e) = &hotkey_status {
            log::warn!("pause hotkey: {e}");
        }

        let page = cfg.rules.first().map(|r| Page::Rule(r.uid)).unwrap_or(Page::Settings);
        Self {
            hotkey_text: cfg.settings.pause_hotkey.clone(),
            shared,
            page,
            queue,
            tray,
            hotkey,
            hotkey_status,
            autostart: autostart::is_enabled(),
            history: HashMap::new(),
            dirty_since: None,
            quitting: false,
            visible: !start_hidden,
            hidden_start_frames: if start_hidden { 5 } else { 0 },
            needs_center: start_hidden,
            notice: notice.map(|n| (n, true)),
            manual_entry: String::new(),
            confirm_delete: false,
            telemetry: Telemetry::default(),
            #[cfg(debug_assertions)]
            screenshot: std::env::var_os("LOUDCONTROL_SCREENSHOT").map(|p| (p.into(), 0)),
            cfg,
        }
    }

    /// Publishes the edited config to the engine and schedules a save.
    fn commit(&mut self) {
        self.cfg.sanitize();
        let cfg = self.cfg.clone();
        self.shared.update_config(|c| *c = cfg);
        self.dirty_since = Some(Instant::now());
    }

    fn save_now(&mut self) {
        self.dirty_since = None;
        if let Err(e) = self.cfg.save(&config::config_path()) {
            log::error!("cannot save config: {e}");
            self.notice = Some((format!("Cannot save settings: {e}"), true));
        }
    }

    /// Replaces the configuration with one reloaded from disk, including the parts that live
    /// outside the engine (the hotkey), and keeps the selected rule when it still exists.
    fn apply_reloaded(&mut self, cfg: Config) {
        config::set_saves_blocked(false);
        let selected = match self.page {
            Page::Rule(uid) => self.cfg.rules.iter().position(|r| r.uid == uid),
            Page::Settings => None,
        };
        self.cfg = cfg;
        let cfg = self.cfg.clone();
        self.shared.update_config(|c| *c = cfg);
        self.dirty_since = None;
        if let Some(i) = selected {
            self.page = self.cfg.rules.get(i).or(self.cfg.rules.last()).map(|r| Page::Rule(r.uid)).unwrap_or(Page::Settings);
        }
        self.confirm_delete = false;
        self.hotkey_text = self.cfg.settings.pause_hotkey.clone();
        self.hotkey_status = self.hotkey.set(&self.hotkey_text);
        self.notice = match &self.hotkey_status {
            Ok(()) => Some(("Configuration reloaded.".into(), false)),
            Err(e) => Some((format!("Configuration reloaded, but the pause hotkey failed: {e}"), true)),
        };
    }

    fn set_paused(&mut self, paused: bool) {
        self.cfg.settings.paused = paused;
        log::info!("ducking {}", if paused { "paused" } else { "resumed" });
        self.commit();
    }

    fn hide(&mut self, ctx: &egui::Context) {
        self.visible = false;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        if self.dirty_since.is_some() {
            self.save_now();
        }
    }

    fn show(&mut self, ctx: &egui::Context) {
        self.visible = true;
        self.hidden_start_frames = 0;
        if std::mem::take(&mut self.needs_center) {
            let (monitor, size) = ctx.input(|i| (i.viewport().monitor_size, i.viewport().outer_rect.map(|r| r.size())));
            let size = size.unwrap_or(vec2(1120.0, 720.0));
            let pos = monitor.map(|m| ((m - size) * 0.5).max(vec2(0.0, 0.0))).unwrap_or(vec2(120.0, 80.0));
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos.to_pos2()));
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.hidden_start_frames > 0 {
            self.hidden_start_frames -= 1;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            ctx.request_repaint_after(Duration::from_millis(50));
        }
        for event in self.queue.drain() {
            match event {
                UiEvent::Show => self.show(ctx),
                UiEvent::TogglePause => self.set_paused(!self.cfg.settings.paused),
                UiEvent::Quit => {
                    self.quitting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }

        self.telemetry = self.shared.telemetry.lock().unwrap().clone();
        if let Some(tray) = &mut self.tray {
            let state = if self.cfg.settings.paused {
                IconState::Paused
            } else if self.telemetry.ducking {
                IconState::Ducking
            } else {
                IconState::Normal
            };
            tray.set_state(state);
        }

        if let Some(t) = self.dirty_since {
            if t.elapsed() >= SAVE_DEBOUNCE {
                self.save_now();
            } else {
                ctx.request_repaint_after(SAVE_DEBOUNCE);
            }
        }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting && self.cfg.settings.close_to_tray && self.tray.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hide(&ctx);
            return;
        }

        // eframe may still run frames for a hidden or minimized window; render nothing and
        // schedule nothing then, so the app costs no CPU while it sits in the tray.
        let minimized = ctx.input(|i| i.viewport().minimized.unwrap_or(false));
        if !self.visible || minimized {
            return;
        }
        self.shared.mark_ui_frame();
        ctx.request_repaint_after(Duration::from_millis(33));
        self.record_history(ctx.input(|i| i.time));

        egui::Panel::top("top")
            .frame(egui::Frame::new().fill(PANEL).inner_margin(Margin::symmetric(16, 10)))
            .show_separator_line(false)
            .show(ui, |ui| self.top_bar(ui));

        // Side panels give way on narrow windows so the editor in the middle keeps its width.
        let total = ui.available_width();
        let rules_width = if total < 1000.0 { 210.0 } else { 250.0 };
        let sessions_max = (total - rules_width - CENTER_MIN_WIDTH).clamp(SESSIONS_MIN_WIDTH, 460.0);

        egui::Panel::left("rules")
            .frame(egui::Frame::new().fill(PANEL).inner_margin(Margin::symmetric(12, 12)))
            .resizable(false)
            .exact_size(rules_width)
            .show_separator_line(false)
            .show(ui, |ui| self.rule_list(ui));

        egui::Panel::right("sessions")
            .frame(egui::Frame::new().fill(PANEL).inner_margin(Margin::symmetric(12, 12)))
            .resizable(true)
            .default_size(300.0_f32.min(sessions_max))
            .size_range(SESSIONS_MIN_WIDTH..=sessions_max)
            .show_separator_line(false)
            .show(ui, |ui| self.session_list(ui));

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(Margin::symmetric(22, 16)))
            .show(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match self.page {
                    Page::Rule(uid) => self.rule_editor(ui, uid),
                    Page::Settings => self.settings_page(ui),
                });
            });

        #[cfg(debug_assertions)]
        self.debug_screenshot(&ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_now();
    }
}

#[cfg(debug_assertions)]
impl App {
    fn debug_screenshot(&mut self, ctx: &egui::Context) {
        let Some((path, frames)) = &mut self.screenshot else { return };
        *frames += 1;
        if *frames == 2 && std::env::var("LOUDCONTROL_SCREENSHOT_PAGE").as_deref() == Ok("settings") {
            self.page = Page::Settings;
        }
        if *frames == 90 {
            let path = path.clone();
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::ScreenshotCallback::new(move |img| {
                write_bmp(&path, &img);
            })));
        }
        if *frames > 90 && path.exists() {
            self.quitting = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

#[cfg(debug_assertions)]
fn write_bmp(path: &std::path::Path, img: &egui::ColorImage) {
    let [w, h] = img.size;
    let mut v = Vec::with_capacity(54 + w * h * 4);
    let data_len = (w * h * 4) as u32;
    v.extend_from_slice(b"BM");
    v.extend_from_slice(&(54 + data_len).to_le_bytes());
    v.extend_from_slice(&[0; 4]);
    v.extend_from_slice(&54u32.to_le_bytes());
    v.extend_from_slice(&40u32.to_le_bytes());
    v.extend_from_slice(&(w as i32).to_le_bytes());
    v.extend_from_slice(&(-(h as i32)).to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&32u16.to_le_bytes());
    v.extend_from_slice(&[0; 24]);
    for p in &img.pixels {
        v.extend_from_slice(&[p.b(), p.g(), p.r(), 255]);
    }
    let _ = std::fs::write(path, v);
}

// ---------------------------------------------------------------------------------------------
// Sections

impl App {
    fn record_history(&mut self, now: f64) {
        for (uid, view) in &self.telemetry.rules {
            let h = self.history.entry(*uid).or_default();
            h.push_back(Sample { t: now, level_db: view.level_db, gain: view.gain });
            while h.front().is_some_and(|s| now - s.t > HISTORY_SECS) {
                h.pop_front();
            }
        }
        let live: Vec<u64> = self.cfg.rules.iter().map(|r| r.uid).collect();
        self.history.retain(|uid, _| live.contains(uid));
    }

    fn top_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label(semibold("LoudControl").size(18.0).color(TEXT));
            ui.add_space(10.0);
            let (text, color) = if self.cfg.settings.paused {
                ("Paused", MUTED)
            } else if self.telemetry.ducking {
                ("Ducking", DUCK)
            } else {
                ("Listening", ACCENT)
            };
            pill(ui, text, color);
            if let Some(err) = &self.telemetry.error {
                ui.add_space(6.0);
                pill(ui, err, DANGER);
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let paused = self.cfg.settings.paused;
                let label = if paused { format!("{ICON_PLAY}   Resume") } else { format!("{ICON_PAUSE}   Pause") };
                let hint = if self.cfg.settings.pause_hotkey.is_empty() {
                    "Pause or resume all ducking".to_owned()
                } else {
                    format!("Pause or resume all ducking ({})", self.cfg.settings.pause_hotkey)
                };
                if accent_button(ui, &label, !paused).on_hover_text(hint).clicked() {
                    self.set_paused(!paused);
                }
            });
        });
        if let Some((msg, is_error)) = self.notice.clone() {
            ui.add_space(6.0);
            egui::Frame::new()
                .fill(if is_error { DANGER.gamma_multiply(0.18) } else { ACCENT.gamma_multiply(0.15) })
                .corner_radius(8)
                .inner_margin(Margin::symmetric(10, 6))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(msg).color(TEXT));
                        if ui.small_button(ICON_CLOSE).clicked() {
                            self.notice = None;
                        }
                    });
                });
        }
    }

    fn rule_list(&mut self, ui: &mut Ui) {
        ui.label(semibold("RULES").size(11.5).color(MUTED));
        ui.add_space(6.0);
        let mut select = None;
        let mut toggled = false;
        for rule in &mut self.cfg.rules {
            let view = self.telemetry.rules.get(&rule.uid).cloned().unwrap_or_default();
            let selected = self.page == Page::Rule(rule.uid);
            let (clicked, toggle_changed) = rule_card(ui, rule, &view, selected);
            if clicked {
                select = Some(rule.uid);
            }
            toggled |= toggle_changed;
            ui.add_space(6.0);
        }
        if toggled {
            self.commit();
        }
        if let Some(uid) = select {
            self.page = Page::Rule(uid);
            self.confirm_delete = false;
        }

        ui.add_space(4.0);
        if wide_button(ui, &format!("{ICON_ADD}   New rule")).clicked() {
            let rule = Rule { name: format!("Rule {}", self.cfg.rules.len() + 1), ..Rule::default() };
            self.page = Page::Rule(rule.uid);
            self.cfg.rules.push(rule);
            self.commit();
        }

        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            let selected = self.page == Page::Settings;
            if nav_item(ui, &format!("{ICON_SETTINGS}   Settings"), selected).clicked() {
                self.page = Page::Settings;
            }
        });
    }

    fn session_list(&mut self, ui: &mut Ui) {
        ui.label(semibold("LIVE AUDIO").size(11.5).color(MUTED));
        ui.add_space(6.0);
        let selected_rule = match self.page {
            Page::Rule(uid) => self.cfg.rules.iter().position(|r| r.uid == uid),
            Page::Settings => None,
        };
        let sessions = self.telemetry.sessions.clone();
        if sessions.is_empty() {
            ui.label(RichText::new("No applications are playing audio.").color(MUTED));
        }
        let mut changed = false;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for s in &sessions {
                let matcher = session_matcher(s);
                let rule = selected_rule.map(|i| &mut self.cfg.rules[i]);
                changed |= session_row(ui, s, matcher, rule);
                ui.add_space(4.0);
            }
        });
        if changed {
            self.commit();
        }
    }

    fn rule_editor(&mut self, ui: &mut Ui, uid: u64) {
        let Some(idx) = self.cfg.rules.iter().position(|r| r.uid == uid) else {
            self.page = self.cfg.rules.first().map(|r| Page::Rule(r.uid)).unwrap_or(Page::Settings);
            return;
        };
        let view = self.telemetry.rules.get(&uid).cloned().unwrap_or_default();
        let sessions = self.telemetry.sessions.clone();
        let paused = self.cfg.settings.paused;
        let history = self.history.get(&uid).cloned().unwrap_or_default();
        let mut changed = false;
        let mut delete = false;
        let mut duplicate = false;

        {
            let rule = &mut self.cfg.rules[idx];

            // Header.
            ui.horizontal(|ui| {
                let name = egui::TextEdit::singleline(&mut rule.name)
                    .font(FontId::proportional(22.0))
                    .frame(egui::Frame::NONE)
                    .hint_text(UNTITLED_RULE)
                    .desired_width((ui.available_width() - 170.0).max(120.0));
                changed |= ui.add(name).changed();
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if self.confirm_delete {
                        if danger_button(ui, "Delete").clicked() {
                            delete = true;
                        }
                        if ui.button("Cancel").clicked() {
                            self.confirm_delete = false;
                        }
                    } else {
                        if ui.button(ICON_DELETE).on_hover_text("Delete rule").clicked() {
                            self.confirm_delete = true;
                        }
                        if ui.button(ICON_COPY).on_hover_text("Duplicate rule").clicked() {
                            duplicate = true;
                        }
                        ui.add_space(6.0);
                        changed |= toggle(ui, &mut rule.enabled).on_hover_text("Enable or disable this rule").changed();
                    }
                });
            });
            ui.add_space(2.0);
            status_line(ui, rule, &view, paused);
            ui.add_space(12.0);

            // Apps.
            card(ui, |ui| {
                let manual = &mut self.manual_entry;
                if ui.available_width() >= APP_COLUMNS_MIN_WIDTH {
                    ui.columns(2, |cols| {
                        changed |= apps_section(&mut cols[0], false, &mut rule.triggers, &rule.targets, &sessions, manual);
                        changed |= apps_section(&mut cols[1], true, &mut rule.targets, &rule.triggers, &sessions, manual);
                    });
                } else {
                    changed |= apps_section(ui, false, &mut rule.triggers, &rule.targets, &sessions, manual);
                    ui.add_space(10.0);
                    changed |= apps_section(ui, true, &mut rule.targets, &rule.triggers, &sessions, manual);
                }
            });
            ui.add_space(10.0);

            // Live graph.
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(section_title("Live"));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        legend(ui, "target volume", DUCK);
                        legend(ui, "threshold", MUTED);
                        legend(ui, "trigger level", ACCENT);
                    });
                });
                ui.add_space(6.0);
                history_graph(ui, &history, rule.threshold_db);
            });
            ui.add_space(10.0);

            // Detection.
            card(ui, |ui| {
                ui.label(section_title("Detection"));
                ui.add_space(8.0);
                param_row(ui, "Threshold", "Trigger sound louder than this starts ducking. Watch the live meter while someone talks and set the line just below it.", |ui| {
                    threshold_slider(ui, &mut rule.threshold_db, view.level_db).changed()
                }, &mut changed);
                param_row(ui, "Ignore shorter than", "Sounds shorter than this (clicks, notification blips) are ignored.", |ui| {
                    ui.add(egui::Slider::new(&mut rule.min_active_ms, 0.0..=500.0).suffix(" ms").fixed_decimals(0)).changed()
                }, &mut changed);
            });
            ui.add_space(10.0);

            // Ducking.
            card(ui, |ui| {
                ui.label(section_title("Ducking"));
                ui.add_space(8.0);
                param_row(ui, "Mode", "Fixed: always duck to the same level. Proportional: the louder the trigger, the deeper the duck.", |ui| {
                    segmented(ui, &mut rule.mode, &[(DuckMode::Fixed, "Fixed"), (DuckMode::Proportional, "Proportional")])
                }, &mut changed);
                param_row(ui, "Lower to", "Target volume while ducked, relative to its normal volume. 0% mutes.", |ui| {
                    let mut pct = rule.duck_level * 100.0;
                    let db = dsp::lin_to_db(rule.duck_level);
                    let db_text = if rule.duck_level <= 0.0 { "mute".to_owned() } else { format!("{db:.1} dB") };
                    let r = ui.add(egui::Slider::new(&mut pct, 0.0..=100.0).suffix(" %").fixed_decimals(0));
                    ui.label(RichText::new(db_text).color(DUCK));
                    if r.changed() {
                        rule.duck_level = pct / 100.0;
                    }
                    r.changed()
                }, &mut changed);
                if rule.mode == DuckMode::Proportional {
                    param_row(ui, "Full duck at", "How far above the threshold the trigger must be for the full duck.", |ui| {
                        ui.add(egui::Slider::new(&mut rule.range_db, 3.0..=48.0).prefix("+").suffix(" dB").fixed_decimals(0)).changed()
                    }, &mut changed);
                }
                param_row(ui, "Fade curve", "Decibel fades sound even to the ear; linear fades drop faster at first.", |ui| {
                    segmented(ui, &mut rule.curve, &[(FadeCurve::Decibel, "Smooth (dB)"), (FadeCurve::Linear, "Linear")])
                }, &mut changed);
            });
            ui.add_space(10.0);

            // Timing.
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(section_title("Timing"));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        for (name, a, h, r) in [("Gentle", 250.0, 1500.0, 2500.0), ("Smooth", 120.0, 900.0, 1200.0), ("Snappy", 40.0, 400.0, 500.0)] {
                            let active = rule.attack_ms == a && rule.hold_ms == h && rule.release_ms == r;
                            if ui.selectable_label(active, name).on_hover_text("Timing preset").clicked() {
                                rule.attack_ms = a;
                                rule.hold_ms = h;
                                rule.release_ms = r;
                                changed = true;
                            }
                        }
                        ui.label(RichText::new("Presets").color(MUTED));
                    });
                });
                ui.add_space(8.0);
                param_row(ui, "Fade down", "How quickly targets get quieter when the trigger starts.", |ui| {
                    ui.add(egui::Slider::new(&mut rule.attack_ms, 0.0..=2000.0).logarithmic(true).suffix(" ms").fixed_decimals(0)).changed()
                }, &mut changed);
                param_row(ui, "Hold", "Keep targets ducked this long after the trigger goes quiet, so pauses between words don't pump the music.", |ui| {
                    ui.add(egui::Slider::new(&mut rule.hold_ms, 0.0..=5000.0).logarithmic(true).suffix(" ms").fixed_decimals(0)).changed()
                }, &mut changed);
                param_row(ui, "Fade up", "How slowly targets come back to their normal volume.", |ui| {
                    ui.add(egui::Slider::new(&mut rule.release_ms, 0.0..=8000.0).logarithmic(true).suffix(" ms").fixed_decimals(0)).changed()
                }, &mut changed);
                ui.add_space(4.0);
                envelope_preview(ui, rule);
            });
        }

        if duplicate {
            let mut copy = self.cfg.rules[idx].clone();
            copy.uid = config::next_uid();
            copy.name = format!("{} copy", copy.name);
            self.page = Page::Rule(copy.uid);
            self.cfg.rules.insert(idx + 1, copy);
            changed = true;
        }
        if delete {
            self.cfg.rules.remove(idx);
            self.confirm_delete = false;
            self.page = self.cfg.rules.get(idx.saturating_sub(1)).map(|r| Page::Rule(r.uid)).unwrap_or(Page::Settings);
            changed = true;
        }
        if changed {
            self.commit();
        }
    }

    fn settings_page(&mut self, ui: &mut Ui) {
        ui.label(RichText::new("Settings").size(22.0).color(TEXT));
        ui.add_space(12.0);
        let mut changed = false;

        card(ui, |ui| {
            ui.label(section_title("General"));
            ui.add_space(8.0);
            param_row(ui, "Start with Windows", "Launch LoudControl in the tray when you sign in.", |ui| {
                let mut on = self.autostart;
                let r = toggle(ui, &mut on);
                if r.changed() {
                    match autostart::set_enabled(on) {
                        Ok(()) => self.autostart = on,
                        Err(e) => self.notice = Some((format!("Cannot change autostart: {e}"), true)),
                    }
                }
                false
            }, &mut changed);
            param_row(ui, "Start minimized", "Open straight to the tray.", |ui| toggle(ui, &mut self.cfg.settings.start_minimized).changed(), &mut changed);
            param_row(ui, "Close to tray", "The window's close button hides it; quit from the tray menu.", |ui| toggle(ui, &mut self.cfg.settings.close_to_tray).changed(), &mut changed);
        });
        ui.add_space(10.0);

        card(ui, |ui| {
            ui.label(section_title("Pause hotkey"));
            ui.add_space(8.0);
            param_row(ui, "Shortcut", "Global shortcut that pauses or resumes all ducking. Examples: Ctrl+Alt+M, Shift+F9. Leave empty to disable.", |ui| {
                let edit = ui.add(egui::TextEdit::singleline(&mut self.hotkey_text).desired_width(180.0).hint_text("e.g. Ctrl+Alt+M"));
                let apply = ui.button("Apply").clicked() || (edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                if apply {
                    self.hotkey_status = self.hotkey.set(&self.hotkey_text);
                    if self.hotkey_status.is_ok() {
                        self.cfg.settings.pause_hotkey = self.hotkey_text.trim().to_owned();
                        return true;
                    }
                }
                match &self.hotkey_status {
                    Ok(()) if !self.cfg.settings.pause_hotkey.is_empty() => {
                        ui.label(RichText::new(format!("{ICON_CHECK} active")).color(ACCENT));
                    }
                    Ok(()) => {}
                    Err(e) => {
                        ui.add(egui::Label::new(RichText::new(e).color(DANGER)).wrap());
                    }
                }
                false
            }, &mut changed);
        });
        ui.add_space(10.0);

        card(ui, |ui| {
            ui.label(section_title("Performance"));
            ui.add_space(8.0);
            param_row(ui, "Poll interval", "How often meters are read while a trigger app is running. Lower reacts faster; 10 ms is plenty. When no trigger app runs, LoudControl polls ten times a second.", |ui| {
                ui.add(egui::Slider::new(&mut self.cfg.settings.tick_ms, 5..=50).suffix(" ms")).changed()
            }, &mut changed);
            let idle = if self.telemetry.idle { "idle (no trigger apps running)" } else { "active" };
            ui.label(RichText::new(format!("Engine: {idle}")).color(MUTED));
        });
        ui.add_space(10.0);

        card(ui, |ui| {
            ui.label(section_title("Files"));
            ui.add_space(8.0);
            let dir = config::app_dir();
            ui.label(RichText::new(dir.display().to_string()).color(MUTED).monospace());
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("Open folder").clicked() {
                    let _ = std::process::Command::new("explorer").arg(&dir).spawn();
                }
                if ui.button("Reload config from disk").clicked() {
                    match Config::load(&config::config_path()) {
                        Ok(cfg) => self.apply_reloaded(cfg),
                        Err(e) => self.notice = Some((format!("Config error: {e}"), true)),
                    }
                }
            });
            ui.add_space(6.0);
            ui.label(RichText::new(format!("LoudControl {}", env!("CARGO_PKG_VERSION"))).color(FAINT));
        });

        if changed {
            self.commit();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Widgets

/// Uses the native Windows UI font and icon font when present; egui's bundled fonts stay as fallback.
fn install_fonts(ctx: &egui::Context) {
    use egui::{FontData, FontDefinitions, FontFamily};
    let windir = std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into());
    let dir = std::path::PathBuf::from(windir).join("Fonts");
    let mut fonts = FontDefinitions::default();
    let mut load = |name: &str, file: &str| -> bool {
        match std::fs::read(dir.join(file)) {
            Ok(bytes) => {
                fonts.font_data.insert(name.to_owned(), Arc::new(FontData::from_owned(bytes)));
                true
            }
            Err(_) => false,
        }
    };
    let ui_font = load("segoe", "segoeui.ttf");
    let semibold = load("segoe-sb", "seguisb.ttf");
    let icons = load("icons", "SegoeIcons.ttf") || load("icons", "segmdl2.ttf");
    let symbols = load("symbols", "seguisym.ttf");

    let defaults = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let mut prop: Vec<String> = Vec::new();
    if ui_font {
        prop.push("segoe".into());
    }
    if icons {
        prop.push("icons".into());
    }
    if symbols {
        prop.push("symbols".into());
    }
    prop.extend(defaults);
    fonts.families.insert(FontFamily::Proportional, prop.clone());

    let mut sb: Vec<String> = Vec::new();
    if semibold {
        sb.push("segoe-sb".into());
    }
    sb.extend(prop);
    fonts.families.insert(FontFamily::Name("semibold".into()), sb);
    ctx.set_fonts(fonts);
}

fn semibold(text: impl Into<String>) -> RichText {
    RichText::new(text).family(egui::FontFamily::Name("semibold".into()))
}

fn apply_style(ctx: &egui::Context) {
    install_fonts(ctx);
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|style| {
        use egui::{FontFamily, TextStyle};
        style.text_styles.insert(TextStyle::Body, FontId::new(14.5, FontFamily::Proportional));
        style.text_styles.insert(TextStyle::Button, FontId::new(14.0, FontFamily::Proportional));
        style.text_styles.insert(TextStyle::Small, FontId::new(12.0, FontFamily::Proportional));
        style.text_styles.insert(TextStyle::Heading, FontId::new(20.0, FontFamily::Proportional));
        style.spacing.item_spacing = vec2(8.0, 6.0);
        style.spacing.button_padding = vec2(10.0, 5.0);
        style.spacing.slider_width = 260.0;
        style.spacing.interact_size.y = 26.0;

        let v = &mut style.visuals;
        v.dark_mode = true;
        v.panel_fill = BG;
        v.window_fill = PANEL;
        v.window_stroke = Stroke::new(1.0, BORDER);
        v.extreme_bg_color = Color32::from_rgb(0x0b, 0x0d, 0x12);
        v.faint_bg_color = CARD;
        v.override_text_color = None;
        v.hyperlink_color = ACCENT;
        v.selection.bg_fill = ACCENT.gamma_multiply(0.35);
        v.selection.stroke = Stroke::new(1.0, ACCENT);
        v.slider_trailing_fill = true;
        v.window_corner_radius = CornerRadius::same(10);
        v.menu_corner_radius = CornerRadius::same(8);

        let w = &mut v.widgets;
        w.noninteractive.bg_fill = CARD;
        w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
        w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        w.inactive.bg_fill = CARD_HI;
        w.inactive.weak_bg_fill = CARD_HI;
        w.inactive.bg_stroke = Stroke::NONE;
        w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
        w.hovered.bg_fill = Color32::from_rgb(0x2b, 0x32, 0x44);
        w.hovered.weak_bg_fill = Color32::from_rgb(0x2b, 0x32, 0x44);
        w.hovered.bg_stroke = Stroke::new(1.0, FAINT);
        w.hovered.fg_stroke = Stroke::new(1.5, TEXT);
        w.active.bg_fill = ACCENT.gamma_multiply(0.5);
        w.active.weak_bg_fill = ACCENT.gamma_multiply(0.5);
        w.active.fg_stroke = Stroke::new(1.5, TEXT);
        w.open.weak_bg_fill = CARD_HI;
        for wv in [&mut w.noninteractive, &mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
            wv.corner_radius = CornerRadius::same(6);
        }
    });
}

fn section_title(text: &str) -> RichText {
    semibold(text).size(13.5).color(TEXT)
}

fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(12)
        .inner_margin(Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

fn pill(ui: &mut Ui, text: &str, color: Color32) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.16))
        .corner_radius(20)
        .inner_margin(Margin::symmetric(10, 3))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 4.0, color);
                ui.label(RichText::new(text).size(12.5).color(color).strong());
            });
        });
}

fn legend(ui: &mut Ui, text: &str, color: Color32) {
    ui.label(RichText::new(text).size(12.0).color(MUTED));
    let (r, _) = ui.allocate_exact_size(vec2(14.0, 10.0), Sense::hover());
    ui.painter().rect_filled(Rect::from_center_size(r.center(), vec2(12.0, 3.0)), 2, color);
    ui.add_space(6.0);
}

fn accent_button(ui: &mut Ui, text: &str, filled: bool) -> egui::Response {
    let (fill, color) = if filled { (ACCENT, BG) } else { (CARD_HI, TEXT) };
    ui.add(egui::Button::new(RichText::new(text).color(color).strong()).fill(fill).corner_radius(8).min_size(vec2(110.0, 30.0)))
}

fn danger_button(ui: &mut Ui, text: &str) -> egui::Response {
    ui.add(egui::Button::new(RichText::new(text).color(Color32::WHITE).strong()).fill(DANGER.gamma_multiply(0.8)))
}

fn wide_button(ui: &mut Ui, text: &str) -> egui::Response {
    let w = ui.available_width();
    ui.add(
        egui::Button::new(RichText::new(text).color(ACCENT).strong())
            .fill(Color32::TRANSPARENT)
            .stroke(Stroke::new(1.0, ACCENT.gamma_multiply(0.5)))
            .corner_radius(8)
            .min_size(vec2(w, 32.0)),
    )
}

fn nav_item(ui: &mut Ui, text: &str, selected: bool) -> egui::Response {
    let w = ui.available_width();
    let fill = if selected { CARD_HI } else { Color32::TRANSPARENT };
    ui.add(egui::Button::new(RichText::new(text).color(if selected { TEXT } else { MUTED })).fill(fill).min_size(vec2(w, 32.0)))
}

/// iOS-style switch.
fn toggle(ui: &mut Ui, on: &mut bool) -> egui::Response {
    let size = vec2(38.0, 21.0);
    let (rect, mut response) = ui.allocate_exact_size(size, Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    let t = ui.ctx().animate_bool_responsive(response.id, *on);
    // The off track must stand out from both card colours, including a selected card.
    let track = egui::lerp(egui::Rgba::from(FAINT)..=egui::Rgba::from(ACCENT), t);
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(11), Color32::from(track));
    if response.hovered() {
        painter.rect_stroke(rect, CornerRadius::same(11), Stroke::new(1.0, FAINT), StrokeKind::Outside);
    }
    let x = egui::lerp(rect.left() + 10.5..=rect.right() - 10.5, t);
    painter.circle_filled(pos2(x, rect.center().y), 7.5, Color32::WHITE);
    response
}

fn segmented<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str)]) -> bool {
    let mut changed = false;
    egui::Frame::new().fill(BG).corner_radius(8).inner_margin(Margin::same(3)).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for (v, label) in options {
                let selected = *value == *v;
                let text = RichText::new(*label).color(if selected { BG } else { MUTED }).strong();
                let fill = if selected { ACCENT } else { Color32::TRANSPARENT };
                if ui.add(egui::Button::new(text).fill(fill).corner_radius(6)).clicked() && !selected {
                    *value = *v;
                    changed = true;
                }
            }
        });
    });
    changed
}

/// One labelled parameter row with a hover explanation.
/// On narrow layouts the label goes on its own line above the control.
fn param_row(ui: &mut Ui, label: &str, hint: &str, add: impl FnOnce(&mut Ui) -> bool, changed: &mut bool) {
    if ui.available_width() < NARROW_ROW_WIDTH {
        ui.add(egui::Label::new(RichText::new(label).size(14.0).color(MUTED)).selectable(false)).on_hover_text(hint);
        ui.horizontal(|ui| {
            ui.spacing_mut().slider_width = (ui.available_width() - 130.0).clamp(120.0, 340.0);
            *changed |= add(ui);
        });
        ui.add_space(4.0);
        return;
    }
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(150.0, 26.0), Sense::hover());
        ui.painter().text(pos2(r.left(), r.center().y), Align2::LEFT_CENTER, label, FontId::proportional(14.0), MUTED);
        ui.interact(r, egui::Id::unique(("param_row", label)), Sense::hover()).on_hover_text(hint);
        ui.spacing_mut().slider_width = (ui.available_width() - 120.0).clamp(120.0, 340.0);
        *changed |= add(ui);
    });
    ui.add_space(2.0);
}

fn db_to_x(db: f32, rect: Rect) -> f32 {
    let t = ((db - METER_MIN_DB) / -METER_MIN_DB).clamp(0.0, 1.0);
    rect.left() + t * rect.width()
}

fn x_to_db(x: f32, rect: Rect) -> f32 {
    let t = ((x - rect.left()) / rect.width()).clamp(0.0, 1.0);
    METER_MIN_DB + t * -METER_MIN_DB
}

/// Threshold control drawn over a live level meter of the rule's triggers.
fn threshold_slider(ui: &mut Ui, threshold_db: &mut f32, level_db: f32) -> egui::Response {
    let width = (ui.spacing().slider_width + 70.0).min(ui.available_width());
    let (rect, mut response) = ui.allocate_exact_size(vec2(width, 26.0), Sense::click_and_drag());
    let bar = Rect::from_min_max(pos2(rect.left(), rect.center().y - 6.0), pos2(rect.right() - 70.0, rect.center().y + 6.0));
    if let Some(p) = response.interact_pointer_pos() {
        let db = x_to_db(p.x, bar).round();
        if db != *threshold_db {
            *threshold_db = db;
            response.mark_changed();
        }
    }
    if response.hovered() {
        // One wheel notch is 1 dB. Raw wheel events are counted because the smoothed delta
        // spreads a notch over several frames; the scroll is consumed so the page stays put.
        let steps: f32 = ui.input(|i| {
            i.events
                .iter()
                .map(|e| match e {
                    egui::Event::MouseWheel { unit, delta, .. } => match unit {
                        egui::MouseWheelUnit::Line => delta.y,
                        egui::MouseWheelUnit::Point => delta.y / 40.0,
                        egui::MouseWheelUnit::Page => delta.y * 10.0,
                    },
                    _ => 0.0,
                })
                .sum()
        });
        ui.ctx().input_mut(|i| i.smooth_scroll_delta = egui::Vec2::ZERO);
        let acc_id = response.id.with("wheel");
        let whole = ui.data_mut(|d| {
            let acc = d.get_temp_mut_or_default::<f32>(acc_id);
            *acc += steps;
            let whole = acc.trunc();
            *acc -= whole;
            whole
        });
        if whole != 0.0 {
            let db = (threshold_db.round() + whole).clamp(METER_MIN_DB, 0.0);
            if db != *threshold_db {
                *threshold_db = db;
                response.mark_changed();
            }
        }
    }

    let painter = ui.painter();
    painter.rect_filled(bar, 4, BG);
    let above = level_db > *threshold_db;
    let level_x = db_to_x(level_db, bar);
    if level_x > bar.left() + 1.0 {
        let fill = Rect::from_min_max(bar.min, pos2(level_x, bar.max.y));
        painter.rect_filled(fill, 4, if above { ACCENT } else { ACCENT.gamma_multiply(0.35) });
    }
    // Scale ticks every 12 dB.
    let mut db = METER_MIN_DB + 6.0;
    while db < 0.0 {
        let x = db_to_x(db, bar);
        painter.line_segment([pos2(x, bar.bottom() + 2.0), pos2(x, bar.bottom() + 5.0)], Stroke::new(1.0, FAINT));
        db += 12.0;
    }
    let tx = db_to_x(*threshold_db, bar);
    painter.line_segment([pos2(tx, bar.top() - 5.0), pos2(tx, bar.bottom() + 5.0)], Stroke::new(2.0, TEXT));
    painter.circle_filled(pos2(tx, bar.top() - 5.0), if response.hovered() || response.dragged() { 5.5 } else { 4.5 }, TEXT);
    painter.text(
        pos2(rect.right(), rect.center().y),
        Align2::RIGHT_CENTER,
        format!("{:.0} dB", threshold_db),
        FontId::proportional(14.0),
        TEXT,
    );
    response.on_hover_text("Drag or scroll to set the threshold")
}

fn level_meter(ui: &mut Ui, width: f32, level_db: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(width, 6.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 3, BG);
    let x = db_to_x(level_db, rect);
    if x > rect.left() + 0.5 {
        painter.rect_filled(Rect::from_min_max(rect.min, pos2(x, rect.max.y)), 3, color);
    }
}

/// Scrolling graph: trigger level (line) against threshold, and target volume (filled area).
fn history_graph(ui: &mut Ui, history: &VecDeque<Sample>, threshold_db: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 130.0), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 8, BG);
    let inner = rect.shrink2(vec2(8.0, 10.0));
    let y_db = |db: f32| {
        let t = ((db - METER_MIN_DB) / -METER_MIN_DB).clamp(0.0, 1.0);
        inner.bottom() - t * inner.height()
    };
    for db in [-12.0, -24.0, -36.0, -48.0, -60.0] {
        let y = y_db(db);
        painter.line_segment([pos2(inner.left(), y), pos2(inner.right(), y)], Stroke::new(1.0, CARD_HI));
        painter.text(pos2(inner.left() + 2.0, y - 1.0), Align2::LEFT_BOTTOM, format!("{db:.0}"), FontId::proportional(10.0), FAINT);
    }

    let Some(last) = history.back() else { return };
    let now = last.t;
    let x_of = |t: f64| inner.right() - ((now - t) / HISTORY_SECS) as f32 * inner.width();

    // Target volume as a filled area from the top: deeper fill = more ducking.
    let mut mesh = egui::Mesh::default();
    let fill = DUCK.gamma_multiply(0.22);
    let mut prev: Option<(f32, f32)> = None;
    for s in history.iter() {
        let x = x_of(s.t);
        let depth = 1.0 - s.gain.clamp(0.0, 1.0);
        let y = inner.top() + depth * inner.height();
        if let Some((px, py)) = prev {
            let base = mesh.vertices.len() as u32;
            mesh.colored_vertex(pos2(px, inner.top()), fill);
            mesh.colored_vertex(pos2(px, py), fill);
            mesh.colored_vertex(pos2(x, inner.top()), fill);
            mesh.colored_vertex(pos2(x, y), fill);
            mesh.add_triangle(base, base + 1, base + 2);
            mesh.add_triangle(base + 1, base + 2, base + 3);
        }
        prev = Some((x, y));
    }
    painter.add(mesh);
    let gain_line: Vec<Pos2> = history
        .iter()
        .map(|s| pos2(x_of(s.t), inner.top() + (1.0 - s.gain.clamp(0.0, 1.0)) * inner.height()))
        .collect();
    painter.line(gain_line, Stroke::new(1.5, DUCK));

    let level_line: Vec<Pos2> = history.iter().map(|s| pos2(x_of(s.t), y_db(s.level_db))).collect();
    painter.line(level_line, Stroke::new(1.5, ACCENT));

    let ty = y_db(threshold_db);
    let mut x = inner.left();
    while x < inner.right() {
        painter.line_segment([pos2(x, ty), pos2((x + 6.0).min(inner.right()), ty)], Stroke::new(1.0, MUTED));
        x += 10.0;
    }

    let gain_db = dsp::lin_to_db(last.gain);
    let label = if last.gain >= 0.999 { "0 dB".to_owned() } else if last.gain <= 0.0 { "muted".to_owned() } else { format!("{gain_db:.1} dB") };
    painter.text(pos2(inner.right() - 4.0, inner.top() + 2.0), Align2::RIGHT_TOP, label, FontId::proportional(13.0), DUCK);
}

/// Static sketch of the attack / hold / release shape for the current timing.
fn envelope_preview(ui: &mut Ui, rule: &Rule) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 56.0), Sense::hover());
    let painter = ui.painter_at(rect);
    let inner = rect.shrink2(vec2(4.0, 6.0));
    let speech = 600.0f32;
    let total = (rule.attack_ms.max(1.0) + speech + rule.hold_ms + rule.release_ms + 300.0).max(1.0);
    let x = |ms: f32| inner.left() + ms / total * inner.width();
    let top = inner.top();
    let low = inner.top() + (1.0 - dsp::gain_for(rule, 1.0)) * inner.height();
    let t0 = 150.0;
    let speech_rect = Rect::from_min_max(pos2(x(t0), inner.top()), pos2(x(t0 + speech), inner.bottom()));
    painter.rect_filled(speech_rect, 4, ACCENT.gamma_multiply(0.10));
    painter.text(speech_rect.center_bottom() - vec2(0.0, 2.0), Align2::CENTER_BOTTOM, "voice", FontId::proportional(11.0), ACCENT.gamma_multiply(0.7));

    let a_end = t0 + rule.attack_ms.min(speech);
    let h_end = t0 + speech + rule.hold_ms;
    let r_end = h_end + rule.release_ms;
    let pts = vec![pos2(inner.left(), top), pos2(x(t0), top), pos2(x(a_end), low), pos2(x(h_end), low), pos2(x(r_end), top), pos2(inner.right(), top)];
    painter.line(pts, Stroke::new(2.0, DUCK));
    painter.text(pos2(x(h_end), low + 3.0), Align2::RIGHT_TOP, "hold", FontId::proportional(11.0), MUTED);
}

fn rule_card(ui: &mut Ui, rule: &mut Rule, view: &RuleView, selected: bool) -> (bool, bool) {
    let mut toggled = false;
    let fill = if selected { CARD_HI } else { CARD };
    let stroke = if selected { Stroke::new(1.0, ACCENT.gamma_multiply(0.6)) } else { Stroke::new(1.0, BORDER) };
    let inner = egui::Frame::new().fill(fill).stroke(stroke).corner_radius(10).inner_margin(Margin::same(10)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            // Activity dot glows with the current duck amount.
            let (r, _) = ui.allocate_exact_size(vec2(10.0, 16.0), Sense::hover());
            let color = if !rule.enabled {
                FAINT
            } else if view.amount > 0.0 {
                lerp_color(ACCENT, DUCK, view.amount)
            } else if view.trigger_sessions > 0 {
                ACCENT
            } else {
                MUTED
            };
            if view.amount > 0.0 {
                ui.painter().circle_filled(r.center(), 7.0, DUCK.gamma_multiply(0.25 * view.amount));
            }
            ui.painter().circle_filled(r.center(), 4.0, color);
            let name_color = if rule.enabled { TEXT } else { MUTED };
            let name = if rule.name.trim().is_empty() { UNTITLED_RULE } else { rule.name.as_str() };
            // Leave room for the switch, or a long name runs underneath it.
            let name_width = (ui.available_width() - 48.0).max(20.0);
            ui.allocate_ui_with_layout(vec2(name_width, 20.0), Layout::left_to_right(Align::Center), |ui| {
                ui.add(egui::Label::new(semibold(name).color(name_color)).truncate().selectable(false));
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                toggled = toggle(ui, &mut rule.enabled).changed();
            });
        });
        let summary = format!("{}  →  {}", summarize(&rule.triggers), summarize(&rule.targets));
        ui.add(egui::Label::new(RichText::new(summary).size(12.0).color(MUTED)).truncate().selectable(false));
    });
    let response = ui.interact(inner.response.rect, egui::Id::unique(("rule_card", rule.uid)), Sense::click());
    (response.clicked() && !toggled, toggled)
}

fn summarize(list: &[AppMatcher]) -> String {
    match list {
        [] => "—".to_owned(),
        [one] => short_label(one),
        [first, rest @ ..] => format!("{} +{}", short_label(first), rest.len()),
    }
}

fn short_label(m: &AppMatcher) -> String {
    let l = m.label();
    l.strip_suffix(".exe").or_else(|| l.strip_suffix(".EXE")).unwrap_or(&l).to_owned()
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(f(a.r(), b.r()), f(a.g(), b.g()), f(a.b(), b.b()))
}

fn status_line(ui: &mut Ui, rule: &Rule, view: &RuleView, paused: bool) {
    let (text, color) = if !rule.enabled {
        ("Rule is off".to_owned(), MUTED)
    } else if paused {
        ("Paused globally".to_owned(), MUTED)
    } else if rule.triggers.is_empty() {
        ("Add at least one app whose sound should trigger ducking".to_owned(), DUCK)
    } else if rule.targets.is_empty() {
        ("Add at least one app to lower".to_owned(), DUCK)
    } else if view.trigger_sessions == 0 {
        ("Waiting — no trigger app is playing audio right now".to_owned(), MUTED)
    } else if view.target_sessions == 0 {
        ("Listening — no target app is playing right now".to_owned(), ACCENT)
    } else if view.amount > 0.0 {
        let n = view.target_sessions;
        (format!("Ducking {n} {} by {:.1} dB", if n == 1 { "app" } else { "apps" }, dsp::lin_to_db(view.gain)), DUCK)
    } else {
        (format!("Listening to {} source(s), ready to duck {} app(s)", view.trigger_sessions, view.target_sessions), ACCENT)
    };
    ui.label(RichText::new(text).color(color));
}

/// Titled trigger or target list of a rule.
fn apps_section(
    ui: &mut Ui,
    is_targets: bool,
    list: &mut Vec<AppMatcher>,
    other: &[AppMatcher],
    sessions: &[SessionView],
    manual: &mut String,
) -> bool {
    ui.label(section_title(if is_targets { "Lower the volume of" } else { "When sound plays in" }));
    ui.add_space(6.0);
    app_list(ui, list, other, sessions, is_targets, manual)
}

/// Editable list of matchers as chips, plus an "Add" menu with running apps and manual entry.
fn app_list(
    ui: &mut Ui,
    list: &mut Vec<AppMatcher>,
    other: &[AppMatcher],
    sessions: &[SessionView],
    is_targets: bool,
    manual: &mut String,
) -> bool {
    let mut changed = false;
    let mut remove = None;
    ui.horizontal_wrapped(|ui| {
        for (i, m) in list.iter().enumerate() {
            let running = *m == AppMatcher::AllOthers && !sessions.is_empty()
                || sessions.iter().any(|s| session_matcher(s).is_some_and(|sm| same_app(&sm, m)));
            if chip(ui, &m.label(), running, is_targets) {
                remove = Some(i);
            }
        }
        ui.menu_button(RichText::new(format!("{ICON_ADD}  Add")).color(ACCENT), |ui| {
            ui.set_min_width(240.0);
            ui.label(RichText::new("Playing now").size(12.0).color(MUTED));
            let mut seen: Vec<AppMatcher> = Vec::new();
            for s in sessions {
                let Some(m) = session_matcher(s) else { continue };
                if contains_app(list, &m) || seen.contains(&m) {
                    continue;
                }
                seen.push(m.clone());
                // A rule's own triggers are never ducked, so an app cannot be on both sides.
                let elsewhere = contains_app(other, &m);
                let hint = if is_targets { "Already a trigger of this rule" } else { "Already lowered by this rule" };
                let button = ui.add_enabled(!elsewhere, egui::Button::new(m.label())).on_disabled_hover_text(hint);
                if button.clicked() {
                    list.push(m);
                    changed = true;
                    ui.close();
                }
            }
            if seen.is_empty() {
                ui.label(RichText::new("nothing else is playing").color(FAINT));
            }
            ui.separator();
            if !list.contains(&AppMatcher::SystemSounds) && ui.button("System sounds").clicked() {
                list.push(AppMatcher::SystemSounds);
                changed = true;
                ui.close();
            }
            if is_targets && !list.contains(&AppMatcher::AllOthers) && ui.button("All other apps").on_hover_text("Everything except this rule's trigger apps").clicked() {
                list.push(AppMatcher::AllOthers);
                changed = true;
                ui.close();
            }
            ui.separator();
            ui.label(RichText::new("By program name").size(12.0).color(MUTED));
            ui.horizontal(|ui| {
                let edit = ui.add(egui::TextEdit::singleline(manual).hint_text("e.g. Spotify.exe").desired_width(150.0));
                let submit = ui.button("Add").clicked() || (edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                let name = manual.trim().to_owned();
                if submit && !name.is_empty() {
                    let m = AppMatcher::Process(name);
                    if !contains_app(list, &m) {
                        list.push(m);
                        changed = true;
                    }
                    manual.clear();
                    ui.close();
                }
            });
        });
    });
    if let Some(i) = remove {
        list.remove(i);
        changed = true;
    }
    if list.is_empty() {
        ui.label(RichText::new(if is_targets { "No apps to lower yet" } else { "No trigger apps yet" }).size(12.0).color(FAINT));
    }
    changed
}

/// Returns true when the remove button was clicked.
fn chip(ui: &mut Ui, text: &str, running: bool, is_target: bool) -> bool {
    let color = if is_target { DUCK } else { ACCENT };
    let mut remove = false;
    // A wrapping layout cannot wrap a frame before it is drawn, so measure the chip and start
    // a new row when it would overflow; otherwise it widens the whole editor.
    let text_width = ui.painter().layout_no_wrap(text.to_owned(), egui::TextStyle::Body.resolve(ui.style()), TEXT).size().x;
    let chip_width = text_width + 40.0;
    let left_in_row = ui.available_size_before_wrap().x;
    if left_in_row < chip_width && left_in_row < ui.max_rect().width() - 1.0 {
        ui.end_row();
    }
    egui::Frame::new()
        .fill(color.gamma_multiply(0.14))
        .stroke(Stroke::new(1.0, color.gamma_multiply(if running { 0.7 } else { 0.25 })))
        .corner_radius(14)
        .inner_margin(Margin { left: 10, right: 4, top: 2, bottom: 2 })
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.label(RichText::new(text).color(if running { TEXT } else { MUTED }));
                let x = ui.add(egui::Button::new(RichText::new(ICON_CLOSE).size(9.0).color(MUTED)).frame(false));
                if x.on_hover_text("Remove").clicked() {
                    remove = true;
                }
            });
        })
        .response
        .on_hover_text(if running { "Playing audio now" } else { "Not playing audio right now" });
    remove
}

fn session_matcher(s: &SessionView) -> Option<AppMatcher> {
    if s.is_system {
        Some(AppMatcher::SystemSounds)
    } else if !s.exe.is_empty() {
        Some(AppMatcher::Process(s.exe.clone()))
    } else {
        None
    }
}

/// Matchers name the same app the way the engine compares them (case and `.exe` ignored).
fn same_app(a: &AppMatcher, b: &AppMatcher) -> bool {
    match (a, b) {
        (AppMatcher::Process(x), AppMatcher::Process(y)) => config::normalize_exe(x) == config::normalize_exe(y),
        _ => a == b,
    }
}

fn contains_app(list: &[AppMatcher], m: &AppMatcher) -> bool {
    list.iter().any(|x| same_app(x, m))
}

/// One live session: name, device, meter, applied ducking, quick add-to-rule buttons.
fn session_row(ui: &mut Ui, s: &SessionView, matcher: Option<AppMatcher>, rule: Option<&mut Rule>) -> bool {
    let mut changed = false;
    let ducked = s.gain < 0.999;
    let stroke = if ducked { Stroke::new(1.0, DUCK.gamma_multiply(0.5)) } else { Stroke::new(1.0, BORDER) };
    egui::Frame::new().fill(CARD).stroke(stroke).corner_radius(10).inner_margin(Margin::symmetric(10, 8)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            let pids: Vec<String> = s.pids.iter().map(u32::to_string).collect();
            ui.add(egui::Label::new(semibold(s.label.clone()).color(TEXT)).truncate().selectable(false))
                .on_hover_text(format!("{}\npid {}\n{}", s.label, pids.join(", "), s.devices.join("\n")));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ducked {
                    let txt = if s.gain <= 0.0 { "muted".to_owned() } else { format!("{:.1} dB", dsp::lin_to_db(s.gain)) };
                    ui.label(RichText::new(txt).size(12.0).color(DUCK).strong());
                } else if let Some(v) = s.user_volume {
                    ui.label(RichText::new(format!("{:.0}%", v * 100.0)).size(12.0).color(MUTED))
                        .on_hover_text("This app's volume in the Windows mixer");
                }
            });
        });
        let device = match s.devices.as_slice() {
            [one] => one.clone(),
            many => format!("{} output devices", many.len()),
        };
        ui.add(egui::Label::new(RichText::new(device).size(11.5).color(FAINT)).truncate())
            .on_hover_text(s.devices.join("\n"));
        let meter_color = if ducked { DUCK } else { ACCENT };
        level_meter(ui, ui.available_width(), s.level_db, meter_color);

        if let (Some(rule), Some(m)) = (rule, matcher) {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                let is_trigger = contains_app(&rule.triggers, &m);
                let is_target = contains_app(&rule.targets, &m);
                if role_button(ui, "trigger", is_trigger, ACCENT).on_hover_text("Sound from this app ducks the targets").clicked() {
                    toggle_in(&mut rule.triggers, &m);
                    if !is_trigger {
                        remove_from(&mut rule.targets, &m);
                    }
                    changed = true;
                }
                if role_button(ui, "duck", is_target, DUCK).on_hover_text("This app gets quieter while a trigger plays").clicked() {
                    toggle_in(&mut rule.targets, &m);
                    if !is_target {
                        remove_from(&mut rule.triggers, &m);
                    }
                    changed = true;
                }
            });
        }
    });
    changed
}

fn role_button(ui: &mut Ui, text: &str, on: bool, color: Color32) -> egui::Response {
    let (fill, fg) = if on { (color.gamma_multiply(0.85), BG) } else { (Color32::TRANSPARENT, MUTED) };
    ui.add(
        egui::Button::new(RichText::new(text).size(12.0).color(fg))
            .fill(fill)
            .stroke(Stroke::new(1.0, if on { color } else { BORDER }))
            .corner_radius(10)
            .small(),
    )
}

fn toggle_in(list: &mut Vec<AppMatcher>, m: &AppMatcher) {
    if contains_app(list, m) {
        remove_from(list, m);
    } else {
        list.push(m.clone());
    }
}

fn remove_from(list: &mut Vec<AppMatcher>, m: &AppMatcher) {
    list.retain(|x| !same_app(x, m));
}
