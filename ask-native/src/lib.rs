//! Bounded UI command/snapshot contracts; callbacks carry rendered permission identity.
pub mod backend;
pub mod backend_status;
pub mod fixture;
use ask_core::permission::{Kind, Offered};
use ask_core::{Id, RequestId, Text};
use ask_runtime::{Event, Instance};
use std::sync::{Arc, Mutex};
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Permission {
    pub instance: Instance,
    pub generation: u64,
    pub epoch: u64,
    pub request: RequestId,
    pub title: Text,
    pub options: Vec<Offered>,
}
impl Permission {
    pub fn from_event(event: Event) -> Option<Self> {
        if let Event::Permission {
            instance,
            generation,
            epoch,
            request,
            title,
            options,
        } = event
        {
            Some(Self {
                instance,
                generation,
                epoch,
                request,
                title,
                options,
            })
        } else {
            None
        }
    }
    pub fn choice(&self, allow: bool) -> Option<Id> {
        let kind = if allow {
            Kind::AllowOnce
        } else {
            Kind::RejectOnce
        };
        self.options
            .iter()
            .find(|o| o.kind == kind)
            .map(|o| o.id.clone())
    }
}
/// Complete latest snapshots may replace stale ones; individual permission events may not.
pub struct Latest<T>(Arc<Mutex<Option<T>>>);
impl<T> Clone for Latest<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<T> Default for Latest<T> {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(None)))
    }
}
impl<T> Latest<T> {
    pub fn publish(&self, value: T) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(value)
    }
    pub fn take(&self) -> Option<T> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

pub mod files;
