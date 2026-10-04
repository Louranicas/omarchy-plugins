//! Narrow modal runtime primitives. Native revocation proof remains an adapter obligation.
pub mod actor;
pub mod backend;
pub mod host;
pub mod presenter;
pub mod protocol;
pub mod socket;
pub mod targets;

pub mod native;

pub mod readiness;

mod suspend;

pub mod process_identity;

pub mod configured;

pub mod vimarchy_action;
pub mod vimarchy_policy;

pub mod workspace_move;
