//! Closed frame schema. Owner is supplied by the authenticated connection host,
//! never by JSON. Decimal-string counters preserve u64 values at JS boundaries.
use crate::actor::Effect;
use modal_contract::{AuthorityEpoch, Fence, Owner, RequestKey};
use serde::Deserialize;
use std::num::NonZeroU64;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u8,
    request_id: String,
    command: WireCommand,
}
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum WireCommand {
    ListTargets {
        epoch: [u8; 16],
        generation: String,
        cursor: u32,
    },
    MoveWorkspace {
        epoch: [u8; 16],
        generation: String,
        token: String,
        displayed_revision: String,
        producer_session: [u8; 16],
        expected: crate::workspace_move::MoveWorkspaceRequest,
    },
    MaximizeWindow {
        epoch: [u8; 16],
        generation: String,
        token: String,
        displayed_revision: String,
        producer_session: [u8; 16],
        expected: crate::vimarchy_action::MaximizeRequest,
    },
    FocusWindow {
        epoch: [u8; 16],
        generation: String,
        token: String,
        displayed_revision: String,
        producer_session: [u8; 16],
    },
    Acquire {
        ttl_ms: u32,
    },
    Renew {
        epoch: [u8; 16],
        generation: String,
        ttl_ms: u32,
    },
    Release {
        epoch: [u8; 16],
        generation: String,
    },
    Execute {
        epoch: [u8; 16],
        generation: String,
        effect: WireEffect,
    },
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireEffect {
    EnterVimarchy,
    EnterVimarchyDouble,
    EnterAsk,
    EnterYoohoo,
    Reset,
}
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    ListTargets {
        fence: Fence,
        cursor: usize,
    },
    MoveWorkspace {
        fence: Fence,
        intent: crate::targets::FocusIntent,
        expected: crate::workspace_move::MoveWorkspaceRequest,
    },
    MaximizeWindow {
        fence: Fence,
        intent: crate::targets::FocusIntent,
        expected: crate::vimarchy_action::MaximizeRequest,
    },
    FocusWindow {
        fence: Fence,
        intent: crate::targets::FocusIntent,
    },
    Acquire {
        ttl: u64,
    },
    Renew {
        fence: Fence,
        ttl: u64,
    },
    Release {
        fence: Fence,
    },
    Execute {
        fence: Fence,
        effect: Effect,
    },
}
#[derive(Debug, PartialEq, Eq)]
pub struct InvalidFrame;
fn counter(value: &str) -> Result<NonZeroU64, InvalidFrame> {
    if value.starts_with('0') || value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(InvalidFrame);
    }
    value
        .parse()
        .ok()
        .and_then(NonZeroU64::new)
        .ok_or(InvalidFrame)
}
fn ttl(value: u32) -> Result<u64, InvalidFrame> {
    if value == 0 || value > 5000 {
        Err(InvalidFrame)
    } else {
        Ok(value as u64)
    }
}
pub fn decode(bytes: &[u8], owner: Owner) -> Result<(RequestKey, Action), InvalidFrame> {
    if bytes.len() > crate::socket::MAX_FRAME {
        return Err(InvalidFrame);
    }
    let envelope: Envelope = serde_json::from_slice(bytes).map_err(|_| InvalidFrame)?;
    if envelope.version != 1 {
        return Err(InvalidFrame);
    }
    let key = RequestKey {
        owner,
        request_id: counter(&envelope.request_id)?,
    };
    let fence = |epoch, generation: &str| -> Result<Fence, InvalidFrame> {
        Ok(Fence {
            owner,
            authority_epoch: AuthorityEpoch(epoch),
            generation: counter(generation)?,
        })
    };
    let action = match envelope.command {
        WireCommand::ListTargets {
            epoch,
            generation,
            cursor,
        } => {
            if cursor as usize > crate::targets::MAX_TARGETS {
                return Err(InvalidFrame);
            }
            Action::ListTargets {
                fence: fence(epoch, &generation)?,
                cursor: cursor as usize,
            }
        }
        WireCommand::MoveWorkspace {
            epoch,
            generation,
            token,
            displayed_revision,
            producer_session,
            expected,
        } => {
            if !expected.valid() {
                return Err(InvalidFrame);
            }
            Action::MoveWorkspace {
                fence: fence(epoch, &generation)?,
                intent: crate::targets::FocusIntent {
                    token: crate::targets::Token::parse(&token).map_err(|_| InvalidFrame)?,
                    displayed_revision: counter(&displayed_revision)?.get(),
                    producer_session,
                },
                expected,
            }
        }
        WireCommand::MaximizeWindow {
            epoch,
            generation,
            token,
            displayed_revision,
            producer_session,
            expected,
        } => {
            if !expected.valid() {
                return Err(InvalidFrame);
            }
            Action::MaximizeWindow {
                fence: fence(epoch, &generation)?,
                intent: crate::targets::FocusIntent {
                    token: crate::targets::Token::parse(&token).map_err(|_| InvalidFrame)?,
                    displayed_revision: counter(&displayed_revision)?.get(),
                    producer_session,
                },
                expected,
            }
        }
        WireCommand::FocusWindow {
            epoch,
            generation,
            token,
            displayed_revision,
            producer_session,
        } => Action::FocusWindow {
            fence: fence(epoch, &generation)?,
            intent: crate::targets::FocusIntent {
                token: crate::targets::Token::parse(&token).map_err(|_| InvalidFrame)?,
                displayed_revision: counter(&displayed_revision)?.get(),
                producer_session,
            },
        },
        WireCommand::Acquire { ttl_ms } => Action::Acquire { ttl: ttl(ttl_ms)? },
        WireCommand::Renew {
            epoch,
            generation,
            ttl_ms,
        } => Action::Renew {
            fence: fence(epoch, &generation)?,
            ttl: ttl(ttl_ms)?,
        },
        WireCommand::Release { epoch, generation } => Action::Release {
            fence: fence(epoch, &generation)?,
        },
        WireCommand::Execute {
            epoch,
            generation,
            effect,
        } => Action::Execute {
            fence: fence(epoch, &generation)?,
            effect: match effect {
                WireEffect::EnterVimarchy => Effect::EnterVimarchy,
                WireEffect::EnterVimarchyDouble => Effect::EnterVimarchyDouble,
                WireEffect::EnterAsk => Effect::EnterAsk,
                WireEffect::EnterYoohoo => Effect::EnterYoohoo,
                WireEffect::Reset => Effect::Reset,
            },
        },
    };
    Ok((key, action))
}
#[cfg(test)]
mod tests {
    use super::*;
    fn owner() -> Owner {
        Owner {
            client: modal_contract::ClientId([1; 16]),
            client_epoch: NonZeroU64::new(1).unwrap(),
        }
    }
    #[test]
    fn counters_are_lossless_and_identity_is_connection_bound() {
        let (key, action) = decode(br#"{"version":1,"request_id":"18446744073709551615","command":{"op":"acquire","ttl_ms":10}}"#, owner()).unwrap();
        assert_eq!(key.request_id.get(), u64::MAX);
        assert_eq!(key.owner, owner());
        assert_eq!(action, Action::Acquire { ttl: 10 });
    }
    #[test]
    fn malformed_replayed_fields_and_untrusted_owner_are_rejected() {
        for frame in [
            r#"{"version":1,"version":1,"request_id":"1","command":{"op":"acquire","ttl_ms":10}}"#,
            r#"{"version":1,"request_id":"1","owner":"someone_else","command":{"op":"acquire","ttl_ms":10}}"#,
            r#"{"version":1,"request_id":"01","command":{"op":"acquire","ttl_ms":10}}"#,
            r#"{"version":1,"request_id":"1","command":{"op":"acquire","ttl_ms":10,"ttl_ms":20}}"#,
            r#"{"version":1,"request_id":"1","command":{"op":"shell","argv":[]}}"#,
        ] {
            assert!(decode(frame.as_bytes(), owner()).is_err());
        }
        assert!(decode(&vec![b' '; 4097], owner()).is_err());
    }
}
