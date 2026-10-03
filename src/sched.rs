//! Round-robin scheduling policy over a fixed task table. Pure logic without
//! hardware access, shared by the kernel and host tests.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Free,
    Ready,
    /// Blocked until the timer tick count reaches this value.
    Sleeping(u64),
    /// Blocked until keyboard or serial input arrives.
    Input,
    /// Finished; its stack is released once the CPU has left it.
    Exited,
}

impl State {
    pub fn name(&self) -> &'static str {
        match self {
            State::Free => "free",
            State::Ready => "ready",
            State::Sleeping(_) => "sleeping",
            State::Input => "input",
            State::Exited => "exited",
        }
    }
}

/// Make every sleeper whose deadline has passed ready again.
pub fn wake_sleepers(states: &mut [State], now: u64) {
    for state in states.iter_mut() {
        if matches!(*state, State::Sleeping(until) if until <= now) {
            *state = State::Ready;
        }
    }
}

/// Make input waiters ready; returns the first one woken.
pub fn wake_input(states: &mut [State]) -> Option<usize> {
    let mut first = None;
    for (index, state) in states.iter_mut().enumerate() {
        if *state == State::Input {
            *state = State::Ready;
            first = first.or(Some(index));
        }
    }
    first
}

/// The next ready task after `current` in round-robin order, `current`
/// itself if it is the only one, and `idle` only when nothing else can run.
pub fn next(states: &[State], current: usize, idle: usize) -> usize {
    (1..=states.len())
        .map(|offset| (current + offset) % states.len())
        .find(|&index| index != idle && states[index] == State::Ready)
        .unwrap_or(idle)
}

#[cfg(test)]
mod tests {
    use super::{next, wake_input, wake_sleepers, State};
    use State::*;

    const IDLE: usize = 1;

    #[test]
    fn round_robin_skips_blocked_and_free_slots() {
        let states = [Ready, Ready, Ready, Free, Sleeping(9), Ready, Exited, Input];
        assert_eq!(next(&states, 0, IDLE), 2);
        assert_eq!(next(&states, 2, IDLE), 5);
        assert_eq!(next(&states, 5, IDLE), 0);
        // Leaving idle picks the first ready task after it.
        assert_eq!(next(&states, IDLE, IDLE), 2);
    }

    #[test]
    fn current_continues_alone_and_idle_runs_when_nothing_is_ready() {
        assert_eq!(next(&[Ready, Ready, Free, Free], 0, IDLE), 0);
        assert_eq!(next(&[Input, Ready, Sleeping(5), Exited], 0, IDLE), IDLE);
        assert_eq!(next(&[Input, Ready, Sleeping(5), Exited], IDLE, IDLE), IDLE);
    }

    #[test]
    fn sleepers_wake_at_their_deadline() {
        let mut states = [Sleeping(10), Ready, Sleeping(11), Input];
        wake_sleepers(&mut states, 9);
        assert_eq!(states, [Sleeping(10), Ready, Sleeping(11), Input]);
        wake_sleepers(&mut states, 10);
        assert_eq!(states, [Ready, Ready, Sleeping(11), Input]);
        wake_sleepers(&mut states, 100);
        assert_eq!(states, [Ready, Ready, Ready, Input]);
    }

    #[test]
    fn input_wakes_only_input_waiters() {
        let mut states = [Sleeping(4), Ready, Input, Free];
        assert_eq!(wake_input(&mut states), Some(2));
        assert_eq!(states, [Sleeping(4), Ready, Ready, Free]);
        assert_eq!(wake_input(&mut states), None);
    }
}
