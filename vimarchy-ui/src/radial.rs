//! Read-only radial geometry and GTK badges. Never emits effects or input.
use gtk::prelude::*;
use std::time::{Duration, Instant};
pub const SPREAD: Duration = Duration::from_millis(220);
pub fn destinations(current: i64) -> Vec<char> {
    (1..=10)
        .filter(|n| *n != current)
        .map(|n| {
            if n == 10 {
                '0'
            } else {
                char::from(b'0' + n as u8)
            }
        })
        .collect()
}
pub fn positions(
    center: [f64; 2],
    bounds: [f64; 2],
    count: usize,
    elapsed: Duration,
    reduced: bool,
) -> Option<Vec<[f64; 2]>> {
    if !(1..=10).contains(&count)
        || !center
            .into_iter()
            .chain(bounds)
            .all(|v| v.is_finite() && v.abs() <= 2_000_000.)
        || bounds.iter().any(|v| *v < 56.)
    {
        return None;
    }
    let t = if reduced {
        1.
    } else {
        (elapsed.as_secs_f64() / SPREAD.as_secs_f64()).min(1.)
    };
    let spread = 1. - (1. - t).powi(3);
    Some(
        (0..count)
            .map(|i| {
                let angle =
                    -std::f64::consts::FRAC_PI_2 + i as f64 * std::f64::consts::TAU / count as f64;
                [
                    (center[0] + angle.cos() * 126. * spread - 24.).clamp(4., bounds[0] - 52.),
                    (center[1] + angle.sin() * 126. * spread - 24.).clamp(4., bounds[1] - 52.),
                ]
            })
            .collect(),
    )
}
pub struct View {
    pub source: String,
    pub current: i64,
    center: [f64; 2],
    started: Instant,
    badges: Vec<(char, gtk::Label)>,
}
impl View {
    pub fn new(fixed: &gtk::Fixed, source: String, current: i64, center: [f64; 2]) -> Self {
        let badges = destinations(current)
            .into_iter()
            .map(|key| {
                let badge = gtk::Label::new(Some(&key.to_string()));
                badge.set_size_request(48, 48);
                badge.add_css_class("workspace-badge");
                badge.set_can_target(false);
                badge.update_property(&[gtk::accessible::Property::Label(&format!(
                    "Workspace {}",
                    if key == '0' { 10 } else { key as u8 - b'0' }
                ))]);
                fixed.put(&badge, center[0] - 24., center[1] - 24.);
                (key, badge)
            })
            .collect();
        Self {
            source,
            current,
            center,
            started: Instant::now(),
            badges,
        }
    }
    pub fn render(
        &self,
        fixed: &gtk::Fixed,
        bounds: [f64; 2],
        selected: Option<char>,
        reduced: bool,
    ) -> bool {
        let Some(points) = positions(
            self.center,
            bounds,
            self.badges.len(),
            self.started.elapsed(),
            reduced,
        ) else {
            return false;
        };
        let opacity = if reduced {
            1.
        } else {
            (self.started.elapsed().as_secs_f64() / SPREAD.as_secs_f64()).min(1.)
        };
        for ((key, badge), point) in self.badges.iter().zip(points) {
            fixed.move_(badge, point[0], point[1]);
            badge.set_opacity(opacity);
            if selected == Some(*key) {
                badge.add_css_class("chosen");
            } else {
                badge.remove_css_class("chosen");
            }
        }
        true
    }
    pub fn clear(self, fixed: &gtk::Fixed) {
        for (_, badge) in self.badges {
            fixed.remove(&badge);
        }
    }
}
/// Actual process-local GTK settings/layout/class oracle; no authority or input.
pub fn verify() -> gtk::glib::ExitCode {
    use std::{cell::Cell, rc::Rc};
    let passed = Rc::new(Cell::new(false));
    let result = passed.clone();
    let app = gtk::Application::builder()
        .application_id("org.omarchy.VimarchyRadialOracle")
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(move |app| {
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .default_width(800)
            .default_height(600)
            .build();
        let fixed = gtk::Fixed::new();
        window.set_child(Some(&fixed));
        window.present();
        let result = result.clone();
        let app = app.clone();
        gtk::glib::timeout_add_local_once(Duration::from_millis(100), move || {
            let display = gtk::prelude::WidgetExt::display(&window);
            let settings = gtk::Settings::for_display(&display);
            let old = settings.is_gtk_enable_animations();
            settings.set_gtk_enable_animations(false);
            let view = View::new(&fixed, "a".into(), 3, [10., 10.]);
            let rendered = view.render(
                &fixed,
                [800., 600.],
                Some('0'),
                !settings.is_gtk_enable_animations(),
            );
            let expected = positions([10., 10.], [800., 600.], 9, Duration::ZERO, true).unwrap();
            gtk::glib::timeout_add_local_once(Duration::from_millis(60), move || {
                let valid = rendered
                    && view.badges.len() == 9
                    && !view.badges.iter().any(|(key, _)| *key == '3')
                    && view
                        .badges
                        .iter()
                        .zip(expected)
                        .all(|((key, label), point)| {
                            let (x, y) = fixed.child_position(label);
                            (x - point[0]).abs() < 0.01
                                && (y - point[1]).abs() < 0.01
                                && label.opacity() == 1.
                                && label.has_css_class("chosen") == (*key == '0')
                                && !label.can_target()
                        });
                settings.set_gtk_enable_animations(old);
                result.set(valid);
                println!("gtk_radial_layout_reduced_motion_selection_observed={valid}");
                view.clear(&fixed);
                window.destroy();
                app.quit();
            });
        });
    });
    app.run_with_args::<&str>(&[]);
    if passed.get() {
        gtk::glib::ExitCode::SUCCESS
    } else {
        gtk::glib::ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keys_omit_current_and_ten_is_zero_without_scratchpad() {
        assert_eq!(
            destinations(1),
            vec!['2', '3', '4', '5', '6', '7', '8', '9', '0']
        );
        assert!(!destinations(10).contains(&'0'));
        assert!(!destinations(-99).contains(&'s'));
    }
    #[test]
    fn source_center_orbit_clamp_and_actual_reduced_motion() {
        let start = positions([400., 300.], [800., 600.], 9, Duration::ZERO, false).unwrap();
        assert!(start.iter().all(|p| *p == [376., 276.]));
        let end = positions([400., 300.], [800., 600.], 9, SPREAD, false).unwrap();
        assert_eq!(end[0], [376., 150.]);
        assert_eq!(
            end,
            positions([400., 300.], [800., 600.], 9, Duration::ZERO, true).unwrap()
        );
        for center in [[0., 0.], [800., 600.], [-100., 900.]] {
            assert!(
                positions(center, [800., 600.], 10, SPREAD, false)
                    .unwrap()
                    .iter()
                    .all(|p| p[0] >= 4. && p[0] <= 748. && p[1] >= 4. && p[1] <= 548.)
            );
        }
        assert!(positions([f64::NAN, 0.], [800., 600.], 10, SPREAD, false).is_none());
        assert!(positions([0., 0.], [40., 40.], 10, SPREAD, false).is_none());
    }
}
