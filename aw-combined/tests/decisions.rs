//! Step ④ — applying the owner's decisions (roadmap 4.2).
//!
//! Every test here builds the same two-device overlap the resolution sheet asks about — a phone on
//! YouTube and a tablet on Kindle, both awake for an hour — and then asserts what a decision does to
//! it. The roadmap's own check for 4.2 is *"resolve on A → after sync, B shows the same resolution
//! and no longer asks"*; the "and B agrees" half is
//! [`the_same_decision_reaches_the_same_answer_on_a_peer`], which runs the pipeline twice with the
//! devices' identities swapped round, because that is what a peer's copy of the same day looks like.

use std::collections::HashMap;

use aw_combined::{
    coalesce, compute_segments, default_min_contention, merge_decisions, parse_records,
    BucketEvents, PipelineInput, Segment, SegmentState, EVENT_ORIGIN_KEY,
};
use aw_models::Event;
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::{json, Map, Value};

const PHONE: &str = "11111111-1111-1111-1111-111111111111";
const TABLET: &str = "22222222-2222-2222-2222-222222222222";

fn t(min: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 10, 10, 0, 0).unwrap() + Duration::minutes(min)
}

fn app(name: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("app".to_string(), json!(name));
    m
}

fn tagged(start: DateTime<Utc>, end: DateTime<Utc>, device: &str, name: &str) -> Event {
    let mut data = app(name);
    data.insert(EVENT_ORIGIN_KEY.to_string(), json!(device));
    Event { id: None, timestamp: start, duration: end - start, data }
}

/// The hostname map the server passes in; ④ inverts it to get the role a signature names.
fn hostnames() -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("jude-phone".to_string(), PHONE.to_string());
    m.insert("jude-tablet".to_string(), TABLET.to_string());
    m
}

/// One hour of phone-YouTube against tablet-Kindle: a contended run well over `min_contention`.
fn overlap(lines: &[String], own: &str) -> Vec<Segment> {
    let records = parse_records(&lines.join("\n"));
    let input = PipelineInput {
        own_device: own.to_string(),
        hostname_to_uuid: hostnames(),
        activity: vec![
            BucketEvents {
                bucket_id: "aw-watcher-window_phone".to_string(),
                events: vec![tagged(t(0), t(60), PHONE, "YouTube")],
            },
            BucketEvents {
                bucket_id: "aw-watcher-window_tablet".to_string(),
                events: vec![tagged(t(0), t(60), TABLET, "Kindle")],
            },
        ],
        idle: vec![],
        min_contention: default_min_contention(),
        decisions: merge_decisions(&records),
    };
    coalesce(compute_segments(input))
}

/// A decision as the resolution sheet writes it. `window` is minutes from 10:00.
fn decision(
    id: &str,
    created_at: &str,
    created_by: &str,
    window: (i64, i64),
    resolution: Value,
    scope: &str,
) -> String {
    json!({
        "id": id,
        "type": "decision",
        "created_at": created_at,
        "created_by": created_by,
        "window": { "start": t(window.0).to_rfc3339(), "end": t(window.1).to_rfc3339() },
        "signature": { "participants": [
            { "device_role": "jude-phone", "device_uuid": PHONE, "app": "YouTube", "category": null },
            { "device_role": "jude-tablet", "device_uuid": TABLET, "app": "Kindle", "category": null },
        ]},
        "resolution": resolution,
        "scope": scope,
    })
    .to_string()
}

fn picks_kindle() -> Value {
    json!({
        "outcome": "foreground",
        "foreground": { "device_role": "jude-tablet", "device_uuid": TABLET, "app": "Kindle" },
        "label": null,
        "deliberate_background": ["YouTube"],
    })
}

#[test]
fn without_a_decision_the_overlap_is_still_asked_about() {
    let segs = overlap(&[], PHONE);
    assert_eq!(segs.len(), 1);
    assert_eq!(segs[0].state, SegmentState::Contended);
    assert!(segs[0].unresolved, "nothing has answered it yet");
    assert!(segs[0].resolved_by.is_none());
}

#[test]
fn an_exact_decision_settles_the_segment_and_credits_the_pick() {
    let segs = overlap(
        &[decision("d_1", "2026-09-10T11:00:00Z", PHONE, (0, 60), picks_kindle(), "once")],
        PHONE,
    );
    assert_eq!(segs.len(), 1);
    assert_eq!(segs[0].state, SegmentState::Settled);
    assert!(!segs[0].unresolved, "R26: it no longer asks");
    assert_eq!(segs[0].resolved_by.as_deref(), Some("d_1"));
    assert!(!segs[0].auto_resolved, "recorded for this window, not by a rule");
    assert_eq!(segs[0].foreground_slice().device, TABLET);
    assert_eq!(segs[0].deliberate_background, vec!["YouTube".to_string()]);
}

/// The window the sheet records comes from a *coalesced* block, so it is at least as wide as the
/// atomic segments underneath it. Covering has to be enough, or a decision matches nothing.
#[test]
fn a_wider_window_still_covers_the_segments_inside_it() {
    let segs = overlap(
        &[decision("d_1", "2026-09-10T11:00:00Z", PHONE, (-30, 90), picks_kindle(), "once")],
        PHONE,
    );
    assert_eq!(segs[0].resolved_by.as_deref(), Some("d_1"));
}

#[test]
fn a_window_that_only_clips_the_segment_does_not_apply() {
    let segs = overlap(
        &[decision("d_1", "2026-09-10T11:00:00Z", PHONE, (0, 30), picks_kindle(), "once")],
        PHONE,
    );
    assert!(segs[0].resolved_by.is_none(), "half a segment is not this segment");
    assert!(segs[0].unresolved);
}

/// R16: `always` matches on signature alone, so it reaches a day the owner never opened.
#[test]
fn a_rule_matches_a_window_it_was_never_recorded_for() {
    let segs = overlap(
        &[decision(
            "d_1",
            "2026-09-10T11:00:00Z",
            PHONE,
            (-600, -540), // ten hours earlier: no overlap with the day under test
            picks_kindle(),
            "always",
        )],
        PHONE,
    );
    assert_eq!(segs[0].resolved_by.as_deref(), Some("d_1"));
    assert!(segs[0].auto_resolved, "the view must be able to say which rule did this");
    assert_eq!(segs[0].foreground_slice().device, TABLET);
}

/// §2.3: exact beats rule, so a one-off correction overrides a standing rule without deleting it.
#[test]
fn an_exact_decision_beats_a_rule_even_an_older_one() {
    let picks_youtube = json!({
        "outcome": "foreground",
        "foreground": { "device_role": "jude-phone", "device_uuid": PHONE, "app": "YouTube" },
        "label": null,
        "deliberate_background": [],
    });
    let segs = overlap(
        &[
            // The rule is the *newer* record. Exact still wins: precedence between two candidates
            // is a tiebreak within a pass, never a way to skip the pass order.
            decision("d_rule", "2026-09-10T20:00:00Z", PHONE, (-600, -540), picks_kindle(), "always"),
            decision("d_once", "2026-09-10T11:00:00Z", PHONE, (0, 60), picks_youtube, "once"),
        ],
        PHONE,
    );
    assert_eq!(segs[0].resolved_by.as_deref(), Some("d_once"));
    assert!(!segs[0].auto_resolved);
    assert_eq!(segs[0].foreground_slice().device, PHONE);
}

#[test]
fn i_was_away_makes_the_time_count_as_nothing() {
    let away = json!({
        "outcome": "ignore", "foreground": null, "label": null, "deliberate_background": [],
    });
    let segs = overlap(
        &[decision("d_1", "2026-09-10T11:00:00Z", PHONE, (0, 60), away, "once")],
        PHONE,
    );
    assert!(segs[0].ignored, "the caller subtracts these from the day total");
    assert_eq!(segs[0].state, SegmentState::Settled);
    assert_eq!(segs[0].resolved_by.as_deref(), Some("d_1"));
}

#[test]
fn something_else_replaces_the_label_but_not_the_winner() {
    let relabel = json!({
        "outcome": "relabel",
        "foreground": null,
        "label": "reading with music on",
        "deliberate_background": ["YouTube"],
    });
    let segs = overlap(
        &[decision("d_1", "2026-09-10T11:00:00Z", PHONE, (0, 60), relabel, "once")],
        PHONE,
    );
    assert_eq!(segs[0].label_override.as_deref(), Some("reading with music on"));
    assert!(!segs[0].ignored, "a relabel says what it was, not that it was nothing");
    assert!(
        segs[0].foreground < segs[0].active.len(),
        "⑤ still picks a winner so the time counts to something"
    );
}

/// `05_DATA_MODEL.md` §8: ignore what we do not understand. A build that guessed here would show
/// a segment as answered on the strength of a word it cannot read.
#[test]
fn an_outcome_from_a_newer_build_resolves_nothing() {
    let future = json!({
        "outcome": "split-evenly", "foreground": null, "label": null, "deliberate_background": [],
    });
    let segs = overlap(
        &[decision("d_1", "2026-09-10T11:00:00Z", PHONE, (0, 60), future, "once")],
        PHONE,
    );
    assert!(segs[0].resolved_by.is_none());
    assert!(segs[0].unresolved, "still an open question, and it still says so");
}

#[test]
fn a_revoked_decision_leaves_the_segment_asking_again() {
    let tombstone = json!({
        "id": "t_1", "type": "tombstone", "created_at": "2026-09-10T12:00:00Z",
        "created_by": TABLET, "revokes": "d_1",
    })
    .to_string();
    let segs = overlap(
        &[
            decision("d_1", "2026-09-10T11:00:00Z", PHONE, (0, 60), picks_kindle(), "once"),
            tombstone,
        ],
        PHONE,
    );
    assert!(segs[0].resolved_by.is_none(), "R12: undo puts the shading back");
    assert!(segs[0].unresolved);
}

/// The roadmap's check for 4.2, as far as one machine can run it: the peer holds the same records
/// and the same events, differing only in which device it *is*, and reaches the same answer.
#[test]
fn the_same_decision_reaches_the_same_answer_on_a_peer() {
    let lines = [decision("d_1", "2026-09-10T11:00:00Z", PHONE, (0, 60), picks_kindle(), "once")];
    let on_phone = overlap(&lines, PHONE);
    let on_tablet = overlap(&lines, TABLET);
    assert_eq!(on_phone, on_tablet, "R18: same input, same output, whoever is asking");
}

/// R18 again, on ④ specifically: the file's line order is whatever Syncthing produced.
#[test]
fn the_order_records_arrive_in_changes_nothing() {
    let a = decision("d_1", "2026-09-10T11:00:00Z", PHONE, (0, 60), picks_kindle(), "once");
    let b = decision("d_2", "2026-09-10T12:00:00Z", TABLET, (0, 60), picks_kindle(), "once");
    let forward = overlap(&[a.clone(), b.clone()], PHONE);
    let backward = overlap(&[b, a], PHONE);
    assert_eq!(forward, backward);
    assert_eq!(forward[0].resolved_by.as_deref(), Some("d_2"), "newest wins the group");
}

/// A rule keyed on a signature must not leak onto an overlap between different apps.
#[test]
fn a_rule_does_not_touch_a_different_contention() {
    let rule = decision("d_1", "2026-09-10T11:00:00Z", PHONE, (0, 60), picks_kindle(), "always");
    let records = parse_records(&rule);
    let input = PipelineInput {
        own_device: PHONE.to_string(),
        hostname_to_uuid: hostnames(),
        activity: vec![
            BucketEvents {
                bucket_id: "aw-watcher-window_phone".to_string(),
                events: vec![tagged(t(0), t(60), PHONE, "Signal")],
            },
            BucketEvents {
                bucket_id: "aw-watcher-window_tablet".to_string(),
                events: vec![tagged(t(0), t(60), TABLET, "Kindle")],
            },
        ],
        idle: vec![],
        min_contention: default_min_contention(),
        decisions: merge_decisions(&records),
    };
    let segs = coalesce(compute_segments(input));
    assert!(segs[0].resolved_by.is_none(), "Signal vs Kindle is a different question");
}

/// A heartbeat-split event puts one device's app into `active` twice. The signature the sheet wrote
/// had one row per device/app, so ④ has to collapse the duplicate or nothing ever matches again.
#[test]
fn a_heartbeat_split_event_still_matches_the_decision_made_about_it() {
    let records = parse_records(&decision(
        "d_1",
        "2026-09-10T11:00:00Z",
        PHONE,
        (0, 60),
        picks_kindle(),
        "once",
    ));
    let input = PipelineInput {
        own_device: PHONE.to_string(),
        hostname_to_uuid: hostnames(),
        activity: vec![
            BucketEvents {
                bucket_id: "aw-watcher-window_phone".to_string(),
                events: vec![tagged(t(0), t(60), PHONE, "YouTube")],
            },
            // The same phone, the same app, arriving as two overlapping buckets.
            BucketEvents {
                bucket_id: "aw-watcher-window_phone-2".to_string(),
                events: vec![tagged(t(0), t(60), PHONE, "YouTube")],
            },
            BucketEvents {
                bucket_id: "aw-watcher-window_tablet".to_string(),
                events: vec![tagged(t(0), t(60), TABLET, "Kindle")],
            },
        ],
        idle: vec![],
        min_contention: default_min_contention(),
        decisions: merge_decisions(&records),
    };
    let segs = coalesce(compute_segments(input));
    assert_eq!(segs[0].resolved_by.as_deref(), Some("d_1"));
}

/// Records written before the server learned to resolve hostnames put the device **uuid** in
/// `device_role`. They are real decisions the owner made, and they must keep applying: a resolved
/// block quietly going back to asking is worse than never having resolved it.
#[test]
fn a_decision_recorded_with_uuid_roles_still_applies() {
    let by_uuid = json!({
        "id": "d_old",
        "type": "decision",
        "created_at": "2026-09-10T11:00:00Z",
        "created_by": PHONE,
        "window": { "start": t(0).to_rfc3339(), "end": t(60).to_rfc3339() },
        // The old spelling: role == uuid, on both participants and on the pick.
        "signature": { "participants": [
            { "device_role": PHONE, "device_uuid": PHONE, "app": "YouTube", "category": null },
            { "device_role": TABLET, "device_uuid": TABLET, "app": "Kindle", "category": null },
        ]},
        "resolution": {
            "outcome": "foreground",
            "foreground": { "device_role": TABLET, "device_uuid": TABLET, "app": "Kindle" },
            "label": null,
            "deliberate_background": ["YouTube"],
        },
        "scope": "once",
    })
    .to_string();

    // `hostnames()` is populated, so the segment's own key now uses hostnames — the case that
    // would have stopped matching.
    let segs = overlap(&[by_uuid], PHONE);
    assert_eq!(segs[0].resolved_by.as_deref(), Some("d_old"));
    assert_eq!(segs[0].foreground_slice().device, TABLET);
}

/// The same, for a standing rule: those are the records that most need to outlive a spelling change.
#[test]
fn a_rule_recorded_with_uuid_roles_still_applies() {
    let rule = json!({
        "id": "r_old",
        "type": "decision",
        "created_at": "2026-09-10T11:00:00Z",
        "created_by": PHONE,
        "window": { "start": t(-600).to_rfc3339(), "end": t(-540).to_rfc3339() },
        "signature": { "participants": [
            { "device_role": PHONE, "device_uuid": PHONE, "app": "YouTube", "category": null },
            { "device_role": TABLET, "device_uuid": TABLET, "app": "Kindle", "category": null },
        ]},
        "resolution": {
            "outcome": "foreground",
            "foreground": { "device_role": TABLET, "device_uuid": TABLET, "app": "Kindle" },
            "label": null,
            "deliberate_background": [],
        },
        "scope": "always",
    })
    .to_string();
    let segs = overlap(&[rule], PHONE);
    assert_eq!(segs[0].resolved_by.as_deref(), Some("r_old"));
    assert!(segs[0].auto_resolved);
}
