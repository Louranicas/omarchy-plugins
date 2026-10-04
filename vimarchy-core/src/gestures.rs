//! Deterministic intent reducer. The caller authenticates/fences every event and
//! effect. Times are monotonic milliseconds; there are no threads or OS effects.
use crate::allocation::{ALPHABET, CAPACITY, MAX_ID_BYTES};
use std::collections::BTreeMap;

pub const HOLD_MS: u64 = 350;
pub const HOLD_GRACE_MS: u64 = 100;
pub const RADIAL_SPREAD_MS: u64 = 220;
pub const SELECTION_FADE_MS: u64 = 500;
pub const SELECTION_CLOSE_MS: u64 = 520;
pub const MAP_TIMEOUT_MS: u64 = 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Generation(pub u64);
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub stable_id: String,
    pub hint: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceTarget {
    Number(u8),
    Scratchpad,
}
impl WorkspaceTarget {
    pub fn from_key(key: char) -> Option<Self> {
        match key {
            '0' => Some(Self::Number(10)),
            '1'..='9' => Some(Self::Number(key as u8 - b'0')),
            's' => Some(Self::Scratchpad),
            _ => None,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Closed,
    Mapping,
    Selecting,
    Moving(Target),
    Radial(Target),
    Exiting,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    Show,
    Hide,
    EnterSelection {
        two_letters: bool,
    },
    ResetOwnedSubmap,
    BeginHold(Target),
    CancelHold(Target),
    Focus(Target),
    DoubleTap {
        target: Target,
        alt: bool,
    },
    SelectSource(Target),
    Swap {
        source: Target,
        target: Target,
    },
    HoldTarget {
        operation_id: u64,
        source: Target,
        target: Target,
    },
    ShowRadial(Target),
    MoveWorkspace {
        source: Target,
        destination: WorkspaceTarget,
        follow: bool,
    },
    BeginSelection(Target),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Mapped,
    /// `press_id` is strictly increasing per reducer generation. OS key-repeat
    /// must be identified by the adapter and submitted with repeat=true.
    Down {
        hint: String,
        press_id: u64,
        alt: bool,
        repeat: bool,
    },
    Up {
        press_id: u64,
    },
    Workspace {
        key: char,
        alt: bool,
    },
    /// Completion of a held-target effect. Disabled actions retain source mode.
    HoldCompleted {
        operation_id: u64,
        completed: bool,
    },
    Cancel,
    Tick,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidGeneration,
    InvalidHints,
    InvalidTimeout,
    StaleGeneration,
    ClockRegression,
    DeadlineOverflow,
    DuplicatePress,
    InvalidState,
    InvalidKey,
}
#[derive(Clone, Debug)]
struct Press {
    target: Target,
    id: u64,
    alt: bool,
    deadline: u64,
}
#[derive(Clone, Debug)]
struct Candidate {
    target: Target,
    deadline: u64,
}
#[derive(Debug)]
pub struct Reducer {
    generation: Generation,
    phase: Phase,
    hints: BTreeMap<String, String>,
    last_now: u64,
    timeout: u64,
    map_deadline: u64,
    exit_deadline: Option<u64>,
    press: Option<Press>,
    candidate: Option<Candidate>,
    last_press: u64,
    hold_pending: Option<u64>,
}
impl Reducer {
    pub fn open(
        generation: Generation,
        hints: BTreeMap<String, String>,
        now: u64,
        timeout_ms: u64,
    ) -> Result<(Self, Vec<Effect>), Error> {
        if generation.0 == 0 {
            return Err(Error::InvalidGeneration);
        }
        if !(120..=600).contains(&timeout_ms) {
            return Err(Error::InvalidTimeout);
        }
        let width = if hints.len() <= 26 { 1 } else { 2 };
        if hints.len() > CAPACITY
            || hints.iter().any(|(h, id)| {
                h.len() != width
                    || !h.chars().all(|c| ALPHABET.contains(c))
                    || id.is_empty()
                    || id.len() > MAX_ID_BYTES
                    || id.contains('\0')
            })
        {
            return Err(Error::InvalidHints);
        }
        let map_deadline = now
            .checked_add(MAP_TIMEOUT_MS)
            .ok_or(Error::DeadlineOverflow)?;
        let empty = hints.is_empty();
        Ok((
            Self {
                generation,
                phase: if empty { Phase::Closed } else { Phase::Mapping },
                hints,
                last_now: now,
                timeout: timeout_ms,
                map_deadline,
                exit_deadline: None,
                press: None,
                candidate: None,
                last_press: 0,
                hold_pending: None,
            },
            if empty { vec![] } else { vec![Effect::Show] },
        ))
    }
    pub fn phase(&self) -> &Phase {
        &self.phase
    }
    pub fn generation(&self) -> Generation {
        self.generation
    }
    fn close(&mut self, out: &mut Vec<Effect>) {
        if self.phase != Phase::Closed {
            out.extend([Effect::ResetOwnedSubmap, Effect::Hide]);
        }
        self.phase = Phase::Closed;
        self.press = None;
        self.candidate = None;
        self.exit_deadline = None;
        self.hold_pending = None;
    }
    fn tick(&mut self, now: u64, out: &mut Vec<Effect>) {
        if self.phase == Phase::Mapping && now >= self.map_deadline {
            self.close(out);
            return;
        }
        if self.exit_deadline.is_some_and(|d| now >= d) {
            self.close(out);
            return;
        }
        // Inclusive double-tap boundary mirrors legacy elapsed <= timeout.
        if self.candidate.as_ref().is_some_and(|c| now > c.deadline) {
            self.candidate = None;
        }
        if self.press.as_ref().is_some_and(|p| now >= p.deadline) {
            let p = self.press.take().expect("checked press");
            self.candidate = None;
            match &self.phase {
                Phase::Selecting => {
                    if p.alt {
                        self.phase = Phase::Radial(p.target.clone());
                        out.push(Effect::ShowRadial(p.target));
                    } else {
                        self.phase = Phase::Moving(p.target.clone());
                        out.push(Effect::SelectSource(p.target));
                    }
                }
                Phase::Moving(source) if *source != p.target => {
                    self.hold_pending = Some(p.id);
                    out.push(Effect::HoldTarget {
                        operation_id: p.id,
                        source: source.clone(),
                        target: p.target,
                    });
                }
                _ => out.push(Effect::CancelHold(p.target)),
            }
        }
    }
    /// Successful calls return effects in order. A business-invalid event is
    /// rejected before advancing time so no timeout effect can be lost in Err.
    pub fn apply(
        &mut self,
        generation: Generation,
        now: u64,
        event: Event,
    ) -> Result<Vec<Effect>, Error> {
        if generation != self.generation {
            return Err(Error::StaleGeneration);
        }
        if now < self.last_now {
            return Err(Error::ClockRegression);
        }
        if let Event::Down {
            hint,
            press_id,
            repeat: false,
            ..
        } = &event
        {
            if !self.hints.contains_key(hint) {
                return Err(Error::InvalidKey);
            }
            if *press_id == 0 || *press_id <= self.last_press {
                return Err(Error::DuplicatePress);
            }
            now.checked_add(HOLD_MS).ok_or(Error::DeadlineOverflow)?;
            now.checked_add(self.timeout)
                .ok_or(Error::DeadlineOverflow)?;
        }
        if matches!(event, Event::Up { .. }) {
            now.checked_add(SELECTION_CLOSE_MS)
                .ok_or(Error::DeadlineOverflow)?;
        }
        if let Event::Workspace { key, .. } = &event
            && WorkspaceTarget::from_key(*key).is_none()
        {
            return Err(Error::InvalidKey);
        }
        self.last_now = now;
        let mut out = vec![];
        if event == Event::Cancel {
            self.close(&mut out);
            return Ok(out);
        }
        self.tick(now, &mut out);
        match event {
            Event::Mapped if self.phase == Phase::Mapping => {
                self.phase = Phase::Selecting;
                out.push(Effect::EnterSelection {
                    two_letters: self.hints.len() > 26,
                });
            }
            Event::Down { repeat: true, .. } => {}
            Event::Down {
                hint,
                press_id,
                alt,
                ..
            } if matches!(
                self.phase,
                Phase::Selecting | Phase::Exiting | Phase::Moving(_)
            ) && self.hold_pending.is_none() =>
            {
                self.last_press = press_id;
                let target = Target {
                    stable_id: self.hints[&hint].clone(),
                    hint,
                };
                if matches!(self.phase, Phase::Selecting | Phase::Exiting)
                    && self
                        .candidate
                        .as_ref()
                        .is_some_and(|c| c.target == target && now <= c.deadline)
                {
                    self.close(&mut out);
                    out.push(Effect::DoubleTap { target, alt });
                } else if self.phase != Phase::Exiting {
                    self.press = Some(Press {
                        target: target.clone(),
                        id: press_id,
                        alt,
                        deadline: now + HOLD_MS,
                    });
                    if self.phase == Phase::Selecting {
                        self.candidate = Some(Candidate {
                            target: target.clone(),
                            deadline: now + self.timeout,
                        });
                    }
                    out.push(Effect::BeginHold(target));
                }
            }
            Event::Up { press_id } if self.press.as_ref().is_some_and(|p| p.id == press_id) => {
                let p = self.press.take().expect("checked press");
                out.push(Effect::CancelHold(p.target.clone()));
                match &self.phase {
                    Phase::Selecting => {
                        out.push(Effect::Focus(p.target.clone()));
                        out.push(Effect::BeginSelection(p.target));
                        self.phase = Phase::Exiting;
                        self.exit_deadline = Some(now + SELECTION_CLOSE_MS);
                    }
                    Phase::Moving(source) if *source != p.target => {
                        out.push(Effect::Swap {
                            source: source.clone(),
                            target: p.target,
                        });
                        self.close(&mut out);
                    }
                    _ => {}
                }
            }
            Event::Workspace { key, alt } => {
                if let Phase::Radial(source) = &self.phase {
                    out.push(Effect::MoveWorkspace {
                        source: source.clone(),
                        destination: WorkspaceTarget::from_key(key).expect("validated"),
                        follow: !alt,
                    });
                    self.close(&mut out);
                }
            }
            Event::HoldCompleted {
                operation_id,
                completed,
            } if self.hold_pending == Some(operation_id) => {
                self.hold_pending = None;
                if completed {
                    self.close(&mut out);
                }
            }
            _ => {}
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ready() -> Reducer {
        let (mut r, _) = Reducer::open(
            Generation(1),
            BTreeMap::from([("a".into(), "1".into()), ("s".into(), "2".into())]),
            0,
            280,
        )
        .unwrap();
        r.apply(Generation(1), 1, Event::Mapped).unwrap();
        r
    }
    fn down(r: &mut Reducer, t: u64, h: &str, id: u64, alt: bool) -> Vec<Effect> {
        r.apply(
            Generation(1),
            t,
            Event::Down {
                hint: h.into(),
                press_id: id,
                alt,
                repeat: false,
            },
        )
        .unwrap()
    }
    #[test]
    fn map_timeout_never_enters_submap() {
        let (mut r, _) = Reducer::open(
            Generation(1),
            BTreeMap::from([("a".into(), "1".into())]),
            0,
            280,
        )
        .unwrap();
        assert_eq!(
            r.apply(Generation(1), 1000, Event::Mapped).unwrap(),
            vec![Effect::ResetOwnedSubmap, Effect::Hide]
        );
        assert_eq!(*r.phase(), Phase::Closed);
    }
    #[test]
    fn tap_then_double_uses_second_modifier() {
        let mut r = ready();
        down(&mut r, 10, "a", 1, false);
        let e = r
            .apply(Generation(1), 30, Event::Up { press_id: 1 })
            .unwrap();
        assert!(e.iter().any(|e| matches!(e, Effect::Focus(_))));
        let e = down(&mut r, 290, "a", 2, true);
        assert!(
            e.iter()
                .any(|e| matches!(e, Effect::DoubleTap { alt: true, .. }))
        );
        assert!(
            r.apply(Generation(1), 291, Event::Up { press_id: 1 })
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn fast_second_down_before_old_release() {
        let mut r = ready();
        down(&mut r, 10, "a", 1, false);
        assert!(
            down(&mut r, 20, "a", 2, false)
                .iter()
                .any(|e| matches!(e, Effect::DoubleTap { .. }))
        );
        assert!(
            r.apply(Generation(1), 21, Event::Up { press_id: 1 })
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn hold_and_late_release_never_focus() {
        let mut r = ready();
        down(&mut r, 10, "a", 1, false);
        let e = r
            .apply(Generation(1), 360, Event::Up { press_id: 1 })
            .unwrap();
        assert!(matches!(e.as_slice(), [Effect::SelectSource(_)]));
        assert!(matches!(r.phase(), Phase::Moving(_)));
    }
    #[test]
    fn held_target_disabled_preserves_source_and_consumes_release() {
        let mut r = ready();
        down(&mut r, 10, "a", 1, false);
        r.apply(Generation(1), 360, Event::Tick).unwrap();
        down(&mut r, 400, "s", 2, false);
        assert!(
            r.apply(Generation(1), 750, Event::Tick)
                .unwrap()
                .iter()
                .any(|e| matches!(e, Effect::HoldTarget { .. }))
        );
        r.apply(
            Generation(1),
            751,
            Event::HoldCompleted {
                operation_id: 2,
                completed: false,
            },
        )
        .unwrap();
        assert!(
            r.apply(Generation(1), 752, Event::Up { press_id: 2 })
                .unwrap()
                .is_empty()
        );
        assert!(matches!(r.phase(), Phase::Moving(_)));
    }
    #[test]
    fn source_then_target_tap_swaps() {
        let mut r = ready();
        down(&mut r, 10, "a", 1, false);
        r.apply(Generation(1), 360, Event::Tick).unwrap();
        down(&mut r, 400, "s", 2, false);
        assert!(
            r.apply(Generation(1), 401, Event::Up { press_id: 2 })
                .unwrap()
                .iter()
                .any(|e| matches!(e, Effect::Swap { .. }))
        );
    }
    #[test]
    fn alt_hold_workspace_alt_destination_does_not_follow() {
        let mut r = ready();
        down(&mut r, 10, "a", 1, true);
        r.apply(Generation(1), 360, Event::Tick).unwrap();
        assert!(
            r.apply(Generation(1), 370, Event::Up { press_id: 1 })
                .unwrap()
                .is_empty()
        );
        assert!(
            r.apply(
                Generation(1),
                380,
                Event::Workspace {
                    key: '0',
                    alt: true
                }
            )
            .unwrap()
            .iter()
            .any(|e| matches!(
                e,
                Effect::MoveWorkspace {
                    destination: WorkspaceTarget::Number(10),
                    follow: false,
                    ..
                }
            ))
        );
        assert_eq!(
            WorkspaceTarget::from_key('s'),
            Some(WorkspaceTarget::Scratchpad)
        );
    }
    #[test]
    fn duplicate_replay_stale_and_clock_rejected() {
        let mut r = ready();
        down(&mut r, 10, "a", 1, false);
        assert_eq!(
            r.apply(Generation(2), 20, Event::Cancel),
            Err(Error::StaleGeneration)
        );
        assert_eq!(
            r.apply(Generation(1), 9, Event::Tick),
            Err(Error::ClockRegression)
        );
        assert_eq!(
            r.apply(
                Generation(1),
                20,
                Event::Down {
                    hint: "a".into(),
                    press_id: 1,
                    alt: false,
                    repeat: false
                }
            ),
            Err(Error::DuplicatePress)
        );
    }
    #[test]
    fn invalid_event_does_not_swallow_expiry() {
        let mut r = ready();
        down(&mut r, 10, "a", 1, false);
        assert_eq!(
            r.apply(
                Generation(1),
                360,
                Event::Workspace {
                    key: 'x',
                    alt: false
                }
            ),
            Err(Error::InvalidKey)
        );
        assert!(matches!(
            r.apply(Generation(1), 360, Event::Tick).unwrap().as_slice(),
            [Effect::SelectSource(_)]
        ));
    }
    #[test]
    fn repeat_does_not_become_double_and_cancel_overrides_timer() {
        let mut r = ready();
        down(&mut r, 10, "a", 1, false);
        assert!(
            r.apply(
                Generation(1),
                20,
                Event::Down {
                    hint: "a".into(),
                    press_id: 1,
                    alt: false,
                    repeat: true
                }
            )
            .unwrap()
            .is_empty()
        );
        assert_eq!(
            r.apply(Generation(1), 360, Event::Cancel).unwrap(),
            vec![Effect::ResetOwnedSubmap, Effect::Hide]
        );
    }
    #[test]
    fn fade_close_deadline_and_double_expiry() {
        let mut r = ready();
        down(&mut r, 10, "a", 1, false);
        r.apply(Generation(1), 20, Event::Up { press_id: 1 })
            .unwrap();
        assert!(
            !down(&mut r, 291, "a", 2, false)
                .iter()
                .any(|e| matches!(e, Effect::DoubleTap { .. }))
        );
        assert_eq!(
            r.apply(Generation(1), 540, Event::Tick).unwrap(),
            vec![Effect::ResetOwnedSubmap, Effect::Hide]
        );
    }
    #[test]
    fn overflow_fails_without_mutation() {
        let mut r = ready();
        assert_eq!(
            r.apply(
                Generation(1),
                u64::MAX,
                Event::Down {
                    hint: "a".into(),
                    press_id: 1,
                    alt: false,
                    repeat: false
                }
            ),
            Err(Error::DeadlineOverflow)
        );
        assert_eq!(r.last_now, 1);
    }
    #[test]
    fn completion_is_bound_to_generation_and_operation() {
        let mut r = ready();
        down(&mut r, 10, "a", 1, false);
        r.apply(Generation(1), 360, Event::Tick).unwrap();
        down(&mut r, 400, "s", 2, false);
        r.apply(Generation(1), 750, Event::Tick).unwrap();
        assert!(
            r.apply(
                Generation(1),
                751,
                Event::HoldCompleted {
                    operation_id: 3,
                    completed: true
                }
            )
            .unwrap()
            .is_empty()
        );
        assert_eq!(
            r.apply(
                Generation(2),
                751,
                Event::HoldCompleted {
                    operation_id: 2,
                    completed: true
                }
            ),
            Err(Error::StaleGeneration)
        );
        r.apply(
            Generation(1),
            752,
            Event::HoldCompleted {
                operation_id: 2,
                completed: false,
            },
        )
        .unwrap();
        down(&mut r, 800, "s", 3, false);
        r.apply(Generation(1), 1150, Event::Tick).unwrap();
        assert!(
            r.apply(
                Generation(1),
                1151,
                Event::HoldCompleted {
                    operation_id: 2,
                    completed: true
                }
            )
            .unwrap()
            .is_empty()
        );
        assert!(matches!(r.phase(), Phase::Moving(_)));
        assert_eq!(
            r.apply(
                Generation(1),
                1152,
                Event::HoldCompleted {
                    operation_id: 3,
                    completed: true
                }
            )
            .unwrap(),
            vec![Effect::ResetOwnedSubmap, Effect::Hide]
        );
        assert!(
            r.apply(
                Generation(1),
                1153,
                Event::HoldCompleted {
                    operation_id: 3,
                    completed: true
                }
            )
            .unwrap()
            .is_empty()
        );
    }
}
