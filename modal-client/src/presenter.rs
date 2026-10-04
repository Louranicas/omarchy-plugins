//! Shared closed presenter lease identity and lossless wire counters.
use modal_contract::{AuthorityEpoch, ClientId, Fence, Owner};
use serde::{Deserialize, Serialize};
use std::num::NonZeroU64;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Counter(NonZeroU64);
impl TryFrom<String> for Counter {
    type Error = &'static str;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        if s.starts_with('0') || s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
            return Err("invalid counter");
        }
        s.parse()
            .ok()
            .and_then(NonZeroU64::new)
            .map(Self)
            .ok_or("invalid counter")
    }
}
impl From<Counter> for String {
    fn from(c: Counter) -> String {
        c.0.to_string()
    }
}
impl Counter {
    pub fn new(n: u64) -> Option<Self> {
        NonZeroU64::new(n).map(Self)
    }
    pub fn get(self) -> u64 {
        self.0.get()
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Auth {
    epoch: [u8; 16],
    client: [u8; 16],
    client_epoch: Counter,
    generation: Counter,
    namespace: String,
}
impl Auth {
    pub fn new(fence: Fence, namespace: String) -> Result<Self, &'static str> {
        let a = Self {
            epoch: fence.authority_epoch.0,
            client: fence.owner.client.0,
            client_epoch: Counter(fence.owner.client_epoch),
            generation: Counter(fence.generation),
            namespace,
        };
        a.validate()?;
        Ok(a)
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.epoch == [0; 16]
            || self.client == [0; 16]
            || self.namespace.len() != 46
            || !self.namespace.starts_with("omarchy-modal-")
            || !self.namespace[14..]
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("invalid auth");
        }
        Ok(())
    }
    pub fn fence(&self) -> Fence {
        Fence {
            authority_epoch: AuthorityEpoch(self.epoch),
            owner: Owner {
                client: ClientId(self.client),
                client_epoch: self.client_epoch.0,
            },
            generation: self.generation.0,
        }
    }
    pub fn namespace(&self) -> &str {
        &self.namespace
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ready {
    version: u8,
    namespace: String,
    authority_epoch: [u8; 16],
    generation: Counter,
    client_id: [u8; 16],
    client_epoch: Counter,
    state: String,
}
pub fn auth_from_ready(json: &str) -> Result<Auth, &'static str> {
    if json.len() > 1024 {
        return Err("ready limit");
    }
    let ready: Ready = serde_json::from_str(json).map_err(|_| "invalid ready")?;
    if ready.version != 1 || ready.state != "mapped" {
        return Err("invalid ready");
    }
    Auth::new(
        Fence {
            authority_epoch: AuthorityEpoch(ready.authority_epoch),
            generation: ready.generation.0,
            owner: Owner {
                client: ClientId(ready.client_id),
                client_epoch: ready.client_epoch.0,
            },
        },
        ready.namespace,
    )
}
