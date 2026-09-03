//! Engine-lifecycle state machine.
//!
//! Pure, synchronous, cross-platform. `Session` owns no I/O and no clock: it
//! consumes lifecycle messages (`start`, engine events, a guard timeout, `stop`)
//! and returns a list of [`Action`]s for the caller to perform. The retry policy
//! is "retry once, silently": the first mid-connection failure re-forks the
//! child; a second failure gives up and engages the fallback.

use crate::logic::quality::Quality;

/// A control message from the host down to the session.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ControlMsg {
    Stop,
    Reconfigure(Quality),
}

/// A stats sample surfaced by the engine while `Live`.
#[derive(Clone, PartialEq, Debug)]
pub struct StatsSnapshot {
    pub fps: f64,
    pub bitrate_kbps: u32,
    pub rtt_ms: u32,
}

/// An event coming up from the engine child.
#[derive(Clone, PartialEq, Debug)]
pub enum EngineEvent {
    Started,
    Stats(StatsSnapshot),
    Error(String),
    Ended,
}

/// A side effect the caller must carry out. `Session` never performs these
/// itself — it only decides the sequence.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Action {
    ForkChild,
    KillChild,
    EmitStarted,
    EmitFailed(String),
    StartGuard,
    ClearGuard,
    /// Forward a buffered `Reconfigure` to the child once it is `Live`.
    ForwardReconfigure(Quality),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum State {
    #[default]
    Idle,
    Connecting,
    Live,
    Stopping,
    Ended,
    Failed,
}

/// The engine lifecycle for one screen-share session.
#[derive(Default)]
pub struct Session {
    state: State,
    /// Silent-retry counter. `0` = no retry spent yet, `1` = the one retry is
    /// spent (the next failure falls back).
    retries: u8,
    /// A `Reconfigure` received before `Live`, replayed after `Started`.
    pending_reconfigure: Option<Quality>,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    /// `Idle -> Connecting`. Fork the child and arm the startup guard.
    pub fn start(&mut self) -> Vec<Action> {
        self.state = State::Connecting;
        vec![Action::ForkChild, Action::StartGuard]
    }

    /// `Connecting -> Live`. Disarm the guard, tell the host we're up, and
    /// replay a buffered `Reconfigure` if one arrived early.
    pub fn on_engine_started(&mut self) -> Vec<Action> {
        self.state = State::Live;
        let mut acts = vec![Action::ClearGuard, Action::EmitStarted];
        if let Some(q) = self.pending_reconfigure.take() {
            acts.push(Action::ForwardReconfigure(q));
        }
        acts
    }

    pub fn on_engine_error(&mut self, msg: &str) -> Vec<Action> {
        self.fail(msg)
    }

    pub fn on_child_exit(&mut self) -> Vec<Action> {
        if self.state == State::Stopping {
            self.state = State::Ended;
            return vec![];
        }
        self.fail("child exited")
    }

    pub fn on_guard_timeout(&mut self) -> Vec<Action> {
        self.fail("timeout")
    }

    /// `* -> Stopping`. Stop always wins; `on_child_exit` then lands in `Ended`.
    pub fn stop(&mut self) -> Vec<Action> {
        self.state = State::Stopping;
        vec![Action::KillChild, Action::ClearGuard]
    }

    /// Host -> session control. `Stop` maps to [`stop`](Self::stop);
    /// `Reconfigure` is forwarded by the caller when `Live` (so the session
    /// returns `[]`), or buffered when not yet `Live`.
    pub fn on_control(&mut self, m: ControlMsg) -> Vec<Action> {
        match m {
            ControlMsg::Stop => self.stop(),
            ControlMsg::Reconfigure(q) => {
                if self.state != State::Live {
                    self.pending_reconfigure = Some(q);
                }
                vec![]
            }
        }
    }

    pub fn is_live(&self) -> bool {
        self.state == State::Live
    }

    /// True once we've given up (the second failure).
    pub fn fallback_engaged(&self) -> bool {
        self.state == State::Failed
    }

    /// Shared failure policy for `on_engine_error` / `on_child_exit` /
    /// `on_guard_timeout`. First failure: silent re-fork, `retries -> 1`, back
    /// to `Connecting`. Second failure: give up, `-> Failed`. Once terminal
    /// (`Failed` / `Ended` / `Stopping`) a late failure is ignored.
    fn fail(&mut self, msg: &str) -> Vec<Action> {
        if matches!(self.state, State::Failed | State::Ended | State::Stopping) {
            return vec![];
        }
        if self.retries == 0 {
            self.retries = 1;
            self.state = State::Connecting;
            vec![Action::KillChild, Action::ForkChild, Action::StartGuard]
        } else {
            self.state = State::Failed;
            vec![Action::KillChild, Action::ClearGuard, Action::EmitFailed(msg.to_string())]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logic::quality::Quality;

    #[test]
    fn happy_path() {
        let mut s = Session::new();
        assert_eq!(s.start(), vec![Action::ForkChild, Action::StartGuard]);
        assert!(!s.is_live());
        assert_eq!(s.on_engine_started(), vec![Action::ClearGuard, Action::EmitStarted]);
        assert!(s.is_live());
        assert_eq!(s.stop(), vec![Action::KillChild, Action::ClearGuard]);
        let _ = s.on_child_exit();
        assert!(!s.is_live());
    }

    #[test]
    fn first_failure_retries_silently_second_falls_back() {
        let mut s = Session::new();
        s.start();
        s.on_engine_started();
        // 1st failure -> silent re-fork
        assert_eq!(s.on_engine_error("gpu reset"), vec![Action::KillChild, Action::ForkChild, Action::StartGuard]);
        assert!(!s.fallback_engaged());
        s.on_engine_started();
        // 2nd failure -> give up
        assert_eq!(
            s.on_engine_error("gpu reset again"),
            vec![Action::KillChild, Action::ClearGuard, Action::EmitFailed("gpu reset again".into())]
        );
        assert!(s.fallback_engaged());
    }

    #[test]
    fn guard_timeout_before_started_counts_as_a_failure() {
        let mut s = Session::new();
        s.start();
        assert_eq!(s.on_guard_timeout(), vec![Action::KillChild, Action::ForkChild, Action::StartGuard]);
        assert_eq!(
            s.on_guard_timeout(),
            vec![Action::KillChild, Action::ClearGuard, Action::EmitFailed("timeout".into())]
        );
    }

    #[test]
    fn child_exit_before_started_is_a_failure_not_a_clean_end() {
        let mut s = Session::new();
        s.start();
        assert_eq!(s.on_child_exit(), vec![Action::KillChild, Action::ForkChild, Action::StartGuard]);
    }

    #[test]
    fn reconfigure_before_live_is_replayed_after_started() {
        let mut s = Session::new();
        s.start();
        assert_eq!(s.on_control(ControlMsg::Reconfigure(Quality::High)), vec![]); // buffered
        let acts = s.on_engine_started();
        assert!(acts.contains(&Action::EmitStarted));
        assert!(acts.contains(&Action::ForwardReconfigure(Quality::High)));
    }

    #[test]
    fn stop_wins_from_any_state() {
        let mut s = Session::new();
        s.start();
        assert_eq!(s.stop(), vec![Action::KillChild, Action::ClearGuard]);
        assert_eq!(s.on_engine_error("late error"), vec![]); // ignored after stop
    }
}
