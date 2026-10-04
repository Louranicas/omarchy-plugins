//! One bounded file worker; every result carries the initiating UI generation.
use crate::Latest;
use ask_files::{Match, Mode, Request};
use ask_preview::Preview;
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, SyncSender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
/// An opaque selection issued by one worker for one displayed search generation.
#[derive(Clone)]
pub struct Selection {
    owner: Arc<()>,
    generation: u64,
    selected: Match,
}
impl Selection {
    pub fn path(&self) -> &std::path::Path {
        &self.selected.path
    }
}
pub struct SearchResults {
    pub rows: Vec<Selection>,
    pub complete: bool,
}
pub enum Kind {
    Search(String),
    Preview(Selection),
}
struct Job {
    generation: u64,
    kind: Kind,
    cancel: Arc<AtomicBool>,
}
pub enum Result {
    Search(SearchResults),
    Preview(Preview),
    Failed(String),
}
pub struct Update {
    pub generation: u64,
    pub result: Result,
}
pub struct Handle {
    owner: Arc<()>,
    tx: SyncSender<Job>,
    pub updates: Latest<Update>,
    latest: Arc<AtomicU64>,
    search_generation: AtomicU64,
    cancel: Mutex<Option<Arc<AtomicBool>>>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}
impl Handle {
    pub fn start(root: PathBuf) -> Self {
        let (tx, rx) = mpsc::sync_channel::<Job>(1);
        let owner = Arc::new(());
        let issuer = owner.clone();
        let updates = Latest::default();
        let output = updates.clone();
        let latest = Arc::new(AtomicU64::new(0));
        let observed = latest.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let join = thread::spawn(move || {
            while !stopping.load(Ordering::Acquire) {
                let Ok(job) = rx.recv_timeout(Duration::from_millis(50)) else {
                    continue;
                };
                if job.cancel.load(Ordering::Acquire)
                    || observed.load(Ordering::Acquire) != job.generation
                {
                    continue;
                }
                let result = match job.kind {
                    Kind::Search(query) => match ask_files::search(
                        &Request {
                            generation: job.generation,
                            query,
                            roots: vec![root.clone()],
                            mode: Mode::Files,
                            repository_depth: 6,
                            max_entries: 5000,
                            max_path_bytes: 4096,
                            timeout: Duration::from_millis(150),
                            result_limit: 100,
                            cross_mounts: false,
                        },
                        &observed,
                    ) {
                        Ok(rows) => Result::Search(SearchResults {
                            complete: rows.complete,
                            rows: rows
                                .rows
                                .into_iter()
                                .map(|selected| Selection {
                                    owner: issuer.clone(),
                                    generation: job.generation,
                                    selected,
                                })
                                .collect(),
                        }),
                        Err(e) => Result::Failed(format!("Search failed: {e:?}")),
                    },
                    Kind::Preview(selected) => {
                        match ask_preview::preview(&selected.selected, &job.cancel) {
                            Ok(preview) => Result::Preview(preview),
                            Err(e) => Result::Failed(format!("Preview failed: {e:?}")),
                        }
                    }
                };
                if !job.cancel.load(Ordering::Acquire)
                    && observed.load(Ordering::Acquire) == job.generation
                {
                    output.publish(Update {
                        generation: job.generation,
                        result,
                    })
                }
            }
        });
        Self {
            owner,
            tx,
            updates,
            latest,
            search_generation: AtomicU64::new(0),
            cancel: Mutex::new(None),
            stop,
            join: Some(join),
        }
    }
    pub fn submit(&self, kind: Kind) -> std::result::Result<u64, &'static str> {
        // Hold the submission lock through validation and enqueue: concurrent
        // commands cannot sneak a stale selection past the generation check.
        let mut previous = self.cancel.lock().unwrap_or_else(|e| e.into_inner());
        if self.stop.load(Ordering::Acquire) {
            return Err("File worker stopped");
        }
        if let Kind::Preview(selection) = &kind
            && (!Arc::ptr_eq(&selection.owner, &self.owner)
                || selection.generation != self.search_generation.load(Ordering::Acquire))
        {
            return Err("Stale or foreign file selection");
        }
        let generation = self
            .latest
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
            .map_err(|_| "File generation exhausted")?
            + 1;
        if matches!(&kind, Kind::Search(_)) {
            self.search_generation.store(generation, Ordering::Release);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        if let Some(old) = previous.replace(cancel.clone()) {
            old.store(true, Ordering::Release)
        }
        self.tx
            .try_send(Job {
                generation,
                kind,
                cancel,
            })
            .map_err(|_| "File worker busy; retry")?;
        Ok(generation)
    }
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        if let Some(c) = self
            .cancel
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            c.store(true, Ordering::Release)
        }
    }
    pub fn finish(mut self) -> bool {
        self.stop();
        self.join.take().is_some_and(|join| join.join().is_ok())
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        self.stop()
    }
}
