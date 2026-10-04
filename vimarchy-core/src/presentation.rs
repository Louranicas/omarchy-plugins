//! Bounded presentation calculations; no rendering, filesystem or input grabs.
use crate::gestures::WorkspaceTarget;
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    pub scale: f64,
    pub window_opacity: f64,
    pub badge_opacity: f64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    NonFinite,
    InvalidOutput,
    InvalidKey,
    DeadlineOverflow,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            scale: 1.0,
            window_opacity: 0.07,
            badge_opacity: 0.21,
        }
    }
}
impl Settings {
    pub fn normalized(scale: f64, window_opacity: f64, badge_opacity: f64) -> Result<Self, Error> {
        if ![scale, window_opacity, badge_opacity]
            .iter()
            .all(|x| x.is_finite())
        {
            return Err(Error::NonFinite);
        }
        Ok(Self {
            scale: scale.clamp(0.75, 1.5),
            window_opacity: window_opacity.clamp(0., 0.3),
            badge_opacity: badge_opacity.clamp(0., 0.3),
        })
    }
    pub fn adjust_scale(&mut self, increase: bool) {
        self.scale = ((self.scale + if increase { 0.1 } else { -0.1 }) * 100.)
            .round()
            .clamp(75., 150.)
            / 100.;
    }
    pub fn preview_opacity(&mut self, badge: bool, value: f64) -> Result<(), Error> {
        if !value.is_finite() {
            return Err(Error::NonFinite);
        }
        let next = ((value * 100.).round() / 100.).clamp(0., 0.3);
        if badge {
            self.badge_opacity = next
        } else {
            self.window_opacity = next
        };
        Ok(())
    }
    pub fn write_deadline(now: u64) -> Result<u64, Error> {
        now.checked_add(150).ok_or(Error::DeadlineOverflow)
    }
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Output {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}
/// Clamp a rectangular radial/card extent inside logical output bounds. If the
/// content exceeds the output, pin its origin to the output (renderer clips).
pub fn clamp_origin(
    desired: [f64; 2],
    extent: [f64; 2],
    output: Output,
) -> Result<[f64; 2], Error> {
    if !desired
        .iter()
        .chain(&extent)
        .chain([output.x, output.y, output.width, output.height].iter())
        .all(|x| x.is_finite())
        || extent.iter().any(|x| *x < 0.)
        || output.width <= 0.
        || output.height <= 0.
    {
        return Err(Error::InvalidOutput);
    }
    let upper = [
        output.x + (output.width - extent[0]).max(0.),
        output.y + (output.height - extent[1]).max(0.),
    ];
    if !upper.iter().all(|x| x.is_finite()) {
        return Err(Error::InvalidOutput);
    }
    Ok([
        desired[0].clamp(output.x, upper[0]),
        desired[1].clamp(output.y, upper[1]),
    ])
}
pub fn radial_destinations(current: WorkspaceTarget) -> Vec<WorkspaceTarget> {
    (1..=10)
        .map(WorkspaceTarget::Number)
        .chain([WorkspaceTarget::Scratchpad])
        .filter(|w| *w != current)
        .collect()
}
/// Prefix decoding belongs before gesture Down/Up. Only complete hints become
/// reducer inputs. Unknown keys pass through; repeated prefix keys are not taps.
#[derive(Default, Debug)]
pub struct HintDecoder {
    prefix: Option<char>,
}
#[derive(Debug, PartialEq, Eq)]
pub enum KeyResult {
    PassThrough,
    Prefix,
    Hint(String),
}
impl HintDecoder {
    pub fn reset(&mut self) {
        self.prefix = None;
    }
    pub fn key(&mut self, key: char, two_letters: bool, repeat: bool) -> KeyResult {
        if !crate::allocation::ALPHABET.contains(key) {
            return KeyResult::PassThrough;
        }
        if repeat {
            return KeyResult::Prefix;
        }
        if !two_letters {
            self.prefix = None;
            return KeyResult::Hint(key.to_string());
        }
        if let Some(first) = self.prefix.take() {
            KeyResult::Hint(format!("{first}{key}"))
        } else {
            self.prefix = Some(key);
            KeyResult::Prefix
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clamped_settings_and_debounce() {
        let mut s = Settings::default();
        for _ in 0..20 {
            s.adjust_scale(true)
        }
        assert_eq!(s.scale, 1.5);
        for _ in 0..20 {
            s.adjust_scale(false)
        }
        assert_eq!(s.scale, 0.75);
        s.preview_opacity(false, 0.176).unwrap();
        assert_eq!(s.window_opacity, 0.18);
        assert_eq!(
            Settings::normalized(f64::NAN, 0., 0.),
            Err(Error::NonFinite)
        );
        assert_eq!(
            Settings::write_deadline(u64::MAX),
            Err(Error::DeadlineOverflow)
        );
        assert_eq!(Settings::write_deadline(100), Ok(250));
    }
    #[test]
    fn radial_empty_workspaces_and_scratchpad() {
        let rows = radial_destinations(WorkspaceTarget::Number(1));
        assert_eq!(rows.len(), 10);
        assert!(rows.contains(&WorkspaceTarget::Number(10)));
        assert!(rows.contains(&WorkspaceTarget::Scratchpad));
        assert!(!rows.contains(&WorkspaceTarget::Number(1)));
    }
    #[test]
    fn screen_clamp_negative_output_and_oversized_content() {
        let o = Output {
            x: -1280.,
            y: 0.,
            width: 1280.,
            height: 720.,
        };
        assert_eq!(
            clamp_origin([-2000., 900.], [200., 200.], o),
            Ok([-1280., 520.])
        );
        assert_eq!(
            clamp_origin([100., 100.], [2000., 1000.], o),
            Ok([-1280., 0.])
        );
    }
    #[test]
    fn two_letter_prefix_unknown_passes_and_repeated_key_is_not_second_tap() {
        let mut d = HintDecoder::default();
        assert_eq!(d.key('a', true, false), KeyResult::Prefix);
        assert_eq!(d.key('a', true, true), KeyResult::Prefix);
        assert_eq!(d.key('?', true, false), KeyResult::PassThrough);
        assert_eq!(d.key('s', true, false), KeyResult::Hint("as".into()));
        d.key('a', true, false);
        d.reset();
        assert_eq!(d.key('s', false, false), KeyResult::Hint("s".into()));
    }
}
