use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CircuitState {
    Closed,
    Open { opened_at: Instant },
    HalfOpen,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CircuitTransition {
    pub from: &'static str,
    pub to: &'static str,
}

#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    state: CircuitState,
    consecutive_failures: u32,
    failure_threshold: u32,
    cooldown: Duration,
}

impl CircuitBreaker {
    #[must_use]
    pub fn new(failure_threshold: u32, cooldown: Duration) -> Self {
        Self {
            state: CircuitState::Closed,
            consecutive_failures: 0,
            failure_threshold,
            cooldown,
        }
    }

    pub fn allow_request(&mut self) -> (bool, Option<CircuitTransition>) {
        match self.state {
            CircuitState::Closed | CircuitState::HalfOpen => (true, None),
            CircuitState::Open { opened_at } => {
                if opened_at.elapsed() >= self.cooldown {
                    let previous = self.state_name();
                    self.state = CircuitState::HalfOpen;
                    (
                        true,
                        Some(CircuitTransition {
                            from: previous,
                            to: self.state_name(),
                        }),
                    )
                } else {
                    (false, None)
                }
            }
        }
    }

    pub fn record_success(&mut self) -> Option<CircuitTransition> {
        self.consecutive_failures = 0;
        let previous = self.state_name();
        self.state = CircuitState::Closed;
        (previous != self.state_name()).then_some(CircuitTransition {
            from: previous,
            to: self.state_name(),
        })
    }

    pub fn record_failure(&mut self) -> Option<CircuitTransition> {
        let previous = self.state_name();
        match self.state {
            CircuitState::Closed => {
                self.consecutive_failures += 1;
                if self.consecutive_failures >= self.failure_threshold {
                    self.state = CircuitState::Open {
                        opened_at: Instant::now(),
                    };
                }
            }
            CircuitState::HalfOpen => {
                self.state = CircuitState::Open {
                    opened_at: Instant::now(),
                };
            }
            CircuitState::Open { .. } => {}
        }
        (previous != self.state_name()).then_some(CircuitTransition {
            from: previous,
            to: self.state_name(),
        })
    }

    #[cfg_attr(not(test), allow(dead_code))]
    #[must_use]
    pub fn state_name(&self) -> &'static str {
        match self.state {
            CircuitState::Closed => "closed",
            CircuitState::Open { .. } => "open",
            CircuitState::HalfOpen => "half-open",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_after_threshold_failures() {
        let mut breaker = CircuitBreaker::new(3, Duration::from_secs(60));
        breaker.record_failure();
        breaker.record_failure();
        assert!(breaker.allow_request().0);
        breaker.record_failure();
        assert_eq!(breaker.state_name(), "open");
        assert!(!breaker.allow_request().0);
    }

    #[test]
    fn cooldown_transitions_to_half_open_probe() {
        let mut breaker = CircuitBreaker::new(1, Duration::from_millis(1));
        breaker.record_failure();
        std::thread::sleep(Duration::from_millis(5));
        assert!(breaker.allow_request().0);
        assert_eq!(breaker.state_name(), "half-open");
    }

    #[test]
    fn success_closes_half_open_circuit() {
        let mut breaker = CircuitBreaker::new(1, Duration::from_millis(1));
        breaker.record_failure();
        std::thread::sleep(Duration::from_millis(5));
        assert!(breaker.allow_request().0);
        breaker.record_success();
        assert_eq!(breaker.state_name(), "closed");
    }

    #[test]
    fn reports_state_transitions() {
        let mut breaker = CircuitBreaker::new(1, Duration::from_millis(1));
        let opened = breaker.record_failure().expect("open transition");
        assert_eq!(opened.from, "closed");
        assert_eq!(opened.to, "open");

        std::thread::sleep(Duration::from_millis(5));
        let half_open = breaker.allow_request().1.expect("half-open transition");
        assert_eq!(half_open.from, "open");
        assert_eq!(half_open.to, "half-open");

        let closed = breaker.record_success().expect("close transition");
        assert_eq!(closed.from, "half-open");
        assert_eq!(closed.to, "closed");
    }
}
