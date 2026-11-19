//! Right-button hold gesture as a pure state machine.
//!
//! The mouse hook withholds the right-button press until it knows what the
//! user meant:
//!
//! * released before the hold time: an ordinary click, delivered late;
//! * moved past the tolerance, or combined with another button or the wheel:
//!   an ordinary drag or chord, delivered late and then left alone;
//! * still held when the hold time elapses: the panel opens and the application
//!   never sees the press.
//!
//! Callers feed real (non-injected) input only and act on the returned
//! [`Outcome`]. Time is any monotonic millisecond counter.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pos {
    pub x: i32,
    pub y: i32,
}

impl Pos {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }

    /// Chebyshev distance: the larger of the two axis deltas.
    pub fn distance(self, other: Pos) -> i32 {
        (self.x - other.x).abs().max((self.y - other.y).abs())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    None,
    /// Released early. Deliver the click the application never saw.
    Click { down: Pos, up: Pos },
    /// The press became a drag or a chord. Deliver the withheld press at `at`
    /// and put the cursor back at `now`.
    Flush { at: Pos, now: Pos },
    /// Hold time elapsed. `held` is false when the button was already released
    /// by the time this was noticed, in which case no [`Effect::Release`]
    /// follows.
    Trigger { pos: Pos, held: bool },
    /// The button was released after the panel opened.
    Release { pos: Pos },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// Whether the event that produced this outcome must be withheld from the
    /// rest of the system.
    pub swallow: bool,
    pub effect: Effect,
}

impl Outcome {
    pub const PASS: Self = Self { swallow: false, effect: Effect::None };
    pub const SWALLOW: Self = Self { swallow: true, effect: Effect::None };

    const fn pass(effect: Effect) -> Self {
        Self { swallow: false, effect }
    }

    const fn swallow(effect: Effect) -> Self {
        Self { swallow: true, effect }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Idle,
    /// Press withheld, waiting to learn what it is.
    Pressed { down: Pos, deadline: u64, tolerance: i32 },
    /// Panel opened, button still down.
    Held,
    /// Press was delivered; stay out of the way until release.
    Passing,
}

#[derive(Debug)]
pub struct Gesture {
    state: State,
}

impl Default for Gesture {
    fn default() -> Self {
        Self::new()
    }
}

impl Gesture {
    pub const fn new() -> Self {
        Self { state: State::Idle }
    }

    pub fn is_idle(&self) -> bool {
        self.state == State::Idle
    }

    /// When [`Gesture::tick`] is next due, if a press is being withheld.
    pub fn deadline(&self) -> Option<u64> {
        match self.state {
            State::Pressed { deadline, .. } => Some(deadline),
            _ => None,
        }
    }

    /// Right button pressed. `hold_ms` and `tolerance` apply to this press only,
    /// so configuration changes take effect on the next press.
    pub fn down(&mut self, pos: Pos, now: u64, hold_ms: u64, tolerance: i32) -> Outcome {
        match self.state {
            State::Idle => {
                self.state = State::Pressed { down: pos, deadline: now + hold_ms, tolerance };
                Outcome::SWALLOW
            }
            // A second press without a release cannot come from real hardware.
            // Keep withholding so the application never sees half a click.
            State::Pressed { .. } | State::Held => Outcome::SWALLOW,
            State::Passing => Outcome::PASS,
        }
    }

    /// Right button released.
    pub fn up(&mut self, pos: Pos, now: u64) -> Outcome {
        match self.state {
            State::Idle => Outcome::PASS,
            State::Pressed { down, deadline, .. } => {
                self.state = State::Idle;
                if now < deadline {
                    Outcome::swallow(Effect::Click { down, up: pos })
                } else {
                    // The hold elapsed but the tick has not been delivered yet.
                    Outcome::swallow(Effect::Trigger { pos, held: false })
                }
            }
            State::Held => {
                self.state = State::Idle;
                Outcome::swallow(Effect::Release { pos })
            }
            State::Passing => {
                self.state = State::Idle;
                Outcome::PASS
            }
        }
    }

    /// Cursor moved. Never swallowed: withholding motion freezes the cursor.
    pub fn motion(&mut self, pos: Pos) -> Outcome {
        match self.state {
            State::Pressed { down, tolerance, .. } if pos.distance(down) > tolerance => {
                self.state = State::Passing;
                Outcome::pass(Effect::Flush { at: down, now: pos })
            }
            _ => Outcome::PASS,
        }
    }

    /// Any other button or the wheel was used. During a withheld press this is
    /// a chord (rocker or wheel gesture): the event is swallowed so the caller
    /// can deliver the press first and then repeat the event in order.
    pub fn other(&mut self, pos: Pos) -> Outcome {
        match self.state {
            State::Pressed { down, .. } => {
                self.state = State::Passing;
                Outcome::swallow(Effect::Flush { at: down, now: pos })
            }
            _ => Outcome::PASS,
        }
    }

    /// Timer fired. Safe to call early or in any state.
    pub fn tick(&mut self, pos: Pos, now: u64) -> Outcome {
        match self.state {
            State::Pressed { deadline, .. } if now >= deadline => {
                self.state = State::Held;
                Outcome::pass(Effect::Trigger { pos, held: true })
            }
            _ => Outcome::PASS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOLD: u64 = 350;
    const TOL: i32 = 6;

    fn p(x: i32, y: i32) -> Pos {
        Pos::new(x, y)
    }

    fn pressed(at: Pos) -> Gesture {
        let mut g = Gesture::new();
        assert_eq!(g.down(at, 0, HOLD, TOL), Outcome::SWALLOW);
        g
    }

    #[test]
    fn quick_click_is_delivered_on_release() {
        let mut g = pressed(p(100, 100));
        assert_eq!(g.deadline(), Some(HOLD));
        let o = g.up(p(102, 101), 120);
        assert_eq!(o, Outcome::swallow(Effect::Click { down: p(100, 100), up: p(102, 101) }));
        assert!(g.is_idle());
        assert_eq!(g.deadline(), None);
    }

    #[test]
    fn hold_triggers_then_release_is_reported_and_swallowed() {
        let mut g = pressed(p(50, 50));
        assert_eq!(
            g.tick(p(50, 50), HOLD),
            Outcome::pass(Effect::Trigger { pos: p(50, 50), held: true })
        );
        assert_eq!(g.deadline(), None);
        assert_eq!(g.motion(p(90, 70)), Outcome::PASS);
        assert_eq!(g.up(p(90, 70), 600), Outcome::swallow(Effect::Release { pos: p(90, 70) }));
        assert!(g.is_idle());
    }

    #[test]
    fn early_tick_is_ignored() {
        let mut g = pressed(p(0, 0));
        assert_eq!(g.tick(p(0, 0), HOLD - 1), Outcome::PASS);
        assert_eq!(g.deadline(), Some(HOLD));
        assert_eq!(g.tick(p(0, 0), HOLD).effect, Effect::Trigger { pos: p(0, 0), held: true });
    }

    #[test]
    fn release_just_before_the_deadline_is_a_click() {
        let mut g = pressed(p(10, 10));
        assert_eq!(
            g.up(p(10, 10), HOLD - 1).effect,
            Effect::Click { down: p(10, 10), up: p(10, 10) }
        );
    }

    #[test]
    fn release_at_the_deadline_before_the_tick_still_triggers() {
        let mut g = pressed(p(10, 10));
        assert_eq!(
            g.up(p(12, 10), HOLD),
            Outcome::swallow(Effect::Trigger { pos: p(12, 10), held: false })
        );
        assert!(g.is_idle());
        // The late tick finds nothing to do.
        assert_eq!(g.tick(p(12, 10), HOLD + 5), Outcome::PASS);
    }

    #[test]
    fn motion_inside_the_tolerance_keeps_the_hold_alive() {
        let mut g = pressed(p(100, 100));
        assert_eq!(g.motion(p(106, 94)), Outcome::PASS);
        assert_eq!(
            g.tick(p(106, 94), HOLD).effect,
            Effect::Trigger { pos: p(106, 94), held: true }
        );
    }

    #[test]
    fn motion_past_the_tolerance_becomes_a_drag() {
        let mut g = pressed(p(100, 100));
        assert_eq!(
            g.motion(p(107, 100)),
            Outcome::pass(Effect::Flush { at: p(100, 100), now: p(107, 100) })
        );
        assert_eq!(g.deadline(), None);
        // Everything after is the application's business.
        assert_eq!(g.motion(p(160, 110)), Outcome::PASS);
        assert_eq!(g.tick(p(160, 110), HOLD), Outcome::PASS);
        assert_eq!(g.up(p(160, 110), 500), Outcome::PASS);
        assert!(g.is_idle());
    }

    #[test]
    fn wheel_or_other_button_during_a_press_is_a_chord() {
        let mut g = pressed(p(20, 20));
        // Swallowed so the caller can replay it after the withheld press.
        assert_eq!(
            g.other(p(21, 20)),
            Outcome::swallow(Effect::Flush { at: p(20, 20), now: p(21, 20) })
        );
        assert_eq!(g.other(p(21, 20)), Outcome::PASS);
        assert_eq!(g.up(p(21, 20), 200), Outcome::PASS);
    }

    #[test]
    fn other_input_is_untouched_outside_a_press() {
        let mut g = Gesture::new();
        assert_eq!(g.other(p(0, 0)), Outcome::PASS);
        assert_eq!(g.motion(p(5, 5)), Outcome::PASS);
        assert_eq!(g.up(p(5, 5), 10), Outcome::PASS);
        assert_eq!(g.tick(p(5, 5), 10_000), Outcome::PASS);

        let mut g = pressed(p(0, 0));
        g.tick(p(0, 0), HOLD);
        // While the panel is open the wheel and other buttons work normally.
        assert_eq!(g.other(p(0, 0)), Outcome::PASS);
    }

    #[test]
    fn repeated_press_without_release_stays_withheld() {
        let mut g = pressed(p(0, 0));
        assert_eq!(g.down(p(0, 0), 10, HOLD, TOL), Outcome::SWALLOW);
        assert_eq!(g.deadline(), Some(HOLD));
        g.tick(p(0, 0), HOLD);
        assert_eq!(g.down(p(0, 0), HOLD + 10, HOLD, TOL), Outcome::SWALLOW);
    }

    #[test]
    fn each_press_uses_its_own_settings() {
        let mut g = Gesture::new();
        g.down(p(0, 0), 1000, 200, 2);
        assert_eq!(g.deadline(), Some(1200));
        assert_eq!(g.motion(p(3, 0)).effect, Effect::Flush { at: p(0, 0), now: p(3, 0) });
        g.up(p(3, 0), 1100);

        g.down(p(0, 0), 2000, 500, 10);
        assert_eq!(g.deadline(), Some(2500));
        assert_eq!(g.motion(p(10, 10)), Outcome::PASS);
    }
}
