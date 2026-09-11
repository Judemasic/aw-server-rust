//! ②b — activity the owner said never counts (roadmap 4.6).
//!
//! The owner's example is the launcher: *"do not count One UI Home"*. Every test here is that
//! sentence in one of its forms — the launcher alone, the launcher against a device genuinely in
//! use, the launcher as the thing that was making the day ask questions.

use std::collections::HashMap;

use aw_combined::{
    coalesce, compute_segments, default_min_contention, BucketEvents, NotCountedRule,
    PipelineInput, Segment, SegmentState, EVENT_ORIGIN_KEY,
};
use aw_models::Event;
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::{json, Map};

const PHONE: &str = "11111111-1111-1111-1111-111111111111";
const TABLET: &str = "22222222-2222-2222-2222-222222222222";

fn t(min: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 10, 10, 0, 0).unwrap() + Duration::minutes(min)
}

fn ev(start: i64, end: i64, device: &str, app: &str) -> Event {
    let mut data = Map::new();
    data.insert("app".to_string(), json!(app));
    data.insert(EVENT_ORIGIN_KEY.to_string(), json!(device));
    Event {
        id: None,
        timestamp: t(start),
        duration: t(end) - t(start),
        data,
    }
}

fn bucket(id: &str, events: Vec<Event>) -> BucketEvents {
    BucketEvents {
        bucket_id: id.to_string(),
        events,
    }
}

fn run(activity: Vec<BucketEvents>, rules: Vec<NotCountedRule>) -> Vec<Segment> {
    coalesce(compute_segments(PipelineInput {
        own_device: PHONE.to_string(),
        hostname_to_uuid: HashMap::new(),
        activity,
        idle: vec![],
        min_contention: default_min_contention(),
        decisions: vec![],
        not_counted: rules,
    }))
}

fn launcher_rule() -> NotCountedRule {
    NotCountedRule::new("One UI Home", false, None).unwrap()
}

/// Seconds the day counts: what `combined_seconds` sums.
fn counted(segments: &[Segment]) -> i64 {
    segments
        .iter()
        .filter(|s| !s.ignored)
        .map(|s| (s.end - s.start).num_seconds())
        .sum()
}

#[test]
fn an_excluded_app_on_its_own_counts_toward_nothing_but_still_draws() {
    let segments = run(
        vec![bucket(
            "aw-watcher-android_phone",
            vec![ev(0, 30, PHONE, "One UI Home")],
        )],
        vec![launcher_rule()],
    );
    assert_eq!(segments.len(), 1, "the block is still there to look at");
    assert!(segments[0].ignored);
    assert!(
        segments[0].not_counted,
        "a rule did this, not an 'I was away' answer"
    );
    assert_eq!(segments[0].excluded_labels, vec!["One UI Home".to_string()]);
    assert_eq!(counted(&segments), 0);
}

#[test]
fn the_other_device_wins_time_the_excluded_app_was_competing_for() {
    // The phone sits on the launcher for half an hour while the tablet is genuinely being read on.
    // Before the rule this is a contended half hour the owner has to answer; after it, it is simply
    // half an hour of Kindle.
    let activity = vec![
        bucket(
            "aw-watcher-android_phone",
            vec![ev(0, 30, PHONE, "One UI Home")],
        ),
        bucket(
            "aw-watcher-window_tablet",
            vec![ev(0, 30, TABLET, "Kindle")],
        ),
    ];

    let before = run(activity.clone(), vec![]);
    assert_eq!(before[0].state, SegmentState::Contended);
    assert!(before[0].unresolved, "without the rule this is a question");

    let after = run(activity, vec![launcher_rule()]);
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].state, SegmentState::Settled);
    assert!(!after[0].unresolved, "the launcher was never a competitor");
    assert!(
        !after[0].ignored,
        "real activity remains, so the time still counts"
    );
    assert_eq!(after[0].foreground_slice().device, TABLET);
    assert_eq!(
        after[0].excluded_labels,
        vec!["One UI Home".to_string()],
        "what was taken out is still sayable"
    );
    assert_eq!(counted(&after), 30 * 60);
}

#[test]
fn no_rules_is_exactly_the_old_behaviour() {
    let activity = vec![
        bucket(
            "aw-watcher-android_phone",
            vec![ev(0, 30, PHONE, "One UI Home")],
        ),
        bucket(
            "aw-watcher-window_tablet",
            vec![ev(0, 30, TABLET, "Kindle")],
        ),
    ];
    let segments = run(activity, vec![]);
    assert!(segments.iter().all(|s| !s.not_counted));
    assert!(segments.iter().all(|s| s.excluded_labels.is_empty()));
    assert_eq!(counted(&segments), 30 * 60);
}

#[test]
fn a_rule_that_matches_nothing_changes_nothing() {
    let segments = run(
        vec![bucket(
            "aw-watcher-window_tablet",
            vec![ev(0, 30, TABLET, "Kindle")],
        )],
        vec![NotCountedRule::new("Solitaire", false, None).unwrap()],
    );
    assert!(!segments[0].ignored);
    assert_eq!(counted(&segments), 30 * 60);
}

#[test]
fn case_insensitivity_is_the_rules_to_ask_for_not_the_default() {
    let activity = vec![bucket(
        "aw-watcher-android_phone",
        vec![ev(0, 30, PHONE, "One UI Home")],
    )];
    let sensitive = run(
        activity.clone(),
        vec![NotCountedRule::new("one ui home", false, None).unwrap()],
    );
    assert!(
        !sensitive[0].ignored,
        "a case-sensitive rule must not match"
    );

    let insensitive = run(
        activity,
        vec![NotCountedRule::new("one ui home", true, None).unwrap()],
    );
    assert!(insensitive[0].ignored);
}

#[test]
fn select_keys_limits_which_fields_a_rule_reads() {
    // A rule written against `title` must not match an app called the same thing, or the owner's
    // careful rule silently becomes a broader one.
    let segments = run(
        vec![bucket(
            "aw-watcher-android_phone",
            vec![ev(0, 30, PHONE, "One UI Home")],
        )],
        vec![NotCountedRule::new("One UI Home", false, Some(vec!["title".to_string()])).unwrap()],
    );
    assert!(!segments[0].ignored);
}

#[test]
fn an_empty_select_keys_is_refused_rather_than_matching_nothing_quietly() {
    assert!(NotCountedRule::new("x", false, Some(vec![])).is_err());
}

#[test]
fn excluding_one_of_three_leaves_the_other_two_contending() {
    // Exclusion removes a competitor; it does not settle a question between the ones left.
    let segments = run(
        vec![
            bucket(
                "aw-watcher-android_phone",
                vec![ev(0, 30, PHONE, "One UI Home")],
            ),
            bucket(
                "aw-watcher-window_tablet",
                vec![ev(0, 30, TABLET, "Kindle")],
            ),
            bucket(
                "aw-watcher-window_desktop-synced-from-desk",
                vec![ev(0, 30, "33333333-3333-3333-3333-333333333333", "vim")],
            ),
        ],
        vec![launcher_rule()],
    );
    assert_eq!(segments[0].state, SegmentState::Contended);
    assert!(segments[0].unresolved);
    assert_eq!(
        segments[0].active.len(),
        2,
        "the launcher is gone, the real two remain"
    );
    assert_eq!(counted(&segments), 30 * 60);
}
