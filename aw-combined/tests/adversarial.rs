//! Edge cases the golden tests in `golden.rs` do not reach.
//!
//! `golden.rs` proves the pipeline computes the documented answer for well-formed input. These
//! probe the boundaries where it could quietly compute a *plausible* wrong one: what ends a
//! contended run, whose idle applies to whom, and what corrupt rows do. Each maps to a specific
//! way the implementation could have been written wrong and still passed `golden.rs`.

use std::collections::HashMap;

use aw_combined::{
    coalesce, compute_segments, BucketEvents, PipelineInput, SegmentState, EVENT_ORIGIN_KEY,
};
use aw_models::Event;
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::{json, Map, Value};

fn ts(sec: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 9, 14, 0, 0).unwrap() + Duration::seconds(sec)
}
fn tm(min: i64) -> DateTime<Utc> {
    ts(min * 60)
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
fn plain_at(s: DateTime<Utc>, e: DateTime<Utc>, d: Map<String, Value>) -> Event {
    Event { id: None, timestamp: s, duration: e - s, data: d }
}
fn tagged_at(s: DateTime<Utc>, e: DateTime<Utc>, dev: &str, mut d: Map<String, Value>) -> Event {
    d.insert(EVENT_ORIGIN_KEY.to_string(), json!(dev));
    Event { id: None, timestamp: s, duration: e - s, data: d }
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

// ---------------------------------------------------------------------------
// Roadmap 3.3 — ⑤ attribution tiebreaks and ⑥ coalesce edge cases.
// ---------------------------------------------------------------------------

/// Equal source durations -> the lexicographically lowest `device` wins (rule 2).
#[test]
fn tiebreak_equal_durations_lowest_device() {
    let input = base(vec![
        BucketEvents {
            bucket_id: "w-synced-from-b".into(),
            events: vec![tagged(0, 600, "device-b", app("X"))],
        },
        BucketEvents {
            bucket_id: "w-synced-from-a".into(),
            events: vec![tagged(0, 600, "device-a", app("Y"))],
        },
    ]);
    let segs = compute_segments(input);
    assert_eq!(segs.len(), 1);
    assert_eq!(segs[0].foreground_slice().device, "device-a");
}

/// One device, two overlapping buckets, equal spans -> lowest `bucket_id`; with equal bucket ids
/// (two overlapping events in one bucket) -> lowest canonical `data` string.
#[test]
fn tiebreak_within_one_device() {
    // Different bucket ids.
    let by_bucket = base(vec![
        BucketEvents {
            bucket_id: "aw-watcher-window_phone".into(),
            events: vec![plain(0, 600, app("win"))],
        },
        BucketEvents {
            bucket_id: "aw-watcher-web_phone".into(),
            events: vec![plain(0, 600, app("web"))],
        },
    ]);
    let segs = compute_segments(by_bucket);
    assert_eq!(segs[0].foreground_slice().bucket_id, "aw-watcher-web_phone");

    // Same bucket id, two overlapping events -> lowest serialised data.
    let by_data = base(vec![BucketEvents {
        bucket_id: "aw-watcher-window_phone".into(),
        events: vec![plain(0, 600, app("bbb")), plain(0, 600, app("aaa"))],
    }]);
    let segs = compute_segments(by_data);
    assert_eq!(segs[0].foreground_slice().data.get("app").unwrap(), &json!("aaa"));
}

/// Idle shortens a device's claim: A active 14:00–15:00 with idle 14:10–14:50 (10 min real in the
/// overlap window), B active 14:00–14:30 (30 min). In their contended overlap **B** is foreground —
/// the raw event span would have made A (60 min) win.
#[test]
fn idle_shortens_the_claim() {
    let mut input = base(vec![
        BucketEvents {
            bucket_id: "aw-watcher-window_phone".into(),
            events: vec![plain_at(tm(0), tm(60), app("A"))],
        },
        BucketEvents {
            bucket_id: "w-synced-from-b".into(),
            events: vec![tagged_at(tm(0), tm(30), "device-b", app("B"))],
        },
    ]);
    input.idle = vec![BucketEvents {
        bucket_id: "aw-watcher-afk_phone".into(),
        events: vec![plain_at(tm(10), tm(50), Map::new())],
    }];
    let segs = compute_segments(input);
    // Contended slice is 14:00–14:10: phone piece is 10 min post-idle, B is 30 min.
    let contended: Vec<_> = segs.iter().filter(|s| s.state == SegmentState::Contended).collect();
    assert_eq!(contended.len(), 1);
    assert_eq!((contended[0].start, contended[0].end), (tm(0), tm(10)));
    assert_eq!(contended[0].foreground_slice().device, "device-b");
}

/// `unresolved` tracks `Contended`, not device count: an absorbed short-contention segment has two
/// devices, `state == Settled`, `unresolved == false`, and still a valid foreground.
#[test]
fn unresolved_tracks_state_not_device_count() {
    let input = base(vec![
        BucketEvents {
            bucket_id: "aw-watcher-window_phone".into(),
            events: vec![plain(0, 40, app("A"))],
        },
        BucketEvents {
            bucket_id: "w-synced-from-tablet".into(),
            events: vec![tagged(0, 40, "tablet", app("B"))],
        },
    ]);
    let segs = compute_segments(input);
    assert_eq!(segs.len(), 1);
    assert_eq!(segs[0].state, SegmentState::Settled);
    assert!(segs[0].absorbed_short_contention);
    assert!(!segs[0].unresolved, "demoted to Settled -> not shaded");
    assert!(segs[0].foreground < segs[0].active.len());
}

/// Coalesce does not merge across a flag change: same foreground activity either side, but one
/// segment `Contended` and the next `Settled`.
#[test]
fn coalesce_keeps_flag_change() {
    let input = base(vec![
        BucketEvents {
            bucket_id: "aw-watcher-window_phone".into(),
            events: vec![plain_at(tm(0), tm(120), app("A"))],
        },
        BucketEvents {
            bucket_id: "w-synced-from-b".into(),
            events: vec![tagged_at(tm(0), tm(60), "device-b", app("B"))],
        },
    ]);
    let merged = coalesce(compute_segments(input));
    // 14:00–15:00 contended (phone foreground, 120 min > 60 min), 15:00–16:00 settled phone.
    assert_eq!(merged.len(), 2);
    assert_eq!(merged[0].state, SegmentState::Contended);
    assert_eq!(merged[1].state, SegmentState::Settled);
    assert_eq!(merged[0].foreground_slice().device, "phone");
    assert_eq!(merged[1].foreground_slice().device, "phone");
}

/// Determinism (R18): shuffling bucket and event order leaves `compute_segments` then `coalesce`
/// byte-identical, including a segment with two slices tied on duration *and* device so tiebreaks
/// 3 and 4 are exercised.
#[test]
fn determinism_through_coalesce() {
    let build = |order: u8| {
        let win = BucketEvents {
            bucket_id: "aw-watcher-window_phone".into(),
            events: vec![plain(0, 600, app("bbb")), plain(0, 600, app("aaa"))],
        };
        let web = BucketEvents {
            bucket_id: "aw-watcher-web_phone".into(),
            events: vec![plain(0, 600, app("ccc"))],
        };
        let tablet = BucketEvents {
            bucket_id: "w-synced-from-tablet".into(),
            events: vec![tagged(200, 800, "tablet", app("Z"))],
        };
        let activity = match order {
            0 => vec![win, web, tablet],
            1 => vec![tablet, win, web],
            _ => vec![web, tablet, win],
        };
        let mut input = base(activity);
        input.min_contention = Duration::seconds(1);
        coalesce(compute_segments(input))
    };
    assert_eq!(build(0), build(1));
    assert_eq!(build(1), build(2));
}

/// The bug found on hardware in roadmap 3.4: one device split into two by per-event origin
/// resolution, then read as contention with itself.
///
/// A `-synced-from-<peer>` bucket accumulates. Events merged before roadmap 3.1 carry no
/// `$aw.origin.device`; events merged after it do. Resolved per *event*, the untagged ones became
/// the hostname and the tagged ones the UUID, so the same tablet was two devices overlapping in
/// time and the combined view reported *"Syncthing-Fork counted for 10m -- also running:
/// Syncthing-Fork"*. Resolved per *bucket*, one tagged event settles all of them.
#[test]
fn one_bucket_is_one_device_even_when_only_some_events_are_tagged() {
    let input = base(vec![BucketEvents {
        bucket_id: "aw-watcher-android-synced-from-jude_s_tab_s10_fe".into(),
        // The untagged (pre-3.1) event overlaps the tagged (post-3.1) one exactly.
        events: vec![
            plain(0, 600, app("Syncthing-Fork")),
            tagged(0, 600, "7b54cfe9-uuid", app("Syncthing-Fork")),
        ],
    }]);
    let segs = compute_segments(input);
    assert_eq!(segs.len(), 1);
    let devs: Vec<&str> = {
        let mut d: Vec<&str> = segs[0].active.iter().map(|a| a.device.as_str()).collect();
        d.sort();
        d.dedup();
        d
    };
    assert_eq!(devs, vec!["7b54cfe9-uuid"], "one bucket must be one device");
    assert_eq!(
        segs[0].state,
        SegmentState::Settled,
        "a device cannot contend with itself"
    );
    assert!(!segs[0].unresolved, "nothing to shade: there is only one device here");
}

/// The tag wins over the bucket suffix for the *whole* bucket, including its untagged events --
/// the hostname is only a fallback for a bucket nothing in which has ever been tagged.
#[test]
fn untagged_events_follow_their_bucket_tag_not_the_hostname() {
    let input = base(vec![BucketEvents {
        bucket_id: "aw-watcher-android-synced-from-somehost".into(),
        events: vec![
            plain(0, 100, app("A")),
            tagged(200, 300, "peer-uuid", app("B")),
        ],
    }]);
    let segs = compute_segments(input);
    assert_eq!(segs.len(), 2);
    for s in &segs {
        assert_eq!(s.active[0].device, "peer-uuid");
    }
}

/// A bucket with no tagged event at all still falls back to the hostname captured from its id,
/// so a peer that has never synced since 3.1 stays visible as its own device (R19).
#[test]
fn wholly_untagged_bucket_still_falls_back_to_hostname() {
    let input = base(vec![BucketEvents {
        bucket_id: "aw-watcher-android-synced-from-jude_s_tab_s10_fe".into(),
        events: vec![plain(0, 100, app("A"))],
    }]);
    let segs = compute_segments(input);
    assert_eq!(segs[0].active[0].device, "jude_s_tab_s10_fe");
}
