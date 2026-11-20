//! Timestamp formatting shared by session metadata and extensions.

use jiff::{Timestamp, tz::TimeZone};

/// Preserve the UTC offset and millisecond, microsecond or nanosecond precision
/// of existing metadata. Memory extraction compares these timestamps as strings.
pub fn format_rfc3339(timestamp: Timestamp) -> String {
    let nanos = timestamp.subsec_nanosecond();
    let precision = if nanos == 0 {
        0
    } else if nanos % 1_000_000 == 0 {
        3
    } else if nanos % 1_000 == 0 {
        6
    } else {
        9
    };
    let utc = TimeZone::UTC.to_datetime(timestamp);
    format!("{utc:.precision$}+00:00")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_metadata_round_trips_across_fractional_precisions() {
        for fraction in ["", ".120", ".123400", ".123456700"] {
            let expected = format!("2026-05-21T12:00:00{fraction}+00:00");
            let old = format!("2026-05-21T20:00:00{fraction}+08:00");
            assert_eq!(format_rfc3339(old.parse().unwrap()), expected);
        }
    }
}
