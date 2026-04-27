use devbridge_core::job::JobState;
use devbridge_core::job_event::PrintStage;
use std::collections::VecDeque;
use tokio::sync::watch;

const MAX_RECENT_JOBS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconState {
    Green,  // idle, OK
    Yellow, // printing in progress
    Red,    // last job failed
    Gray,   // service offline
}

#[derive(Debug, Clone)]
pub struct RecentJob {
    pub job_id: String,
    pub target_printer: String,
    pub status: JobDisplayStatus,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobDisplayStatus {
    InProgress,
    Completed,
    Failed,
}

/// Per-event filter for the tray. The Pending default is critical:
/// on a server-mode tray, we MUST drop all events until detection
/// proves who this RDP session belongs to. Returning the wrong answer
/// here leaks other users' print notifications (issue #42).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterState {
    /// Detection has not yet succeeded. Drop ALL events. Default at startup.
    Pending,
    /// Client mode confirmed — no filtering, pass all events.
    Disabled,
    /// Server mode confirmed — pass only events from this user.
    User(String),
}

pub struct JobTracker {
    pub recent_jobs: VecDeque<RecentJob>,
    pub icon_state: IconState,
    filter_tx: watch::Sender<FilterState>,
}

impl JobTracker {
    /// Create a new tracker. Starts with Gray icon, empty job list,
    /// and `FilterState::Pending` (drops all events until set otherwise).
    pub fn new() -> Self {
        let (filter_tx, _rx) = watch::channel(FilterState::Pending);
        Self {
            recent_jobs: VecDeque::new(),
            icon_state: IconState::Gray,
            filter_tx,
        }
    }

    /// Subscribe to filter state changes. Used by `fetch_initial_jobs`
    /// to wait until detection completes before querying the API.
    pub fn filter_subscribe(&self) -> watch::Receiver<FilterState> {
        self.filter_tx.subscribe()
    }

    /// Read the current filter state (cheap clone). Test-only — production
    /// code uses `filter_subscribe()` to wait on transitions instead.
    #[cfg(test)]
    pub fn filter_state(&self) -> FilterState {
        self.filter_tx.borrow().clone()
    }

    /// Set the filter state from the detector loop. Notifies all subscribers.
    pub fn set_filter_state(&mut self, state: FilterState) {
        // send() returns Err only if all receivers dropped — harmless here.
        let _ = self.filter_tx.send(state);
    }

    /// Returns true if the event passes the filter.
    /// `Pending` drops everything (fail-closed). `Disabled` passes everything.
    /// `User(u)` passes only events from `u` (case-insensitive).
    pub fn should_process(&self, requesting_user: &Option<String>) -> bool {
        match &*self.filter_tx.borrow() {
            FilterState::Pending => false,
            FilterState::Disabled => true,
            FilterState::User(filter) => match requesting_user {
                None => false,
                Some(user) => user.eq_ignore_ascii_case(filter),
            },
        }
    }

    /// Track a newly created job. Adds to front of deque, caps at MAX_RECENT_JOBS.
    /// Sets icon to Yellow (printing in progress). Uses the current time.
    pub fn on_job_created(&mut self, job_id: String, target_printer: String) {
        self.add_job(job_id, target_printer, chrono::Utc::now(), JobDisplayStatus::InProgress);
        self.icon_state = IconState::Yellow;
    }

    /// Add a historical job with a specific timestamp and status.
    /// Used by `fetch_initial_jobs` to populate the menu from the API on
    /// startup with the real created_at timestamps so the "ago" display
    /// is accurate, not all "just now".
    pub fn add_job(
        &mut self,
        job_id: String,
        target_printer: String,
        timestamp: chrono::DateTime<chrono::Utc>,
        status: JobDisplayStatus,
    ) {
        let job = RecentJob {
            job_id,
            target_printer,
            status,
            timestamp,
        };
        self.recent_jobs.push_front(job);
        if self.recent_jobs.len() > MAX_RECENT_JOBS {
            self.recent_jobs.pop_back();
        }
    }

    /// Update a tracked job based on a print stage event.
    /// Completed → Green icon, Failed → Red icon. Non-terminal stages keep Yellow.
    pub fn on_print_event(&mut self, job_id: &str, stage: PrintStage, success: bool) {
        if let Some(job) = self.recent_jobs.iter_mut().find(|j| j.job_id == job_id) {
            if stage == PrintStage::Completed && success {
                job.status = JobDisplayStatus::Completed;
                self.icon_state = IconState::Green;
            } else if stage == PrintStage::Failed || !success {
                job.status = JobDisplayStatus::Failed;
                self.icon_state = IconState::Red;
            }
        }
    }

    /// Update a tracked job based on a JobState change event.
    /// This is the AUTHORITATIVE source for completion since the server's
    /// state machine fires StateChanged for every transition (the per-stage
    /// PrintJobEvents from the client are stored in DB but not all of them
    /// reach the WebSocket consistently). Returns the new display status if
    /// the job transitioned to a terminal state (Completed/Failed/Cancelled),
    /// so the caller can show a notification.
    pub fn on_state_changed(&mut self, job_id: &str, state: JobState) -> Option<JobDisplayStatus> {
        if let Some(job) = self.recent_jobs.iter_mut().find(|j| j.job_id == job_id) {
            match state {
                JobState::Completed => {
                    job.status = JobDisplayStatus::Completed;
                    self.icon_state = IconState::Green;
                    return Some(JobDisplayStatus::Completed);
                }
                JobState::Failed | JobState::Cancelled => {
                    job.status = JobDisplayStatus::Failed;
                    self.icon_state = IconState::Red;
                    return Some(JobDisplayStatus::Failed);
                }
                _ => {
                    // Queued / Downloading / Printing → still in progress
                }
            }
        }
        None
    }

    /// Update online/offline state. Gray if offline.
    /// If coming online and currently Gray, set Green.
    /// If currently Red/Yellow (from a job event), preserve that state.
    pub fn set_online(&mut self, online: bool) {
        if online {
            if self.icon_state == IconState::Gray {
                self.icon_state = IconState::Green;
            }
        } else {
            self.icon_state = IconState::Gray;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_tracker_starts_gray() {
        let tracker = JobTracker::new();
        assert_eq!(tracker.icon_state, IconState::Gray);
        assert!(tracker.recent_jobs.is_empty());
    }

    #[test]
    fn job_created_sets_yellow() {
        let mut tracker = JobTracker::new();
        tracker.set_online(true);
        tracker.on_job_created("job-1".into(), "pjsnvs printer".into());
        assert_eq!(tracker.icon_state, IconState::Yellow);
        assert_eq!(tracker.recent_jobs.len(), 1);
        assert_eq!(tracker.recent_jobs[0].job_id, "job-1");
        assert_eq!(tracker.recent_jobs[0].target_printer, "pjsnvs printer");
        assert_eq!(tracker.recent_jobs[0].status, JobDisplayStatus::InProgress);
    }

    #[test]
    fn state_changed_to_completed_marks_done() {
        let mut tracker = JobTracker::new();
        tracker.on_job_created("job-1".into(), "pjsnvs printer".into());
        let result = tracker.on_state_changed("job-1", JobState::Completed);
        assert_eq!(result, Some(JobDisplayStatus::Completed));
        assert_eq!(tracker.icon_state, IconState::Green);
        assert_eq!(tracker.recent_jobs[0].status, JobDisplayStatus::Completed);
    }

    #[test]
    fn state_changed_to_failed_marks_failed() {
        let mut tracker = JobTracker::new();
        tracker.on_job_created("job-1".into(), "pjsnvs printer".into());
        let result = tracker.on_state_changed("job-1", JobState::Failed);
        assert_eq!(result, Some(JobDisplayStatus::Failed));
        assert_eq!(tracker.icon_state, IconState::Red);
        assert_eq!(tracker.recent_jobs[0].status, JobDisplayStatus::Failed);
    }

    #[test]
    fn state_changed_intermediate_returns_none() {
        let mut tracker = JobTracker::new();
        tracker.on_job_created("job-1".into(), "pjsnvs printer".into());
        assert_eq!(tracker.on_state_changed("job-1", JobState::Printing), None);
        assert_eq!(tracker.recent_jobs[0].status, JobDisplayStatus::InProgress);
    }

    /// Regression test for the bug where the tray got stuck on "printing"
    /// because it ignored JobEvent::StateChanged and only watched for
    /// PrintJobEvent::Completed which never reached it. This test simulates
    /// the WebSocket event sequence the tray actually receives in production:
    ///   1. JobEvent::Created (server emits when IPP job arrives)
    ///   2. JobEvent::StateChanged(Printing) (server emits when client picks up)
    ///   3. JobEvent::StateChanged(Completed) (server emits when client finishes)
    /// After step 3 the job MUST show as Completed, not stuck on InProgress.
    #[test]
    fn full_lifecycle_created_printing_completed() {
        let mut tracker = JobTracker::new();

        // 1. Job created on the server
        tracker.on_job_created("job-abc".into(), "pjsnvs printer".into());
        assert_eq!(tracker.recent_jobs[0].status, JobDisplayStatus::InProgress);
        assert_eq!(tracker.icon_state, IconState::Yellow);

        // 2. Job state changes to printing — still in progress
        let r1 = tracker.on_state_changed("job-abc", JobState::Printing);
        assert_eq!(r1, None);
        assert_eq!(tracker.recent_jobs[0].status, JobDisplayStatus::InProgress);

        // 3. Job state changes to completed — MUST update tracker to done.
        // This is the critical assertion the original code failed because it
        // ignored StateChanged events expecting PrintJobEvent which never came.
        let r2 = tracker.on_state_changed("job-abc", JobState::Completed);
        assert_eq!(r2, Some(JobDisplayStatus::Completed));
        assert_eq!(tracker.recent_jobs[0].status, JobDisplayStatus::Completed);
        assert_eq!(tracker.icon_state, IconState::Green);
    }

    /// Regression test for the failure path: created → failed.
    #[test]
    fn full_lifecycle_created_failed() {
        let mut tracker = JobTracker::new();
        tracker.on_job_created("job-fail".into(), "pjpop printer".into());

        let r = tracker.on_state_changed("job-fail", JobState::Failed);
        assert_eq!(r, Some(JobDisplayStatus::Failed));
        assert_eq!(tracker.recent_jobs[0].status, JobDisplayStatus::Failed);
        assert_eq!(tracker.icon_state, IconState::Red);
    }

    #[test]
    fn job_completed_sets_green() {
        let mut tracker = JobTracker::new();
        tracker.set_online(true);
        tracker.on_job_created("job-1".into(), "test.pdf".into());
        tracker.on_print_event("job-1", PrintStage::Completed, true);
        assert_eq!(tracker.icon_state, IconState::Green);
        assert_eq!(tracker.recent_jobs[0].status, JobDisplayStatus::Completed);
    }

    #[test]
    fn job_failed_sets_red() {
        let mut tracker = JobTracker::new();
        tracker.set_online(true);
        tracker.on_job_created("job-1".into(), "test.pdf".into());
        tracker.on_print_event("job-1", PrintStage::Failed, false);
        assert_eq!(tracker.icon_state, IconState::Red);
        assert_eq!(tracker.recent_jobs[0].status, JobDisplayStatus::Failed);
    }

    #[test]
    fn max_5_recent_jobs() {
        let mut tracker = JobTracker::new();
        for i in 0..7 {
            tracker.on_job_created(format!("job-{i}"), format!("doc-{i}.pdf"));
        }
        assert_eq!(tracker.recent_jobs.len(), 5);
        // Most recent first
        assert_eq!(tracker.recent_jobs[0].job_id, "job-6");
        assert_eq!(tracker.recent_jobs[1].job_id, "job-5");
        assert_eq!(tracker.recent_jobs[4].job_id, "job-2");
    }

    #[test]
    fn set_online_preserves_red() {
        let mut tracker = JobTracker::new();
        tracker.set_online(true);
        tracker.on_job_created("job-1".into(), "test.pdf".into());
        tracker.on_print_event("job-1", PrintStage::Failed, false);
        assert_eq!(tracker.icon_state, IconState::Red);
        // Coming online should NOT reset Red to Green
        tracker.set_online(true);
        assert_eq!(tracker.icon_state, IconState::Red);
    }

    #[test]
    fn set_online_transitions() {
        let mut tracker = JobTracker::new();
        assert_eq!(tracker.icon_state, IconState::Gray);
        // Gray → online → Green
        tracker.set_online(true);
        assert_eq!(tracker.icon_state, IconState::Green);
        // Green → offline → Gray
        tracker.set_online(false);
        assert_eq!(tracker.icon_state, IconState::Gray);
    }

    // -- FilterState tests (issue #42) ---------------------------------

    #[test]
    fn pending_drops_all_events_with_user() {
        let tracker = JobTracker::new();
        // Default state is Pending — must drop everything until detection completes.
        assert!(!tracker.should_process(&Some("anyone".into())));
    }

    #[test]
    fn pending_drops_all_events_without_user() {
        let tracker = JobTracker::new();
        assert!(!tracker.should_process(&None));
    }

    #[test]
    fn disabled_passes_all_events() {
        let mut tracker = JobTracker::new();
        tracker.set_filter_state(FilterState::Disabled);
        assert!(tracker.should_process(&Some("alice".into())));
        assert!(tracker.should_process(&None));
    }

    #[test]
    fn user_state_matches_case_insensitive() {
        let mut tracker = JobTracker::new();
        tracker.set_filter_state(FilterState::User("Admin".into()));
        assert!(tracker.should_process(&Some("admin".into())));
        assert!(tracker.should_process(&Some("ADMIN".into())));
        assert!(tracker.should_process(&Some("Admin".into())));
        assert!(!tracker.should_process(&Some("other_user".into())));
    }

    #[test]
    fn user_state_drops_none_requesting_user() {
        let mut tracker = JobTracker::new();
        tracker.set_filter_state(FilterState::User("admin".into()));
        assert!(!tracker.should_process(&None));
    }

    #[test]
    fn filter_subscribe_yields_current_state() {
        let mut tracker = JobTracker::new();
        let mut rx = tracker.filter_subscribe();
        assert!(matches!(*rx.borrow(), FilterState::Pending));
        tracker.set_filter_state(FilterState::Disabled);
        rx.borrow_and_update();
        assert!(matches!(*rx.borrow(), FilterState::Disabled));
    }
}
