//! Preserve unchanged GTK rows while ACP text streams into the latest message.
use ask_core::{Text, session::Role};
use gtk::prelude::*;
use std::{cell::Cell, rc::Rc};

/// At most one deferred follow. Any intervening adjustment movement revokes it,
/// including a user moving away and back before GTK dispatches the idle task.
pub struct TailFollow {
    adjustment: gtk::Adjustment,
    pending: Rc<Cell<bool>>,
    valid: Rc<Cell<bool>>,
}
impl TailFollow {
    pub fn new(adjustment: &gtk::Adjustment) -> Self {
        let valid = Rc::new(Cell::new(false));
        let changed = valid.clone();
        adjustment.connect_value_changed(move |_| changed.set(false));
        Self {
            adjustment: adjustment.clone(),
            pending: Rc::new(Cell::new(false)),
            valid,
        }
    }
    pub fn schedule(&self, alive: impl FnOnce() -> bool + 'static) {
        if self.pending.get()
            || self.adjustment.value() + self.adjustment.page_size() < self.adjustment.upper() - 40.
        {
            return;
        }
        self.pending.set(true);
        self.valid.set(true);
        let pending = self.pending.clone();
        let valid = self.valid.clone();
        let adjustment = self.adjustment.clone();
        gtk::glib::idle_add_local_once(move || {
            pending.set(false);
            if valid.replace(false) && alive() {
                adjustment.set_value((adjustment.upper() - adjustment.page_size()).max(0.));
            }
        });
    }
}

struct Row {
    role: Role,
    text: Text,
    container: gtk::Box,
    who: gtk::Label,
    body: gtk::Label,
}
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Changes {
    pub created: usize,
    pub removed: usize,
    pub body_updates: usize,
}
pub struct Transcript {
    container: gtk::Box,
    rows: Vec<Row>,
}
impl Transcript {
    pub fn new() -> Self {
        let container = gtk::Box::new(gtk::Orientation::Vertical, 12);
        container.update_property(&[gtk::accessible::Property::Label("Conversation transcript")]);
        Self {
            container,
            rows: vec![],
        }
    }
    pub fn widget(&self) -> &gtk::Box {
        &self.container
    }
    pub fn update(&mut self, messages: &[(Role, Text)]) -> Changes {
        let mut changes = Changes::default();
        while self.rows.len() > messages.len() {
            self.container.remove(&self.rows.pop().unwrap().container);
            changes.removed += 1;
        }
        for (index, (role, text)) in messages.iter().enumerate() {
            let name = if *role == Role::Human { "You" } else { "Agent" };
            if let Some(row) = self.rows.get_mut(index) {
                if row.role != *role {
                    row.who.set_text(name);
                    row.role = *role;
                }
                if row.text != *text {
                    row.body.set_text(text.as_str());
                    row.text = text.clone();
                    changes.body_updates += 1;
                }
            } else {
                let container = gtk::Box::new(gtk::Orientation::Vertical, 3);
                let who = gtk::Label::new(Some(name));
                who.set_xalign(0.);
                who.add_css_class("heading");
                let body = gtk::Label::new(Some(text.as_str()));
                body.set_xalign(0.);
                body.set_wrap(true);
                body.set_selectable(true);
                container.append(&who);
                container.append(&body);
                self.container.append(&container);
                self.rows.push(Row {
                    role: *role,
                    text: text.clone(),
                    container,
                    who,
                    body,
                });
                changes.created += 1;
            }
        }
        changes
    }
}
/// Actual GTK object/selection checks on the private display, without a provider.
pub fn fixture_check() -> Result<(), Box<dyn std::error::Error>> {
    gtk::init()?;
    let adjustment = gtk::Adjustment::new(900., 0., 1000., 10., 100., 100.);
    let follow = TailFollow::new(&adjustment);
    let drain = || {
        let context = gtk::glib::MainContext::default();
        while context.pending() {
            context.iteration(false);
        }
    };
    // Tail growth follows once; repeated updates coalesce into one callback.
    let callbacks = Rc::new(Cell::new(0));
    for _ in 0..32 {
        let callbacks = callbacks.clone();
        follow.schedule(move || {
            callbacks.set(callbacks.get() + 1);
            true
        });
    }
    adjustment.set_upper(1200.);
    drain();
    assert_eq!(adjustment.value(), 1100.);
    assert_eq!(callbacks.get(), 1);
    // User moves after the stream update but before the pending idle callback.
    follow.schedule(|| true);
    adjustment.set_upper(1400.);
    adjustment.set_value(400.);
    drain();
    assert_eq!(adjustment.value(), 400.);
    // Moving away and back also revokes this particular queued follow.
    adjustment.set_value(1300.);
    follow.schedule(|| true);
    adjustment.set_upper(1600.);
    adjustment.set_value(400.);
    adjustment.set_value(1300.);
    drain();
    assert_eq!(adjustment.value(), 1300.);
    // A closed/replaced surface cannot apply its queued scroll.
    adjustment.set_value(1500.);
    follow.schedule(|| false);
    adjustment.set_upper(1800.);
    drain();
    assert_eq!(adjustment.value(), 1500.);
    // Reading history does not schedule follow; a later fresh tail update can.
    adjustment.set_value(400.);
    follow.schedule(|| panic!("history must not follow"));
    drain();
    adjustment.set_value(1700.);
    follow.schedule(|| true);
    adjustment.set_upper(2000.);
    drain();
    assert_eq!(adjustment.value(), 1900.);
    println!(
        "tail_follow_fixture_pass=true coalesced=32 scroll_revocation=true lifetime_revocation=true"
    );
    let mut view = Transcript::new();
    let mut messages = (0..64)
        .map(|n| {
            (
                if n % 2 == 0 {
                    Role::Human
                } else {
                    Role::Assistant
                },
                Text::new(format!("Message {n} 日本語")).unwrap(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        view.update(&messages),
        Changes {
            created: 64,
            ..Changes::default()
        }
    );
    let first = view.rows[0].body.clone();
    first.select_region(0, 7);
    let selection = first.selection_bounds();
    assert!(selection.is_some());
    let objects = view
        .rows
        .iter()
        .map(|r| r.container.clone())
        .collect::<Vec<_>>();
    let started = std::time::Instant::now();
    let mut body_updates = 0;
    let mut streamed = "Stream 日本語 😀 ".to_owned();
    for _ in 0..256 {
        streamed.push_str("next ");
        messages[63].1 = Text::new(streamed.clone()).unwrap();
        let change = view.update(&messages);
        assert_eq!(change.created, 0);
        assert_eq!(change.removed, 0);
        assert_eq!(change.body_updates, 1);
        assert_eq!(view.rows[63].body.text(), streamed);
        body_updates += change.body_updates;
        assert_eq!(first.selection_bounds(), selection);
        assert!(
            view.rows
                .iter()
                .zip(&objects)
                .all(|(r, prior)| r.container == *prior)
        );
    }
    let elapsed = started.elapsed();
    assert_eq!(view.update(&messages), Changes::default());
    messages[0] = (Role::Assistant, Text::new("Replacement role/body").unwrap());
    assert_eq!(view.update(&messages).body_updates, 1);
    assert_eq!(view.rows[0].who.text(), "Agent");
    messages.truncate(2);
    assert_eq!(view.update(&messages).removed, 62);
    assert_eq!(view.rows.len(), 2);
    messages.push((Role::Human, Text::new("New turn").unwrap()));
    assert_eq!(view.update(&messages).created, 1);
    assert_eq!(view.rows[2].body.text(), "New turn");
    assert_eq!(view.update(&[]).removed, 3);
    println!(
        "transcript_fixture_pass=true messages=64 streamed_updates=256 created_during_stream=0 body_updates={body_updates} unchanged_selection_preserved=true elapsed_us={} measurement=local_debug_gtk_no_layout",
        elapsed.as_micros()
    );
    Ok(())
}
