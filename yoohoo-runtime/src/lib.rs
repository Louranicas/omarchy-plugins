pub mod controller;
pub mod ipc;
pub mod native;
pub mod presenter;
pub mod reconnect;
pub mod service;
use serde::{Deserialize, Serialize};
use yoohoo_core::{
    config::Config,
    identity::{Address, WindowKey},
    reducer::{Attention, Client, Effect, Envelope, Event, Health, Source, SourceHealth},
    selection::{Direction, Selection},
};
#[derive(Debug)]
pub enum Error {
    Core(yoohoo_core::Error),
    Invalid,
    Stale,
    Exhausted,
}
impl From<yoohoo_core::Error> for Error {
    fn from(e: yoohoo_core::Error) -> Self {
        Self::Core(e)
    }
}
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub sound: bool,
    pub volume: f64,
    pub history: bool,
    pub reduced_motion: bool,
}
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    List,
    Open,
    Close {
        generation: u64,
    },
    Next {
        generation: u64,
    },
    Previous {
        generation: u64,
    },
    MoveSelection {
        generation: u64,
        revision: u64,
        next: bool,
    },
    Activate {
        generation: u64,
        revision: u64,
        id: u64,
    },
    AcceptCycle {
        generation: u64,
        revision: u64,
    },
    Clear {
        revision: u64,
        id: u64,
    },
    Settings {
        revision: u64,
        settings: Settings,
    },
}
#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    pub request_id: u64,
    pub command: Command,
}
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Row {
    pub id: u64,
    pub title: String,
    pub class: String,
    pub workspace: String,
    pub count: u64,
    pub selected: bool,
}
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct View {
    pub revision: u64,
    pub generation: u64,
    pub open: bool,
    pub origin: String,
    pub stale: bool,
    pub native_health: String,
    pub notification_health: String,
    pub audio_health: String,
    pub storage_health: String,
    pub rows: Vec<Row>,
    pub total: usize,
    pub capped: bool,
    pub settings: Settings,
    pub pending_activation: bool,
    pub effect_backend: String,
}
#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub request_id: u64,
    pub ok: bool,
    pub error: Option<String>,
    pub view: View,
}
/// Bound to displayed state; not authorization to dispatch or acquire a modal lease.
#[derive(Debug, Clone)]
pub struct ActivationIntent {
    pub operation: u64,
    pub key: WindowKey,
    pub revision: u64,
    pub generation: u64,
}
pub struct Runtime {
    pub(crate) attention: Attention,
    pub(crate) instance: String,
    pub(crate) sequence: u64,
    selection: Selection,
    origin: String,
    operation: u64,
    pending: Option<ActivationIntent>,
    pub(crate) effects: Vec<Effect>,
}
impl Runtime {
    pub fn empty() -> Result<Self, Error> {
        let config = Config {
            sound_enabled: false,
            ..Config::default()
        };
        let mut attention = Attention::new(config)?;
        attention.set_health(SourceHealth {
            native: Health::Unavailable,
            notifications: Health::Unavailable,
            audio: Health::Unavailable,
            storage: Health::Unavailable,
        })?;
        Ok(Self {
            attention,
            instance: String::new(),
            sequence: 0,
            selection: Selection::default(),
            origin: "native".into(),
            operation: 0,
            pending: None,
            effects: Vec::new(),
        })
    }
    pub fn fixture() -> Result<Self, Error> {
        let mut r = Self::empty()?;
        r.origin = "isolated fixture".into();
        r.instance = "fixture".into();
        let clients = (1..=3)
            .map(|n| {
                Ok(Client {
                    key: WindowKey::new(
                        "fixture",
                        Address::parse(&format!("0x{n:x}"))?,
                        &format!("fixture-{n}"),
                    )?,
                    process: None,
                    title: format!("Build {n} — 日本語 👩‍💻"),
                    class: "Fixture terminal".into(),
                    workspace: format!("{n}"),
                    workspace_id: n,
                })
            })
            .collect::<Result<Vec<_>, yoohoo_core::Error>>()?;
        r.effects = r
            .attention
            .resnapshot("fixture", 0, clients.clone(), None, &[], 0)?;
        for (index, c) in clients.into_iter().enumerate() {
            r.apply(
                Event::Attention {
                    key: c.key,
                    source: Source::Native,
                },
                index as u64 + 1,
            )?;
        }
        r.attention.set_health(SourceHealth {
            native: Health::Disabled,
            notifications: Health::Unavailable,
            audio: Health::Unavailable,
            storage: Health::Unavailable,
        })?;
        Ok(r)
    }
    pub(crate) fn apply(&mut self, event: Event, now: u64) -> Result<(), Error> {
        let next = self.sequence.checked_add(1).ok_or(Error::Exhausted)?;
        let effects = self.attention.apply(Envelope {
            instance: self.instance.clone(),
            sequence: next,
            event,
            monotonic_ms: now,
            wall_ms: now.min(i64::MAX as u64) as i64,
        })?;
        self.sequence = next;
        self.pending = None;
        self.stage(effects)?;
        self.refresh_selection()?;
        Ok(())
    }
    fn stage(&mut self, effects: Vec<Effect>) -> Result<(), Error> {
        // No tag or audio backend is installed yet. Keep a bounded typed intent batch;
        // never tell callers these effects executed. Unsupported sound completes as failure.
        for effect in &effects {
            if let Effect::PlaySound { operation } = effect {
                self.attention.audio_finished(*operation, false)?;
            }
        }
        if effects.len() > 8192 {
            return Err(Error::Invalid);
        }
        self.effects = effects;
        Ok(())
    }
    fn refresh_selection(&mut self) -> Result<(), Error> {
        if self.selection.is_open() {
            let snapshot = self.attention.snapshot(false);
            self.selection.update(
                self.selection.generation(),
                snapshot.windows.into_iter().map(|w| w.key).collect(),
                snapshot.revision,
            )?;
        }
        Ok(())
    }
    fn key(&self, id: u64) -> Result<WindowKey, Error> {
        self.attention
            .snapshot(false)
            .windows
            .into_iter()
            .find(|w| w.opaque_id == id)
            .map(|w| w.key)
            .ok_or(Error::Stale)
    }
    fn activate(&mut self, generation: u64, revision: u64, key: WindowKey) -> Result<(), Error> {
        if !self.selection.is_open() || self.selection.generation() != generation {
            return Err(Error::Stale);
        }
        let key = self.attention.activation_target(&key, revision)?.clone();
        self.operation = self.operation.checked_add(1).ok_or(Error::Exhausted)?;
        self.pending = Some(ActivationIntent {
            operation: self.operation,
            key,
            revision,
            generation,
        });
        Ok(())
    }
    pub fn take_activation(&mut self) -> Option<ActivationIntent> {
        let pending = self.pending.take()?;
        if !self.selection.is_open()
            || self.selection.generation() != pending.generation
            || self
                .attention
                .activation_target(&pending.key, pending.revision)
                .is_err()
        {
            return None;
        }
        Some(pending)
    }
    pub fn execute(&mut self, command: Command, now: u64) -> Result<(), Error> {
        match command {
            Command::List => {}
            Command::Open => {
                self.pending = None;
                self.selection.open()?;
                self.refresh_selection()?;
            }
            Command::Close { generation } => {
                self.selection.close(generation)?;
                self.pending = None;
            }
            Command::Next { generation } => self.selection.step(generation, Direction::Next)?,
            Command::Previous { generation } => {
                self.selection.step(generation, Direction::Previous)?
            }
            Command::MoveSelection {
                generation,
                revision,
                next,
            } => {
                if revision != self.attention.revision() {
                    return Err(Error::Stale);
                }
                self.selection.move_selection(
                    generation,
                    if next {
                        Direction::Next
                    } else {
                        Direction::Previous
                    },
                )?;
            }
            Command::Activate {
                generation,
                revision,
                id,
            } => self.activate(generation, revision, self.key(id)?)?,
            Command::AcceptCycle {
                generation,
                revision,
            } => {
                let (key, displayed) = self.selection.accept_cycle(generation)?;
                if displayed != revision {
                    return Err(Error::Stale);
                }
                self.activate(generation, revision, key)?;
            }
            Command::Clear { revision, id } => {
                if revision != self.attention.revision() {
                    return Err(Error::Stale);
                }
                let key = self.key(id)?;
                let effects = self
                    .attention
                    .clear(&key, now.min(i64::MAX as u64) as i64, now)?;
                self.stage(effects)?;
                self.pending = None;
                self.refresh_selection()?;
            }
            Command::Settings { revision, settings } => {
                if revision != self.attention.revision() {
                    return Err(Error::Stale);
                }
                let mut config = self.attention.config().clone();
                config.sound_enabled = settings.sound;
                config.volume = settings.volume;
                config.history_enabled = settings.history;
                config.reduced_motion = settings.reduced_motion;
                self.attention.reload(config, now)?;
                self.pending = None;
                self.refresh_selection()?;
            }
        }
        Ok(())
    }
    pub fn view(&self) -> View {
        let s = self.attention.snapshot(true);
        let total = s.windows.len();
        let rows = s
            .windows
            .into_iter()
            .take(100)
            .map(|w| Row {
                id: w.opaque_id,
                title: w.title.unwrap_or_default(),
                class: w.class.unwrap_or_default(),
                workspace: w.workspace.unwrap_or_default(),
                count: w.count,
                selected: self.selection.selected() == Some(&w.key),
            })
            .collect();
        let config = self.attention.config();
        View {
            revision: s.revision,
            generation: self.selection.generation(),
            open: self.selection.is_open(),
            origin: self.origin.clone(),
            stale: s.stale,
            native_health: format!("{:?}", s.health.native),
            notification_health: format!("{:?}", s.health.notifications),
            audio_health: format!("{:?}", s.health.audio),
            storage_health: format!("{:?}", s.health.storage),
            rows,
            total,
            capped: total > 100,
            settings: Settings {
                sound: config.sound_enabled,
                volume: config.volume,
                history: config.history_enabled,
                reduced_motion: config.reduced_motion,
            },
            pending_activation: self.pending.is_some(),
            effect_backend: "Pending modal/tag/audio backend; settings are volatile".into(),
        }
    }
    pub fn handle(&mut self, request: Request, now: u64) -> Response {
        let result = if request.version != 1 || request.request_id == 0 {
            Err(Error::Invalid)
        } else {
            self.execute(request.command, now)
        };
        Response {
            request_id: request.request_id,
            ok: result.is_ok(),
            error: result.err().map(|e| format!("{e:?}")),
            view: self.view(),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn opening_never_activates_and_cycle_requires_cycle() {
        let mut r = Runtime::fixture().unwrap();
        r.execute(Command::Open, 4).unwrap();
        let v = r.view();
        assert!(r.take_activation().is_none());
        assert!(
            r.execute(
                Command::AcceptCycle {
                    generation: v.generation,
                    revision: v.revision
                },
                4
            )
            .is_err()
        );
        r.execute(
            Command::Next {
                generation: v.generation,
            },
            4,
        )
        .unwrap();
        r.execute(
            Command::AcceptCycle {
                generation: v.generation,
                revision: v.revision,
            },
            4,
        )
        .unwrap();
        assert!(r.take_activation().is_some());
    }
    #[test]
    fn stale_ui_and_revision_cannot_request_activation() {
        let mut r = Runtime::fixture().unwrap();
        r.execute(Command::Open, 4).unwrap();
        let v = r.view();
        r.execute(Command::Open, 5).unwrap();
        assert!(
            r.execute(
                Command::Activate {
                    generation: v.generation,
                    revision: v.revision,
                    id: v.rows[0].id
                },
                5
            )
            .is_err()
        );
        let v = r.view();
        r.execute(
            Command::Clear {
                revision: v.revision,
                id: v.rows[0].id,
            },
            5,
        )
        .unwrap();
        assert!(
            r.execute(
                Command::Activate {
                    generation: v.generation,
                    revision: v.revision,
                    id: v.rows[1].id
                },
                5
            )
            .is_err()
        );
    }
    #[test]
    fn fixture_health_and_settings_never_claim_live_backends() {
        let r = Runtime::fixture().unwrap();
        let v = r.view();
        assert_eq!(v.native_health, "Disabled");
        assert_eq!(v.storage_health, "Unavailable");
        assert!(!v.settings.history);
        assert!(!v.settings.sound);
        assert!(!v.pending_activation);
    }
}
