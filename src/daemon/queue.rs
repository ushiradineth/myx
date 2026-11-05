//! Private native queue storage. IPC copies only a bounded, revision-checked page.
use std::collections::HashMap;

use crate::domain::{QueueSnapshot, Snapshot};
use crate::ipc::protocol::{ErrorCode, IpcError, QueuePage};

#[derive(Default)]
pub(super) struct NativeQueue {
    revision: u64,
    queue: QueueSnapshot,
    titles: HashMap<String, String>,
}

impl NativeQueue {
    pub fn observe(&mut self, mut queue: QueueSnapshot) {
        queue.complete_catalog_context = false;
        if self.queue != queue {
            self.revision = self.revision.saturating_add(1);
            self.queue = queue;
        }
    }

    pub fn prefix(&self, snapshot: &mut Snapshot) {
        snapshot.queue_revision = self.revision;
        snapshot.queue_previous_total = self.queue.previous.len();
        snapshot.queue_upcoming_total = self.queue.upcoming.len();
        let bounded = |entries: &[crate::domain::QueueEntry]| {
            let mut budget = 32 * 1024;
            entries
                .iter()
                .take(10)
                .take_while(|entry| {
                    let bytes = serde_json::to_vec(entry).map_or(usize::MAX, |value| value.len());
                    if bytes > budget {
                        false
                    } else {
                        budget -= bytes;
                        true
                    }
                })
                .cloned()
                .collect()
        };
        snapshot.queue = QueueSnapshot {
            previous: bounded(&self.queue.previous),
            upcoming: bounded(&self.queue.upcoming),
            complete_catalog_context: false,
        };
    }

    pub fn page(
        &self,
        revision: Option<u64>,
        offset: usize,
        limit: u8,
        previous: bool,
    ) -> Result<QueuePage, IpcError> {
        if limit == 0 || limit > 50 {
            return Err(IpcError::new(
                ErrorCode::InvalidRequest,
                "Queue page limit must be 1..=50",
            ));
        }
        if revision.is_some_and(|revision| revision != self.revision) {
            return Err(IpcError::new(
                ErrorCode::CollectionChanged,
                "Native queue changed; request its first page",
            ));
        }
        let source = if previous {
            &self.queue.previous
        } else {
            &self.queue.upcoming
        };
        let mut entries: Vec<_> = source
            .iter()
            .skip(offset)
            .take(usize::from(limit))
            .cloned()
            .collect();
        for entry in &mut entries {
            if entry.title.is_none() {
                entry.title = self.titles.get(&entry.uri).cloned();
            }
        }
        let page = QueuePage {
            session_id: String::new(),
            auth_revision: 0,
            revision: self.revision,
            offset,
            total: source.len(),
            previous,
            entries,
            complete_catalog_context: false,
        };
        if serde_json::to_vec(&page).map_or(true, |bytes| {
            bytes.len() > crate::ipc::protocol::MAX_FRAME - 4096
        }) {
            return Err(IpcError::new(
                ErrorCode::InvalidRequest,
                "Native queue page exceeds frame budget; request fewer entries",
            ));
        }
        Ok(page)
    }

    pub fn remember(&mut self, uri: String, title: String) {
        // Metadata is a bounded owner cache, not a reason to walk the whole queue.
        if self.titles.len() >= 512 {
            self.titles.clear();
        }
        self.titles.insert(uri, title);
    }

    pub fn clear_authority(&mut self) -> u64 {
        self.revision = self.revision.saturating_add(1);
        self.queue = QueueSnapshot::default();
        self.titles.clear();
        self.revision
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::queue::{QueueEntry, QueueSource};

    #[test]
    fn huge_native_queue_has_small_status_and_exact_revision_pages() {
        let entries: Vec<_> = (0..100_000)
            .map(|i| QueueEntry {
                uid: format!("uid-{i}"),
                source: QueueSource::Autoplay,
                uri: "spotify:track:0123456789abcdefghijkl".into(),
                title: None,
            })
            .collect();
        let mut queue = NativeQueue::default();
        queue.observe(QueueSnapshot {
            upcoming: entries.clone(),
            previous: vec![entries[1].clone()],
            complete_catalog_context: true,
        });
        let mut snapshot = Snapshot::starting("/config".into(), "myx".into());
        queue.prefix(&mut snapshot);
        assert_eq!(snapshot.queue.upcoming.len(), 10);
        assert_eq!(snapshot.queue_upcoming_total, 100_000);
        assert!(!snapshot.queue.complete_catalog_context);
        assert!(serde_json::to_vec(&snapshot).unwrap().len() < crate::ipc::protocol::MAX_FRAME);
        let page = queue
            .page(Some(snapshot.queue_revision), 5106, 50, false)
            .unwrap();
        assert_eq!(page.entries, entries[5106..5156]);
        assert!(!page.complete_catalog_context);
        assert!(queue.page(None, 0, 51, false).is_err());
        queue.observe(QueueSnapshot::default());
        assert_eq!(
            queue
                .page(Some(page.revision), 5156, 50, false)
                .unwrap_err()
                .code,
            ErrorCode::CollectionChanged
        );
        // An already copied page is stable even when a slow client hasn't read it.
        assert_eq!(page.entries[0].uid, "uid-5106");
    }
}
