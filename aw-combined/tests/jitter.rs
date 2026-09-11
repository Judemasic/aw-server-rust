//! Roadmap 4.5c — a hole of a few milliseconds is not a gap, and one app is one block.
//!
//! These are regression tests for a bug that no unit test could have found, because every unit test
//! in the crate built its segments so that they met exactly. Real watcher events do not:
//!
//! ```text
//! 08:17:52.436  9.108s  Photos       HomeActivity
//! 08:18:01.558  4.226s  Photos       StoryViewActivity      <- 14ms hole before it
//! 08:18:05.792  4.907s  Photos       HomeActivity           <-  8ms hole before it
//! 08:18:11.233  1.570s  Photos       HomeActivity           <- 534ms hole before it
//! 08:18:14.668  2.419s  One UI Home  Launcher
//! 08:18:18.095  2.164s  One UI Home  Launcher               <- 1.008s hole before it
//! ```
//!
//! The owner, reading that day: *"see how there are repeats even though they are after each other
//! and are the same thing — why?"* Three separate reasons, all fixed here: the holes defeated
//! contiguity, the differing `classname` defeated the merge, and with no neighbour to be absorbed
//! into, the sliver threshold did nothing at all.

use std::collections::HashMap;

use aw_combined::{
    coalesce, compute_segments, default_min_contention, smooth, BucketEvents, PipelineInput,
    Segment, SmoothOptions, EVENT_ORIGIN_KEY,
};
use aw_models::Event;
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::{json, Map, Value};

/// Milliseconds since a fixed start, so a test can put a 33ms hole between two events.
fn at(ms: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 11, 8, 0, 0).unwrap() + Duration::milliseconds(ms)
}

/// One Android-shaped event: an app *and* the screen inside it.
fn screen(a_ms: i64, b_ms: i64, app: &str, classname: &str) -> Event {
    let mut data = Map::new();
    data.insert("app".to_string(), json!(app));
    data.insert("classname".to_string(), json!(classname));
    data.insert(EVENT_ORIGIN_KEY.to_string(), json!("phone"));
    Event {
        id: None,
        timestamp: at(a_ms),
        duration: at(b_ms) - at(a_ms),
        data,
    }
}

fn event(a_ms: i64, b_ms: i64, app: &str) -> Event {
    screen(a_ms, b_ms, app, "MainActivity")
}

fn input(events: Vec<Event>) -> PipelineInput {
    PipelineInput {
        own_device: "phone".to_string(),
        hostname_to_uuid: HashMap::new(),
        activity: vec![BucketEvents {
            bucket_id: "aw-watcher-android_phone".to_string(),
            events,
        }],
        idle: Vec::new(),
        min_contention: default_min_contention(),
        decisions: Vec::new(),
        not_counted: Vec::new(),
    }
}

/// The day as the view gets it: ⑥ then ⑦.
fn day(events: Vec<Event>, sliver_secs: i64) -> Vec<Segment> {
    smooth(
        coalesce(compute_segments(input(events))),
        SmoothOptions::from_sliver_secs(sliver_secs),
    )
}

fn labels(segs: &[Segment]) -> Vec<String> {
    segs.iter()
        .map(|s| aw_combined::activity_label(&s.foreground_slice().data))
        .collect()
}

fn counted_ms(segs: &[Segment]) -> i64 {
    segs.iter()
        .map(|s| s.counted_span().num_milliseconds())
        .sum()
}

#[test]
fn a_hole_of_a_few_milliseconds_is_not_a_gap() {
    // Two stretches of one app, 33ms apart -- the largest sub-50ms hole on the measured day.
    let segs = day(
        vec![event(0, 9_000, "Photos"), event(9_033, 20_000, "Photos")],
        15,
    );
    assert_eq!(labels(&segs), vec!["Photos"], "one app, one block");
}

#[test]
fn the_bridged_hole_is_drawn_over_but_never_counted() {
    let segs = day(
        vec![event(0, 9_000, "Photos"), event(9_033, 20_000, "Photos")],
        15,
    );
    let seg = &segs[0];
    assert_eq!(
        (seg.end - seg.start).num_milliseconds(),
        20_000,
        "the block is drawn across the whole span"
    );
    assert_eq!(seg.bridged_ms, 33, "and remembers what it drew over");
    assert_eq!(
        seg.counted_span().num_milliseconds(),
        19_967,
        "but counts only what a watcher actually recorded"
    );
}

#[test]
fn a_real_gap_still_breaks_the_day() {
    // Eight seconds: the shortest *real* absence measured on the owner's day, and four times the
    // jitter tolerance. Time away is time away and must not be papered over.
    let segs = day(
        vec![event(0, 9_000, "Photos"), event(17_000, 30_000, "Photos")],
        0,
    );
    assert_eq!(labels(&segs), vec!["Photos", "Photos"]);
}

#[test]
fn one_app_with_two_screens_is_one_block_and_two_shares() {
    let segs = day(
        vec![
            screen(0, 9_000, "Photos", "HomeActivity"),
            screen(9_000, 13_000, "Photos", "StoryViewActivity"),
            screen(13_000, 18_000, "Photos", "HomeActivity"),
        ],
        0, // smoothing off: this is ⑥'s doing alone
    );
    assert_eq!(labels(&segs), vec!["Photos"], "one stretch of Photos");

    let shares = &segs[0].foreground_shares;
    assert_eq!(shares.len(), 2, "two screens inside it");
    // Longest first: Home held 9s + 5s, Story held 4s.
    assert_eq!(shares[0].data["classname"], json!("HomeActivity"));
    assert_eq!(shares[0].ms, 14_000);
    assert_eq!(shares[1].data["classname"], json!("StoryViewActivity"));
    assert_eq!(shares[1].ms, 4_000);
    assert_eq!(
        shares.iter().map(|s| s.ms).sum::<i64>(),
        segs[0].counted_span().num_milliseconds(),
        "the shares account for exactly the block's counted time"
    );
}

/// The owner's own reading of 2026-09-11 08:17:52 onward, holes and screens as measured.
#[test]
fn the_owners_photos_stretch_stops_being_six_blocks() {
    let events = vec![
        screen(0, 215_615, "Photos", "HomeActivity"),
        screen(215_615, 226_694, "One UI Home", "Launcher"),
        screen(226_727, 235_835, "Photos", "HomeActivity"),
        screen(235_849, 240_075, "Photos", "StoryViewActivity"),
        screen(240_083, 244_990, "Photos", "HomeActivity"),
        screen(245_524, 247_094, "Photos", "HomeActivity"),
        screen(248_959, 251_378, "One UI Home", "Launcher"),
        screen(252_386, 254_550, "One UI Home", "Launcher"),
        screen(254_563, 332_323, "Photos", "HomeActivity"),
    ];
    let literal = day(events.clone(), 0);
    assert_eq!(
        labels(&literal),
        vec!["Photos", "One UI Home", "Photos", "One UI Home", "Photos",],
        "with smoothing off the launcher visits are still real, but each app draws once"
    );

    let smoothed = day(events.clone(), 15);
    assert_eq!(
        labels(&smoothed),
        vec!["Photos"],
        "at the owner's 15s the whole stretch is one block of Photos"
    );
    assert_eq!(
        smoothed[0].absorbed_labels,
        vec!["One UI Home".to_string()],
        "and it says what it swallowed rather than hiding it"
    );

    // Nothing was invented: the day still counts exactly what the watcher recorded.
    let recorded: i64 = events.iter().map(|e| e.duration.num_milliseconds()).sum();
    assert_eq!(counted_ms(&smoothed), recorded);
    assert_eq!(counted_ms(&literal), recorded);
}

/// The owner's own example: *"the smoothing should take youtube, photos, gallery, then youtube"*.
#[test]
fn a_detour_through_two_apps_is_still_went_and_came_back() {
    let segs = day(
        vec![
            event(0, 300_000, "YouTube"),
            event(300_000, 310_000, "Photos"),
            event(310_000, 322_000, "Gallery"),
            event(322_000, 600_000, "YouTube"),
        ],
        15,
    );
    assert_eq!(labels(&segs), vec!["YouTube"]);
    assert_eq!(
        segs[0].absorbed_labels,
        vec!["Gallery".to_string(), "Photos".to_string()]
    );
}

#[test]
fn a_run_of_slivers_lands_in_the_anchor_not_in_another_sliver() {
    // Three slivers in a row, each under the threshold but summing to more than it. Peeling from
    // the ends is what stops the middle one building a block of its own.
    let segs = day(
        vec![
            event(0, 300_000, "YouTube"),
            event(300_000, 310_000, "A"),
            event(310_000, 322_000, "B"),
            event(322_000, 336_000, "C"),
            event(336_000, 600_000, "YouTube"),
        ],
        15,
    );
    assert_eq!(labels(&segs), vec!["YouTube"]);
    assert_eq!(
        segs[0].absorbed_labels,
        vec!["A".to_string(), "B".to_string(), "C".to_string()]
    );
}

#[test]
fn an_unbracketed_run_stays_literal() {
    // Different apps on either side: there is no evidence this was one stretch of anything, so
    // rule 5 leaves it alone. Everything here is over the 5s noise floor.
    let segs = day(
        vec![
            event(0, 300_000, "YouTube"),
            event(300_000, 310_000, "A"),
            event(310_000, 322_000, "B"),
            event(322_000, 600_000, "Reddit"),
        ],
        15,
    );
    assert_eq!(labels(&segs), vec!["YouTube", "A", "B", "Reddit"]);
}

/// The bug in one assertion: with holes in the day, the threshold used to change nothing at all.
#[test]
fn the_threshold_does_something_at_last() {
    let events = vec![
        event(0, 60_000, "YouTube"),
        event(60_014, 68_000, "One UI Home"),
        event(68_033, 130_000, "YouTube"),
    ];
    assert_eq!(
        labels(&day(events.clone(), 0)).len(),
        3,
        "off means literal, whatever the holes"
    );
    assert_eq!(
        labels(&day(events.clone(), 15)),
        vec!["YouTube"],
        "and at 15s the launcher flick is rounded away"
    );
}

#[test]
fn raising_the_threshold_never_adds_blocks_on_a_jittery_day() {
    let events = vec![
        event(0, 30_000, "YouTube"),
        event(30_008, 34_000, "One UI Home"),
        event(34_033, 41_000, "YouTube"),
        event(41_534, 43_000, "Photos"),
        event(44_008, 47_000, "Photos"),
        event(47_013, 120_000, "Reddit"),
    ];
    let recorded: i64 = events.iter().map(|e| e.duration.num_milliseconds()).sum();
    let mut last = usize::MAX;
    for secs in [0, 5, 10, 15, 30, 60, 300] {
        let segs = day(events.clone(), secs);
        assert!(
            segs.len() <= last,
            "{secs}s produced more blocks than a smaller threshold"
        );
        assert_eq!(
            counted_ms(&segs),
            recorded,
            "{secs}s changed how much the day counts"
        );
        last = segs.len();
    }
}
