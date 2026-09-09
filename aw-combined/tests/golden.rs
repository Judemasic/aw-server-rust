//! Golden tests for the public pipeline — the 04 §6 worked example plus every rule from the
//! roadmap 3.2 plan, asserting exact boundaries and states, not just counts.

use std::collections::HashMap;

use aw_combined::{
    coalesce, compute_segments, default_min_contention, ActiveSlice, BucketEvents, PipelineInput,
    Segment, SegmentState, EVENT_ORIGIN_KEY,
};
use aw_models::Event;
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::{json, Map, Value};

fn t(min: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 9, 14, 0, 0).unwrap() + Duration::minutes(min)
}
fn ts(sec: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 9, 14, 0, 0).unwrap() + Duration::seconds(sec)
}

fn app(name: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("app".to_string(), json!(name));
    m
}

/// Event with `$aw.origin.device` set, spanning `[start, start+dur)`.
fn tagged(start: DateTime<Utc>, end: DateTime<Utc>, device: &str, mut data: Map<String, Value>) -> Event {
    data.insert(EVENT_ORIGIN_KEY.to_string(), json!(device));
    Event {
        id: None,
        timestamp: start,
        duration: end - start,
        data,
    }
}
fn plain(start: DateTime<Utc>, end: DateTime<Utc>, data: Map<String, Value>) -> Event {
    Event {
        id: None,
        timestamp: start,
        duration: end - start,
        data,
    }
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

fn devices(seg: &Segment) -> Vec<String> {
    let mut d: Vec<String> = seg.active.iter().map(|s| s.device.clone()).collect();
    d.sort();
    d.dedup();
    d
}

// 1. 04 §6 worked example.
#[test]
fn worked_example() {
    let input = base(vec![
        BucketEvents {
            bucket_id: "aw-watcher-window_phone".to_string(),
            events: vec![plain(t(0), t(60), app("YouTube"))],
        },
        BucketEvents {
            bucket_id: "aw-watcher-window-synced-from-tablet".to_string(),
            events: vec![tagged(t(30), t(45), "tablet", app("Kindle"))],
        },
    ]);
    let segs = compute_segments(input);
    assert_eq!(segs.len(), 3);

    assert_eq!((segs[0].start, segs[0].end), (t(0), t(30)));
    assert_eq!(segs[0].state, SegmentState::Settled);
    assert_eq!(devices(&segs[0]), vec!["phone"]);

    assert_eq!((segs[1].start, segs[1].end), (t(30), t(45)));
    assert_eq!(segs[1].state, SegmentState::Contended);
    assert_eq!(devices(&segs[1]), vec!["phone", "tablet"]);

    assert_eq!((segs[2].start, segs[2].end), (t(45), t(60)));
    assert_eq!(segs[2].state, SegmentState::Settled);
    assert_eq!(devices(&segs[2]), vec!["phone"]);
}

// 2. Three devices (R1) — one Contended segment with three distinct devices.
#[test]
fn three_devices() {
    let input = base(vec![
        BucketEvents {
            bucket_id: "aw-watcher-window_phone".to_string(),
            events: vec![plain(t(0), t(30), app("YouTube"))],
        },
        BucketEvents {
            bucket_id: "w-synced-from-tablet".to_string(),
            events: vec![tagged(t(0), t(30), "tablet", app("Kindle"))],
        },
        BucketEvents {
            bucket_id: "w-synced-from-laptop".to_string(),
            events: vec![tagged(t(0), t(30), "laptop", app("Firefox"))],
        },
    ]);
    let segs = compute_segments(input);
    assert_eq!(segs.len(), 1);
    assert_eq!(segs[0].state, SegmentState::Contended);
    assert_eq!(devices(&segs[0]), vec!["laptop", "phone", "tablet"]);
}

// 3. Untagged import, hostname in map -> mapped UUID.
#[test]
fn untagged_hostname_in_map() {
    let mut input = base(vec![BucketEvents {
        bucket_id: "aw-watcher-android-synced-from-jude_s_s25_ultra".to_string(),
        events: vec![plain(t(0), t(10), app("Maps"))],
    }]);
    input
        .hostname_to_uuid
        .insert("jude_s_s25_ultra".to_string(), "s25-uuid".to_string());
    let segs = compute_segments(input);
    assert_eq!(segs.len(), 1);
    assert_eq!(devices(&segs[0]), vec!["s25-uuid"]);
}

// 4. Untagged import, hostname not in map -> raw hostname, event still present.
#[test]
fn untagged_hostname_not_in_map() {
    let input = base(vec![BucketEvents {
        bucket_id: "aw-watcher-android-synced-from-jude_s_s25_ultra".to_string(),
        events: vec![plain(t(0), t(10), app("Maps"))],
    }]);
    let segs = compute_segments(input);
    assert_eq!(segs.len(), 1);
    assert_eq!(devices(&segs[0]), vec!["jude_s_s25_ultra"]);
}

// 5. UUID-suffixed bucket, not in map -> that UUID (the roadmap 1.5 case).
#[test]
fn uuid_suffixed_bucket() {
    let input = base(vec![BucketEvents {
        bucket_id: "aw-stopwatch-synced-from-ad0c6c34-d388-4ef0-b906-976bd760b22d".to_string(),
        events: vec![plain(t(0), t(10), app("stopwatch"))],
    }]);
    let segs = compute_segments(input);
    assert_eq!(segs.len(), 1);
    assert_eq!(
        devices(&segs[0]),
        vec!["ad0c6c34-d388-4ef0-b906-976bd760b22d"]
    );
}

// 6. Short contended run absorbed — one isolated 40 s overlap -> Settled + flag.
#[test]
fn short_contention_absorbed() {
    let input = base(vec![
        BucketEvents {
            bucket_id: "aw-watcher-window_phone".to_string(),
            events: vec![plain(ts(0), ts(40), app("YouTube"))],
        },
        BucketEvents {
            bucket_id: "w-synced-from-tablet".to_string(),
            events: vec![tagged(ts(0), ts(40), "tablet", app("Kindle"))],
        },
    ]);
    let segs = compute_segments(input);
    assert_eq!(segs.len(), 1);
    assert_eq!(segs[0].state, SegmentState::Settled);
    assert!(segs[0].absorbed_short_contention);
    // Slices are kept for 3.3.
    assert_eq!(devices(&segs[0]), vec!["phone", "tablet"]);
}

// 7. Run threshold — two adjacent 40 s contended segments, total 80 s -> both stay Contended.
#[test]
fn contended_run_over_threshold_stays() {
    let input = base(vec![
        BucketEvents {
            bucket_id: "aw-watcher-window_phone".to_string(),
            events: vec![
                plain(ts(0), ts(40), app("YouTube")),
                plain(ts(40), ts(80), app("Twitch")),
            ],
        },
        BucketEvents {
            bucket_id: "w-synced-from-tablet".to_string(),
            events: vec![tagged(ts(0), ts(80), "tablet", app("Kindle"))],
        },
    ]);
    let segs = compute_segments(input);
    assert_eq!(segs.len(), 2);
    assert_eq!((segs[0].start, segs[0].end), (ts(0), ts(40)));
    assert_eq!((segs[1].start, segs[1].end), (ts(40), ts(80)));
    assert_eq!(segs[0].state, SegmentState::Contended);
    assert_eq!(segs[1].state, SegmentState::Contended);
    assert!(!segs[0].absorbed_short_contention);
    assert!(!segs[1].absorbed_short_contention);
}

// 8. Exactly 60 s stays Contended (strictly-less-than test).
#[test]
fn exactly_min_contention_stays() {
    let input = base(vec![
        BucketEvents {
            bucket_id: "aw-watcher-window_phone".to_string(),
            events: vec![plain(ts(0), ts(60), app("YouTube"))],
        },
        BucketEvents {
            bucket_id: "w-synced-from-tablet".to_string(),
            events: vec![tagged(ts(0), ts(60), "tablet", app("Kindle"))],
        },
    ]);
    let segs = compute_segments(input);
    assert_eq!(segs.len(), 1);
    assert_eq!(segs[0].state, SegmentState::Contended);
    assert!(!segs[0].absorbed_short_contention);
}

// 9. Idle subtraction removes the only overlap -> no Contended segment.
#[test]
fn idle_subtraction_removes_overlap() {
    let mut input = base(vec![
        BucketEvents {
            bucket_id: "aw-watcher-window_phone".to_string(),
            events: vec![plain(t(0), t(60), app("YouTube"))],
        },
        BucketEvents {
            bucket_id: "w-synced-from-tablet".to_string(),
            events: vec![tagged(t(20), t(40), "tablet", app("Kindle"))],
        },
    ]);
    input.idle = vec![BucketEvents {
        bucket_id: "aw-watcher-afk_phone".to_string(),
        events: vec![plain(t(20), t(40), Map::new())],
    }];
    let segs = compute_segments(input);
    // No Contended segment anywhere: the only overlap was inside the phone's idle window.
    assert!(segs.iter().all(|s| s.state == SegmentState::Settled));
    // Phone contributes nothing in 14:20-14:40; tablet's activity there is Settled(tablet).
    let in_window: Vec<&Segment> = segs
        .iter()
        .filter(|s| s.start >= t(20) && s.end <= t(40))
        .collect();
    assert!(!in_window.is_empty());
    for s in in_window {
        assert_eq!(devices(s), vec!["tablet"]);
    }
}

// 10. Idle splits an interval into two Settled segments.
#[test]
fn idle_splits_interval() {
    let mut input = base(vec![BucketEvents {
        bucket_id: "aw-watcher-window_phone".to_string(),
        events: vec![plain(t(0), t(60), app("YouTube"))],
    }]);
    input.idle = vec![BucketEvents {
        bucket_id: "aw-watcher-afk_phone".to_string(),
        events: vec![plain(t(20), t(40), Map::new())],
    }];
    let segs = compute_segments(input);
    assert_eq!(segs.len(), 2);
    assert_eq!((segs[0].start, segs[0].end), (t(0), t(20)));
    assert_eq!((segs[1].start, segs[1].end), (t(40), t(60)));
    assert!(segs.iter().all(|s| s.state == SegmentState::Settled));
}

// 11. Atomic boundaries preserved — two consecutive activities on one device -> two segments,
//     each carrying its own data.
#[test]
fn atomic_boundaries_preserved() {
    let input = base(vec![BucketEvents {
        bucket_id: "aw-watcher-window_phone".to_string(),
        events: vec![
            plain(t(0), t(30), app("YouTube")),
            plain(t(30), t(60), app("Kindle")),
        ],
    }]);
    let segs = compute_segments(input);
    assert_eq!(segs.len(), 2);
    assert_eq!(segs[0].active[0].data.get("app").unwrap(), &json!("YouTube"));
    assert_eq!(segs[1].active[0].data.get("app").unwrap(), &json!("Kindle"));
}

// 12. Zero-duration events add no boundary.
#[test]
fn zero_duration_dropped() {
    let input = base(vec![
        BucketEvents {
            bucket_id: "aw-watcher-window_phone".to_string(),
            events: vec![plain(t(0), t(60), app("YouTube"))],
        },
        BucketEvents {
            bucket_id: "aw-watcher-android-unlock_phone".to_string(),
            events: vec![plain(t(30), t(30), Map::new())],
        },
    ]);
    let segs = compute_segments(input);
    assert_eq!(segs.len(), 1);
    assert_eq!((segs[0].start, segs[0].end), (t(0), t(60)));
}

// 13. Determinism — shuffled bucket and event order yields identical output.
#[test]
fn determinism_under_shuffle() {
    let make = |order: u8| {
        let phone = BucketEvents {
            bucket_id: "aw-watcher-window_phone".to_string(),
            events: vec![
                plain(t(0), t(30), app("YouTube")),
                plain(t(30), t(60), app("Kindle")),
            ],
        };
        let tablet = BucketEvents {
            bucket_id: "w-synced-from-tablet".to_string(),
            events: vec![
                tagged(t(45), t(50), "tablet", app("Maps")),
                tagged(t(10), t(20), "tablet", app("Slack")),
            ],
        };
        let laptop = BucketEvents {
            bucket_id: "w-synced-from-laptop".to_string(),
            events: vec![tagged(t(5), t(55), "laptop", app("Firefox"))],
        };
        let activity = match order {
            0 => vec![phone, tablet, laptop],
            1 => vec![laptop, phone, tablet],
            _ => vec![tablet, laptop, phone],
        };
        let mut input = base(activity);
        input.min_contention = Duration::seconds(1); // keep every contended run
        compute_segments(input)
    };
    let a = make(0);
    let b = make(1);
    let c = make(2);
    assert_eq!(a, b);
    assert_eq!(b, c);
    assert!(a.len() > 1);
}

// Extra: a device with two overlapping activity buckets is one device, two slices.
#[test]
fn two_slices_one_device_not_contended() {
    let input = base(vec![
        BucketEvents {
            bucket_id: "aw-watcher-window_phone".to_string(),
            events: vec![plain(t(0), t(30), app("YouTube"))],
        },
        BucketEvents {
            bucket_id: "aw-watcher-web_phone".to_string(),
            events: vec![plain(t(0), t(30), app("chrome-tab"))],
        },
    ]);
    let segs = compute_segments(input);
    assert_eq!(segs.len(), 1);
    assert_eq!(segs[0].active.len(), 2);
    assert_eq!(segs[0].state, SegmentState::Settled);
}

#[allow(dead_code)]
fn _slice_type_is_public(_: ActiveSlice) {}

// ---------------------------------------------------------------------------
// Roadmap 3.3 — ⑤ provisional attribution and ⑥ coalesce (worked examples).
// ---------------------------------------------------------------------------

// A1. Rule 1 (R17) actually discriminates: the longest-running *originating activity* is foreground,
//     not the lowest device UUID. Phone YouTube 14:00–15:00 (60 min) vs tablet Kindle 14:30–14:45
//     (15 min). The tablet's device string sorts LOWER than the phone's, so if source spans were
//     wrongly set to the segment's own length every slice would be equal and the tiebreak would pick
//     the tablet — this test would then fail, which is its whole point.
#[test]
fn attribution_longest_activity_wins_over_tiebreak() {
    let input = base(vec![
        BucketEvents {
            bucket_id: "aw-watcher-window_phone".to_string(),
            events: vec![plain(t(0), t(60), app("YouTube"))],
        },
        BucketEvents {
            bucket_id: "w-synced-from-tablet".to_string(),
            events: vec![tagged(t(30), t(45), "aaa-tablet", app("Kindle"))],
        },
    ]);
    let segs = compute_segments(input);
    assert_eq!(segs.len(), 3);
    // 14:30–14:45 is the contended overlap.
    let mid = &segs[1];
    assert_eq!((mid.start, mid.end), (t(30), t(45)));
    assert_eq!(mid.state, SegmentState::Contended);
    assert!(mid.unresolved);
    assert_eq!(mid.foreground_slice().device, "phone", "60 min beats 15 min despite UUID order");
    assert_eq!(mid.foreground_slice().data.get("app").unwrap(), &json!("YouTube"));
    let bg: Vec<_> = mid.background_slices().map(|s| s.device.as_str()).collect();
    assert_eq!(bg, vec!["aaa-tablet"]);
}

// A2. Coalesce merges a heartbeat-split session: one device, YouTube 14:00–14:30 then
//     YouTube 14:30–15:00 as two events with identical data -> two atomic segments, one after
//     coalesce spanning 14:00–15:00.
#[test]
fn coalesce_merges_heartbeat_split_session() {
    let input = base(vec![BucketEvents {
        bucket_id: "aw-watcher-window_phone".to_string(),
        events: vec![
            plain(t(0), t(30), app("YouTube")),
            plain(t(30), t(60), app("YouTube")),
        ],
    }]);
    let segs = compute_segments(input);
    assert_eq!(segs.len(), 2, "atomic: a boundary at every event edge");
    let merged = coalesce(segs);
    assert_eq!(merged.len(), 1);
    assert_eq!((merged[0].start, merged[0].end), (t(0), t(60)));
    // The merged foreground slice describes the whole span.
    assert_eq!(merged[0].foreground_slice().source_start, t(0));
    assert_eq!(merged[0].foreground_slice().source_end, t(60));
}

// A3. Coalesce does NOT merge across an app change.
#[test]
fn coalesce_keeps_app_change() {
    let input = base(vec![BucketEvents {
        bucket_id: "aw-watcher-window_phone".to_string(),
        events: vec![
            plain(t(0), t(30), app("YouTube")),
            plain(t(30), t(60), app("Kindle")),
        ],
    }]);
    let merged = coalesce(compute_segments(input));
    assert_eq!(merged.len(), 2);
}

// A4. Coalesce does NOT merge across a time gap, even with the same app either side.
#[test]
fn coalesce_keeps_gap() {
    let input = base(vec![BucketEvents {
        bucket_id: "aw-watcher-window_phone".to_string(),
        events: vec![
            plain(t(0), t(20), app("YouTube")),
            plain(t(30), t(50), app("YouTube")),
        ],
    }]);
    let merged = coalesce(compute_segments(input));
    assert_eq!(merged.len(), 2, "a 10-minute hole is not bridged");
}
