//! Pure, bounded domain logic. This crate never executes desktop commands.
#![forbid(unsafe_code)]

pub mod allocation;
pub mod snapshot;

pub mod gestures;

pub mod actions;

pub mod presentation;
