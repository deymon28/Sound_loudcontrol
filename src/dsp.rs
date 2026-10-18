//! Pure ducking logic: trigger detection, hold, attack/release envelope and gain curves.
//! Kept free of any OS calls so it can be unit-tested.

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
/// A reading this close to the loudest other app counts as a copy of it.
const MIRROR_MATCH_DB: f32 = 0.5;
/// A reading this far above every other app is sound of the app's own.
const MIRROR_EXCESS_DB: f32 = 6.0;
/// A reading this far below another app rules out a copy of the device output.
const MIRROR_BELOW_DB: f32 = 3.0;
/// Own sound has to be this much louder than the other apps to count while mirroring.
const MIRROR_MARGIN_DB: f32 = 1.0;
const MIRROR_ON_MATCHES: u32 = 60;
const MIRROR_ON_EXCESS: u32 = 5;
const MIRROR_MAX_SCORE: u32 = 200;
const MIRROR_BELOW_PENALTY: u32 = 8;

/// Some apps keep a copy of the device output inside their own audio session. Discord, for
/// example, records the output it plays to, and Windows counts that recording on Discord's
/// session meter, so game sound reads as Discord sound. Such a meter equals the loudest other
/// app whenever the app itself is silent, rises above everything while it plays its own sound,
/// and never drops below the others. The source of the copied sound shows the same equality but
/// never rises above the copy, so it is not mistaken for a mirror.
///
/// Once a session is recognized as a mirror, only sound clearly louder than the other apps on
/// its device counts.
#[derive(Debug, Clone, Default)]
pub struct MirrorFilter {
    score: u32,
    excess: u32,
    mirror: bool,
}

impl MirrorFilter {
    pub fn is_mirror(&self) -> bool {
        self.mirror
    }

    /// `own` is the session's peak; `others_max` and `others_sum` are the loudest and the summed
    /// output (peak × volume) of the other sessions on the same device. Returns the peak that
    /// belongs to the app itself.
    pub fn update(&mut self, own: f32, others_max: f32, others_sum: f32) -> f32 {
        if others_max >= MIRROR_AUDIBLE {
            let diff_db = lin_to_db(own) - lin_to_db(others_max);
            if diff_db.abs() <= MIRROR_MATCH_DB {
                self.score = (self.score + 1).min(MIRROR_MAX_SCORE);
            } else if diff_db >= MIRROR_EXCESS_DB {
                self.excess = (self.excess + 1).min(MIRROR_MAX_SCORE);
            } else if diff_db <= -MIRROR_BELOW_DB {
                self.score = self.score.saturating_sub(MIRROR_BELOW_PENALTY);
                if self.score == 0 {
                    self.excess = 0;
                }
            }
        }
        if self.score >= MIRROR_ON_MATCHES && self.excess >= MIRROR_ON_EXCESS {
            self.mirror = true;
        } else if self.score == 0 {
            self.mirror = false;
        }
        if self.mirror && own <= others_sum * db_to_lin(MIRROR_MARGIN_DB) { 0.0 } else { own }
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

    #[test]
    fn mirror_of_device_output_is_recognized_and_filtered() {
        let mut f = MirrorFilter::default();
        for i in 0..200 {
            f.update(game(i).max(voice(i)), game(i), game(i));
        }
        assert!(f.is_mirror());
        assert_eq!(f.update(game(3), game(3), game(3)), 0.0, "copied game sound is dropped");
        let speech = db_to_lin(-10.0);
        assert_eq!(f.update(speech, game(3), game(3)), speech, "own sound still counts");
        assert_eq!(f.update(speech, 0.0, 0.0), speech, "own sound counts when nothing else plays");
    }

    #[test]
    fn source_of_mirrored_sound_is_not_a_mirror() {
        let mut f = MirrorFilter::default();
        for i in 0..400 {
            let copy = game(i).max(voice(i));
            assert_eq!(f.update(game(i), copy, copy), game(i));
        }
        assert!(!f.is_mirror());
        // Even with no speech at all the source is never louder than its copy.
        let mut f = MirrorFilter::default();
        for i in 0..400 {
            f.update(game(i), game(i), game(i));
        }
        assert!(!f.is_mirror());
    }

    #[test]
    fn independent_app_is_not_a_mirror() {
        let mut f = MirrorFilter::default();
        for i in 0..400 {
            let v = voice(i);
            assert_eq!(f.update(v, game(i), game(i)), v);
        }
        assert!(!f.is_mirror());
    }

    #[test]
    fn mirror_ends_when_the_app_stops_copying() {
        let mut f = MirrorFilter::default();
        for i in 0..200 {
            f.update(game(i).max(voice(i)), game(i), game(i));
        }
        assert!(f.is_mirror());
        for i in 0..100 {
            f.update(voice(i), game(i), game(i));
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
