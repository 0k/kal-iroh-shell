//! Reload handshake between the shell and the page.
//!
//! When a new release becomes current, the shell must reload the page
//! without losing what the user is doing:
//!
//! 1. the shell calls `window.__kalIrohShell.prepareReload(request)` in
//!    the page (native script evaluation);
//! 2. the page saves its transient state, then answers on
//!    `POST /__shell/reload-ack` with `ready`, or `busy` + a reason while
//!    the user is in the middle of something (microphone on, upload,
//!    message being sent).  It renews `busy` while the reason holds and
//!    sends `ready` as soon as it clears;
//! 3. on `ready` the shell reloads the view natively.  A page that does
//!    not answer within [`ANSWER_TIMEOUT`], or stops renewing `busy`
//!    within [`BUSY_LEASE`], is considered broken and reloaded anyway.
//!
//! [`Handshake`] is the pure state machine; the shell's tasks feed it
//! events and execute the [`Action`]s it returns.

use std::time::{Duration, Instant};

pub use kal_iroh_shell_protocol::Ack;

/// Longest wait for the page's first answer to "prepare to reload".
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(5);
/// How long one `busy` answer postpones the reload.  The page renews it
/// well before it lapses (npm `@0k/kal-iroh-shell`).
pub const BUSY_LEASE: Duration = Duration::from_secs(30);

/// What the shell must do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Ask the page to prepare (`request` identifies the answers).
    Prepare { request: u64 },
    /// The page is ready: reload.
    Reload,
    /// The page did not answer in time: reload anyway.
    ForceReload { why: &'static str },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    Idle,
    Preparing {
        request: u64,
        deadline: Instant,
        busy_reason: Option<String>,
    },
}

/// The handshake for one page.
#[derive(Debug)]
pub struct Handshake {
    phase: Phase,
    last_request: u64,
}

impl Default for Handshake {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            last_request: 0,
        }
    }
}

impl Handshake {
    /// Whether a reload is being negotiated with the page.
    pub fn pending(&self) -> bool {
        matches!(self.phase, Phase::Preparing { .. })
    }

    /// A release the page does not run became current.  A handshake
    /// already in progress simply carries on: the reload loads whatever
    /// release is current at that moment.
    pub fn on_new_release(&mut self, now: Instant) -> Vec<Action> {
        if matches!(self.phase, Phase::Preparing { .. }) {
            return vec![];
        }
        self.last_request += 1;
        self.phase = Phase::Preparing {
            request: self.last_request,
            deadline: now + ANSWER_TIMEOUT,
            busy_reason: None,
        };
        vec![Action::Prepare {
            request: self.last_request,
        }]
    }

    /// The page answered `request`.  Stale or unexpected answers are
    /// ignored.
    pub fn on_ack(&mut self, request: u64, ack: Ack, now: Instant) -> Vec<Action> {
        let Phase::Preparing {
            request: current,
            deadline,
            busy_reason,
        } = &mut self.phase
        else {
            return vec![];
        };
        if *current != request {
            return vec![];
        }
        match ack {
            Ack::Ready => {
                self.phase = Phase::Idle;
                vec![Action::Reload]
            }
            Ack::Busy { reason } => {
                *deadline = now + BUSY_LEASE;
                *busy_reason = Some(reason);
                vec![]
            }
        }
    }

    /// Time passed: force the reload when the page let its deadline lapse.
    pub fn on_tick(&mut self, now: Instant) -> Vec<Action> {
        let Phase::Preparing {
            deadline,
            busy_reason,
            ..
        } = &self.phase
        else {
            return vec![];
        };
        if now < *deadline {
            return vec![];
        }
        let why = if busy_reason.is_some() {
            "busy lease not renewed"
        } else {
            "no answer to prepare-to-reload"
        };
        self.phase = Phase::Idle;
        vec![Action::ForceReload { why }]
    }

    /// The reason the page gave for postponing, while it does.
    pub fn busy_reason(&self) -> Option<&str> {
        match &self.phase {
            Phase::Preparing { busy_reason, .. } => busy_reason.as_deref(),
            Phase::Idle => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preparing(h: &mut Handshake, t0: Instant) -> u64 {
        match h.on_new_release(t0).as_slice() {
            [Action::Prepare { request }] => *request,
            other => panic!("expected a prepare, got {other:?}"),
        }
    }

    #[test]
    fn a_ready_page_is_reloaded_at_once() {
        let mut h = Handshake::default();
        let t0 = Instant::now();
        let req = preparing(&mut h, t0);

        assert_eq!(h.on_ack(req, Ack::Ready, t0), vec![Action::Reload]);
        assert_eq!(
            h.on_tick(t0 + BUSY_LEASE * 2),
            vec![],
            "nothing left pending"
        );
    }

    #[test]
    fn a_silent_page_is_reloaded_after_the_answer_timeout() {
        let mut h = Handshake::default();
        let t0 = Instant::now();
        preparing(&mut h, t0);

        assert_eq!(
            h.on_tick(t0 + ANSWER_TIMEOUT - Duration::from_millis(1)),
            vec![]
        );
        assert_eq!(
            h.on_tick(t0 + ANSWER_TIMEOUT),
            vec![Action::ForceReload {
                why: "no answer to prepare-to-reload"
            }]
        );
    }

    #[test]
    fn a_busy_page_postpones_while_it_renews_then_reloads_when_ready() {
        let mut h = Handshake::default();
        let t0 = Instant::now();
        let req = preparing(&mut h, t0);
        let busy = || Ack::Busy {
            reason: "microphone on".into(),
        };

        h.on_ack(req, busy(), t0);
        let t1 = t0 + BUSY_LEASE - Duration::from_secs(1);
        assert_eq!(h.on_tick(t1), vec![]);
        h.on_ack(req, busy(), t1);
        assert_eq!(
            h.on_tick(t0 + BUSY_LEASE + Duration::from_secs(1)),
            vec![],
            "renewed"
        );
        assert_eq!(h.busy_reason(), Some("microphone on"));

        assert_eq!(h.on_ack(req, Ack::Ready, t1), vec![Action::Reload]);
    }

    #[test]
    fn a_busy_page_that_stops_renewing_is_reloaded_anyway() {
        let mut h = Handshake::default();
        let t0 = Instant::now();
        let req = preparing(&mut h, t0);
        h.on_ack(
            req,
            Ack::Busy {
                reason: "upload".into(),
            },
            t0,
        );

        assert_eq!(
            h.on_tick(t0 + BUSY_LEASE),
            vec![Action::ForceReload {
                why: "busy lease not renewed"
            }]
        );
    }

    #[test]
    fn answers_to_an_older_request_are_ignored() {
        let mut h = Handshake::default();
        let t0 = Instant::now();
        let old = preparing(&mut h, t0);
        h.on_ack(old, Ack::Ready, t0);
        let new = preparing(&mut h, t0);

        assert_eq!(h.on_ack(old, Ack::Ready, t0), vec![]);
        assert_ne!(old, new);
    }

    #[test]
    fn a_second_release_during_a_handshake_does_not_restart_it() {
        let mut h = Handshake::default();
        let t0 = Instant::now();
        preparing(&mut h, t0);

        assert_eq!(h.on_new_release(t0 + Duration::from_secs(1)), vec![]);
        assert_eq!(
            h.on_tick(t0 + ANSWER_TIMEOUT),
            vec![Action::ForceReload {
                why: "no answer to prepare-to-reload"
            }],
            "the first deadline still holds"
        );
    }
}
