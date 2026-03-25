use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CircuitState {
    Closed,
    Open { opened_at: Instant },
    HalfOpen,
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

    pub fn allow_request(&mut self) -> bool {
        match self.state {
            CircuitState::Closed | CircuitState::HalfOpen => true,
            CircuitState::Open { opened_at } => {
                if opened_at.elapsed() >= self.cooldown {
                    self.state = CircuitState::HalfOpen;
                    true
                } else {
                    false
                }
            }
        }
    }

    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.state = CircuitState::Closed;
    }

    pub fn record_failure(&mut self) {
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
        assert!(breaker.allow_request());
        breaker.record_failure();
        assert_eq!(breaker.state_name(), "open");
        assert!(!breaker.allow_request());
    }

    #[test]
    fn cooldown_transitions_to_half_open_probe() {
        let mut breaker = CircuitBreaker::new(1, Duration::from_millis(1));
        breaker.record_failure();
        std::thread::sleep(Duration::from_millis(5));
        assert!(breaker.allow_request());
        assert_eq!(breaker.state_name(), "half-open");
    }

    #[test]
    fn success_closes_half_open_circuit() {
        let mut breaker = CircuitBreaker::new(1, Duration::from_millis(1));
        breaker.record_failure();
        std::thread::sleep(Duration::from_millis(5));
        assert!(breaker.allow_request());
        breaker.record_success();
        assert_eq!(breaker.state_name(), "closed");
    }
}
