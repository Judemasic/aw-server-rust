//! Edge cases the golden tests in `golden.rs` do not reach.
//!
//! `golden.rs` proves the pipeline computes the documented answer for well-formed input. These
//! probe the boundaries where it could quietly compute a *plausible* wrong one: what ends a
//! contended run, whose idle applies to whom, and what corrupt rows do. Each maps to a specific
//! way the implementation could have been written wrong and still passed `golden.rs`.

use std::collections::HashMap;

use aw_combined::{compute_segments, BucketEvents, PipelineInput, SegmentState, EVENT_ORIGIN_KEY};
use aw_models::Event;
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::{json, Map, Value};

fn ts(sec: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 9, 14, 0, 0).unwrap() + Duration::seconds(sec)
}
fn app(name: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("app".to_string(), json!(name));
    m
}
fn tagged(s: i64, e: i64, dev: &str, mut d: Map<String, Value>) -> Event {
    d.insert(EVENT_ORIGIN_KEY.to_string(), json!(dev));
    Event { id: None, timestamp: ts(s), duration: ts(e) - ts(s), data: d }
}
fn plain(s: i64, e: i64, d: Map<String, Value>) -> Event {
    Event { id: None, timestamp: ts(s), duration: ts(e) - ts(s), data: d }
}
fn base(activity: Vec<BucketEvents>) -> PipelineInput {
    PipelineInput {
        own_device: "phone".into(),
        hostname_to_uuid: HashMap::new(),
        activity,
        idle: vec![],
        min_contention: aw_combined::default_min_contention(),
    }
}

/// A gap in the middle must END a contended run, so two 40s halves separated by a gap are
/// each demoted independently rather than summed to 80s.
#[test]
fn gap_breaks_the_run() {
    let input = base(vec![
        BucketEvents {
            bucket_id: "aw-watcher-window_phone".into(),
            events: vec![plain(0, 40, app("A")), plain(100, 140, app("A"))],
        },
        BucketEvents {
            bucket_id: "w-synced-from-tablet".into(),
            events: vec![
                tagged(0, 40, "tablet", app("B")),
                tagged(100, 140, "tablet", app("B")),
            ],
        },
    ]);
    let segs = compute_segments(input);
    assert_eq!(segs.len(), 2, "gap must not be bridged");
    for s in &segs {
        assert_eq!(s.state, SegmentState::Settled, "each 40s run is short on its own");
        assert!(s.absorbed_short_contention);
    }
}

/// A Settled segment in the middle must END the run: 40s + settled + 40s stays two short runs.
#[test]
fn settled_segment_breaks_the_run() {
    let input = base(vec![
        BucketEvents {
            bucket_id: "aw-watcher-window_phone".into(),
            events: vec![plain(0, 120, app("A"))],
        },
        BucketEvents {
            bucket_id: "w-synced-from-tablet".into(),
            events: vec![tagged(0, 40, "tablet", app("B")), tagged(80, 120, "tablet", app("B"))],
        },
    ]);
    let segs = compute_segments(input);
    // 0-40 contended(40s), 40-80 settled(phone), 80-120 contended(40s)
    assert_eq!(segs.len(), 3);
    assert_eq!(segs[0].state, SegmentState::Settled);
    assert!(segs[0].absorbed_short_contention);
    assert_eq!(segs[1].state, SegmentState::Settled);
    assert!(!segs[1].absorbed_short_contention, "genuinely settled, not absorbed");
    assert_eq!(segs[2].state, SegmentState::Settled);
    assert!(segs[2].absorbed_short_contention);
}

/// Overlapping idle events on the same device must not resurrect activity.
#[test]
fn overlapping_idle_holes() {
    let mut input = base(vec![BucketEvents {
        bucket_id: "aw-watcher-window_phone".into(),
        events: vec![plain(0, 600, app("A"))],
    }]);
    input.idle = vec![BucketEvents {
        bucket_id: "aw-watcher-afk_phone".into(),
        events: vec![plain(100, 300, Map::new()), plain(200, 400, Map::new())],
    }];
    let segs = compute_segments(input);
    assert_eq!(segs.len(), 2);
    assert_eq!((segs[0].start, segs[0].end), (ts(0), ts(100)));
    assert_eq!((segs[1].start, segs[1].end), (ts(400), ts(600)));
}

/// Idle for device A must NOT subtract device B's activity.
#[test]
fn idle_is_per_device() {
    let mut input = base(vec![
        BucketEvents {
            bucket_id: "aw-watcher-window_phone".into(),
            events: vec![plain(0, 600, app("A"))],
        },
        BucketEvents {
            bucket_id: "w-synced-from-tablet".into(),
            events: vec![tagged(0, 600, "tablet", app("B"))],
        },
    ]);
    // idle belongs to the phone only
    input.idle = vec![BucketEvents {
        bucket_id: "aw-watcher-afk_phone".into(),
        events: vec![plain(100, 300, Map::new())],
    }];
    let segs = compute_segments(input);
    let covered: Vec<_> = segs
        .iter()
        .filter(|s| s.start >= ts(100) && s.end <= ts(300))
        .collect();
    assert!(!covered.is_empty());
    for s in covered {
        let devs: Vec<_> = s.active.iter().map(|a| a.device.as_str()).collect();
        assert_eq!(devs, vec!["tablet"], "phone idle must not remove tablet activity");
    }
}

/// Idle covering the whole activity leaves nothing at all.
#[test]
fn idle_swallows_everything() {
    let mut input = base(vec![BucketEvents {
        bucket_id: "aw-watcher-window_phone".into(),
        events: vec![plain(100, 200, app("A"))],
    }]);
    input.idle = vec![BucketEvents {
        bucket_id: "aw-watcher-afk_phone".into(),
        events: vec![plain(0, 600, Map::new())],
    }];
    assert!(compute_segments(input).is_empty());
}

/// Shuffling events WITHIN a bucket (not just bucket order) must not change output.
#[test]
fn determinism_within_bucket() {
    let build = |rev: bool| {
        let mut evs = vec![
            plain(0, 100, app("A")),
            plain(100, 200, app("B")),
            plain(200, 300, app("C")),
        ];
        if rev {
            evs.reverse();
        }
        let mut input = base(vec![
            BucketEvents { bucket_id: "aw-watcher-window_phone".into(), events: evs },
            BucketEvents {
                bucket_id: "w-synced-from-tablet".into(),
                events: vec![tagged(50, 250, "tablet", app("Z"))],
            },
        ]);
        input.min_contention = Duration::seconds(1);
        compute_segments(input)
    };
    assert_eq!(build(false), build(true));
}

/// Negative duration (corrupt row) must be dropped, not panic or invert.
#[test]
fn negative_duration_dropped() {
    let input = base(vec![BucketEvents {
        bucket_id: "aw-watcher-window_phone".into(),
        events: vec![
            Event { id: None, timestamp: ts(200), duration: Duration::seconds(-100), data: app("bad") },
            plain(0, 100, app("good")),
        ],
    }]);
    let segs = compute_segments(input);
    assert_eq!(segs.len(), 1);
    assert_eq!((segs[0].start, segs[0].end), (ts(0), ts(100)));
}

/// A non-string origin tag (corrupt) must fall through to the bucket-suffix rule, not panic.
#[test]
fn non_string_origin_tag_falls_through() {
    let mut d = app("A");
    d.insert(EVENT_ORIGIN_KEY.to_string(), json!(42));
    let input = base(vec![BucketEvents {
        bucket_id: "aw-watcher-window-synced-from-laptop".into(),
        events: vec![plain(0, 100, d)],
    }]);
    let segs = compute_segments(input);
    assert_eq!(segs[0].active[0].device, "laptop");
}

/// Three devices where only two overlap at a time, chained, forms ONE contended run.
#[test]
fn chained_contention_is_one_run() {
    let input = base(vec![
        BucketEvents {
            bucket_id: "aw-watcher-window_phone".into(),
            events: vec![plain(0, 40, app("A"))],
        },
        BucketEvents {
            bucket_id: "w-synced-from-tablet".into(),
            events: vec![tagged(0, 80, "tablet", app("B"))],
        },
        BucketEvents {
            bucket_id: "w-synced-from-laptop".into(),
            events: vec![tagged(40, 80, "laptop", app("C"))],
        },
    ]);
    // 0-40 phone+tablet, 40-80 tablet+laptop: contiguous, 80s total -> both stay Contended.
    let segs = compute_segments(input);
    assert_eq!(segs.len(), 2);
    assert_eq!(segs[0].state, SegmentState::Contended);
    assert_eq!(segs[1].state, SegmentState::Contended);
}
