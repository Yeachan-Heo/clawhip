#[derive(Debug, Clone)]
pub struct DelayedEntry {
    pub deliver_at_ms: u64,
    pub record: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct TimerWheel {
    seconds: Vec<Vec<DelayedEntry>>,
    minutes: Vec<Vec<DelayedEntry>>,
    hours: Vec<Vec<DelayedEntry>>,
    days: Vec<Vec<DelayedEntry>>,
    current_ms: u64,
}

impl TimerWheel {
    #[must_use]
    pub fn new(current_ms: u64) -> Self {
        Self {
            seconds: vec![Vec::new(); 60],
            minutes: vec![Vec::new(); 60],
            hours: vec![Vec::new(); 24],
            days: vec![Vec::new(); 365],
            current_ms,
        }
    }

    pub fn schedule(&mut self, entry: DelayedEntry) {
        let delta = entry.deliver_at_ms.saturating_sub(self.current_ms);
        if delta < 60_000 {
            let slot = usize::try_from((entry.deliver_at_ms / 1_000) % 60).unwrap_or(0);
            self.seconds[slot].push(entry);
        } else if delta < 3_600_000 {
            let slot = usize::try_from((entry.deliver_at_ms / 60_000) % 60).unwrap_or(0);
            self.minutes[slot].push(entry);
        } else if delta < 86_400_000 {
            let slot = usize::try_from((entry.deliver_at_ms / 3_600_000) % 24).unwrap_or(0);
            self.hours[slot].push(entry);
        } else {
            let slot = usize::try_from((entry.deliver_at_ms / 86_400_000) % 365).unwrap_or(0);
            self.days[slot].push(entry);
        }
    }

    pub fn tick(&mut self, now_ms: u64) -> Vec<DelayedEntry> {
        let mut due = Vec::new();
        self.drain_seconds(now_ms, &mut due);
        self.drain_minutes(now_ms, &mut due);
        self.drain_hours(now_ms, &mut due);
        self.drain_days(now_ms, &mut due);
        self.current_ms = now_ms;
        due
    }

    fn drain_seconds(&mut self, now_ms: u64, due: &mut Vec<DelayedEntry>) {
        let old_sec = self.current_ms / 1_000;
        let new_sec = now_ms / 1_000;
        for second in old_sec..=new_sec {
            let slot = usize::try_from(second % 60).unwrap_or(0);
            let entries = std::mem::take(&mut self.seconds[slot]);
            for entry in entries {
                if entry.deliver_at_ms <= now_ms {
                    due.push(entry);
                } else {
                    self.seconds[slot].push(entry);
                }
            }
        }
    }

    fn drain_minutes(&mut self, now_ms: u64, due: &mut Vec<DelayedEntry>) {
        let old = self.current_ms / 60_000;
        let new = now_ms / 60_000;
        for minute in old..=new {
            let slot = usize::try_from(minute % 60).unwrap_or(0);
            let entries = std::mem::take(&mut self.minutes[slot]);
            for entry in entries {
                if entry.deliver_at_ms <= now_ms {
                    due.push(entry);
                } else {
                    self.schedule(entry);
                }
            }
        }
    }

    fn drain_hours(&mut self, now_ms: u64, due: &mut Vec<DelayedEntry>) {
        let old = self.current_ms / 3_600_000;
        let new = now_ms / 3_600_000;
        for hour in old..=new {
            let slot = usize::try_from(hour % 24).unwrap_or(0);
            let entries = std::mem::take(&mut self.hours[slot]);
            for entry in entries {
                if entry.deliver_at_ms <= now_ms {
                    due.push(entry);
                } else {
                    self.schedule(entry);
                }
            }
        }
    }

    fn drain_days(&mut self, now_ms: u64, due: &mut Vec<DelayedEntry>) {
        let old = self.current_ms / 86_400_000;
        let new = now_ms / 86_400_000;
        for day in old..=new {
            let slot = usize::try_from(day % 365).unwrap_or(0);
            let entries = std::mem::take(&mut self.days[slot]);
            for entry in entries {
                if entry.deliver_at_ms <= now_ms {
                    due.push(entry);
                } else {
                    self.schedule(entry);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivers_due_entries() {
        let mut wheel = TimerWheel::new(0);
        wheel.schedule(DelayedEntry {
            deliver_at_ms: 5_000,
            record: b"x".to_vec(),
        });
        assert_eq!(wheel.tick(5_000).len(), 1);
    }

    #[test]
    fn leaves_future_entries_pending() {
        let mut wheel = TimerWheel::new(0);
        wheel.schedule(DelayedEntry {
            deliver_at_ms: 90_000,
            record: b"x".to_vec(),
        });
        assert!(wheel.tick(30_000).is_empty());
        assert_eq!(wheel.tick(90_000).len(), 1);
    }
}
