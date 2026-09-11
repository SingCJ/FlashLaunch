use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

use rayon::{ThreadPool, ThreadPoolBuilder};

use crate::*;

pub(crate) struct SearchWorker {
    mailbox: Arc<SearchMailbox>,
    handle: Option<JoinHandle<()>>,
}

#[derive(Clone)]
pub(crate) struct SearchWorkerRequest {
    pub(crate) generation: u64,
    pub(crate) hwnd_value: isize,
    pub(crate) spec: SearchQuerySpec,
    pub(crate) root_plan: Arc<RootOwnershipPlan>,
    pub(crate) scoring: Arc<ScoringConfig>,
    pub(crate) recent_items: Arc<Vec<String>>,
    pub(crate) query_launch_rules: Arc<Vec<QueryLaunchRule>>,
    pub(crate) effective_limit: usize,
    pub(crate) search_threads: usize,
    pub(crate) include_score_detail: bool,
    pub(crate) include_explanation: bool,
}

enum SearchWorkerCommand {
    Search(Box<SearchWorkerRequest>),
    InvalidateSession,
}

#[derive(Default)]
struct SearchMailboxState {
    request: Option<Box<SearchWorkerRequest>>,
    invalidate_session: bool,
    shutdown: bool,
}

#[derive(Default)]
struct SearchMailbox {
    state: Mutex<SearchMailboxState>,
    changed: Condvar,
}

impl SearchMailbox {
    fn submit(&self, request: SearchWorkerRequest) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.shutdown {
            return;
        }
        state.request = Some(Box::new(request));
        self.changed.notify_one();
    }

    fn invalidate_session(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.shutdown {
            return;
        }
        state.invalidate_session = true;
        state.request = None;
        self.changed.notify_one();
    }

    fn take(&self) -> Option<SearchWorkerCommand> {
        let mut state = self.state.lock().ok()?;
        loop {
            if state.shutdown {
                state.request = None;
                state.invalidate_session = false;
                return None;
            }
            if state.invalidate_session {
                state.invalidate_session = false;
                return Some(SearchWorkerCommand::InvalidateSession);
            }
            if let Some(request) = state.request.take() {
                return Some(SearchWorkerCommand::Search(request));
            }
            state = self.changed.wait(state).ok()?;
        }
    }

    fn shutdown(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.shutdown = true;
            state.request = None;
            state.invalidate_session = false;
            self.changed.notify_all();
        }
    }
}

impl SearchWorker {
    pub(crate) fn new(search_threads: usize) -> Self {
        let mailbox = Arc::new(SearchMailbox::default());
        let worker_mailbox = Arc::clone(&mailbox);
        let handle = thread::Builder::new()
            .name("flashlaunch-search-coordinator".to_string())
            .spawn(move || run_search_worker(worker_mailbox, search_threads))
            .ok();
        Self { mailbox, handle }
    }

    pub(crate) fn search(&self, request: SearchWorkerRequest) {
        self.mailbox.submit(request);
    }

    pub(crate) fn invalidate_session(&self) {
        self.mailbox.invalidate_session();
    }

    pub(crate) fn request_shutdown(&mut self) {
        self.mailbox.shutdown();
    }

    pub(crate) fn join(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for SearchWorker {
    fn drop(&mut self) {
        self.request_shutdown();
        self.join();
    }
}

fn build_scan_pool(search_threads: usize) -> ThreadPool {
    let search_threads = search_threads.clamp(MIN_SEARCH_THREADS, available_search_threads());
    ThreadPoolBuilder::new()
        .num_threads(search_threads)
        .thread_name(|index| format!("flashlaunch-scan-{index}"))
        .build()
        .expect("failed to create search scan pool")
}

fn run_search_worker(mailbox: Arc<SearchMailbox>, initial_search_threads: usize) {
    set_current_thread_background_priority();
    let mut pool_size =
        initial_search_threads.clamp(MIN_SEARCH_THREADS, available_search_threads());
    let mut scan_pool = build_scan_pool(pool_size);
    let mut refinement_session: Option<RefinementSession> = None;
    while let Some(command) = mailbox.take() {
        let SearchWorkerCommand::Search(request) = command else {
            if let Some(mut session) = refinement_session.take() {
                session.shutdown();
            }
            continue;
        };
        if ACTIVE_SEARCH_GENERATION.load(Ordering::Relaxed) != request.generation {
            continue;
        }
        if request.spec.mode == SearchQueryMode::NormalSearch {
            if refinement_session
                .as_ref()
                .is_some_and(|session| session.can_refine(&request))
                && refinement_session
                    .as_ref()
                    .is_some_and(|session| session.update((*request).clone()))
            {
                continue;
            }
            if let Some(mut session) = refinement_session.take() {
                session.shutdown();
            }
            refinement_session = Some(RefinementSession::start(*request));
            continue;
        }
        if let Some(mut session) = refinement_session.take() {
            session.shutdown();
        }
        let SearchWorkerRequest {
            generation,
            hwnd_value,
            spec,
            root_plan,
            scoring,
            recent_items,
            query_launch_rules,
            effective_limit,
            search_threads,
            include_score_detail,
            include_explanation,
        } = *request;
        let requested_pool_size =
            search_threads.clamp(MIN_SEARCH_THREADS, available_search_threads());
        if requested_pool_size != pool_size {
            scan_pool = build_scan_pool(requested_pool_size);
            pool_size = requested_pool_size;
        }
        search_streaming(
            generation,
            hwnd_value,
            spec,
            root_plan,
            scoring,
            recent_items,
            query_launch_rules,
            effective_limit,
            &scan_pool,
            include_score_detail,
            include_explanation,
        );
    }
    if let Some(mut session) = refinement_session {
        session.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(generation: u64, query: &str) -> SearchWorkerRequest {
        SearchWorkerRequest {
            generation,
            hwnd_value: 0,
            spec: parse_search_query(query),
            root_plan: Arc::new(RootOwnershipPlan::default()),
            scoring: Arc::new(ScoringConfig::default()),
            recent_items: Arc::new(Vec::new()),
            query_launch_rules: Arc::new(Vec::new()),
            effective_limit: DEFAULT_RESULT_LIMIT,
            search_threads: MIN_SEARCH_THREADS,
            include_score_detail: true,
            include_explanation: true,
        }
    }

    #[test]
    fn mailbox_keeps_only_newest_search_request() {
        let mailbox = SearchMailbox::default();
        mailbox.submit(request(1, "first"));
        mailbox.submit(request(2, "second"));
        mailbox.submit(request(3, "third"));

        let SearchWorkerCommand::Search(selected) = mailbox.take().unwrap() else {
            panic!("expected search command");
        };

        assert_eq!(selected.generation, 3);
        assert_eq!(selected.spec.raw, "third");
    }

    #[test]
    fn mailbox_invalidation_discards_pending_search() {
        let mailbox = SearchMailbox::default();
        mailbox.submit(request(1, "first"));
        mailbox.invalidate_session();

        assert!(matches!(
            mailbox.take(),
            Some(SearchWorkerCommand::InvalidateSession)
        ));
    }

    #[test]
    fn mailbox_preserves_invalidation_before_later_search() {
        let mailbox = SearchMailbox::default();
        mailbox.invalidate_session();
        mailbox.submit(request(2, "second"));

        assert!(matches!(
            mailbox.take(),
            Some(SearchWorkerCommand::InvalidateSession)
        ));
        let SearchWorkerCommand::Search(selected) = mailbox.take().unwrap() else {
            panic!("expected search command");
        };
        assert_eq!(selected.generation, 2);
    }

    #[test]
    fn mailbox_shutdown_discards_pending_search() {
        let mailbox = SearchMailbox::default();
        mailbox.submit(request(1, "first"));
        mailbox.shutdown();

        assert!(mailbox.take().is_none());
    }
}
