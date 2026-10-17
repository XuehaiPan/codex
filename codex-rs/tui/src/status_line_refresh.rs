//! Independent, input-independent refresh deadlines for reporting metadata.
//!
//! Deadlines bound the next attempt, not network completion. Each class has one in-flight request
//! and one coalesced follow-up. Context generations and globally unique request IDs reject ABA
//! responses, including responses queued by a widget that has already been replaced.

use codex_protocol::ThreadId;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::task::AbortOnDropHandle;

pub(crate) const EVENT_COALESCE: Duration = Duration::from_millis(250);
pub(crate) const LOCAL_INTERVAL: Duration = Duration::from_secs(5);
pub(crate) const PR_MIN_INTERVAL: Duration = Duration::from_secs(10);
pub(crate) const PR_INTERVAL: Duration = Duration::from_secs(60);
static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StatusRefreshContext {
    pub(crate) thread_id: ThreadId,
    pub(crate) environment_id: String,
    pub(crate) generation: u64,
    pub(crate) cwd: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StatusProbeToken {
    pub(crate) thread_id: ThreadId,
    pub(crate) environment_id: String,
    pub(crate) generation: u64,
    pub(crate) cwd: PathBuf,
    pub(crate) request_id: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StatusProbeKind {
    LocalGit,
    PullRequest,
}

/// Retiring a request also cancels its task; no second owner needs to mirror pending state.
struct PendingProbe {
    token: StatusProbeToken,
    task: Option<AbortOnDropHandle<()>>,
}

#[derive(Default)]
struct Probe {
    enabled: bool,
    next: Option<Instant>,
    pending: Option<PendingProbe>,
    last_started: Option<Instant>,
    dirty: Option<Instant>,
}

#[derive(Default)]
pub(crate) struct StatusLineRefreshState {
    context: Option<StatusRefreshContext>,
    local: Probe,
    pr: Probe,
}

impl StatusLineRefreshState {
    pub(crate) fn configure(
        &mut self,
        context: StatusRefreshContext,
        local: bool,
        pr: bool,
        now: Instant,
    ) {
        if self.context.as_ref() != Some(&context) {
            self.cancel();
            self.context = Some(context);
        }
        for (probe, enabled) in [(&mut self.local, local), (&mut self.pr, pr)] {
            if probe.enabled != enabled {
                let last_started = probe.last_started;
                *probe = Probe {
                    enabled,
                    last_started,
                    next: enabled.then_some(now),
                    ..Default::default()
                };
            }
        }
        if self.pr.enabled
            && self.pr.pending.is_none()
            && let Some(last) = self.pr.last_started
        {
            self.pr.next = self.pr.next.map(|next| next.max(last + PR_MIN_INTERVAL));
        }
    }

    pub(crate) fn has_dirty(&self, kind: StatusProbeKind) -> bool {
        self.probe(kind).dirty.is_some()
    }

    pub(crate) fn cancel(&mut self) {
        self.context = None;
        self.local = Probe::default();
        self.pr = Probe::default();
    }

    fn probe(&self, kind: StatusProbeKind) -> &Probe {
        match kind {
            StatusProbeKind::LocalGit => &self.local,
            StatusProbeKind::PullRequest => &self.pr,
        }
    }

    fn probe_mut(&mut self, kind: StatusProbeKind) -> &mut Probe {
        match kind {
            StatusProbeKind::LocalGit => &mut self.local,
            StatusProbeKind::PullRequest => &mut self.pr,
        }
    }

    pub(crate) fn pending(&self, kind: StatusProbeKind) -> Option<&StatusProbeToken> {
        self.probe(kind)
            .pending
            .as_ref()
            .map(|pending| &pending.token)
    }

    pub(crate) fn attach_task(&mut self, kind: StatusProbeKind, task: tokio::task::JoinHandle<()>) {
        let task = AbortOnDropHandle::new(task);
        if let Some(pending) = &mut self.probe_mut(kind).pending {
            pending.task = Some(task);
        }
    }

    pub(crate) fn invalidate(&mut self, now: Instant) {
        self.invalidate_probe(StatusProbeKind::LocalGit, now + EVENT_COALESCE);
        self.invalidate_probe(StatusProbeKind::PullRequest, now + EVENT_COALESCE);
    }

    fn invalidate_probe(&mut self, kind: StatusProbeKind, mut deadline: Instant) {
        let probe = self.probe_mut(kind);
        if !probe.enabled {
            return;
        }
        if kind == StatusProbeKind::PullRequest
            && let Some(last) = probe.last_started
        {
            deadline = deadline.max(last + PR_MIN_INTERVAL);
        }
        let next = if probe.pending.is_some() {
            &mut probe.dirty
        } else {
            &mut probe.next
        };
        *next = Some(next.map_or(deadline, |existing| existing.min(deadline)));
    }

    /// A changed branch/HEAD cancels the obsolete PR but retains same-context minimum spacing.
    pub(crate) fn invalidate_pull_request_identity(&mut self, now: Instant) {
        self.pr.pending = None;
        self.pr.dirty = None;
        self.invalidate_probe(StatusProbeKind::PullRequest, now);
    }

    /// A validated lifecycle transition already carries fresh local data for the new context.
    pub(crate) fn seed_local(&mut self, now: Instant) {
        if self.local.enabled {
            self.local.pending = None;
            self.local.dirty = None;
            self.local.last_started = Some(now);
            self.local.next = Some(now + LOCAL_INTERVAL);
        }
    }

    pub(crate) fn due(&mut self, now: Instant) -> Vec<StatusProbeKind> {
        let Some(context) = self.context.clone() else {
            return Vec::new();
        };
        let mut due = Vec::new();
        for kind in [StatusProbeKind::LocalGit, StatusProbeKind::PullRequest] {
            let probe = self.probe_mut(kind);
            if !probe.enabled || probe.pending.is_some() || probe.next.is_none_or(|next| now < next)
            {
                continue;
            }
            probe.pending = Some(PendingProbe {
                token: StatusProbeToken {
                    thread_id: context.thread_id,
                    environment_id: context.environment_id.clone(),
                    generation: context.generation,
                    cwd: context.cwd.clone(),
                    request_id: NEXT_REQUEST.fetch_add(1, Ordering::Relaxed),
                },
                task: None,
            });
            probe.last_started = Some(now);
            probe.next = Some(
                now + match kind {
                    StatusProbeKind::LocalGit => LOCAL_INTERVAL,
                    StatusProbeKind::PullRequest => PR_INTERVAL,
                },
            );
            due.push(kind);
        }
        due
    }

    /// Reject before touching pending/dirty state. A stale response cannot complete a newer probe.
    pub(crate) fn accept(&mut self, token: &StatusProbeToken) -> bool {
        for kind in [StatusProbeKind::LocalGit, StatusProbeKind::PullRequest] {
            let probe = self.probe_mut(kind);
            if probe
                .pending
                .as_ref()
                .is_some_and(|pending| &pending.token == token)
            {
                probe.pending = None;
                if let Some(dirty) = probe.dirty.take() {
                    probe.next = Some(probe.next.map_or(dirty, |next| next.min(dirty)));
                }
                return true;
            }
        }
        false
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        [&self.local, &self.pr]
            .into_iter()
            .filter(|probe| probe.enabled && probe.pending.is_none())
            .filter_map(|probe| probe.next)
            .min()
    }
}

#[cfg(test)]
#[path = "status_line_refresh_tests.rs"]
mod tests;
