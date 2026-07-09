//! Aggregated analytics read model.
//!
//! `Summary` is the single, globally-consistent view that every instance reads
//! (persisted at `layout::SUMMARY_KEY`). `compute_summary` is the pure
//! aggregation over a set of records — shared by the worker aggregator and
//! reused for offline tests.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use time::{Duration, OffsetDateTime};

use super::types::{DurationPoint, RenderRecord, VolumePoint};

/// Aggregated analytics, refreshed each aggregation cycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
    pub volume_by_day: Vec<VolumePoint>,
    pub duration_by_day: Vec<DurationPoint>,
    pub templates: Vec<TemplateSummary>,
    /// Global most-recent renders (newest first).
    pub recent: Vec<RenderRecord>,
    pub totals: Totals,
}

/// Per-template rollup: lifetime count plus that template's most-recent renders.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateSummary {
    pub template_name: String,
    pub total_renders: u64,
    /// This template's most-recent renders (newest first).
    pub recent: Vec<RenderRecord>,
}

/// Headline metrics over the trailing 24h.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Totals {
    pub renders_24h: u64,
    pub success_rate_24h: f64,
    pub p90_latency_ms_24h: u32,
}

impl Summary {
    /// An empty summary stamped at `now` — returned when no aggregation has run yet.
    pub fn empty(now: OffsetDateTime) -> Self {
        Self {
            generated_at: now,
            volume_by_day: Vec::new(),
            duration_by_day: Vec::new(),
            templates: Vec::new(),
            recent: Vec::new(),
            totals: Totals::default(),
        }
    }
}

/// Aggregate a flat set of render records into a `Summary`.
///
/// `recent_n` bounds both the global and per-template recent lists. `now`
/// anchors the trailing-24h totals window (passed in for deterministic tests).
pub fn compute_summary(records: &[RenderRecord], recent_n: usize, now: OffsetDateTime) -> Summary {
    // Volume per day (all renders).
    let mut vol: HashMap<time::Date, u64> = HashMap::new();
    for r in records {
        *vol.entry(r.timestamp.date()).or_insert(0) += 1;
    }
    let mut volume_by_day: Vec<VolumePoint> = vol
        .into_iter()
        .map(|(date, renders)| VolumePoint { date, renders })
        .collect();
    volume_by_day.sort_by_key(|v| v.date);

    // Average duration per day (successful renders only).
    let mut dur: HashMap<time::Date, (u64, u64)> = HashMap::new();
    for r in records {
        if r.success {
            let e = dur.entry(r.timestamp.date()).or_insert((0, 0));
            e.0 += r.duration_ms as u64;
            e.1 += 1;
        }
    }
    let mut duration_by_day: Vec<DurationPoint> = dur
        .into_iter()
        .map(|(date, (total, count))| DurationPoint {
            date,
            avg_duration_ms: total as f64 / count as f64,
        })
        .collect();
    duration_by_day.sort_by_key(|d| d.date);

    // Per-template rollups with bounded recent lists.
    let mut by_tpl: HashMap<&str, Vec<&RenderRecord>> = HashMap::new();
    for r in records {
        by_tpl.entry(r.template_name.as_str()).or_default().push(r);
    }
    let mut templates: Vec<TemplateSummary> = by_tpl
        .into_iter()
        .map(|(name, mut recs)| {
            recs.sort_by_key(|r| std::cmp::Reverse(r.timestamp));
            let total_renders = recs.len() as u64;
            let recent = recs.into_iter().take(recent_n).cloned().collect();
            TemplateSummary {
                template_name: name.to_string(),
                total_renders,
                recent,
            }
        })
        .collect();
    templates.sort_by(|a, b| {
        b.total_renders
            .cmp(&a.total_renders)
            .then_with(|| a.template_name.cmp(&b.template_name))
    });

    // Global recent.
    let mut all: Vec<&RenderRecord> = records.iter().collect();
    all.sort_by_key(|r| std::cmp::Reverse(r.timestamp));
    let recent: Vec<RenderRecord> = all.iter().take(recent_n).map(|r| (*r).clone()).collect();

    // Trailing-24h totals.
    let cutoff = now - Duration::hours(24);
    let window: Vec<&RenderRecord> = records.iter().filter(|r| r.timestamp >= cutoff).collect();
    let renders_24h = window.len() as u64;
    let successes = window.iter().filter(|r| r.success).count();
    let success_rate_24h = if renders_24h == 0 {
        0.0
    } else {
        successes as f64 / renders_24h as f64
    };
    let mut latencies: Vec<u32> = window
        .iter()
        .filter(|r| r.success)
        .map(|r| r.duration_ms)
        .collect();
    latencies.sort_unstable();
    let p90_latency_ms_24h = percentile(&latencies, 0.9);

    Summary {
        generated_at: now,
        volume_by_day,
        duration_by_day,
        templates,
        recent,
        totals: Totals {
            renders_24h,
            success_rate_24h,
            p90_latency_ms_24h,
        },
    }
}

/// Nearest-rank percentile over a pre-sorted slice.
fn percentile(sorted: &[u32], p: f64) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = (((sorted.len() - 1) as f64) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn rec(name: &str, ts: OffsetDateTime, success: bool, duration_ms: u32) -> RenderRecord {
        RenderRecord {
            render_id: format!("{}-{}", name, duration_ms),
            timestamp: ts,
            template_ref: format!("{}:latest", name),
            template_name: name.to_string(),
            template_tag: "latest".to_string(),
            manifest_hash: "sha256:m".to_string(),
            data_hash: "sha256:d".to_string(),
            pdf_hash: "sha256:p".to_string(),
            success,
            duration_ms,
            pdf_size_bytes: 100,
            error: if success {
                None
            } else {
                Some("boom".to_string())
            },
        }
    }

    #[test]
    fn test_compute_summary_rollups() {
        let now = datetime!(2026-07-09 12:00 UTC);
        let records = vec![
            rec("invoice", datetime!(2026-07-09 10:00 UTC), true, 100),
            rec("invoice", datetime!(2026-07-09 11:00 UTC), true, 300),
            rec("invoice", datetime!(2026-07-08 11:00 UTC), false, 50),
            rec("letter", datetime!(2026-07-09 09:00 UTC), true, 200),
        ];

        let s = compute_summary(&records, 10, now);

        // Volume by day: 3 on the 9th, 1 on the 8th.
        assert_eq!(s.volume_by_day.len(), 2);
        assert_eq!(s.volume_by_day[0].renders, 1); // 2026-07-08
        assert_eq!(s.volume_by_day[1].renders, 3); // 2026-07-09

        // Templates sorted by count desc: invoice(3) then letter(1).
        assert_eq!(s.templates[0].template_name, "invoice");
        assert_eq!(s.templates[0].total_renders, 3);
        assert_eq!(s.templates[1].template_name, "letter");
        assert_eq!(s.templates[1].total_renders, 1);
        // Per-template recent is newest-first.
        assert_eq!(s.templates[0].recent[0].duration_ms, 300);

        // Totals over trailing 24h (cutoff 2026-07-08 12:00): the 07-08 11:00
        // record is excluded, leaving 3 successful renders.
        assert_eq!(s.totals.renders_24h, 3);
        assert_eq!(s.totals.success_rate_24h, 1.0);
        // p90 over [100,200,300] -> nearest-rank idx round(2*0.9)=2 -> 300.
        assert_eq!(s.totals.p90_latency_ms_24h, 300);
    }

    #[test]
    fn test_recent_n_bounds_lists() {
        let now = datetime!(2026-07-09 12:00 UTC);
        let records: Vec<RenderRecord> = (0..5)
            .map(|i| {
                rec(
                    "invoice",
                    datetime!(2026-07-09 09:00 UTC) + Duration::minutes(i),
                    true,
                    100 + i as u32,
                )
            })
            .collect();
        let s = compute_summary(&records, 2, now);
        assert_eq!(s.recent.len(), 2);
        assert_eq!(s.templates[0].recent.len(), 2);
        // Newest first.
        assert_eq!(s.recent[0].duration_ms, 104);
    }

    #[test]
    fn test_empty() {
        let now = datetime!(2026-07-09 12:00 UTC);
        let s = compute_summary(&[], 10, now);
        assert!(s.recent.is_empty());
        assert_eq!(s.totals.renders_24h, 0);
        assert_eq!(s.totals.success_rate_24h, 0.0);
        assert_eq!(s.totals.p90_latency_ms_24h, 0);
    }
}
