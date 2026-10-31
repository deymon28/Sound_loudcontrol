//! Pure ducking logic: trigger detection, hold, attack/release envelope and gain curves.
//! Kept free of any OS calls so it can be unit-tested.

use std::collections::VecDeque;

use crate::config::{DuckMode, FadeCurve, Rule};

pub const FLOOR_DB: f32 = -96.0;
/// Deepest attenuation the decibel curve fades through when the duck level is 0 (mute).
const MAX_DUCK_DB: f32 = -60.0;
/// Fall rate of the loudness follower used by proportional mode.
const FOLLOWER_DECAY_DB_PER_S: f32 = 20.0;

pub fn lin_to_db(x: f32) -> f32 {
    if x <= 1e-5 { FLOOR_DB } else { (20.0 * x.log10()).max(FLOOR_DB) }
}

pub fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Per-rule runtime state, advanced once per engine tick.
#[derive(Debug, Clone)]
pub struct RuleState {
    above_ms: f32,
    triggered: bool,
    hold_left_ms: f32,
    follower_db: f32,
    /// Smoothed duck amount: 0 = untouched, 1 = fully ducked.
    pub amount: f32,
}

impl Default for RuleState {
    fn default() -> Self {
        Self { above_ms: 0.0, triggered: false, hold_left_ms: 0.0, follower_db: FLOOR_DB, amount: 0.0 }
    }
}

impl RuleState {
    #[cfg(test)]
    pub fn triggered(&self) -> bool {
        self.triggered
    }

    /// Advances the state by `dt_ms` given the loudest trigger peak in dBFS.
    /// `force_off` (pause, disabled rule) releases the duck smoothly.
    pub fn update(&mut self, rule: &Rule, level_db: f32, dt_ms: f32, force_off: bool) {
        let above = !force_off && level_db > rule.threshold_db;

        if force_off {
            self.triggered = false;
            self.above_ms = 0.0;
            self.hold_left_ms = 0.0;
        } else if above {
            self.above_ms += dt_ms;
            if self.triggered || self.above_ms >= rule.min_active_ms {
                self.triggered = true;
                self.hold_left_ms = rule.hold_ms;
            }
        } else {
            self.above_ms = (self.above_ms - dt_ms).max(0.0);
            if self.triggered {
                self.hold_left_ms -= dt_ms;
                if self.hold_left_ms <= 0.0 {
                    self.triggered = false;
                    self.above_ms = 0.0;
                }
            }
        }

        // The follower tracks peaks while sound is present and freezes during hold,
        // so proportional depth reflects the most recent speech, not the silence after it.
        if above {
            let decayed = self.follower_db - FOLLOWER_DECAY_DB_PER_S * dt_ms / 1000.0;
            self.follower_db = level_db.max(decayed);
        } else if !self.triggered {
            self.follower_db = FLOOR_DB;
        }

        let target = if !self.triggered {
            0.0
        } else {
            match rule.mode {
                DuckMode::Fixed => 1.0,
                DuckMode::Proportional => {
                    ((self.follower_db - rule.threshold_db) / rule.range_db.max(1.0)).clamp(0.0, 1.0)
                }
            }
        };

        self.amount = approach(self.amount, target, dt_ms, rule.attack_ms, rule.release_ms);
    }
}

/// Moves `current` toward `target` at a rate of one full swing per `attack_ms` (rising)
/// or `release_ms` (falling).
fn approach(current: f32, target: f32, dt_ms: f32, attack_ms: f32, release_ms: f32) -> f32 {
    if target > current {
        if attack_ms <= 0.0 { target } else { (current + dt_ms / attack_ms).min(target) }
    } else if target < current {
        if release_ms <= 0.0 { target } else { (current - dt_ms / release_ms).max(target) }
    } else {
        current
    }
}

/// Other apps on the device count as playing from this level up.
const MIRROR_AUDIBLE: f32 = 0.000_316; // -70 dBFS
/// A reading this close to a playing app counts as a copy of it.
const MIRROR_MATCH_DB: f32 = 0.5;
/// A reading this far above every playing app is sound of the app's own.
const MIRROR_EXCESS_DB: f32 = 6.0;
/// A reading this far below another session rules out a copy of the device output.
const MIRROR_BELOW_DB: f32 = 3.0;
/// Own sound has to be this much louder than the other apps to count while mirroring.
const MIRROR_MARGIN_DB: f32 = 1.0;
const MIRROR_ON_MATCHES: u32 = 60;
const MIRROR_ON_EXCESS: u32 = 5;
const MIRROR_MAX_SCORE: u32 = 200;
const MIRROR_BELOW_PENALTY: u32 = 8;
/// A session stays a known source for this many updates after it was last below another one.
const SOURCE_TTL: u32 = 500;
/// A copy of a session's sound and the session's own meter can be an engine period or two apart,
/// either way.
const COPY_PEAK_SKEW_MS: f64 = 40.0;
/// After a session's volume changes (ducking), copies of its sound keep the old volume for about
/// 0.1 s.
const COPY_VOLUME_LAG_MS: f64 = 300.0;

/// What a `MirrorFilter` needs to know about another session on the same device.
#[derive(Debug, Clone, Copy)]
pub struct OtherSession {
    /// Peak × session volume: what the session adds to the device output.
    pub output: f32,
    /// The loudest a copy of the session's sound can read now (see `OutputHistory`).
    pub ceiling: f32,
    /// The quietest a copy of the session's sound can read now.
    pub floor: f32,
    pub mirror: bool,
    pub source: bool,
}

/// A session's own peak meter, as its `MirrorFilter` weighs it.
#[derive(Debug, Clone, Copy)]
pub struct OwnPeak {
    pub now: f32,
    /// The lowest and highest readings of the last `COPY_PEAK_SKEW_MS`.
    pub low: f32,
    pub high: f32,
}

impl OwnPeak {
    #[cfg(test)]
    pub fn steady(peak: f32) -> Self {
        Self { now: peak, low: peak, high: peak }
    }
}

/// Recent readings of one session. Meters of copies of its sound in other sessions are not read
/// at the same moment as its own: a copy can show a sound that has just stopped or not yet
/// started, and it follows a volume change about 0.1 s late, so right after a duck it reads up to
/// the duck depth louder than the session's output. Comparing single readings would take those
/// differences for the copying app's own sound.
#[derive(Debug, Clone, Default)]
pub struct OutputHistory {
    /// (time in ms, peak, volume), oldest first.
    readings: VecDeque<(f64, f32, f32)>,
}

impl OutputHistory {
    pub fn push(&mut self, now_ms: f64, peak: f32, volume: f32) {
        self.readings.push_back((now_ms, peak, volume));
        // The previous reading always stays, so slow polling still covers the skew.
        while self.readings.len() > 2 && now_ms - self.readings[0].0 > COPY_VOLUME_LAG_MS {
            self.readings.pop_front();
        }
    }

    /// Readings within `COPY_PEAK_SKEW_MS` of the latest one, and always the previous reading.
    fn skew_window(&self) -> impl Iterator<Item = &(f64, f32, f32)> {
        let now = self.readings.back().map_or(0.0, |r| r.0);
        let previous = self.readings.len().saturating_sub(2);
        self.readings
            .iter()
            .enumerate()
            .filter(move |&(i, r)| i >= previous || now - r.0 <= COPY_PEAK_SKEW_MS)
            .map(|(_, r)| r)
    }

    /// The session as the filters of other sessions see it.
    pub fn session(&self, mirror: bool, source: bool) -> OtherSession {
        let Some(&(_, peak, volume)) = self.readings.back() else {
            return OtherSession { output: 0.0, ceiling: 0.0, floor: 0.0, mirror, source };
        };
        let (mut peak_max, mut peak_min) = (peak, peak);
        for &(_, p, _) in self.skew_window() {
            peak_max = peak_max.max(p);
            peak_min = peak_min.min(p);
        }
        let (mut volume_max, mut volume_min) = (volume, volume);
        for &(_, _, v) in &self.readings {
            volume_max = volume_max.max(v);
            volume_min = volume_min.min(v);
        }
        OtherSession { output: peak * volume, ceiling: peak_max * volume_max, floor: peak_min * volume_min, mirror, source }
    }

    /// The session's own peak for its own filter.
    pub fn own(&self) -> OwnPeak {
        let now = self.readings.back().map_or(0.0, |r| r.1);
        let (mut low, mut high) = (now, now);
        for &(_, p, _) in self.skew_window() {
            low = low.min(p);
            high = high.max(p);
        }
        OwnPeak { now, low, high }
    }
}

/// Some apps keep a copy of the device output inside their own audio session. Discord records
/// the output it plays to during a call, and audio visualizers record it to draw it; Windows
/// counts that recording on the app's session meter, so game sound reads as the app's sound.
///
/// A copy never reads below any other session on the device; an app that does is a known
/// source of sound. A session is recognized as a mirror when it repeatedly equals a known source
/// and also rises well above every known source (its own sound, or the sound of another
/// mirroring app, which no meter can tell apart). Other mirrors are never a reference: they
/// carry the same sound, including the app's own.
///
/// Once a session is recognized as a mirror, only sound clearly louder than the other apps on
/// its device that are not mirrors counts. Comparisons with other sessions allow for the skew
/// between a copy and what it copies (`OutputHistory`): a session is quieter than another only
/// when it stays quieter, and a mirror's own sound counts only when it lasts and stays above
/// everything the other apps played around it.
#[derive(Debug, Clone, Default)]
pub struct MirrorFilter {
    score: u32,
    excess: u32,
    mirror: bool,
    source_left: u32,
}

impl MirrorFilter {
    pub fn is_mirror(&self) -> bool {
        self.mirror
    }

    /// Whether the session was recently quieter than another one, which a copy never is.
    pub fn is_source(&self) -> bool {
        self.source_left > 0
    }

    /// `own` is the session's peak and `others` are the other sessions on the same device, as of
    /// the previous update. `may_mirror` is false for sessions that must always count as sources
    /// (ducking targets: taking one for a mirror would let its own sound keep itself ducked).
    /// Returns the peak that belongs to the app itself.
    pub fn update(&mut self, own: OwnPeak, others: &[OtherSession], may_mirror: bool) -> f32 {
        let all_floor = others.iter().map(|o| o.floor).fold(0.0, f32::max);
        let source_max = others.iter().filter(|o| o.source).map(|o| o.output).fold(0.0, f32::max);
        let own_db = lin_to_db(own.now);
        self.source_left = self.source_left.saturating_sub(1);
        if all_floor >= MIRROR_AUDIBLE && lin_to_db(own.high) <= lin_to_db(all_floor) - MIRROR_BELOW_DB {
            self.source_left = SOURCE_TTL;
            self.score = self.score.saturating_sub(MIRROR_BELOW_PENALTY);
            if self.score == 0 {
                self.excess = 0;
            }
        } else if source_max >= MIRROR_AUDIBLE {
            let diff_db = own_db - lin_to_db(source_max);
            if diff_db.abs() <= MIRROR_MATCH_DB {
                self.score = (self.score + 1).min(MIRROR_MAX_SCORE);
            } else if diff_db >= MIRROR_EXCESS_DB {
                self.excess = (self.excess + 1).min(MIRROR_MAX_SCORE);
            }
        }
        if !may_mirror {
            self.score = 0;
            self.excess = 0;
        }
        if self.score >= MIRROR_ON_MATCHES && self.excess >= MIRROR_ON_EXCESS {
            self.mirror = true;
            self.source_left = 0;
        } else if self.score == 0 {
            self.mirror = false;
        }
        let rest: f32 = others.iter().filter(|o| !o.mirror).map(|o| o.ceiling).sum();
        if self.mirror && own.low <= rest * db_to_lin(MIRROR_MARGIN_DB) { 0.0 } else { own.now }
    }
}

/// Volume multiplier applied to targets for a given duck amount.
pub fn gain_for(rule: &Rule, amount: f32) -> f32 {
    let amount = amount.clamp(0.0, 1.0);
    if amount <= 0.0 {
        return 1.0;
    }
    match rule.curve {
        FadeCurve::Linear => 1.0 - amount * (1.0 - rule.duck_level),
        FadeCurve::Decibel => {
            if rule.duck_level <= 0.0 && amount >= 1.0 {
                return 0.0;
            }
            let duck_db = lin_to_db(rule.duck_level).max(MAX_DUCK_DB);
            db_to_lin(amount * duck_db)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule() -> Rule {
        Rule {
            threshold_db: -40.0,
            min_active_ms: 30.0,
            attack_ms: 100.0,
            hold_ms: 200.0,
            release_ms: 400.0,
            duck_level: 0.25,
            ..Rule::default()
        }
    }

    fn run(state: &mut RuleState, rule: &Rule, level: f32, ms: u32) {
        for _ in 0..ms / 10 {
            state.update(rule, level, 10.0, false);
        }
    }

    #[test]
    fn short_blip_below_min_active_is_ignored() {
        let (r, mut s) = (rule(), RuleState::default());
        run(&mut s, &r, -10.0, 20);
        run(&mut s, &r, FLOOR_DB, 100);
        assert!(!s.triggered());
        assert_eq!(s.amount, 0.0);
    }

    #[test]
    fn attack_hold_release_timing() {
        let (r, mut s) = (rule(), RuleState::default());
        run(&mut s, &r, -10.0, 30);
        assert!(s.triggered());
        run(&mut s, &r, -10.0, 100);
        assert!((s.amount - 1.0).abs() < 1e-4, "fully ducked after attack");

        // Silence shorter than hold keeps the duck.
        run(&mut s, &r, FLOOR_DB, 190);
        assert!(s.triggered());
        assert!((s.amount - 1.0).abs() < 1e-4);

        // After hold expires, release takes release_ms.
        run(&mut s, &r, FLOOR_DB, 20);
        assert!(!s.triggered());
        run(&mut s, &r, FLOOR_DB, 200);
        assert!(s.amount > 0.4 && s.amount < 0.6, "half released, got {}", s.amount);
        run(&mut s, &r, FLOOR_DB, 220);
        assert_eq!(s.amount, 0.0);
    }

    #[test]
    fn force_off_releases_smoothly() {
        let (r, mut s) = (rule(), RuleState::default());
        run(&mut s, &r, -10.0, 200);
        s.update(&r, -10.0, 10.0, true);
        assert!(!s.triggered());
        assert!(s.amount < 1.0 && s.amount > 0.9);
    }

    #[test]
    fn proportional_depth_follows_loudness() {
        let mut r = rule();
        r.mode = DuckMode::Proportional;
        r.range_db = 20.0;
        r.attack_ms = 0.0;
        let mut s = RuleState::default();
        run(&mut s, &r, -30.0, 50);
        assert!((s.amount - 0.5).abs() < 1e-3, "got {}", s.amount);
        run(&mut s, &r, -5.0, 10);
        assert!((s.amount - 1.0).abs() < 1e-3);
    }

    /// Game sound wandering between -66 and -42 dBFS, and speech on every fifth tick.
    fn game(i: u32) -> f32 {
        db_to_lin(-66.0 + (i * 7 % 25) as f32)
    }
    fn voice(i: u32) -> f32 {
        if i.is_multiple_of(5) { db_to_lin(-10.0) } else { 0.0 }
    }

    /// A session whose output has not changed lately.
    fn steady(output: f32, mirror: bool, source: bool) -> OtherSession {
        OtherSession { output, ceiling: output, floor: output, mirror, source }
    }

    fn source(output: f32) -> OtherSession {
        steady(output, false, true)
    }

    /// Runs filters for sessions on one device, read every 10 ms like the engine does.
    /// `raw(i)` gives every session's (peak, volume) at tick `i`; `on_tick` sees the filters and
    /// what they returned.
    fn simulate_with(
        sessions: usize,
        ticks: u32,
        raw: impl Fn(u32) -> Vec<(f32, f32)>,
        mut on_tick: impl FnMut(u32, &[MirrorFilter], &[f32]),
    ) -> Vec<MirrorFilter> {
        let mut f = vec![MirrorFilter::default(); sessions];
        let mut h = vec![OutputHistory::default(); sessions];
        let mut own = vec![0.0; sessions];
        for i in 0..ticks {
            let raw = raw(i);
            let snapshot: Vec<OtherSession> = f
                .iter()
                .zip(&mut h)
                .zip(&raw)
                .map(|((f, h), &(peak, volume))| {
                    h.push(f64::from(i) * 10.0, peak, volume);
                    h.session(f.is_mirror(), f.is_source())
                })
                .collect();
            for (n, filter) in f.iter_mut().enumerate() {
                let others: Vec<OtherSession> =
                    snapshot.iter().enumerate().filter(|&(m, _)| m != n).map(|(_, o)| *o).collect();
                own[n] = filter.update(h[n].own(), &others, true);
            }
            on_tick(i, &f, &own);
        }
        f
    }

    /// Like `simulate_with` for sessions at full volume; `raw(i)` gives every session's peak.
    fn simulate(sessions: usize, ticks: u32, raw: impl Fn(u32) -> Vec<f32>) -> Vec<MirrorFilter> {
        simulate_with(sessions, ticks, |i| raw(i).into_iter().map(|p| (p, 1.0)).collect(), |_, _, _| {})
    }

    /// A game, Discord copying the device output with speech of its own, and `copies` pure
    /// copies (visualizers).
    fn call(copies: usize, ticks: u32) -> Vec<MirrorFilter> {
        simulate(copies + 2, ticks, |i| {
            let mix = game(i).max(voice(i));
            let mut raw = vec![game(i), mix];
            raw.extend(std::iter::repeat_n(mix, copies));
            raw
        })
    }

    #[test]
    fn mirror_of_device_output_is_recognized_and_filtered() {
        let f = call(0, 300);
        assert!(!f[0].is_mirror(), "the game is the source");
        assert!(f[0].is_source());
        let mut discord = f[1].clone();
        assert!(discord.is_mirror());
        let game_now = [source(game(3))];
        assert_eq!(discord.update(OwnPeak::steady(game(3)), &game_now, true), 0.0, "copied game sound is dropped");
        let speech = db_to_lin(-10.0);
        assert_eq!(discord.update(OwnPeak::steady(speech), &game_now, true), speech, "own sound still counts");
        assert_eq!(discord.update(OwnPeak::steady(speech), &[], true), speech, "own sound counts when nothing else plays");
    }

    #[test]
    fn several_mirrors_do_not_hide_each_others_sound() {
        let f = call(1, 300);
        assert!(!f[0].is_mirror());
        assert!(f[1].is_mirror() && f[2].is_mirror());
        // The visualizer carries the same speech; it must not count against Discord's own sound.
        let speech = db_to_lin(-10.0);
        let mut discord = f[1].clone();
        let others = [source(game(3)), steady(speech, true, false)];
        assert_eq!(discord.update(OwnPeak::steady(speech), &others, true), speech);
        let others = [source(game(3)), steady(game(3), true, false)];
        assert_eq!(discord.update(OwnPeak::steady(game(3)), &others, true), 0.0);
    }

    #[test]
    fn output_history_covers_the_lag_of_copies() {
        let mut h = OutputHistory::default();
        h.push(0.0, db_to_lin(-20.0), 1.0);
        h.push(10.0, db_to_lin(-60.0), 0.1);
        let s = h.session(false, false);
        assert!((lin_to_db(s.output) - -80.0).abs() < 0.01);
        assert!((lin_to_db(s.ceiling) - -20.0).abs() < 0.01, "a copy may still carry the last peak at full volume");
        assert!((lin_to_db(s.floor) - -80.0).abs() < 0.01);

        // Peaks count for 40 ms, volumes for 300 ms.
        h.push(100.0, db_to_lin(-60.0), 0.1);
        h.push(110.0, db_to_lin(-60.0), 0.1);
        let s = h.session(false, false);
        assert!((lin_to_db(s.ceiling) - -60.0).abs() < 0.01, "got {}", lin_to_db(s.ceiling));
        h.push(400.0, db_to_lin(-60.0), 0.1);
        let s = h.session(false, false);
        assert!((lin_to_db(s.ceiling) - -80.0).abs() < 0.01, "got {}", lin_to_db(s.ceiling));

        // With slow polling the previous reading still counts.
        h.push(2000.0, db_to_lin(-30.0), 1.0);
        let s = h.session(false, false);
        assert!((lin_to_db(s.ceiling) - -30.0).abs() < 0.01);
        assert!((lin_to_db(s.floor) - -80.0).abs() < 0.01);
        let own = h.own();
        assert!((lin_to_db(own.now) - -30.0).abs() < 0.01);
        assert!((lin_to_db(own.low) - -60.0).abs() < 0.01, "own peaks ignore the volume");
        assert!((lin_to_db(own.high) - -30.0).abs() < 0.01);
    }

    /// Notes that decay slowly and then stop at once, read every 10 ms.
    fn music(i: u32) -> f32 {
        let t = i % 40;
        if t < 30 { db_to_lin(-20.0 - 0.3 * t as f32) } else { db_to_lin(-70.0) }
    }

    #[test]
    fn skewed_copy_is_not_taken_for_own_sound() {
        // Music being ducked and released, and an app whose meter copies it one reading late or
        // early and follows its volume 100 ms late. The app speaks now and then during the first
        // 6 s.
        for ahead in [false, true] {
            let volume = |i: u32| if i < 600 || (i / 150).is_multiple_of(2) { 1.0 } else { 0.1 };
            let speech = |i: u32| if i < 600 && i % 50 < 10 { db_to_lin(-10.0) } else { 0.0 };
            let raw = |i: u32| {
                let copied = if ahead { i + 1 } else { i.saturating_sub(1) };
                let copy = music(copied) * volume(i.saturating_sub(10));
                vec![(music(i), volume(i)), (copy.max(speech(i)), 1.0)]
            };
            let mut leaked = Vec::new();
            let f = simulate_with(2, 3000, raw, |i, f, own| {
                if i >= 600 {
                    assert!(f[1].is_mirror() && !f[1].is_source(), "ahead {ahead}, tick {i}: the copy stays a mirror");
                    if own[1] > 0.0 {
                        leaked.push(i);
                    }
                }
                assert!(!f[0].is_mirror(), "ahead {ahead}, tick {i}: the music is the source");
            });
            assert!(leaked.is_empty(), "ahead {ahead}: copied music counted as own sound at ticks {leaked:?}");
            let mut app = f[1].clone();
            let speech = db_to_lin(-10.0);
            assert_eq!(app.update(OwnPeak::steady(speech), &[source(music(3))], true), speech, "own sound still counts");
        }
    }

    #[test]
    fn without_own_sound_nothing_is_taken_for_a_mirror() {
        // Nothing proves who copies whom, so every meter is used as it is.
        let f = simulate(3, 400, |i| vec![game(i); 3]);
        assert!(f.iter().all(|f| !f.is_mirror()));
    }

    #[test]
    fn loudest_source_next_to_a_mirror_is_not_a_mirror() {
        // Music louder than the game the whole time; Discord copies both and speaks now and then.
        let music = db_to_lin(-20.0);
        let f = simulate(3, 1000, |i| vec![music, game(i), music.max(voice(i))]);
        assert!(!f[0].is_mirror(), "music");
        assert!(!f[1].is_mirror(), "game");
        assert!(f[2].is_mirror(), "discord");
    }

    #[test]
    fn independent_app_is_not_a_mirror() {
        let f = simulate(2, 400, |i| vec![game(i), voice(i)]);
        assert!(f.iter().all(|f| !f.is_mirror()));
    }

    #[test]
    fn targets_are_never_mirrors() {
        let mut f = MirrorFilter::default();
        for i in 0..400 {
            let mix = game(i).max(voice(i));
            assert_eq!(f.update(OwnPeak::steady(mix), &[source(game(i))], false), mix);
        }
        assert!(!f.is_mirror());
    }

    #[test]
    fn mirror_ends_when_the_app_stops_copying() {
        let mut f = call(0, 300).remove(1);
        assert!(f.is_mirror());
        for i in 0..100 {
            f.update(OwnPeak::steady(voice(i)), &[source(game(i))], true);
        }
        assert!(!f.is_mirror());
    }

    #[test]
    fn gain_curves() {
        let mut r = rule();
        assert_eq!(gain_for(&r, 0.0), 1.0);
        assert!((gain_for(&r, 1.0) - 0.25).abs() < 1e-4);
        // Decibel midpoint is the geometric mean: -6 dB of -12 dB.
        assert!((gain_for(&r, 0.5) - 0.5).abs() < 1e-3);
        r.curve = FadeCurve::Linear;
        assert!((gain_for(&r, 0.5) - 0.625).abs() < 1e-4);
        r.curve = FadeCurve::Decibel;
        r.duck_level = 0.0;
        assert_eq!(gain_for(&r, 1.0), 0.0);
    }
}
