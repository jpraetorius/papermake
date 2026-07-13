//! S3 key layout for the buffered-analytics store.
//!
//! Two independent partitionings of the same records:
//! - `analytics/raw/dt=<render-date>/…` — raw NDJSON, partitioned by *render*
//!   date, consumed by the aggregator.
//! - `expiry/dt=<expiry-date>/…` — expiry index, partitioned by *expiry* date,
//!   consumed by retention pruning.
//!
//! The aggregated read model lives at a single well-known key.

use time::Date;

/// Well-known key for the aggregated analytics summary.
pub const SUMMARY_KEY: &str = "analytics/agg/summary.json";

/// Prefix for raw analytics NDJSON (partitioned by render date).
pub const RAW_PREFIX: &str = "analytics/raw/";

/// Prefix for the expiry index (partitioned by expiry date).
pub const EXPIRY_PREFIX: &str = "expiry/";

/// Prefix for batch-job documents.
pub const JOBS_PREFIX: &str = "jobs/";

/// Key for a batch job's document.
pub fn job_key(job_id: &str) -> String {
    format!("{}{}/job.json", JOBS_PREFIX, job_id)
}

/// Format a date as `YYYY-MM-DD` (stable, independent of the `time` Display impl).
pub fn date_str(date: Date) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        date.month() as u8,
        date.day()
    )
}

/// Key for a raw analytics NDJSON object, partitioned by render date and scoped
/// to the writing instance so concurrent instances never collide.
pub fn raw_key(render_date: Date, instance_id: &str, unix_millis: u128, seq: u64) -> String {
    format!(
        "{}dt={}/{}/{}-{}.ndjson",
        RAW_PREFIX,
        date_str(render_date),
        instance_id,
        unix_millis,
        seq
    )
}

/// Prefix for all raw NDJSON of a given render date.
pub fn raw_date_prefix(render_date: Date) -> String {
    format!("{}dt={}/", RAW_PREFIX, date_str(render_date))
}

/// Key for an expiry-index NDJSON object, partitioned by expiry date and scoped
/// to the writing instance.
pub fn expiry_key(expiry_date: Date, instance_id: &str, unix_millis: u128, seq: u64) -> String {
    format!(
        "{}dt={}/{}/{}-{}.ndjson",
        EXPIRY_PREFIX,
        date_str(expiry_date),
        instance_id,
        unix_millis,
        seq
    )
}

/// Prefix for all expiry-index NDJSON of a given expiry date.
pub fn expiry_date_prefix(expiry_date: Date) -> String {
    format!("{}dt={}/", EXPIRY_PREFIX, date_str(expiry_date))
}

/// Extract the `dt=YYYY-MM-DD` date from a raw/expiry key, if present.
pub fn parse_dt(key: &str) -> Option<Date> {
    let idx = key.find("dt=")?;
    let rest = &key[idx + 3..];
    let date_part = rest.split('/').next()?;
    let mut it = date_part.split('-');
    let y: i32 = it.next()?.parse().ok()?;
    let m: u8 = it.next()?.parse().ok()?;
    let d: u8 = it.next()?.parse().ok()?;
    let month = time::Month::try_from(m).ok()?;
    Date::from_calendar_date(y, month, d).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Month;

    #[test]
    fn test_date_str() {
        let d = Date::from_calendar_date(2026, Month::July, 9).unwrap();
        assert_eq!(date_str(d), "2026-07-09");
    }

    #[test]
    fn test_raw_key() {
        let d = Date::from_calendar_date(2026, Month::July, 9).unwrap();
        assert_eq!(
            raw_key(d, "inst-1", 1_700_000_000_000, 3),
            "analytics/raw/dt=2026-07-09/inst-1/1700000000000-3.ndjson"
        );
    }

    #[test]
    fn test_expiry_key_and_prefix() {
        let d = Date::from_calendar_date(2026, Month::August, 1).unwrap();
        assert_eq!(
            expiry_key(d, "inst-1", 42, 0),
            "expiry/dt=2026-08-01/inst-1/42-0.ndjson"
        );
        assert_eq!(expiry_date_prefix(d), "expiry/dt=2026-08-01/");
    }

    #[test]
    fn test_parse_dt() {
        let d = Date::from_calendar_date(2026, Month::July, 9).unwrap();
        assert_eq!(
            parse_dt("analytics/raw/dt=2026-07-09/inst-1/123-0.ndjson"),
            Some(d)
        );
        assert_eq!(parse_dt("expiry/dt=2026-07-09/x.ndjson"), Some(d));
        assert_eq!(parse_dt("no-date-here"), None);
    }
}
