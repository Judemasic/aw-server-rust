//! R6 — "the invariant everything else rests on" — asserted as a property, not one worked example.
//!
//! R6: at any instant exactly one activity is foreground, so the combined track's total equals
//! wall-clock time the person was active on *some* device, never the sum of the devices' separate
//! totals. R17: provisional attribution gives every segment a single foreground slice even with no
//! decision. These check both hold across several shaped inputs, and that `coalesce` conserves the
//! total. See `aw-combined/src/attribute.rs` and `aw-combined/src/coalesce.rs`.

use std::collections::HashMap;

use aw_combined::{
    coalesce, compute_segments, default_min_contention, BucketEvents, PipelineInput, Segment,
    EVENT_ORIGIN_KEY,
};
use aw_models::Event;
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::{json, Map, Value};

fn t(min: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 9, 14, 0, 0).unwrap() + Duration::minutes(min)
}
fn app(name: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("app".to_string(), json!(name));
    m
}
fn plain(start: DateTime<Utc>, end: DateTime<Utc>, data: Map<String, Value>) -> Event {
    Event { id: None, timestamp: start, duration: end - start, data }
}
fn tagged(start: DateTime<Utc>, end: DateTime<Utc>, dev: &str, mut data: Map<String, Value>) -> Event {
    data.insert(EVENT_ORIGIN_KEY.to_string(), json!(dev));
    Event { id: None, timestamp: start, duration: end - start, data }
}
fn base(activity: Vec<BucketEvents>) -> PipelineInput {
    PipelineInput {
        own_device: "phone".to_string(),
        hostname_to_uuid: HashMap::new(),
        activity,
        idle: vec![],
        min_contention: default_min_contention(),
    }
}

/// Property 1 (sorted, non-overlapping) + property 2 (exactly one valid foreground per segment).
fn assert_core(segs: &[Segment]) {
    for w in segs.windows(2) {
        assert!(w[0].end <= w[1].start, "segments overlap or are unsorted: {:?} then {:?}", w[0], w[1]);
    }
    for s in segs {
        assert!(!s.active.is_empty(), "segment has no slices");
        assert!(s.foreground < s.active.len(), "foreground index {} out of range {}", s.foreground, s.active.len());
    }
}

fn total_secs(segs: &[Segment]) -> i64 {
    segs.iter().map(|s| (s.end - s.start).num_seconds()).sum()
}

/// Property 3: totals equal wall-clock. Input hand-built so the covered time is computable:
/// phone YouTube 14:00–15:00 with an idle hole 14:10–14:20; tablet Kindle 14:30–15:30 (overlap);
/// phone Slack 16:00–16:15 after a gap. Post-idle union:
///   14:00–14:10 (600s) ∪ 14:20–15:30 (4200s) ∪ 16:00–16:15 (900s) = 5700s.
/// NOT the sum of device durations (50 + 60 + 15 = 125 min = 7500s), which R6 exists to prevent.
#[test]
fn totals_equal_wall_clock_not_device_sum() {
    let mut input = base(vec![
        BucketEvents {
            bucket_id: "aw-watcher-window_phone".to_string(),
            events: vec![
                plain(t(0), t(60), app("YouTube")),
                plain(t(120), t(135), app("Slack")),
            ],
        },
        BucketEvents {
            bucket_id: "w-synced-from-tablet".to_string(),
            events: vec![tagged(t(30), t(90), "tablet", app("Kindle"))],
        },
    ]);
    input.idle = vec![BucketEvents {
        bucket_id: "aw-watcher-afk_phone".to_string(),
        events: vec![plain(t(10), t(20), Map::new())],
    }];
    // Keep the 14:30–15:00 contention visible (30 min > default 1 min).
    let segs = compute_segments(input);
    assert_core(&segs);
    assert_eq!(total_secs(&segs), 5700, "combined total must be the union measure, not the device sum");

    // Property 4: coalesce conserves the total and keeps property 1.
    let merged = coalesce(segs.clone());
    assert_core(&merged);
    assert_eq!(total_secs(&merged), 5700, "coalesce changed the total");
    assert!(merged.len() <= segs.len());
}

/// Property 5: a device with two overlapping buckets still yields one foreground per segment and a
/// total equal to that device's single covered span (the two buckets do not double-count).
#[test]
fn two_overlapping_buckets_one_device() {
    let input = base(vec![
        BucketEvents {
            bucket_id: "aw-watcher-window_phone".to_string(),
            events: vec![plain(t(0), t(60), app("Firefox"))],
        },
        BucketEvents {
            bucket_id: "aw-watcher-web_phone".to_string(),
            events: vec![plain(t(15), t(45), app("gmail"))],
        },
    ]);
    let segs = compute_segments(input);
    assert_core(&segs);
    assert_eq!(total_secs(&segs), 3600, "one device active 14:00–15:00 is 3600s regardless of bucket count");
    for s in &segs {
        assert_eq!(s.foreground_slice().device, "phone");
    }
    let merged = coalesce(segs.clone());
    assert_core(&merged);
    assert_eq!(total_secs(&merged), 3600);
}

/// Property 1–4 over a busy three-device input.
#[test]
fn shaped_three_device_chained() {
    let mut input = base(vec![
        BucketEvents {
            bucket_id: "aw-watcher-window_phone".to_string(),
            events: vec![plain(t(0), t(50), app("YouTube")), plain(t(90), t(120), app("Maps"))],
        },
        BucketEvents {
            bucket_id: "w-synced-from-tablet".to_string(),
            events: vec![tagged(t(20), t(70), "tablet", app("Kindle"))],
        },
        BucketEvents {
            bucket_id: "w-synced-from-laptop".to_string(),
            events: vec![tagged(t(40), t(100), "laptop", app("Slack"))],
        },
    ]);
    input.min_contention = Duration::seconds(1);
    let segs = compute_segments(input);
    assert_core(&segs);
    // Intervals [0,50] ∪ [20,70] ∪ [40,100] ∪ [90,120] collapse to one unbroken [0,120]:
    // 120 min = 7200s. The device-duration sum (50 + 30 + 50 + 60 = 190 min) is what R6 rejects.
    assert_eq!(total_secs(&segs), 7200);
    let merged = coalesce(segs.clone());
    assert_core(&merged);
    assert_eq!(total_secs(&merged), 7200);
}
