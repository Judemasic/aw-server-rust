//! Roadmap 4.5 — smoothing, driven through the *whole* public pipeline rather than against
//! hand-built segments.
//!
//! The unit tests in `smooth.rs` prove the rules. These prove the thing the step was actually
//! opened for: that a real day's events, fed in as events, stop shattering into crumbs — and that
//! nothing about the underlying data changed to make that happen.

use std::collections::HashMap;

use aw_combined::{
    coalesce, compute_segments, default_min_contention, smooth, BucketEvents, PipelineInput,
    Segment, SmoothOptions, EVENT_ORIGIN_KEY,
};
use aw_models::Event;
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::{json, Map, Value};

fn ts(sec: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 10, 9, 0, 0).unwrap() + Duration::seconds(sec)
}

fn app(name: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("app".to_string(), json!(name));
    m
}

fn event(a: i64, b: i64, device: &str, name: &str) -> Event {
    let mut data = app(name);
    data.insert(EVENT_ORIGIN_KEY.to_string(), json!(device));
    Event {
        id: None,
        timestamp: ts(a),
        duration: ts(b) - ts(a),
        data,
    }
}

fn input(buckets: Vec<(&str, Vec<Event>)>) -> PipelineInput {
    PipelineInput {
        own_device: "phone".to_string(),
        hostname_to_uuid: HashMap::new(),
        activity: buckets
            .into_iter()
            .map(|(id, events)| BucketEvents {
                bucket_id: id.to_string(),
                events,
            })
            .collect(),
        idle: Vec::new(),
        min_contention: default_min_contention(),
        decisions: Vec::new(),
        not_counted: Vec::new(),
    }
}

/// The day as the view gets it: ⑥ then ⑦.
fn day(input: PipelineInput, sliver_secs: i64) -> Vec<Segment> {
    smooth(
        coalesce(compute_segments(input)),
        SmoothOptions::from_sliver_secs(sliver_secs),
    )
}

fn labels(segs: &[Segment]) -> Vec<String> {
    segs.iter()
        .map(|s| aw_combined::activity_label(&s.foreground_slice().data))
        .collect()
}

/// The case the owner is looking at: one stretch of one app, interrupted by a flick to the
/// launcher and back, drawn as three blocks.
#[test]
fn a_flick_to_the_launcher_and_back_stops_being_three_blocks() {
    let events = vec![
        event(0, 600, "phone", "YouTube"),
        event(600, 608, "phone", "One UI Home"),
        event(608, 1800, "phone", "YouTube"),
    ];

    let literal = day(input(vec![("aw-watcher-android", events.clone())]), 0);
    assert_eq!(
        labels(&literal),
        vec!["YouTube", "One UI Home", "YouTube"],
        "off means literal: every sliver still draws"
    );

    let smoothed = day(input(vec![("aw-watcher-android", events)]), 15);
    assert_eq!(labels(&smoothed), vec!["YouTube"]);
    assert_eq!(smoothed[0].start, ts(0));
    assert_eq!(smoothed[0].end, ts(1800));
    assert_eq!(smoothed[0].smoothed_seconds, 8);
    assert_eq!(
        smoothed[0].absorbed_labels,
        vec!["One UI Home".to_string()],
        "the block has to be able to say what it swallowed"
    );
}

/// The threshold is the whole setting: the same day at 5s and at 15s is two different drawings,
/// and neither is written anywhere.
#[test]
fn the_threshold_is_what_decides_and_the_day_recomputes_from_it() {
    let events = vec![
        event(0, 600, "phone", "YouTube"),
        event(600, 612, "phone", "One UI Home"),
        event(612, 1800, "phone", "YouTube"),
    ];
    let build = || input(vec![("aw-watcher-android", events.clone())]);

    assert_eq!(labels(&day(build(), 10)).len(), 3, "12s is over a 10s threshold");
    assert_eq!(labels(&day(build(), 15)).len(), 1, "and under a 15s one");
    // And back again from the same events: nothing was consumed on the way through.
    assert_eq!(labels(&day(build(), 10)).len(), 3);
}

/// Smoothing may never move a second out of the day. Absorbing a sliver grows the block that took
/// it, so the drawn total is the same drawn total.
#[test]
fn the_day_is_the_same_length_however_it_is_smoothed() {
    let events = vec![
        event(0, 600, "phone", "YouTube"),
        event(600, 603, "phone", "One UI Home"),
        event(603, 900, "phone", "Firefox"),
        event(900, 907, "phone", "One UI Home"),
        event(907, 1800, "phone", "Firefox"),
    ];
    let build = || input(vec![("aw-watcher-android", events.clone())]);
    let total = |segs: &[Segment]| -> i64 {
        segs.iter().map(|s| (s.end - s.start).num_seconds()).sum()
    };

    let literal = day(build(), 0);
    for secs in [5, 10, 15, 30, 60] {
        assert_eq!(
            total(&day(build(), secs)),
            total(&literal),
            "smoothing at {secs}s changed how much day there is"
        );
    }
}

/// Raising the threshold must never make the day *more* shattered.
///
/// Found on the owner's real day (2026-09-09, S25U) the first time this shipped: 0s gave 304 blocks
/// and 5s gave **306**. Rule 3 sent a bracketed sliver into its `prev` neighbour while the noise
/// floor sent the same sliver into its *longer* one, so turning smoothing up changed which block a
/// crumb joined and cost a merge further down. A setting that can shatter the day by being turned
/// up is not one anyone can reason about, so the target no longer depends on which rule fired.
#[test]
fn raising_the_threshold_never_adds_blocks() {
    // Two apps, several crumbs, and a title change inside one app -- the shapes that were
    // interacting badly. `A` twice with different titles never coalesces, which is the case that
    // made the choice of neighbour matter at all.
    let titled = |a: i64, b: i64, name: &str, title: &str| {
        let mut data = app(name);
        data.insert("title".to_string(), json!(title));
        data.insert(EVENT_ORIGIN_KEY.to_string(), json!("phone"));
        Event {
            id: None,
            timestamp: ts(a),
            duration: ts(b) - ts(a),
            data,
        }
    };
    let events = vec![
        titled(0, 600, "ActivityWatch", "one"),
        event(600, 603, "phone", "My Files"),
        titled(603, 611, "ActivityWatch", "two"),
        event(611, 618, "phone", "One UI Home"),
        titled(618, 1200, "ActivityWatch", "three"),
        event(1200, 1212, "phone", "One UI Home"),
        titled(1212, 2400, "ActivityWatch", "three"),
    ];

    let mut previous = usize::MAX;
    for secs in [0, 5, 10, 15, 20, 30, 60, 120] {
        let n = day(input(vec![("aw-watcher-android", events.clone())]), secs).len();
        assert!(
            n <= previous,
            "{secs}s gave {n} blocks where the threshold below it gave {previous}"
        );
        previous = n;
    }
}

/// A real overlap is a question, and a question is never rounded away — whatever the threshold is
/// set to. Two devices, both awake for four minutes, is well over the 60s contention floor.
#[test]
fn an_overlap_worth_asking_about_survives_any_threshold() {
    let events_a = vec![event(0, 240, "phone", "YouTube")];
    let events_b = vec![event(0, 240, "tablet", "Firefox")];

    for secs in [0, 15, 60, 600] {
        let segs = day(
            input(vec![
                ("aw-watcher-android", events_a.clone()),
                ("aw-watcher-window-synced-from-tablet", events_b.clone()),
            ]),
            secs,
        );
        assert_eq!(
            segs.iter().filter(|s| s.unresolved).count(),
            1,
            "the overlap stopped asking at a {secs}s threshold"
        );
    }
}
