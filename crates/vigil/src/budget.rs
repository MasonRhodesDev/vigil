use std::time::{Duration, Instant};

const REPORT_EVERY: Duration = Duration::from_secs(5);
const FALLBACK_REFRESH_MHZ: u32 = 60_000;

pub struct FrameBudget {
    budget: Duration,
    over: u32,
    worst: Duration,
    reported: Option<Instant>,
}

impl FrameBudget {
    pub fn new(refresh_mhz: u32) -> Self {
        let refresh_mhz = if refresh_mhz == 0 {
            FALLBACK_REFRESH_MHZ
        } else {
            refresh_mhz
        };
        Self {
            budget: Duration::from_secs_f64(1000.0 / f64::from(refresh_mhz)),
            over: 0,
            worst: Duration::ZERO,
            reported: None,
        }
    }

    pub fn record(&mut self, draw: Duration, took: Duration, now: Instant) -> Option<String> {
        if took <= self.budget {
            return None;
        }
        self.over += 1;
        self.worst = self.worst.max(took);
        if self
            .reported
            .is_some_and(|last| now.duration_since(last) < REPORT_EVERY)
        {
            return None;
        }
        let line = format!(
            "slow present: {:.1} ms (draw {:.1} ms, copy and flip {:.1} ms) against a {:.1} ms \
             frame budget ({} over budget since the last report, worst {:.1} ms)",
            took.as_secs_f64() * 1000.0,
            draw.as_secs_f64() * 1000.0,
            took.saturating_sub(draw).as_secs_f64() * 1000.0,
            self.budget.as_secs_f64() * 1000.0,
            self.over,
            self.worst.as_secs_f64() * 1000.0,
        );
        self.reported = Some(now);
        self.over = 0;
        self.worst = Duration::ZERO;
        Some(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn a_present_inside_the_refresh_interval_is_silent() {
        let mut budget = FrameBudget::new(60_000);
        assert_eq!(budget.record(MS, 16 * MS, Instant::now()), None);
    }

    #[test]
    fn slow_presents_report_at_most_once_per_interval_and_count_the_rest() {
        let mut budget = FrameBudget::new(60_000);
        let start = Instant::now();
        let first = budget
            .record(3 * MS, 40 * MS, start)
            .expect("first slow present");
        assert!(
            first.starts_with(
                "slow present: 40.0 ms (draw 3.0 ms, copy and flip 37.0 ms) against a 16.7 ms"
            ),
            "{first}"
        );
        let second = start + Duration::from_secs(1);
        assert_eq!(budget.record(MS, 90 * MS, second), None);
        assert_eq!(
            budget.record(MS, 30 * MS, second + Duration::from_secs(1)),
            None
        );
        let later = budget
            .record(MS, 20 * MS, start + REPORT_EVERY)
            .expect("report after the interval");
        assert!(
            later.ends_with("(3 over budget since the last report, worst 90.0 ms)"),
            "{later}"
        );
    }

    #[test]
    fn an_unknown_refresh_rate_budgets_for_sixty_hertz() {
        let mut budget = FrameBudget::new(0);
        assert_eq!(budget.record(MS, 16 * MS, Instant::now()), None);
        assert!(budget.record(MS, 17 * MS, Instant::now()).is_some());
    }
}
