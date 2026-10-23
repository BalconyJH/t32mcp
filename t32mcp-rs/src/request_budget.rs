// SPDX-FileCopyrightText: 2026 Lauterbach GmbH
// SPDX-License-Identifier: Apache-2.0

use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

const QUEUED: u8 = 0;
const STARTED: u8 = 1;
const EXPIRED: u8 = 2;

pub struct RequestBudget {
    deadline: Instant,
    state: AtomicU8,
}

impl RequestBudget {
    pub fn new(started: Instant, timeout: Duration) -> Result<Self, String> {
        let deadline = started
            .checked_add(timeout)
            .ok_or_else(|| "Request timeout is too large".to_string())?;
        Ok(Self {
            deadline,
            state: AtomicU8::new(QUEUED),
        })
    }

    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    pub fn begin(&self) -> Result<(), String> {
        if self.remaining().is_zero() {
            return Err(self.expire());
        }
        self.state
            .compare_exchange(QUEUED, STARTED, Ordering::SeqCst, Ordering::SeqCst)
            .map(|_| ())
            .map_err(|_| self.expire())
    }

    pub fn expire(&self) -> String {
        // Winning this transition guarantees that the worker cannot start later.
        match self
            .state
            .compare_exchange(QUEUED, EXPIRED, Ordering::SeqCst, Ordering::SeqCst)
        {
            Ok(_) | Err(EXPIRED) => {
                "Request deadline exceeded (not_started): the queued operation was not started."
                    .to_string()
            }
            Err(_) => unknown_completion(),
        }
    }
}

pub fn unknown_completion() -> String {
    "Request deadline exceeded (unknown_completion): processing already started; \
     completion and target state are unknown. This does not cancel an in-progress \
     call or terminate a PRACTICE script. Do not automatically retry."
        .to_string()
}

pub fn remaining(deadline: Instant) -> Result<Duration, String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err("Request deadline exceeded".to_string())
    } else {
        Ok(remaining)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_queue_entries_never_start() {
        let budget = RequestBudget::new(Instant::now(), Duration::ZERO).unwrap();
        assert!(budget.begin().unwrap_err().contains("not_started"));
        assert!(budget.begin().unwrap_err().contains("not_started"));
    }

    #[test]
    fn cancellation_before_dispatch_is_final() {
        let budget = RequestBudget::new(Instant::now(), Duration::from_secs(60)).unwrap();
        assert!(budget.expire().contains("not_started"));
        assert!(budget.begin().unwrap_err().contains("not_started"));
    }

    #[test]
    fn started_work_is_not_claimed_to_be_cancelled() {
        let budget = RequestBudget::new(Instant::now(), Duration::from_secs(60)).unwrap();
        budget.begin().unwrap();
        assert!(budget.expire().contains("unknown_completion"));
        assert!(budget.expire().contains("Do not automatically retry"));
    }

    #[test]
    fn request_entry_time_is_not_reset_when_creating_a_budget() {
        let started = Instant::now() - Duration::from_secs(2);
        let budget = RequestBudget::new(started, Duration::from_secs(1)).unwrap();
        assert!(budget.remaining().is_zero());
        assert!(budget.begin().unwrap_err().contains("not_started"));
    }

    #[test]
    fn impossible_deadlines_are_errors_instead_of_panics() {
        assert!(RequestBudget::new(Instant::now(), Duration::MAX).is_err());
    }
}
