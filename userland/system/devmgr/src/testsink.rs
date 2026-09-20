//! The crate's one recording [`Sink`] for tests.
//!
//! Several test modules need to read back what an audit record actually
//! carried; a copy per module drifts, and three of them had already narrowed
//! to ids only, which cannot check a field.

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::RefCell;

use tairix_log::{Event, Sink};

/// One captured record: its id and its rendered fields.
struct Captured {
    id: u32,
    fields: Vec<(String, String)>,
}

/// A [`Sink`] that keeps every record it was handed.
#[derive(Default)]
pub(crate) struct RecordingSink {
    events: RefCell<Vec<Captured>>,
}

impl RecordingSink {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Every record's id, in the order they were emitted.
    pub(crate) fn ids(&self) -> Vec<u32> {
        self.events.borrow().iter().map(|e| e.id).collect()
    }

    /// The rendered value of `key` on the first record with `id`.
    pub(crate) fn field_of(&self, id: u32, key: &str) -> Option<String> {
        self.events
            .borrow()
            .iter()
            .find(|e| e.id == id)
            .and_then(|e| e.fields.iter().find(|(k, _)| k == key))
            .map(|(_, v)| v.clone())
    }
}

impl Sink for RecordingSink {
    fn write_event(&self, event: &Event<'_>) {
        self.events.borrow_mut().push(Captured {
            id: event.id.0,
            fields: event
                .fields
                .iter()
                .map(|f| (f.key.to_string(), f.value.to_string()))
                .collect(),
        });
    }
}
