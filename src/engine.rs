//! Real-time ducking engine. Runs on its own thread: polls session meters, evaluates rules,
//! and drives target session volumes, always restoring the user's volume afterwards.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::audio::{AudioSystem, Session};
use crate::config::{self, AppMatcher, Config, Rule, normalize_exe};
use crate::dsp::{self, MirrorFilter, OtherSession, OutputHistory, OwnPeak, RuleState};
use crate::timer::Ticker;

/// Poll interval when no trigger application is running.
const IDLE_TICK_MS: u32 = 100;
/// Volume differences smaller than this are not written.
const WRITE_EPSILON: f32 = 0.0005;
/// A volume reading further than this from our last write means the user moved the slider.
const USER_CHANGE_EPSILON: f32 = 0.004;
/// Display meters fall at this rate.
const METER_FALL_DB_PER_S: f32 = 40.0;
/// Pending restores older than this are dropped.
const RESTORE_MAX_AGE_S: u64 = 3 * 24 * 3600;
/// Below this duck gain a user's slider move is taken as the new volume itself: dividing by a
/// near-zero gain would turn a small nudge of a muted app into full volume after the duck.
const MIN_REBASE_GAIN: f32 = 0.05;
/// After this many failures in a row the engine stops restarting itself.
const MAX_ENGINE_RESTARTS: u32 = 5;
/// An engine run at least this long before failing resets the failure count.
const HEALTHY_RUN: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Default)]
pub struct SessionView {
    pub label: String,
    pub exe: String,
    pub is_system: bool,
    pub pids: Vec<u32>,
    pub devices: Vec<String>,
    /// Display meter, dBFS (loudest of the app's sessions).
    pub level_db: f32,
    /// Multiplier currently applied by ducking (1 = untouched; lowest of the app's sessions).
    pub gain: f32,
    /// The user's own volume for this app (what it returns to).
    pub user_volume: Option<f32>,
}

#[derive(Debug, Clone, Default)]
pub struct RuleView {
    /// Loudest trigger peak, dBFS (display meter).
    pub level_db: f32,
    pub amount: f32,
    pub gain: f32,
    pub trigger_sessions: usize,
    pub target_sessions: usize,
}

#[derive(Debug, Clone, Default)]
pub struct Telemetry {
    pub sessions: Vec<SessionView>,
    pub rules: HashMap<u64, RuleView>,
    pub ducking: bool,
    pub idle: bool,
    pub error: Option<String>,
}

/// State shared between the engine thread and the UI.
pub struct Shared {
    pub config: Mutex<Config>,
    config_rev: AtomicU64,
    pub telemetry: Mutex<Telemetry>,
    shutdown: AtomicBool,
    /// Set by the engine thread once it has restored every volume and exited.
    stopped: (Mutex<bool>, Condvar),
    /// Milliseconds since `epoch` of the last UI frame; meters are only computed for a visible UI.
    ui_seen_ms: AtomicU64,
    epoch: Instant,
    /// Called when the ducking/pause state flips, so the tray icon can update.
    pub notify: Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl Shared {
    pub fn new(config: Config) -> Arc<Self> {
        Arc::new(Self {
            config: Mutex::new(config),
            config_rev: AtomicU64::new(1),
            telemetry: Mutex::new(Telemetry::default()),
            shutdown: AtomicBool::new(false),
            stopped: (Mutex::new(false), Condvar::new()),
            ui_seen_ms: AtomicU64::new(0),
            epoch: Instant::now(),
            notify: Mutex::new(None),
        })
    }

    /// Applies an edit to the configuration and hands it to the engine.
    pub fn update_config(&self, f: impl FnOnce(&mut Config)) {
        let mut cfg = self.config.lock().unwrap();
        f(&mut cfg);
        drop(cfg);
        self.config_rev.fetch_add(1, Ordering::Release);
    }

    pub fn config_snapshot(&self) -> Config {
        self.config.lock().unwrap().clone()
    }

    pub fn mark_ui_frame(&self) {
        self.ui_seen_ms.store(self.epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    fn ui_visible(&self) -> bool {
        let now = self.epoch.elapsed().as_millis() as u64;
        now.saturating_sub(self.ui_seen_ms.load(Ordering::Relaxed)) < 500
    }

    /// Asks the engine to stop and blocks until it has restored all volumes (or `timeout` passes).
    /// Safe to call from any thread, any number of times.
    pub fn stop_engine_and_wait(&self, timeout: Duration) -> bool {
        self.shutdown.store(true, Ordering::Release);
        let (lock, cv) = &self.stopped;
        let guard = lock.lock().unwrap();
        let (guard, _) = cv.wait_timeout_while(guard, timeout, |stopped| !*stopped).unwrap();
        *guard
    }

    fn notify(&self) {
        if let Some(f) = self.notify.lock().unwrap().as_ref() {
            f();
        }
    }
}

pub struct EngineHandle {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl EngineHandle {
    pub fn start(shared: Arc<Shared>) -> Self {
        let s = shared.clone();
        let thread = std::thread::Builder::new()
            .name("ducking-engine".into())
            .spawn(move || run(s))
            .expect("spawn engine thread");
        Self { shared, thread: Some(thread) }
    }

    /// Stops the engine and waits until every ducked volume has been restored.
    pub fn stop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
        if let Some(t) = self.thread.take()
            && t.join().is_err() {
                log::error!("engine thread panicked during shutdown");
            }
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------------------------

/// A session whose volume the engine currently controls.
#[derive(Debug, Clone)]
struct Managed {
    /// The user's volume, restored when ducking ends.
    base: f32,
    last_written: f32,
    gain: f32,
    /// Kept so the volume can still be handed back if the session disappears while ducked.
    session_id: String,
    label: String,
}

impl Managed {
    fn new(current: f32, session_id: String, label: String) -> Self {
        Self { base: current, last_written: current, gain: 1.0, session_id, label }
    }

    /// Detects a slider move by the user (a reading that differs from our last write) and folds
    /// it into the base volume. Returns true when the base changed.
    fn absorb_user_change(&mut self, current: f32) -> bool {
        if (current - self.last_written).abs() <= USER_CHANGE_EPSILON {
            return false;
        }
        self.base = if self.gain >= MIN_REBASE_GAIN { (current / self.gain).min(1.0) } else { current };
        self.last_written = current;
        true
    }

    fn restore_entry(&self, instance_id: &str, now: u64) -> RestoreEntry {
        RestoreEntry {
            instance_id: instance_id.to_owned(),
            session_id: self.session_id.clone(),
            exe: self.label.clone(),
            volume: self.base,
            saved_unix: now,
        }
    }
}

/// Crash-safety record of volumes that must be returned to the user.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct RestoreFile {
    entries: Vec<RestoreEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RestoreEntry {
    instance_id: String,
    session_id: String,
    exe: String,
    volume: f32,
    saved_unix: u64,
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn restore_path() -> std::path::PathBuf {
    config::app_dir().join("restore.toml")
}

struct Engine {
    shared: Arc<Shared>,
    audio: AudioSystem,
    cfg: Config,
    cfg_rev: u64,
    rule_states: HashMap<u64, RuleState>,
    /// `cfg.rules` with matchers pre-normalized, index-aligned.
    compiled: Vec<CompiledRule>,
    managed: HashMap<String, Managed>,
    /// Volumes from a previous run that still have to be restored when their app shows up.
    pending_restore: Vec<RestoreEntry>,
    restore_dirty: bool,
    meters: HashMap<String, f32>,
    /// Per session: whether its meter also carries other apps' sound.
    mirrors: HashMap<String, MirrorFilter>,
    /// Per session: recent readings, which copies of its sound in other sessions trail.
    histories: HashMap<String, OutputHistory>,
    /// Milliseconds of engine time, for `histories`.
    clock_ms: f64,
    rule_meters: HashMap<u64, f32>,
    last_telemetry: Instant,
    was_ducking: bool,
    was_paused: bool,
}

fn run(shared: Arc<Shared>) {
    /// Signals `stopped` however the engine thread ends, including by panic.
    struct StoppedGuard(Arc<Shared>);
    impl Drop for StoppedGuard {
        fn drop(&mut self) {
            let (lock, cv) = &self.0.stopped;
            *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
            cv.notify_all();
        }
    }
    let _guard = StoppedGuard(shared.clone());
    run_engine(shared);
}

fn run_engine(shared: Arc<Shared>) {
    if let Err(e) = crate::audio::init_com() {
        log::error!("COM initialization failed: {e}");
        shared.telemetry.lock().unwrap().error = Some(format!("COM initialization failed: {e}"));
        return;
    }
    // A panic inside the loop must neither leave apps ducked nor leave the program running
    // without an engine: volumes are restored and the engine starts again from a clean state.
    let mut failures = 0;
    loop {
        let Some(mut engine) = Engine::start(shared.clone()) else { return };
        let started = Instant::now();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| engine.main_loop()));
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| engine.restore_all())).is_err() {
            log::error!("restoring volumes failed; restore.toml keeps them for the next start");
        }
        if outcome.is_ok() {
            log::info!("engine stopped, all volumes restored");
            return;
        }
        drop(engine);
        if started.elapsed() >= HEALTHY_RUN {
            failures = 0;
        }
        failures += 1;
        if failures > MAX_ENGINE_RESTARTS {
            log::error!("engine failed {failures} times in a row; giving up");
            set_error(&shared, Some("Ducking engine failed repeatedly; see the log and restart LoudControl".into()));
            return;
        }
        log::error!("engine failed; restarting ({failures}/{MAX_ENGINE_RESTARTS})");
        if !sleep_unless_shutdown(&shared, Duration::from_secs(1)) {
            return;
        }
    }
}

/// Sets the error shown in the UI; tolerates a mutex poisoned by an earlier panic.
fn set_error(shared: &Shared, error: Option<String>) {
    shared.telemetry.lock().unwrap_or_else(|e| e.into_inner()).error = error;
}

/// Sleeps in short steps; returns false as soon as shutdown is requested.
fn sleep_unless_shutdown(shared: &Shared, total: Duration) -> bool {
    let step = Duration::from_millis(50);
    let mut left = total;
    while !left.is_zero() {
        if shared.shutdown.load(Ordering::Acquire) {
            return false;
        }
        let d = left.min(step);
        std::thread::sleep(d);
        left -= d;
    }
    !shared.shutdown.load(Ordering::Acquire)
}

impl Engine {
    /// Opens the audio system (retrying while it is unavailable) and loads pending restores.
    /// Returns `None` if shutdown was requested meanwhile.
    fn start(shared: Arc<Shared>) -> Option<Self> {
        // The audio service can be briefly unavailable (e.g. right after logon); keep retrying.
        let audio = loop {
            match AudioSystem::new() {
                Ok(a) => break a,
                Err(e) => {
                    log::warn!("audio system unavailable, retrying: {e}");
                    set_error(&shared, Some(format!("Audio system unavailable: {e}")));
                    if !sleep_unless_shutdown(&shared, Duration::from_secs(2)) {
                        return None;
                    }
                }
            }
        };
        set_error(&shared, None);

        let pending_restore = load_restore_file();
        if !pending_restore.is_empty() {
            log::warn!("{} volume(s) left ducked earlier will be restored", pending_restore.len());
        }
        let cfg = shared.config_snapshot();
        let compiled = cfg.rules.iter().map(CompiledRule::new).collect();
        log::info!("engine started");
        Some(Engine {
            compiled,
            cfg_rev: shared.config_rev.load(Ordering::Acquire),
            cfg,
            shared,
            audio,
            rule_states: HashMap::new(),
            managed: HashMap::new(),
            pending_restore,
            restore_dirty: true,
            meters: HashMap::new(),
            mirrors: HashMap::new(),
            histories: HashMap::new(),
            clock_ms: 0.0,
            rule_meters: HashMap::new(),
            last_telemetry: Instant::now().checked_sub(Duration::from_secs(1)).unwrap_or_else(Instant::now),
            was_ducking: false,
            was_paused: false,
        })
    }

    fn main_loop(&mut self) {
        let mut ticker = Ticker::new();
        let mut last = Instant::now();
        self.audio.refresh(true);
        self.apply_pending_restore();

        while !self.shared.shutdown.load(Ordering::Acquire) {
            let now = Instant::now();
            let dt_ms = (now - last).as_secs_f32() * 1000.0;
            last = now;

            let rev = self.shared.config_rev.load(Ordering::Acquire);
            if rev != self.cfg_rev {
                self.cfg_rev = rev;
                self.cfg = self.shared.config_snapshot();
                self.compiled = self.cfg.rules.iter().map(CompiledRule::new).collect();
                let live: Vec<u64> = self.cfg.rules.iter().map(|r| r.uid).collect();
                self.rule_states.retain(|uid, _| live.contains(uid));
                self.rule_meters.retain(|uid, _| live.contains(uid));
            }

            let refreshed = self.audio.refresh(false);
            for id in &refreshed.removed {
                self.meters.remove(id);
                self.mirrors.remove(id);
                self.histories.remove(id);
                if let Some(m) = self.managed.remove(id) {
                    // The app closed (or lost its device) while ducked. Windows remembers the
                    // lowered volume for the app's next start, so the user's volume is handed
                    // back when its session returns.
                    log::info!("{} went away while ducked; {:.0}% will be restored when it returns", m.label, m.base * 100.0);
                    self.pending_restore.push(m.restore_entry(id, unix_now()));
                    self.restore_dirty = true;
                }
            }
            if !refreshed.added.is_empty() {
                self.apply_pending_restore();
            }
            if !refreshed.removed.is_empty() {
                self.prune_pending_restore();
            }

            let busy = self.tick(dt_ms.clamp(0.0, 250.0));

            if self.restore_dirty {
                self.restore_dirty = false;
                self.save_restore_file();
            }

            let wait = if busy { self.cfg.settings.tick_ms } else { IDLE_TICK_MS };
            ticker.sleep_ms(wait);
        }
    }

    /// One evaluation pass. Returns whether fast polling is needed.
    fn tick(&mut self, dt_ms: f32) -> bool {
        let paused = self.cfg.settings.paused;
        self.clock_ms += f64::from(dt_ms);
        let ui_visible = self.shared.ui_visible();
        let publish = self.last_telemetry.elapsed() >= Duration::from_millis(if ui_visible { 30 } else { 250 });

        // Peak and output (peak × volume) per session, read once per tick. Other sessions are read
        // when they share a device with a trigger, to tell a trigger's own sound from a copy of
        // theirs (see `dsp::MirrorFilter`), or for the UI.
        let read_all = ui_visible && publish;
        let trigger_devices: Vec<&str> = self
            .audio
            .sessions
            .values()
            .filter(|s| self.cfg.rules.iter().zip(&self.compiled).any(|(r, c)| r.enabled && c.is_trigger(s)))
            .map(|s| s.device_id.as_str())
            .collect();
        let mut raw: HashMap<&str, (OwnPeak, OtherSession)> = HashMap::with_capacity(self.audio.sessions.len());
        for (key, s) in &self.audio.sessions {
            if read_all || trigger_devices.contains(&s.device_id.as_str()) {
                let peak = s.peak();
                if !self.histories.contains_key(key) {
                    self.histories.insert(key.clone(), OutputHistory::default());
                }
                let history = self.histories.get_mut(key).expect("inserted above");
                history.push(self.clock_ms, peak, s.volume().unwrap_or(1.0));
                let filter = self.mirrors.get(key);
                let other = history.session(
                    filter.is_some_and(MirrorFilter::is_mirror),
                    filter.is_some_and(MirrorFilter::is_source),
                );
                raw.insert(key.as_str(), (history.own(), other));
            }
        }
        let mut peaks: HashMap<&str, f32> = HashMap::with_capacity(raw.len());
        let mut others: Vec<OtherSession> = Vec::new();
        for (key, s) in &self.audio.sessions {
            let Some(&(own, _)) = raw.get(key.as_str()) else { continue };
            others.clear();
            for (other_key, other) in &self.audio.sessions {
                if other_key != key
                    && other.device_id == s.device_id
                    && let Some(&(_, o)) = raw.get(other_key.as_str())
                {
                    others.push(o);
                }
            }
            let is_target = self.cfg.rules.iter().zip(&self.compiled).any(|(r, c)| r.enabled && c.is_target(s));
            if !self.mirrors.contains_key(key) {
                self.mirrors.insert(key.clone(), MirrorFilter::default());
            }
            let filter = self.mirrors.get_mut(key).expect("inserted above");
            let was_mirror = filter.is_mirror();
            peaks.insert(key.as_str(), filter.update(own, &others, !is_target));
            if filter.is_mirror() != was_mirror {
                if filter.is_mirror() {
                    log::info!("{} on {} also carries other apps' sound; only its own sound counts", s.label(), s.device_name);
                } else {
                    log::info!("{} on {} no longer carries other apps' sound", s.label(), s.device_name);
                }
            }
        }

        let mut target_gain: HashMap<&str, f32> = HashMap::new();
        let mut any_trigger_present = false;
        let mut rule_views = HashMap::with_capacity(self.cfg.rules.len());

        for (rule, compiled) in self.cfg.rules.iter().zip(&self.compiled) {
            let mut level = 0.0f32;
            let mut trigger_sessions = 0;
            for (key, s) in &self.audio.sessions {
                if compiled.is_trigger(s) {
                    trigger_sessions += 1;
                    level = level.max(peaks.get(key.as_str()).copied().unwrap_or(0.0));
                }
            }
            if rule.enabled && trigger_sessions > 0 {
                any_trigger_present = true;
            }
            let level_db = dsp::lin_to_db(level);
            let state = self.rule_states.entry(rule.uid).or_default();
            let force_off = paused || !rule.enabled || rule.targets.is_empty();
            state.update(rule, level_db, dt_ms, force_off);
            let gain = dsp::gain_for(rule, state.amount);

            let mut target_sessions = 0;
            for (key, s) in &self.audio.sessions {
                if compiled.is_target(s) {
                    target_sessions += 1;
                    if gain < 1.0 {
                        let g = target_gain.entry(key.as_str()).or_insert(1.0);
                        *g = g.min(gain);
                    }
                }
            }

            let meter = self.rule_meters.entry(rule.uid).or_insert(dsp::FLOOR_DB);
            *meter = fall_meter(*meter, level_db, dt_ms);
            rule_views.insert(
                rule.uid,
                RuleView {
                    level_db: *meter,
                    amount: state.amount,
                    gain,
                    trigger_sessions,
                    target_sessions,
                },
            );
        }

        for (key, s) in &self.audio.sessions {
            let g = target_gain.get(key.as_str()).copied().unwrap_or(1.0);
            apply_gain(s, key, g, &mut self.managed, &mut self.restore_dirty);
        }

        let ducking = !self.managed.is_empty();
        if ducking != self.was_ducking || paused != self.was_paused {
            self.was_ducking = ducking;
            self.was_paused = paused;
            self.shared.notify();
        }

        if publish {
            let since_ms = self.last_telemetry.elapsed().as_secs_f32() * 1000.0;
            self.last_telemetry = Instant::now();
            // One row per application: the same app (and "System sounds") can have a session on
            // every output device.
            let mut sessions: Vec<SessionView> = Vec::new();
            let mut index: HashMap<String, usize> = HashMap::new();
            for (key, s) in &self.audio.sessions {
                let level_db = dsp::lin_to_db(peaks.get(key.as_str()).copied().unwrap_or(0.0));
                let meter = self.meters.entry(key.clone()).or_insert(dsp::FLOOR_DB);
                *meter = fall_meter(*meter, level_db, since_ms);
                let managed = self.managed.get(key);
                let gain = managed.map(|m| m.gain).unwrap_or(1.0);
                let user_volume = managed.map(|m| m.base).or_else(|| s.volume().ok());
                let group = if s.is_system {
                    "@system".to_owned()
                } else if !s.exe_norm.is_empty() {
                    s.exe_norm.clone()
                } else {
                    key.clone()
                };
                match index.get(&group) {
                    Some(&i) => {
                        let v = &mut sessions[i];
                        v.level_db = v.level_db.max(*meter);
                        v.gain = v.gain.min(gain);
                        if managed.is_some() || v.user_volume.is_none() {
                            v.user_volume = user_volume;
                        }
                        if !v.devices.contains(&s.device_name) {
                            v.devices.push(s.device_name.clone());
                        }
                        if !v.pids.contains(&s.pid) {
                            v.pids.push(s.pid);
                        }
                    }
                    None => {
                        index.insert(group, sessions.len());
                        sessions.push(SessionView {
                            label: s.label(),
                            exe: s.exe.clone(),
                            is_system: s.is_system,
                            pids: vec![s.pid],
                            devices: vec![s.device_name.clone()],
                            level_db: *meter,
                            gain,
                            user_volume,
                        });
                    }
                }
            }
            sessions.sort_by_key(|a| (a.is_system, a.label.to_lowercase()));
            let mut t = self.shared.telemetry.lock().unwrap();
            t.sessions = sessions;
            t.rules = rule_views;
            t.ducking = ducking;
            t.idle = !any_trigger_present;
        }

        let releasing = self.rule_states.values().any(|s| s.amount > 0.0);
        (!paused && any_trigger_present) || releasing || ducking
    }

    fn restore_all(&mut self) {
        let now = unix_now();
        for (key, m) in std::mem::take(&mut self.managed) {
            let restored = match self.audio.sessions.get(&key) {
                Some(s) => match s.set_volume(m.base) {
                    Ok(()) => {
                        log::info!("restored {} to {:.0}%", m.label, m.base * 100.0);
                        true
                    }
                    Err(e) => {
                        log::warn!("cannot restore {}: {e}", m.label);
                        false
                    }
                },
                None => false,
            };
            if !restored {
                // Kept in restore.toml, so the next start (or the app's return) fixes it.
                self.pending_restore.push(m.restore_entry(&key, now));
            }
        }
        self.save_restore_file();
    }

    /// Forgets pending restores that are too old to be trusted.
    fn prune_pending_restore(&mut self) {
        let now = unix_now();
        let before = self.pending_restore.len();
        self.pending_restore.retain(|e| now.saturating_sub(e.saved_unix) < RESTORE_MAX_AGE_S);
        if self.pending_restore.len() != before {
            self.restore_dirty = true;
        }
    }

    fn apply_pending_restore(&mut self) {
        if self.pending_restore.is_empty() {
            return;
        }
        let before = self.pending_restore.len();
        let sessions = &self.audio.sessions;
        let managed = &self.managed;
        self.pending_restore.retain(|e| {
            let hit = sessions.get_key_value(&e.instance_id).or_else(|| {
                sessions.iter().find(|(_, s)| !e.session_id.is_empty() && s.session_id == e.session_id)
            });
            match hit {
                Some((key, _)) if managed.contains_key(key) => {
                    // Another live session of the same app is being ducked right now; it carries
                    // its own base volume and restores it when released.
                    log::debug!("dropping pending restore of {}: the app is ducked again", e.exe);
                    false
                }
                Some((_, s)) => {
                    match s.set_volume(e.volume) {
                        Ok(()) => log::info!("restored {} to {:.0}% (left ducked earlier)", e.exe, e.volume * 100.0),
                        Err(err) => log::warn!("cannot restore {}: {err}", e.exe),
                    }
                    false
                }
                None => true,
            }
        });
        if self.pending_restore.len() != before {
            self.restore_dirty = true;
        }
    }

    fn save_restore_file(&self) {
        let now = unix_now();
        let mut file = RestoreFile { entries: self.managed.iter().map(|(key, m)| m.restore_entry(key, now)).collect() };
        file.entries.extend(self.pending_restore.iter().cloned());
        let path = restore_path();
        if file.entries.is_empty() {
            if path.exists() {
                let _ = std::fs::remove_file(&path);
            }
            return;
        }
        match toml::to_string(&file) {
            Ok(text) => {
                if let Err(e) = config::write_atomic(&path, text.as_bytes()) {
                    log::warn!("cannot write restore file: {e}");
                }
            }
            Err(e) => log::warn!("cannot serialize restore file: {e}"),
        }
    }
}

/// Drives one session toward `gain` × the user's volume, taking ownership only while ducked.
fn apply_gain(session: &Session, key: &str, gain: f32, managed: &mut HashMap<String, Managed>, dirty: &mut bool) {
    if gain >= 0.9995 {
        if let Some(mut m) = managed.remove(key) {
            // A slider move during the last step of the fade still counts.
            if let Ok(current) = session.volume()
                && m.absorb_user_change(current)
            {
                log::info!("{} volume changed by user; new base {:.0}%", m.label, m.base * 100.0);
            }
            if let Err(e) = session.set_volume(m.base) {
                log::warn!("cannot restore volume of {}: {e}", m.label);
            }
            log::debug!("released {} at {:.0}%", m.label, m.base * 100.0);
            *dirty = true;
        }
        return;
    }

    let current = match session.volume() {
        Ok(v) => v,
        Err(e) => {
            log::debug!("cannot read volume of {}: {e}", session.label());
            return;
        }
    };
    let m = match managed.get_mut(key) {
        Some(m) => {
            // The user moved this app's slider while it was ducked: treat it as a new base.
            if m.absorb_user_change(current) {
                *dirty = true;
                log::info!("{} volume changed by user; new base {:.0}%", m.label, m.base * 100.0);
            }
            m
        }
        None => {
            log::debug!("ducking {} from {:.0}%", session.label(), current * 100.0);
            *dirty = true;
            managed
                .entry(key.to_owned())
                .or_insert_with(|| Managed::new(current, session.session_id.clone(), session.label()))
        }
    };
    m.gain = gain;
    let want = (m.base * gain).clamp(0.0, 1.0);
    if (want - m.last_written).abs() > WRITE_EPSILON {
        match session.set_volume(want) {
            Ok(()) => m.last_written = want,
            Err(e) => log::debug!("cannot set volume of {}: {e}", session.label()),
        }
    }
}

fn load_restore_file() -> Vec<RestoreEntry> {
    let Ok(text) = std::fs::read_to_string(restore_path()) else { return Vec::new() };
    let now = unix_now();
    match toml::from_str::<RestoreFile>(&text) {
        Ok(f) => f.entries.into_iter().filter(|e| now.saturating_sub(e.saved_unix) < RESTORE_MAX_AGE_S).collect(),
        Err(e) => {
            log::warn!("ignoring unreadable restore file: {e}");
            Vec::new()
        }
    }
}

fn fall_meter(current: f32, level_db: f32, dt_ms: f32) -> f32 {
    level_db.max(current - METER_FALL_DB_PER_S * dt_ms / 1000.0).max(dsp::FLOOR_DB)
}

#[derive(Debug, Clone)]
enum Selector {
    Exe(String),
    System,
    Others,
}

impl Selector {
    fn new(m: &AppMatcher) -> Self {
        match m {
            AppMatcher::Process(name) => Selector::Exe(normalize_exe(name)),
            AppMatcher::SystemSounds => Selector::System,
            AppMatcher::AllOthers => Selector::Others,
        }
    }

    /// `exe_norm` is the session's executable name as normalized by [`normalize_exe`].
    fn matches(&self, is_system: bool, exe_norm: &str) -> bool {
        match self {
            Selector::Exe(name) => !is_system && !exe_norm.is_empty() && exe_norm == name,
            Selector::System => is_system,
            Selector::Others => false,
        }
    }
}

/// A rule's matchers in a form that is cheap to evaluate every tick.
#[derive(Debug, Clone)]
pub struct CompiledRule {
    triggers: Vec<Selector>,
    targets: Vec<Selector>,
}

impl CompiledRule {
    pub fn new(rule: &Rule) -> Self {
        Self {
            triggers: rule.triggers.iter().map(Selector::new).collect(),
            targets: rule.targets.iter().map(Selector::new).collect(),
        }
    }

    pub fn is_trigger(&self, s: &Session) -> bool {
        self.is_trigger_app(s.is_system, &s.exe_norm)
    }

    pub fn is_target(&self, s: &Session) -> bool {
        self.is_target_app(s.is_system, &s.exe_norm)
    }

    fn is_trigger_app(&self, is_system: bool, exe_norm: &str) -> bool {
        self.triggers.iter().any(|m| m.matches(is_system, exe_norm))
    }

    /// Targets never include the rule's own triggers, which would otherwise duck themselves.
    fn is_target_app(&self, is_system: bool, exe_norm: &str) -> bool {
        !self.is_trigger_app(is_system, exe_norm)
            && self.targets.iter().any(|m| matches!(m, Selector::Others) || m.matches(is_system, exe_norm))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compiled(triggers: Vec<AppMatcher>, targets: Vec<AppMatcher>) -> CompiledRule {
        CompiledRule::new(&Rule { triggers, targets, ..Rule::default() })
    }

    fn exe(name: &str) -> AppMatcher {
        AppMatcher::Process(name.to_owned())
    }

    #[test]
    fn matching_ignores_case_and_exe_suffix() {
        let r = compiled(vec![exe("Discord.EXE")], vec![exe("spotify")]);
        assert!(r.is_trigger_app(false, "discord"));
        assert!(r.is_target_app(false, "spotify"));
        assert!(!r.is_target_app(false, "discord"));
        assert!(!r.is_target_app(true, ""), "system sounds are not matched by a process name");
        assert!(!r.is_target_app(false, ""), "sessions without a known exe never match by name");
    }

    #[test]
    fn own_triggers_are_never_targets() {
        let r = compiled(vec![exe("discord.exe"), AppMatcher::SystemSounds], vec![AppMatcher::AllOthers, exe("Discord")]);
        assert!(!r.is_target_app(false, "discord"));
        assert!(!r.is_target_app(true, ""));
        assert!(r.is_target_app(false, "spotify"));
        assert!(r.is_target_app(false, ""), "all other apps includes sessions without a known exe");
    }

    #[test]
    fn user_change_rebases_volume() {
        let mut m = Managed::new(0.8, String::new(), "app".into());
        m.gain = 0.25;
        m.last_written = 0.2;
        assert!(!m.absorb_user_change(0.2 + USER_CHANGE_EPSILON / 2.0), "rounding noise is not a user change");
        assert!(m.absorb_user_change(0.1));
        assert!((m.base - 0.4).abs() < 1e-6, "ducked reading is scaled back up, got {}", m.base);
        assert_eq!(m.last_written, 0.1);
        assert!(m.absorb_user_change(0.5));
        assert_eq!(m.base, 1.0, "base is capped at full volume");
    }

    #[test]
    fn user_change_while_muted_is_taken_literally() {
        let mut m = Managed::new(0.8, String::new(), "app".into());
        m.gain = 0.0;
        m.last_written = 0.0;
        assert!(m.absorb_user_change(0.3));
        assert!((m.base - 0.3).abs() < 1e-6, "a nudge while muted must not become full volume");
    }

    #[test]
    fn restore_file_round_trips() {
        let m = Managed::new(0.6, "{sid}|app.exe".into(), "app.exe".into());
        let file = RestoreFile { entries: vec![m.restore_entry("inst-1", 42)] };
        let back: RestoreFile = toml::from_str(&toml::to_string(&file).unwrap()).unwrap();
        let e = &back.entries[0];
        assert_eq!((e.instance_id.as_str(), e.session_id.as_str(), e.exe.as_str()), ("inst-1", "{sid}|app.exe", "app.exe"));
        assert_eq!((e.volume, e.saved_unix), (0.6, 42));
    }
}
