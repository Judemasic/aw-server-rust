//! Roadmap 4.2a — a windowed decision answers a **stretch of time**, not a cast of competitors.
//!
//! Every test here is a block whose cast changes part-way through, which is the shape 4.2's own
//! tests never built and the owner met immediately on hardware: `coalesce` glues neighbouring
//! segments whenever the *winner* is unchanged, so one block on screen routinely contains several
//! different casts, and the sheet records the cast of the glued block. Under 4.2's exact-signature
//! matching that three-way cast matched none of the two-way segments underneath, and the answer
//! landed nowhere without any error.
//!
//! The second half of the rule is the one that keeps the fix honest: a `foreground` pick settles
//! only the time its activity was actually running (**R11** — a decision is data *about* events,
//! never an edit to them). The owner's own words for it: picking Spotify gets you the one second of
//! Spotify, and the stretches either side of it are still two open questions.

use std::collections::HashMap;

use aw_combined::{
    activity_label, coalesce, compute_segments, default_min_contention, merge_decisions,
    parse_records, BucketEvents, PipelineInput, Segment, SegmentState, EVENT_ORIGIN_KEY,
};
use aw_models::Event;
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::{json, Map, Value};

const PHONE: &str = "11111111-1111-1111-1111-111111111111";
const TABLET: &str = "22222222-2222-2222-2222-222222222222";

fn t(min: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 10, 10, 0, 0).unwrap() + Duration::minutes(min)
}

fn tagged(from: i64, to: i64, device: &str, name: &str) -> Event {
    let mut data = Map::new();
    data.insert("app".to_string(), json!(name));
    data.insert(EVENT_ORIGIN_KEY.to_string(), json!(device));
    Event { id: None, timestamp: t(from), duration: t(to) - t(from), data }
}

fn hostnames() -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("jude-phone".to_string(), PHONE.to_string());
    m.insert("jude-tablet".to_string(), TABLET.to_string());
    m
}

/// Run the day. `phone` and `tablet` are each that device's run of apps, back to back.
fn day(phone: Vec<Event>, tablet: Vec<Event>, lines: &[String]) -> Vec<Segment> {
    let input = PipelineInput {
        own_device: PHONE.to_string(),
        hostname_to_uuid: hostnames(),
        activity: vec![
            BucketEvents { bucket_id: "aw-watcher-window_phone".to_string(), events: phone },
            BucketEvents { bucket_id: "aw-watcher-window_tablet".to_string(), events: tablet },
        ],
        idle: vec![],
        min_contention: default_min_contention(),
        decisions: merge_decisions(&parse_records(&lines.join("\n"))),
    };
    coalesce(compute_segments(input))
}

/// A decision as the sheet writes it, with the cast it read off the **glued** block.
fn decision(window: (i64, i64), cast: &[(&str, &str)], resolution: Value, scope: &str) -> String {
    let participants: Vec<Value> = cast
        .iter()
        .map(|(device, app)| {
            let role = if *device == PHONE { "jude-phone" } else { "jude-tablet" };
            json!({ "device_role": role, "device_uuid": device, "app": app, "category": null })
        })
        .collect();
    json!({
        "id": "d_1",
        "type": "decision",
        "created_at": "2026-09-10T12:00:00Z",
        "created_by": PHONE,
        "window": { "start": t(window.0).to_rfc3339(), "end": t(window.1).to_rfc3339() },
        "signature": { "participants": participants },
        "resolution": resolution,
        "scope": scope,
    })
    .to_string()
}

fn picks(device: &str, app: &str) -> Value {
    let role = if device == PHONE { "jude-phone" } else { "jude-tablet" };
    json!({
        "outcome": "foreground",
        "foreground": { "device_role": role, "device_uuid": device, "app": app },
        "label": null,
        "deliberate_background": [],
    })
}

fn label(seg: &Segment) -> String {
    activity_label(&seg.foreground_slice().data)
}

/// The owner's first case: the **loser** changes part-way through and the winner does not.
///
/// Phone gaming for an hour; the tablet on YouTube for the first half and Spotify for the second.
/// One block on screen, a three-way cast, one answer — and the whole hour settles to the game.
#[test]
fn a_decision_settles_a_block_whose_loser_changes_part_way_through() {
    let segs = day(
        vec![tagged(0, 60, PHONE, "Game")],
        vec![tagged(0, 30, TABLET, "YouTube"), tagged(30, 60, TABLET, "Spotify")],
        &[decision(
            (0, 60),
            &[(PHONE, "Game"), (TABLET, "YouTube"), (TABLET, "Spotify")],
            picks(PHONE, "Game"),
            "once",
        )],
    );

    assert_eq!(segs.len(), 1, "one answer, one block: {segs:#?}");
    assert_eq!(segs[0].state, SegmentState::Settled);
    assert!(!segs[0].unresolved, "R26: it no longer asks");
    assert_eq!(segs[0].resolved_by.as_deref(), Some("d_1"));
    assert_eq!(segs[0].foreground_slice().device, PHONE);
    assert_eq!(label(&segs[0]), "Game");
    assert_eq!((segs[0].start, segs[0].end), (t(0), t(60)));
}

/// The owner's second case, in their own words: *"if i choose spotify then i will have 1 second of
/// spotify and still need two more decisions"*.
///
/// A pick settles the time it was running and **not one second more**. The stretches either side
/// are still contended, because nothing about them has been answered.
#[test]
fn a_pick_settles_only_the_time_it_was_actually_running() {
    let segs = day(
        vec![tagged(0, 60, PHONE, "Game")],
        vec![
            tagged(0, 29, TABLET, "YouTube"),
            tagged(29, 31, TABLET, "Spotify"),
            tagged(31, 60, TABLET, "YouTube"),
        ],
        &[decision(
            (0, 60),
            &[(PHONE, "Game"), (TABLET, "YouTube"), (TABLET, "Spotify")],
            picks(TABLET, "Spotify"),
            "once",
        )],
    );

    assert_eq!(segs.len(), 3, "the answer splits the block in three: {segs:#?}");

    assert_eq!(segs[1].state, SegmentState::Settled, "the Spotify stretch is answered");
    assert_eq!(segs[1].resolved_by.as_deref(), Some("d_1"));
    assert_eq!(segs[1].foreground_slice().device, TABLET);
    assert_eq!(label(&segs[1]), "Spotify");
    assert_eq!((segs[1].start, segs[1].end), (t(29), t(31)));

    for i in [0, 2] {
        assert_eq!(segs[i].state, SegmentState::Contended, "segment {i}: {:#?}", segs[i]);
        assert!(segs[i].unresolved, "segment {i} is still an open question");
        assert!(segs[i].resolved_by.is_none(), "segment {i} was never answered");
    }
}

/// The block the owner actually hit on the S25U: the **winner** stops running before the block ends.
///
/// Picking an activity must never credit it with time no watcher recorded, so the tail goes on
/// asking rather than being quietly swallowed (**R11**).
#[test]
fn a_pick_never_credits_an_activity_that_had_already_stopped() {
    let segs = day(
        vec![tagged(0, 50, PHONE, "ActivityWatch"), tagged(50, 60, PHONE, "One UI Home")],
        vec![tagged(0, 60, TABLET, "ActivityWatch")],
        &[decision(
            (0, 60),
            &[(PHONE, "ActivityWatch"), (PHONE, "One UI Home"), (TABLET, "ActivityWatch")],
            picks(PHONE, "ActivityWatch"),
            "once",
        )],
    );

    assert_eq!(segs.len(), 2, "{segs:#?}");

    assert_eq!(segs[0].state, SegmentState::Settled);
    assert_eq!(segs[0].resolved_by.as_deref(), Some("d_1"));
    assert_eq!(segs[0].foreground_slice().device, PHONE);
    assert_eq!(label(&segs[0]), "ActivityWatch");
    assert_eq!((segs[0].start, segs[0].end), (t(0), t(50)));

    assert_eq!(segs[1].state, SegmentState::Contended, "the tail was never answered");
    assert!(segs[1].unresolved);
    assert!(segs[1].resolved_by.is_none());
    assert!(
        !(segs[1].foreground_slice().device == PHONE && label(&segs[1]) == "ActivityWatch"),
        "the phone's launcher time must not be renamed to the app that was picked: {:#?}",
        segs[1]
    );
    assert!(
        segs[1].active.iter().any(|s| activity_label(&s.data) == "One UI Home"),
        "and the launcher is still in the data, untouched"
    );
}

/// A `once` decision no longer needs its cast to match anything: the window and the pick are the
/// whole test. This is the 4.2a change stated on its own.
#[test]
fn a_once_decision_no_longer_needs_its_cast_to_match() {
    let segs = day(
        vec![tagged(0, 60, PHONE, "Game")],
        vec![tagged(0, 60, TABLET, "YouTube")],
        &[decision(
            (0, 60),
            &[(PHONE, "something else entirely"), (TABLET, "and another")],
            picks(PHONE, "Game"),
            "once",
        )],
    );

    assert_eq!(segs.len(), 1);
    assert_eq!(segs[0].resolved_by.as_deref(), Some("d_1"));
    assert_eq!(label(&segs[0]), "Game");
}

/// `ignore` and `relabel` name the *time*, not a competitor, so a changing cast is irrelevant to
/// them and they cover the whole window.
#[test]
fn i_was_away_covers_the_whole_window_however_the_cast_changes() {
    let segs = day(
        vec![tagged(0, 60, PHONE, "Game")],
        vec![tagged(0, 30, TABLET, "YouTube"), tagged(30, 60, TABLET, "Spotify")],
        &[decision(
            (0, 60),
            &[(PHONE, "Game"), (TABLET, "YouTube")],
            json!({
                "outcome": "ignore",
                "foreground": null,
                "label": null,
                "deliberate_background": [],
            }),
            "once",
        )],
    );

    assert_eq!(segs.len(), 1, "{segs:#?}");
    assert!(segs[0].ignored, "every part of the window counts as nothing");
    assert_eq!(segs[0].state, SegmentState::Settled);
    assert_eq!((segs[0].start, segs[0].end), (t(0), t(60)));
}

/// R16 is unchanged: a **rule** is a statement about the cast, and still matches on it.
#[test]
fn a_rule_still_matches_on_the_cast_and_not_on_time() {
    let segs = day(
        vec![tagged(0, 60, PHONE, "Game")],
        vec![tagged(0, 60, TABLET, "Spotify")],
        &[decision(
            (0, 60),
            &[(PHONE, "Game"), (TABLET, "YouTube")],
            picks(PHONE, "Game"),
            "always",
        )],
    );

    assert_eq!(segs.len(), 1);
    assert!(
        segs[0].resolved_by.is_none(),
        "the rule names a cast this day never had: {segs:#?}"
    );
    assert!(segs[0].unresolved);
}

/// A rule whose cast matches but whose pick is not in this segment resolves nothing — the same
/// honesty rule as a windowed decision, reached by the other pass.
#[test]
fn a_rule_whose_pick_is_absent_resolves_nothing() {
    let segs = day(
        vec![tagged(0, 60, PHONE, "Game")],
        vec![tagged(0, 60, TABLET, "YouTube")],
        &[decision(
            (0, 60),
            &[(PHONE, "Game"), (TABLET, "YouTube")],
            picks(TABLET, "Kindle"),
            "always",
        )],
    );

    assert_eq!(segs.len(), 1);
    assert!(segs[0].resolved_by.is_none(), "{segs:#?}");
    assert!(segs[0].unresolved);
}
