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
    /// Latency distribution over all successful renders (fixed buckets).
    #[serde(default)]
    pub duration_histogram: Vec<DurationBucket>,
    pub templates: Vec<TemplateSummary>,
    /// Global most-recent renders (newest first).
    pub recent: Vec<RenderRecord>,
    pub totals: Totals,
}

/// Per-template rollup: lifetime count, per-tag breakdown, and recent renders.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateSummary {
    pub template_name: String,
    pub total_renders: u64,
    /// Renders per tag for this template (newest-count first).
    #[serde(default)]
    pub by_tag: Vec<TagCount>,
    /// This template's most-recent renders (newest first).
    pub recent: Vec<RenderRecord>,
}

/// Render count for a single tag of a template.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagCount {
    pub tag: String,
    pub renders: u64,
}

/// One bucket of the latency histogram: renders whose duration is `< upper_ms`,
/// or the open-ended top bucket when `upper_ms` is `None`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DurationBucket {
    pub upper_ms: Option<u32>,
    pub count: u64,
}

/// Upper edges (ms) of the latency histogram buckets; a final open-ended bucket
/// (`≥` the last edge) is appended.
pub const DURATION_BUCKET_EDGES_MS: [u32; 5] = [100, 250, 500, 1000, 2000];

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
            duration_histogram: Vec::new(),
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
    // Dedup by render_id, keeping the latest by timestamp. Idempotent re-renders
    // (content-addressed ids) and batch retries append duplicate raw records for
    // the same render_id; count each render once.
    let deduped: Vec<RenderRecord> = {
        let mut latest: HashMap<&str, &RenderRecord> = HashMap::new();
        for r in records {
            latest
                .entry(r.render_id.as_str())
                .and_modify(|cur| {
                    if r.timestamp > cur.timestamp {
                        *cur = r;
                    }
                })
                .or_insert(r);
        }
        latest.into_values().cloned().collect()
    };
    let records: &[RenderRecord] = &deduped;

    // Volume per day: total renders and the failing subset.
    let mut vol: HashMap<time::Date, (u64, u64)> = HashMap::new();
    for r in records {
        let e = vol.entry(r.timestamp.date()).or_insert((0, 0));
        e.0 += 1;
        if !r.success {
            e.1 += 1;
        }
    }
    let mut volume_by_day: Vec<VolumePoint> = vol
        .into_iter()
        .map(|(date, (renders, failures))| VolumePoint {
            date,
            renders,
            failures,
        })
        .collect();
    volume_by_day.sort_by_key(|v| v.date);

    // Duration per day (successful renders only): mean and p90.
    let mut dur: HashMap<time::Date, Vec<u32>> = HashMap::new();
    for r in records {
        if r.success {
            dur.entry(r.timestamp.date())
                .or_default()
                .push(r.duration_ms);
        }
    }
    let mut duration_by_day: Vec<DurationPoint> = dur
        .into_iter()
        .map(|(date, mut ds)| {
            ds.sort_unstable();
            let total: u64 = ds.iter().map(|&d| d as u64).sum();
            DurationPoint {
                date,
                avg_duration_ms: total as f64 / ds.len() as f64,
                p90_duration_ms: percentile(&ds, 0.9),
                p95_duration_ms: percentile(&ds, 0.95),
                p99_duration_ms: percentile(&ds, 0.99),
            }
        })
        .collect();
    duration_by_day.sort_by_key(|d| d.date);

    // Latency distribution over all successful renders (fixed buckets).
    let mut hist = vec![0u64; DURATION_BUCKET_EDGES_MS.len() + 1];
    for r in records.iter().filter(|r| r.success) {
        let idx = DURATION_BUCKET_EDGES_MS
            .iter()
            .position(|&edge| r.duration_ms < edge)
            .unwrap_or(DURATION_BUCKET_EDGES_MS.len());
        hist[idx] += 1;
    }
    let duration_histogram: Vec<DurationBucket> = hist
        .into_iter()
        .enumerate()
        .map(|(i, count)| DurationBucket {
            upper_ms: DURATION_BUCKET_EDGES_MS.get(i).copied(),
            count,
        })
        .collect();

    // Per-template rollups with bounded recent lists and per-tag counts.
    let mut by_tpl: HashMap<&str, Vec<&RenderRecord>> = HashMap::new();
    for r in records {
        by_tpl.entry(r.template_name.as_str()).or_default().push(r);
    }
    let mut templates: Vec<TemplateSummary> = by_tpl
        .into_iter()
        .map(|(name, mut recs)| {
            recs.sort_by_key(|r| std::cmp::Reverse(r.timestamp));
            let total_renders = recs.len() as u64;
            // Count renders per tag, then order by count desc, tag asc.
            let mut tags: HashMap<&str, u64> = HashMap::new();
            for r in &recs {
                *tags.entry(r.template_tag.as_str()).or_insert(0) += 1;
            }
            let mut by_tag: Vec<TagCount> = tags
                .into_iter()
                .map(|(tag, renders)| TagCount {
                    tag: tag.to_string(),
                    renders,
                })
                .collect();
            by_tag.sort_by(|a, b| b.renders.cmp(&a.renders).then_with(|| a.tag.cmp(&b.tag)));
            let recent = recs.into_iter().take(recent_n).cloned().collect();
            TemplateSummary {
                template_name: name.to_string(),
                total_renders,
                by_tag,
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
        duration_histogram,
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
            // Unique per (name, duration, timestamp) so distinct test records
            // don't accidentally dedup; the dedup test sets ids explicitly.
            render_id: format!("{}-{}-{}", name, duration_ms, ts.unix_timestamp()),
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
            expiry_date: None,
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

        // Volume by day: 3 on the 9th, 1 on the 8th (that one failed).
        assert_eq!(s.volume_by_day.len(), 2);
        assert_eq!(s.volume_by_day[0].renders, 1); // 2026-07-08
        assert_eq!(s.volume_by_day[0].failures, 1);
        assert_eq!(s.volume_by_day[1].renders, 3); // 2026-07-09
        assert_eq!(s.volume_by_day[1].failures, 0);

        // Duration per day (successful only): 07-08 excluded (its render failed),
        // leaving 07-09 with [100,200,300] → avg 200, p90 300.
        assert_eq!(s.duration_by_day.len(), 1);
        assert_eq!(s.duration_by_day[0].avg_duration_ms, 200.0);
        assert_eq!(s.duration_by_day[0].p90_duration_ms, 300);
        // Nearest-rank over [100,200,300]: p95/p99 both land on the top sample.
        assert_eq!(s.duration_by_day[0].p95_duration_ms, 300);
        assert_eq!(s.duration_by_day[0].p99_duration_ms, 300);

        // Latency histogram over successes [100,200,300]: <250 has 2, <500 has 1.
        assert_eq!(
            s.duration_histogram.len(),
            DURATION_BUCKET_EDGES_MS.len() + 1
        );
        assert_eq!(s.duration_histogram[0].upper_ms, Some(100));
        assert_eq!(s.duration_histogram[0].count, 0);
        assert_eq!(s.duration_histogram[1].count, 2); // 100, 200
        assert_eq!(s.duration_histogram[2].count, 1); // 300
        assert_eq!(s.duration_histogram.last().unwrap().upper_ms, None);

        // Templates sorted by count desc: invoice(3) then letter(1).
        assert_eq!(s.templates[0].template_name, "invoice");
        assert_eq!(s.templates[0].total_renders, 3);
        assert_eq!(s.templates[1].template_name, "letter");
        assert_eq!(s.templates[1].total_renders, 1);
        // Per-template recent is newest-first.
        assert_eq!(s.templates[0].recent[0].duration_ms, 300);
        // All records use tag "latest".
        assert_eq!(s.templates[0].by_tag.len(), 1);
        assert_eq!(s.templates[0].by_tag[0].tag, "latest");
        assert_eq!(s.templates[0].by_tag[0].renders, 3);

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
    fn test_by_tag_breakdown() {
        let now = datetime!(2026-07-09 12:00 UTC);
        let mut a = rec("invoice", datetime!(2026-07-09 10:00 UTC), true, 100);
        let mut b = rec("invoice", datetime!(2026-07-09 10:01 UTC), true, 100);
        let c = rec("invoice", datetime!(2026-07-09 10:02 UTC), true, 100); // tag "latest"
        a.template_tag = "v2".to_string();
        b.template_tag = "v2".to_string();

        let s = compute_summary(&[a, b, c], 10, now);
        let t = &s.templates[0];
        assert_eq!(t.total_renders, 3);
        // Ordered by count desc: v2 (2) before latest (1).
        assert_eq!(t.by_tag[0].tag, "v2");
        assert_eq!(t.by_tag[0].renders, 2);
        assert_eq!(t.by_tag[1].tag, "latest");
        assert_eq!(t.by_tag[1].renders, 1);
    }

    #[test]
    fn test_compute_summary_dedups_by_render_id() {
        let now = datetime!(2026-07-09 12:00 UTC);
        // Two raw records for ONE render_id (an earlier failed attempt and a later
        // success) must count ONCE, keeping the latest.
        let early = rec("invoice", datetime!(2026-07-09 10:00 UTC), false, 100);
        let mut late = early.clone();
        late.timestamp = datetime!(2026-07-09 11:00 UTC);
        late.success = true;
        late.error = None;
        assert_eq!(early.render_id, late.render_id);
        let other = rec("invoice", datetime!(2026-07-09 11:30 UTC), true, 200);

        let s = compute_summary(&[early, late, other], 10, now);
        // 2 distinct render_ids, not 3 raw records.
        assert_eq!(s.totals.renders_24h, 2);
        // Deduped record keeps the latest (success) => 100% success.
        assert_eq!(s.totals.success_rate_24h, 1.0);
        assert_eq!(s.recent.len(), 2);
        assert_eq!(s.volume_by_day.iter().map(|v| v.renders).sum::<u64>(), 2);
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
