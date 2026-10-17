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
