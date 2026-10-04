//! Closed, bounded daemon/presenter protocol. Full lease and namespace bind each
//! frame; displayed revision binds input. No target token or raw dispatch crosses UI.
pub use modal_client::presenter::{Auth, Counter, auth_from_ready};
#[cfg(test)]
use modal_contract::{AuthorityEpoch, ClientId, Fence, Owner};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::num::NonZeroU64;
use vimarchy_core::{
    allocation::CAPACITY,
    gestures::{self, Event},
};
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u8,
    pub request: Counter,
    pub auth: Auth,
    pub command: Command,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Read {
        cursor: usize,
        revision: Option<Counter>,
    },
    Input {
        revision: Counter,
        event: Input,
    },
    Close {
        revision: Counter,
    },
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Input {
    Down {
        hint: String,
        press: Counter,
        alt: bool,
        repeat: bool,
    },
    Up {
        press: Counter,
    },
    Workspace {
        key: char,
        alt: bool,
    },
    Cancel,
}
impl Input {
    pub fn event(self) -> Result<Event, &'static str> {
        Ok(match self {
            Self::Down {
                hint,
                press,
                alt,
                repeat,
            } => {
                if !(1..=2).contains(&hint.len())
                    || !hint
                        .bytes()
                        .all(|b| vimarchy_core::allocation::ALPHABET.contains(b as char))
                {
                    return Err("invalid hint");
                }
                Event::Down {
                    hint,
                    press_id: press.get(),
                    alt,
                    repeat,
                }
            }
            Self::Up { press } => Event::Up {
                press_id: press.get(),
            },
            Self::Workspace { key, alt } => {
                if gestures::WorkspaceTarget::from_key(key).is_none() {
                    return Err("invalid workspace");
                }
                Event::Workspace { key, alt }
            }
            Self::Cancel => Event::Cancel,
        })
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Row {
    Radial {
        source_hint: String,
        current_workspace: i64,
    },
    Output {
        name: String,
        origin: [f64; 2],
        size: [f64; 2],
    },
    Hint {
        hint: String,
        output: String,
        at: [f64; 2],
        size: [f64; 2],
        focused: bool,
    },
}
impl Row {
    pub fn validate(&self) -> Result<(), &'static str> {
        if let Self::Radial { source_hint, .. } = self {
            return if (1..=2).contains(&source_hint.len())
                && source_hint
                    .chars()
                    .all(|c| vimarchy_core::allocation::ALPHABET.contains(c))
            {
                Ok(())
            } else {
                Err("invalid radial source")
            };
        }
        let (name, position, size, hint) = match self {
            Self::Radial { .. } => unreachable!("handled metadata"),
            Self::Output { name, origin, size } => (name, origin, size, None),
            Self::Hint {
                hint,
                output,
                at,
                size,
                ..
            } => (output, at, size, Some(hint)),
        };
        if name.is_empty()
            || name.len() > 256
            || name.contains('\0')
            || !position
                .iter()
                .chain(size)
                .all(|v| v.is_finite() && v.abs() <= 1_000_000.)
            || size.iter().any(|v| *v <= 0.)
        {
            return Err("invalid geometry");
        }
        if hint.is_some_and(|h| {
            !(1..=2).contains(&h.len())
                || !h
                    .bytes()
                    .all(|b| vimarchy_core::allocation::ALPHABET.contains(b as char))
        }) {
            return Err("invalid hint");
        }
        Ok(())
    }
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub version: u8,
    pub request: Counter,
    pub auth: Auth,
    pub revision: Counter,
    pub result: ResultBody,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResultBody {
    Page {
        rows: Vec<Row>,
        next: Option<usize>,
        phase: Phase,
    },
    Applied {
        phase: Phase,
    },
    Closed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Mapping,
    Selecting,
    Moving,
    Radial,
    Exiting,
    Closed,
}
impl From<&gestures::Phase> for Phase {
    fn from(p: &gestures::Phase) -> Self {
        match p {
            gestures::Phase::Mapping => Self::Mapping,
            gestures::Phase::Selecting => Self::Selecting,
            gestures::Phase::Moving(_) => Self::Moving,
            gestures::Phase::Radial(_) => Self::Radial,
            gestures::Phase::Exiting => Self::Exiting,
            gestures::Phase::Closed => Self::Closed,
        }
    }
}
fn validate_radial(rows: &[Row], phase: Phase) -> Result<(), &'static str> {
    let radial: Vec<_> = rows
        .iter()
        .filter_map(|row| {
            if let Row::Radial { source_hint, .. } = row {
                Some(source_hint)
            } else {
                None
            }
        })
        .collect();
    if phase == Phase::Radial {
        if radial.len() != 1
            || !rows
                .iter()
                .any(|row| matches!(row,Row::Hint{hint,..} if hint==radial[0]))
        {
            return Err("radial source missing or ambiguous");
        }
    } else if !radial.is_empty() {
        return Err("radial source outside radial phase");
    }
    Ok(())
}
pub const MAX_ROWS: usize = CAPACITY + 128 + 1;
pub const PAGE_ROWS: usize = 4;
pub fn decode(bytes: &[u8]) -> Result<Request, &'static str> {
    if bytes.len() > 4096 {
        return Err("frame limit");
    }
    let r: Request = serde_json::from_slice(bytes).map_err(|_| "invalid frame")?;
    if r.version != 1 {
        return Err("invalid version");
    }
    r.auth.validate()?;
    if let Command::Read { cursor, .. } = &r.command
        && *cursor > MAX_ROWS
    {
        return Err("invalid cursor");
    }
    Ok(r)
}

/// Presenter-side authenticated client. A failed frame permanently poisons this
/// link; there is no reconnect or blind mutation retry.
pub struct Link {
    client: modal_client::data::Client,
    auth: Auth,
    request: u64,
    revision: Option<Counter>,
    failed: bool,
}
impl Link {
    pub fn connect(
        socket: &std::path::Path,
        controller: desktop_io::ProcessIdentity,
        auth: Auth,
    ) -> std::io::Result<Self> {
        auth.validate()
            .map_err(|_| std::io::Error::other("invalid auth"))?;
        Ok(Self {
            client: modal_client::data::Client::connect(socket, controller)?,
            auth,
            request: 0,
            revision: None,
            failed: false,
        })
    }
    fn exchange(&mut self, command: Command) -> std::io::Result<Reply> {
        self.exchange_until(
            command,
            std::time::Instant::now() + std::time::Duration::from_millis(500),
        )
    }
    fn exchange_until(
        &mut self,
        command: Command,
        deadline: std::time::Instant,
    ) -> std::io::Result<Reply> {
        if self.failed {
            return Err(std::io::Error::other("presenter link failed"));
        }
        let result = (|| {
            self.request = self
                .request
                .checked_add(1)
                .ok_or_else(|| std::io::Error::other("counter exhausted"))?;
            let request = Counter::new(self.request)
                .ok_or_else(|| std::io::Error::other("counter exhausted"))?;
            let frame = Request {
                version: 1,
                request,
                auth: self.auth.clone(),
                command,
            };
            let bytes = serde_json::to_vec(&frame)?;
            let reply = self.client.exchange(
                &bytes,
                deadline.min(std::time::Instant::now() + std::time::Duration::from_millis(500)),
            )?;
            let reply: Reply = serde_json::from_slice(&reply)?;
            if reply.version != 1
                || reply.request != request
                || reply.auth != self.auth
                || self.revision.is_some_and(|r| r != reply.revision)
            {
                return Err(std::io::Error::other("stale presenter reply"));
            }
            self.revision = Some(reply.revision);
            Ok(reply)
        })();
        if result.is_err() {
            self.failed = true;
            self.client.invalidate();
        }
        result
    }
    pub fn model(&mut self) -> std::io::Result<(Vec<Row>, Phase)> {
        let mut rows = Vec::new();
        let mut cursor = 0;
        let mut phase = None;
        let mut outputs = std::collections::BTreeSet::new();
        let mut hints = std::collections::BTreeSet::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let result = (|| {
            loop {
                if std::time::Instant::now() >= deadline {
                    return Err(std::io::Error::other("model deadline"));
                }
                let reply = self.exchange_until(
                    Command::Read {
                        cursor,
                        revision: self.revision,
                    },
                    deadline,
                )?;
                let ResultBody::Page {
                    rows: page,
                    next,
                    phase: received,
                } = reply.result
                else {
                    return Err(std::io::Error::other("model unavailable"));
                };
                if page.len() > PAGE_ROWS
                    || rows.len() + page.len() > MAX_ROWS
                    || phase.is_some_and(|p| p != received)
                {
                    return Err(std::io::Error::other("inconsistent model"));
                }
                phase = Some(received);
                for row in page {
                    row.validate()
                        .map_err(|_| std::io::Error::other("invalid model row"))?;
                    match &row {
                        Row::Radial { .. } => {}
                        Row::Output { name, .. } => {
                            if !outputs.insert(name.clone()) || outputs.len() > 128 {
                                return Err(std::io::Error::other("duplicate output"));
                            }
                        }
                        Row::Hint { hint, output, .. } => {
                            if !hints.insert(hint.clone())
                                || !outputs.contains(output)
                                || hints.len() > CAPACITY
                            {
                                return Err(std::io::Error::other("invalid hint output"));
                            }
                        }
                    }
                    rows.push(row);
                }
                match next {
                    Some(n) if n == rows.len() && n > cursor && n < MAX_ROWS => cursor = n,
                    None => break,
                    _ => return Err(std::io::Error::other("invalid pagination")),
                }
            }
            let phase = phase.unwrap_or(Phase::Closed);
            validate_radial(&rows, phase).map_err(std::io::Error::other)?;
            Ok((rows, phase))
        })();
        if result.is_err() {
            self.failed = true;
            self.client.invalidate();
        }
        result
    }
    pub fn input(&mut self, event: Input) -> std::io::Result<Phase> {
        let revision = self
            .revision
            .ok_or_else(|| std::io::Error::other("model missing"))?;
        let reply = self.exchange(Command::Input { revision, event })?;
        match reply.result {
            ResultBody::Applied { phase } => Ok(phase),
            _ => {
                self.failed = true;
                self.client.invalidate();
                Err(std::io::Error::other("input response invalid"))
            }
        }
    }
    pub fn close(&mut self) -> std::io::Result<()> {
        let revision = self
            .revision
            .ok_or_else(|| std::io::Error::other("model missing"))?;
        let reply = self.exchange(Command::Close { revision })?;
        self.failed = true;
        self.client.invalidate();
        if matches!(reply.result, ResultBody::Closed) {
            Ok(())
        } else {
            Err(std::io::Error::other("close unconfirmed"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_forged_ui_readiness_and_lossy_counters() {
        let auth = Auth::new(
            Fence {
                authority_epoch: AuthorityEpoch([1; 16]),
                owner: Owner {
                    client: ClientId([2; 16]),
                    client_epoch: NonZeroU64::new(1).unwrap(),
                },
                generation: NonZeroU64::new(u64::MAX).unwrap(),
            },
            format!("omarchy-modal-{}", "a".repeat(32)),
        )
        .unwrap();
        let frame = Request {
            version: 1,
            request: Counter::new(u64::MAX).unwrap(),
            auth,
            command: Command::Input {
                revision: Counter::new(1).unwrap(),
                event: Input::Cancel,
            },
        };
        let bytes = serde_json::to_vec(&frame).unwrap();
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.request.get(), u64::MAX);
        let text = String::from_utf8(bytes).unwrap();
        assert!(
            decode(
                text.replace("\"kind\":\"cancel\"", "\"kind\":\"mapped\"")
                    .as_bytes()
            )
            .is_err()
        );
        assert!(Counter::try_from("01".to_owned()).is_err());
        assert!(Counter::try_from("18446744073709551616".to_owned()).is_err());
    }
    #[test]
    fn geometry_and_hint_validation_prevents_unbounded_or_nonfinite_layout() {
        for row in [
            Row::Output {
                name: "x".into(),
                origin: [f64::NAN, 0.],
                size: [10., 10.],
            },
            Row::Hint {
                hint: "aaa".into(),
                output: "x".into(),
                at: [0., 0.],
                size: [1., 1.],
                focused: false,
            },
            Row::Output {
                name: "x\0y".into(),
                origin: [0., 0.],
                size: [1., 1.],
            },
        ] {
            assert!(row.validate().is_err());
        }
    }
    #[test]
    fn radial_metadata_must_match_phase_and_displayed_source() {
        let hint = Row::Hint {
            hint: "a".into(),
            output: "screen".into(),
            at: [0., 0.],
            size: [10., 10.],
            focused: false,
        };
        let radial = Row::Radial {
            source_hint: "a".into(),
            current_workspace: 2,
        };
        assert!(validate_radial(&[hint.clone(), radial.clone()], Phase::Radial).is_ok());
        assert!(validate_radial(std::slice::from_ref(&hint), Phase::Radial).is_err());
        assert!(validate_radial(std::slice::from_ref(&radial), Phase::Radial).is_err());
        assert!(validate_radial(&[hint.clone(), radial.clone()], Phase::Selecting).is_err());
        assert!(validate_radial(&[hint, radial.clone(), radial], Phase::Radial).is_err());
        assert!(
            Row::Radial {
                source_hint: "aaa".into(),
                current_workspace: 2
            }
            .validate()
            .is_err()
        );
    }
}
