//! Pure label formatting.

use chrono::TimeDelta;

/// Whole minutes until the start, rounded **up**, at least 1.
///
/// 59 s left is `1`, never `0`, so the label never shows `0m` before the
/// meeting starts. Tier selection uses the same value, so the color always
/// matches the label.
pub fn minutes_left(remaining: TimeDelta) -> i64 {
    let secs = remaining.num_seconds() + i64::from(remaining.subsec_nanos() > 0);
    ((secs + 59) / 60).max(1)
}

/// `1h 20m`, `2h`, `25m`, `1m`.
pub fn remaining(remaining: TimeDelta) -> String {
    let minutes = minutes_left(remaining);
    match (minutes / 60, minutes % 60) {
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h {m}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        let cases = [
            (TimeDelta::milliseconds(1), "1m"),
            (TimeDelta::seconds(59), "1m"),
            (TimeDelta::seconds(60), "1m"),
            (TimeDelta::seconds(61), "2m"),
            (TimeDelta::milliseconds(60_001), "2m"),
            (TimeDelta::minutes(25), "25m"),
            (TimeDelta::minutes(59) + TimeDelta::seconds(1), "1h"),
            (TimeDelta::minutes(60), "1h"),
            (TimeDelta::minutes(80), "1h 20m"),
            (TimeDelta::minutes(79) + TimeDelta::seconds(30), "1h 20m"),
            (TimeDelta::hours(13) + TimeDelta::minutes(5), "13h 5m"),
            // Not expected (the event would be ongoing), but must not panic or
            // show 0m.
            (TimeDelta::zero(), "1m"),
            (TimeDelta::seconds(-30), "1m"),
        ];
        for (delta, label) in cases {
            assert_eq!(remaining(delta), label, "{delta}");
        }
    }
}
