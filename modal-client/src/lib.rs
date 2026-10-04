//! Persistent authenticated client for the single modal arbiter.
//! A returned intent token remains subject to server-side fence and source checks.
pub mod data;
pub mod presenter;
mod transport;
use modal_contract::{AuthorityEpoch, ClientId, Fence, Owner};
use modal_runtime::targets::{MAX_TARGETS, NativeTarget, Row, Token};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::BTreeSet,
    num::NonZeroU64,
    path::Path,
    time::{Duration, Instant},
};

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Transport,
    Protocol,
    Expired,
    Unavailable,
    Exhausted,
    Remote(String),
    EffectUnconfirmed,
}
#[derive(Clone, Copy, Debug)]
pub struct Lease {
    pub fence: Fence,
    pub deadline: Instant,
    pub presenter: Option<PresenterIdentity>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PresenterIdentity {
    process: desktop_io::ProcessIdentity,
    namespace: [u8; 16],
}
impl PresenterIdentity {
    pub fn nonce(&self) -> [u8; 16] {
        self.namespace
    }
    pub fn process(&self) -> desktop_io::ProcessIdentity {
        self.process
    }
    pub fn namespace(&self) -> String {
        format!(
            "omarchy-modal-{}",
            self.namespace
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        )
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WirePresenter {
    pid: u32,
    start_ticks: String,
    namespace: String,
}
impl WirePresenter {
    fn parse(self) -> Result<PresenterIdentity, Error> {
        if self.pid == 0 || self.pid > i32::MAX as u32 {
            return Err(Error::Protocol);
        }
        let raw = self
            .namespace
            .strip_prefix("omarchy-modal-")
            .ok_or(Error::Protocol)?;
        if raw.len() != 32
            || !raw
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return Err(Error::Protocol);
        }
        let mut namespace = [0; 16];
        for (i, b) in namespace.iter_mut().enumerate() {
            *b = u8::from_str_radix(&raw[i * 2..i * 2 + 2], 16).map_err(|_| Error::Protocol)?;
        }
        Ok(PresenterIdentity {
            process: desktop_io::ProcessIdentity {
                pid: self.pid,
                start_ticks: counter(&self.start_ticks)?.get(),
            },
            namespace,
        })
    }
}
#[derive(Debug)]
pub struct View {
    fence: Fence,
    revision: u64,
    session: [u8; 16],
    rows: Vec<Row>,
}
impl View {
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRow {
    token: String,
    instance: String,
    stable_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    version: u8,
    request_id: String,
    status: Option<String>,
    error: Option<String>,
    epoch: Option<[u8; 16]>,
    generation: Option<String>,
    expires_ms: Option<String>,
    client_id: Option<[u8; 16]>,
    client_epoch: Option<String>,
    revision: Option<String>,
    producer_session: Option<[u8; 16]>,
    next: Option<usize>,
    rows: Option<Vec<WireRow>>,
    presenter: Option<WirePresenter>,
}
fn counter(s: &str) -> Result<NonZeroU64, Error> {
    if s.starts_with('0') || s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
        return Err(Error::Protocol);
    }
    s.parse()
        .ok()
        .and_then(NonZeroU64::new)
        .ok_or(Error::Protocol)
}
pub struct Client {
    transport: transport::Transport,
    request: u64,
    lease: Option<Lease>,
    broken: bool,
}
impl Client {
    /// Identity must come from the owned arbiter launcher or trusted discovery.
    /// The connection never reconnects implicitly; a replacement gets a new owner.
    pub fn connect(root: &Path, identity: desktop_io::ProcessIdentity) -> Result<Self, Error> {
        Ok(Self {
            transport: transport::Transport::connect(root, identity)
                .map_err(|_| Error::Transport)?,
            request: 0,
            lease: None,
            broken: false,
        })
    }
    pub fn lease(&self) -> Option<Lease> {
        self.lease
            .filter(|l| !self.broken && Instant::now() < l.deadline)
    }
    fn fail(&mut self, error: Error) -> Error {
        self.broken = true;
        self.lease = None;
        self.transport.close();
        error
    }
    fn exchange(&mut self, command: serde_json::Value, deadline: Instant) -> Result<Reply, Error> {
        if self.broken {
            return Err(Error::Unavailable);
        }
        self.request = self
            .request
            .checked_add(1)
            .ok_or_else(|| self.fail(Error::Exhausted))?;
        let bytes = serde_json::to_vec(
            &json!({"version":1,"request_id":self.request.to_string(),"command":command}),
        )
        .map_err(|_| self.fail(Error::Protocol))?;
        let bytes = self
            .transport
            .exchange(&bytes, deadline)
            .map_err(|_| self.fail(Error::Transport))?;
        let reply: Reply =
            serde_json::from_slice(&bytes).map_err(|_| self.fail(Error::Protocol))?;
        if reply.version != 1
            || counter(&reply.request_id).map_err(|e| self.fail(e))?.get() != self.request
            || reply.status.is_some() == reply.error.is_some()
        {
            return Err(self.fail(Error::Protocol));
        }
        if let Some(error) = &reply.error {
            if error.len() > 128 {
                return Err(self.fail(Error::Protocol));
            }
            if error.contains("EffectUnconfirmed") {
                return Err(self.fail(Error::EffectUnconfirmed));
            }
            return Err(self.fail(Error::Remote(error.clone())));
        }
        Ok(reply)
    }
    pub fn acquire(&mut self, ttl: Duration) -> Result<Lease, Error> {
        if self.lease.is_some() {
            return Err(Error::Unavailable);
        }
        self.grant(ttl, false)
    }
    pub fn renew(&mut self, ttl: Duration) -> Result<Lease, Error> {
        self.grant(ttl, true)
    }
    fn grant(&mut self, ttl: Duration, renew: bool) -> Result<Lease, Error> {
        if ttl.is_zero() || ttl > Duration::from_secs(5) || ttl.as_millis() == 0 {
            return Err(Error::Protocol);
        }
        let start = Instant::now();
        let deadline = start + Duration::from_millis(ttl.as_millis() as u64);
        let command = if renew {
            let lease = self.lease().ok_or(Error::Expired)?;
            json!({"op":"renew","epoch":lease.fence.authority_epoch.0,"generation":lease.fence.generation.to_string(),"ttl_ms":ttl.as_millis()})
        } else {
            json!({"op":"acquire","ttl_ms":ttl.as_millis()})
        };
        let reply = self.exchange(command, deadline.min(start + Duration::from_millis(500)))?;
        let parsed = (|| {
            if reply.status.as_deref() != Some("granted")
                || reply.rows.is_some()
                || reply.revision.is_some()
                || reply.producer_session.is_some()
                || reply.next.is_some()
            {
                return Err(Error::Protocol);
            }
            counter(reply.expires_ms.as_deref().ok_or(Error::Protocol)?)?;
            let fence = Fence {
                authority_epoch: AuthorityEpoch(reply.epoch.ok_or(Error::Protocol)?),
                generation: counter(reply.generation.as_deref().ok_or(Error::Protocol)?)?,
                owner: Owner {
                    client: ClientId(reply.client_id.ok_or(Error::Protocol)?),
                    client_epoch: counter(reply.client_epoch.as_deref().ok_or(Error::Protocol)?)?,
                },
            };
            if fence.authority_epoch.0 == [0; 16]
                || fence.owner.client.0 == [0; 16]
                || (renew && self.lease.ok_or(Error::Expired)?.fence != fence)
            {
                return Err(Error::Protocol);
            }
            if Instant::now() >= deadline {
                return Err(Error::Expired);
            }
            let presenter = reply.presenter.map(WirePresenter::parse).transpose()?;
            if renew && self.lease.ok_or(Error::Expired)?.presenter != presenter {
                return Err(Error::Protocol);
            }
            Ok(Lease {
                fence,
                deadline,
                presenter,
            })
        })()
        .map_err(|e| self.fail(e))?;
        self.lease = Some(parsed);
        Ok(parsed)
    }
    pub fn targets(&mut self) -> Result<View, Error> {
        let lease = self.lease().ok_or(Error::Expired)?;
        let deadline = lease
            .deadline
            .min(Instant::now() + Duration::from_millis(500));
        let mut cursor = 0;
        let mut revision = None;
        let mut session = None;
        let mut rows = Vec::new();
        let mut tokens = BTreeSet::new();
        let mut identities = BTreeSet::new();
        loop {
            let reply=self.exchange(json!({"op":"list_targets","epoch":lease.fence.authority_epoch.0,"generation":lease.fence.generation.to_string(),"cursor":cursor}),deadline)?;
            let result = (|| {
                if reply.status.as_deref() != Some("targets")
                    || reply.epoch.is_some()
                    || reply.generation.is_some()
                    || reply.client_id.is_some()
                    || reply.client_epoch.is_some()
                    || reply.expires_ms.is_some()
                    || reply.presenter.is_some()
                {
                    return Err(Error::Protocol);
                }
                let r = counter(reply.revision.as_deref().ok_or(Error::Protocol)?)?.get();
                let s = reply.producer_session.ok_or(Error::Protocol)?;
                if s == [0; 16]
                    || revision.is_some_and(|v| v != r)
                    || session.is_some_and(|v| v != s)
                {
                    return Err(Error::Protocol);
                }
                revision = Some(r);
                session = Some(s);
                let incoming = reply.rows.ok_or(Error::Protocol)?;
                if incoming.len() > 8 || rows.len() + incoming.len() > MAX_TARGETS {
                    return Err(Error::Protocol);
                }
                for row in incoming {
                    let token = Token::parse(&row.token).map_err(|_| Error::Protocol)?;
                    let target = NativeTarget::new(&row.instance, &row.stable_id)
                        .map_err(|_| Error::Protocol)?;
                    if target.stable_id() != row.stable_id
                        || !tokens.insert(token.clone())
                        || !identities.insert(target.clone())
                    {
                        return Err(Error::Protocol);
                    }
                    rows.push(Row { token, target });
                }
                if let Some(next) = reply.next
                    && (next != rows.len() || next <= cursor || next >= MAX_TARGETS)
                {
                    return Err(Error::Protocol);
                }
                Ok(reply.next)
            })()
            .map_err(|e| self.fail(e))?;
            match result {
                Some(next) => cursor = next,
                None => break,
            }
        }
        Ok(View {
            fence: lease.fence,
            revision: revision.ok_or(Error::Protocol)?,
            session: session.ok_or(Error::Protocol)?,
            rows,
        })
    }
    pub fn focus(&mut self, view: &View, index: usize) -> Result<(), Error> {
        let lease = self.lease().ok_or(Error::Expired)?;
        if view.fence != lease.fence {
            return Err(Error::Expired);
        }
        let row = view.rows.get(index).ok_or(Error::Protocol)?;
        self.mutation(json!({"op":"focus_window","epoch":lease.fence.authority_epoch.0,"generation":lease.fence.generation.to_string(),"token":row.token.as_str(),"displayed_revision":view.revision.to_string(),"producer_session":view.session}),lease.deadline.min(Instant::now()+Duration::from_millis(500)))
    }

    pub fn move_workspace(
        &mut self,
        view: &View,
        index: usize,
        expected: &modal_runtime::workspace_move::MoveWorkspaceRequest,
    ) -> Result<(), Error> {
        let lease = self.lease().ok_or(Error::Expired)?;
        if view.fence != lease.fence {
            return Err(Error::Expired);
        }
        if !expected.valid() {
            return Err(Error::Protocol);
        }
        let row = view.rows.get(index).ok_or(Error::Protocol)?;
        self.mutation(json!({"op":"move_workspace","epoch":lease.fence.authority_epoch.0,"generation":lease.fence.generation.to_string(),"token":row.token.as_str(),"displayed_revision":view.revision.to_string(),"producer_session":view.session,"expected":expected}),lease.deadline.min(Instant::now()+Duration::from_millis(500)))
    }

    pub fn maximize(
        &mut self,
        view: &View,
        index: usize,
        expected: &modal_runtime::vimarchy_action::MaximizeRequest,
    ) -> Result<(), Error> {
        let lease = self.lease().ok_or(Error::Expired)?;
        if view.fence != lease.fence {
            return Err(Error::Expired);
        }
        if !expected.valid() {
            return Err(Error::Protocol);
        }
        let row = view.rows.get(index).ok_or(Error::Protocol)?;
        self.mutation(json!({"op":"maximize_window","epoch":lease.fence.authority_epoch.0,"generation":lease.fence.generation.to_string(),"token":row.token.as_str(),"displayed_revision":view.revision.to_string(),"producer_session":view.session,"expected":expected}),lease.deadline.min(Instant::now()+Duration::from_millis(500)))
    }

    pub fn execute(&mut self, effect: modal_runtime::actor::Effect) -> Result<(), Error> {
        use modal_runtime::actor::Effect;
        let name = match effect {
            Effect::EnterVimarchy => "enter_vimarchy",
            Effect::EnterVimarchyDouble => "enter_vimarchy_double",
            Effect::EnterAsk => "enter_ask",
            Effect::EnterYoohoo => "enter_yoohoo",
            Effect::Reset => "reset",
        };
        let lease = self.lease().ok_or(Error::Expired)?;
        self.mutation(json!({"op":"execute","epoch":lease.fence.authority_epoch.0,"generation":lease.fence.generation.to_string(),"effect":name}),lease.deadline.min(Instant::now()+Duration::from_millis(500)))
    }
    pub fn release(&mut self) -> Result<(), Error> {
        let lease = self.lease().ok_or(Error::Expired)?;
        let result=self.mutation(json!({"op":"release","epoch":lease.fence.authority_epoch.0,"generation":lease.fence.generation.to_string()}),lease.deadline.min(Instant::now()+Duration::from_millis(500)));
        self.lease = None;
        result
    }
    fn mutation(&mut self, command: serde_json::Value, deadline: Instant) -> Result<(), Error> {
        let reply = self.exchange(command, deadline).map_err(|e| match e {
            Error::Transport | Error::Protocol => self.fail(Error::EffectUnconfirmed),
            other => other,
        })?;
        self.complete(reply).map_err(|e| match e {
            Error::Protocol => self.fail(Error::EffectUnconfirmed),
            other => other,
        })
    }
    fn complete(&mut self, reply: Reply) -> Result<(), Error> {
        if reply.status.as_deref() != Some("complete")
            || reply.epoch.is_some()
            || reply.generation.is_some()
            || reply.expires_ms.is_some()
            || reply.client_id.is_some()
            || reply.client_epoch.is_some()
            || reply.rows.is_some()
            || reply.revision.is_some()
            || reply.producer_session.is_some()
            || reply.next.is_some()
            || reply.presenter.is_some()
        {
            return Err(self.fail(Error::Protocol));
        }
        Ok(())
    }
}
