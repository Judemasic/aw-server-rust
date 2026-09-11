//! Which settings travel between devices, and what one sync cycle should do about them.
//!
//! A port of `SharedSettings.kt`, and deliberately a close one. Two implementations of this rule
//! that disagree would not fail loudly: they would converge on different answers and quietly
//! categorise the same day two ways depending on which device was asked. Where the Kotlin makes a
//! choice, the choice is repeated here with the reason it was made, rather than re-derived.
//!
//! Pure -- no I/O, no clock, no datastore. The caller supplies what this device holds, what every
//! device's `settings.jsonl` says, and what was agreed last time; it gets back what to publish and
//! what to accept.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

/// One `type: "setting"` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    pub key: String,
    /// The raw JSON body the datastore holds, as a string. Not parsed: a setting's *value* is
    /// aw-webui's business, and a sync that understood it would need updating every time the web
    /// UI learned a new shape.
    pub value: String,
    pub updated_at: String,
    pub updated_by: String,
}

/// The settings that mean the same thing on every device.
///
/// **An allowlist, never a denylist**, for the reason `SharedSettings.kt` gives: the shared list
/// and the device-local list grow at different speeds, aw-webui gains keys on its own schedule,
/// and a device-local key that leaked by default would be noticed only after it had overwritten
/// another device's copy. Anything unlisted stays put.
///
/// This list must match the Kotlin's. A key shared by one side and not the other is a key that
/// travels in one direction, which is worse than one that does not travel at all.
pub const SHARED_SETTING_KEYS: [&str; 9] = [
    // What counts as what. Renaming YouTube to "fun" edits `classes`.
    "classes",
    "category_sets",
    "active_set_ids",
    // Which of two equally deep matching rules wins. An answer about what something *is*, so it
    // means the same everywhere -- and a pin on one device only would make two devices categorise
    // the same day differently.
    "category_pins",
    // What is deliberately not counted, and what is always counted.
    "privacy_filters",
    "always_active_pattern",
    // Where a day and a week begin: they change what a day's totals *mean*, and two devices
    // disagreeing here disagree about which day an evening belongs to.
    "startOfDay",
    "startOfWeek",
    "durationDefault",
];

/// Key prefixes shared as well as [`SHARED_SETTING_KEYS`]. This project's own namespace for rules.
pub const SHARED_SETTING_PREFIXES: [&str; 3] = ["category.", "label.", "rule."];

pub fn is_shared_setting_key(key: &str) -> bool {
    SHARED_SETTING_KEYS.contains(&key) || SHARED_SETTING_PREFIXES.iter().any(|p| key.starts_with(p))
}

/// What one cycle should do.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SettingsPlan {
    /// New lines for *this device's own* `settings.jsonl` -- the local edits being published.
    /// Every file has exactly one writer (**R20**), so these go in ours and nobody else's.
    pub lines_to_append: Vec<Setting>,
    /// Settings to write into this device's datastore, because another device's value won.
    pub values_to_apply: BTreeMap<String, String>,
    /// What to remember as agreed, for the next cycle.
    pub applied: BTreeMap<String, String>,
}

impl SettingsPlan {
    pub fn is_empty(&self) -> bool {
        self.lines_to_append.is_empty() && self.values_to_apply.is_empty()
    }
}

/// The winning line per key, across every device's file.
///
/// Newest wins; ties break on the lowest `updated_by`, deliberately the same rule decisions use
/// (**R18/R29**) so a person only has to hold one model. An unparseable or missing timestamp sorts
/// *below* every real one -- it loses -- but still compares deterministically against other broken
/// ones by falling back to the raw string, because guessing at a broken timestamp would reintroduce
/// exactly the arrival-order dependence R18 forbids.
pub fn effective_settings(settings: &[Setting]) -> BTreeMap<String, Setting> {
    let mut winners: BTreeMap<String, Setting> = BTreeMap::new();
    for setting in settings {
        match winners.get(&setting.key) {
            Some(current) if !wins_over(setting, current) => {}
            _ => {
                winners.insert(setting.key.clone(), setting.clone());
            }
        }
    }
    winners
}

/// Whether `a` beats `b` for the same key.
fn wins_over(a: &Setting, b: &Setting) -> bool {
    let (ta, tb) = (parse_instant(&a.updated_at), parse_instant(&b.updated_at));
    match (ta, tb) {
        (Some(x), Some(y)) if x != y => x > y,
        (Some(_), None) => true,
        (None, Some(_)) => false,
        // Same instant, or both unparseable: fall through to the raw string, then the fields the
        // Kotlin comparator uses, in the same order.
        _ => match a.updated_at.cmp(&b.updated_at) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => match a.updated_by.cmp(&b.updated_by) {
                std::cmp::Ordering::Less => true,
                std::cmp::Ordering::Greater => false,
                std::cmp::Ordering::Equal => a.value < b.value,
            },
        },
    }
}

/// RFC3339, or nothing. `Instant.parse` in the Kotlin, which is the same grammar.
fn parse_instant(raw: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

/// Decide what to publish and what to accept.
///
/// * `local` -- this device's stored settings, values as the raw JSON bodies the datastore holds.
/// * `merged` -- the winning line per key across all devices ([`effective_settings`]).
/// * `applied` -- what this function concluded last time. **The piece that makes the difference**
///   between "the owner edited this here" and "a peer edited it and we have not applied it yet":
///   both look like `local != merged`, and only a comparison against what was last agreed tells
///   them apart. Device-local state.
/// * `joining` -- true when this device has never agreed anything with this folder. **Read the
///   note below before changing it.**
/// * `now` -- the timestamp for lines published here.
/// * `device_uuid` -- this device, as `updated_by`.
///
/// The rule per key is two lines long:
///
/// * **`local != applied`** -- the owner changed it *here*. Publish it. Ours is the newest line, so
///   it wins the merge on every device, including ones that changed it too but earlier.
/// * **otherwise** -- no news here. Take what the merge says, and write it locally if it differs.
///
/// Two devices editing the same key between one sync and the next both publish; last-write-wins
/// with the lowest-uuid tiebreak picks one, and the loser's next cycle finds `local == applied` and
/// accepts the winner. Convergence takes one extra cycle and no coordination.
///
/// A key absent from `local` is one the owner never saved -- aw-webui stores nothing until a
/// setting is changed. It is never published, because publishing "absent" would mean publishing
/// this build's defaults over a peer's real choice; but it *is* accepted from a peer, which is what
/// makes a fresh device pick up an established one's categories.
///
/// # Joining a folder that already has devices in it
///
/// The rule above has a hole, and it is the one every new desktop falls into. `local != applied`
/// is read as "the owner changed this here" -- but on a device that has never synced, `applied` is
/// empty, so *everything* local reads as an edit. A PC whose `classes` is whatever the web UI
/// wrote on first launch would publish that, stamped `now`, and the newest line wins: two phones'
/// worth of carefully built categories replaced by a third device's defaults, on every device, in
/// one pass.
///
/// It never fired on Android because those devices joined a folder that was empty, where there was
/// nothing to overwrite. Adding a desktop to an established set is the case it was waiting for.
///
/// So a device with nothing agreed and a folder with something in it **accepts first**: every key
/// the folder has is taken, and only keys nobody has are published. That is also what a person
/// means by "add this computer to my sync" -- they are asking for their categories *here*, not
/// offering this machine's defaults to everything else. From the second pass onward `applied` is
/// populated and the ordinary rule applies, so a real edit made here afterwards still wins.
///
/// A device joining a genuinely empty folder is not joining anything: `merged` is empty, nothing
/// is accepted, and it publishes normally.
pub fn plan_settings_sync(
    local: &BTreeMap<String, String>,
    merged: &BTreeMap<String, Setting>,
    applied: &BTreeMap<String, String>,
    joining: bool,
    now: &str,
    device_uuid: &str,
) -> SettingsPlan {
    let mut plan = SettingsPlan::default();

    let keys: BTreeSet<&String> = local
        .keys()
        .chain(merged.keys())
        .chain(applied.keys())
        .filter(|k| is_shared_setting_key(k))
        .collect();

    for key in keys {
        let local_value = local.get(key);
        let merged_value = merged.get(key).map(|s| &s.value);

        // Joining an established folder: whatever this device happens to hold is not an edit,
        // because it has never agreed anything. Take what is there.
        let can_publish = !(joining && merged_value.is_some());

        if let Some(local_value) = local_value {
            if can_publish && Some(local_value) != applied.get(key) {
                plan.lines_to_append.push(Setting {
                    key: key.clone(),
                    value: local_value.clone(),
                    updated_at: now.to_string(),
                    updated_by: device_uuid.to_string(),
                });
                plan.applied.insert(key.clone(), local_value.clone());
                continue;
            }
        }
        if let Some(merged_value) = merged_value {
            if Some(merged_value) != local_value {
                plan.values_to_apply
                    .insert(key.clone(), merged_value.clone());
            }
            plan.applied.insert(key.clone(), merged_value.clone());
            continue;
        }
        // No line anywhere and nothing new here: a key only we hold, already published. Keep
        // remembering it so a later edit is still recognisable as one.
        if let Some(local_value) = local_value {
            plan.applied.insert(key.clone(), local_value.clone());
        }
    }
    plan
}

/// Read a `type: "setting"` line. `None` for anything that is not one.
///
/// Missing, null and non-string fields all read as the empty string, matching `optString` in the
/// Kotlin -- two implementations that disagreed here would group lines differently.
pub fn parse_setting_line(line: &str) -> Option<Setting> {
    let json: Value = serde_json::from_str(line.trim()).ok()?;
    if json.get("type").and_then(Value::as_str) != Some("setting") {
        return None;
    }
    let key = json.get("key").and_then(Value::as_str).unwrap_or_default();
    if key.is_empty() {
        return None;
    }
    Some(Setting {
        key: key.to_string(),
        value: json
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        updated_at: json
            .get("updated_at")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        updated_by: json
            .get("updated_by")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

/// Every setting line in a file body.
pub fn parse_settings(text: &str) -> Vec<Setting> {
    text.lines().filter_map(parse_setting_line).collect()
}

/// Serialise to exactly one line.
///
/// Field order matches the Kotlin's `JSONObject.put` sequence. It does not have to -- both sides
/// parse by name -- but a file two devices append to is a file a person will read, and lines that
/// differ only in field order make a diff of it useless.
pub fn setting_to_json_line(setting: &Setting) -> String {
    serde_json::json!({
        "type": "setting",
        "key": setting.key,
        "value": setting.value,
        "updated_at": setting.updated_at,
        "updated_by": setting.updated_by,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setting(key: &str, value: &str, at: &str, by: &str) -> Setting {
        Setting {
            key: key.to_string(),
            value: value.to_string(),
            updated_at: at.to_string(),
            updated_by: by.to_string(),
        }
    }

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn only_listed_keys_travel() {
        assert!(is_shared_setting_key("classes"));
        assert!(is_shared_setting_key("category_pins"));
        assert!(is_shared_setting_key("category.youtube"));
        // How this screen looks, which a phone in a pocket and a tablet on a desk may
        // reasonably disagree about.
        assert!(!is_shared_setting_key("theme"));
        assert!(!is_shared_setting_key("landingpage"));
        // These name buckets, and a bucket id carries the hostname that produced it -- copied
        // across, they would point at buckets the receiving device does not have.
        assert!(!is_shared_setting_key("views"));
        assert!(!is_shared_setting_key("saved_queries"));
    }

    #[test]
    fn the_newest_line_wins() {
        let winners = effective_settings(&[
            setting("classes", "old", "2026-09-01T00:00:00Z", "a"),
            setting("classes", "new", "2026-09-02T00:00:00Z", "b"),
        ]);
        assert_eq!(winners["classes"].value, "new");
    }

    #[test]
    fn a_tie_breaks_on_the_lowest_device_id() {
        // Same rule as decisions, so there is one model to hold rather than two.
        let winners = effective_settings(&[
            setting("classes", "from-b", "2026-09-02T00:00:00Z", "bbb"),
            setting("classes", "from-a", "2026-09-02T00:00:00Z", "aaa"),
        ]);
        assert_eq!(winners["classes"].value, "from-a");
    }

    #[test]
    fn a_broken_timestamp_loses_to_a_real_one() {
        // But does not win by accident, and does not crash the merge.
        let winners = effective_settings(&[
            setting("classes", "broken", "not a date", "a"),
            setting("classes", "real", "2020-01-01T00:00:00Z", "z"),
        ]);
        assert_eq!(winners["classes"].value, "real");
    }

    #[test]
    fn an_offset_timestamp_compares_as_the_same_instant() {
        // Two devices in different timezones writing the same moment must not order by text.
        let winners = effective_settings(&[
            setting("classes", "utc", "2026-09-02T00:00:00Z", "a"),
            setting("classes", "later", "2026-09-02T03:00:00+02:00", "b"),
        ]);
        assert_eq!(winners["classes"].value, "later");
    }

    #[test]
    fn an_edit_here_is_published() {
        // local differs from what we last agreed: the owner changed it on this device.
        let plan = plan_settings_sync(
            &map(&[("classes", "mine")]),
            &BTreeMap::new(),
            &map(&[("classes", "agreed")]),
            false,
            "2026-09-11T00:00:00Z",
            "me",
        );
        assert_eq!(plan.lines_to_append.len(), 1);
        assert_eq!(plan.lines_to_append[0].value, "mine");
        assert_eq!(plan.lines_to_append[0].updated_by, "me");
        assert!(plan.values_to_apply.is_empty());
        assert_eq!(plan.applied["classes"], "mine");
    }

    #[test]
    fn a_peers_edit_is_accepted() {
        // local matches what we agreed, so we have no news; the merge does.
        let merged =
            effective_settings(&[setting("classes", "theirs", "2026-09-11T00:00:00Z", "them")]);
        let plan = plan_settings_sync(
            &map(&[("classes", "agreed")]),
            &merged,
            &map(&[("classes", "agreed")]),
            false,
            "2026-09-11T01:00:00Z",
            "me",
        );
        assert!(plan.lines_to_append.is_empty());
        assert_eq!(plan.values_to_apply["classes"], "theirs");
        assert_eq!(plan.applied["classes"], "theirs");
    }

    #[test]
    fn a_fresh_device_takes_an_established_ones_categories() {
        // Nothing local at all -- aw-webui stores nothing until a setting is changed.
        let merged =
            effective_settings(&[setting("classes", "theirs", "2026-09-11T00:00:00Z", "them")]);
        let plan = plan_settings_sync(
            &BTreeMap::new(),
            &merged,
            &BTreeMap::new(),
            false,
            "2026-09-11T01:00:00Z",
            "me",
        );
        assert_eq!(plan.values_to_apply["classes"], "theirs");
        assert!(
            plan.lines_to_append.is_empty(),
            "a device with no value must not publish its defaults over a peer's real choice"
        );
    }

    #[test]
    fn agreeing_with_the_merge_does_nothing_at_all() {
        let merged =
            effective_settings(&[setting("classes", "same", "2026-09-11T00:00:00Z", "them")]);
        let plan = plan_settings_sync(
            &map(&[("classes", "same")]),
            &merged,
            &map(&[("classes", "same")]),
            false,
            "2026-09-11T01:00:00Z",
            "me",
        );
        assert!(plan.is_empty(), "{plan:?}");
    }

    #[test]
    fn a_local_only_key_is_published_once_and_then_left_alone() {
        // First cycle: nothing agreed yet, so it goes out.
        let first = plan_settings_sync(
            &map(&[("classes", "mine")]),
            &BTreeMap::new(),
            &BTreeMap::new(),
            false,
            "2026-09-11T00:00:00Z",
            "me",
        );
        assert_eq!(first.lines_to_append.len(), 1);

        // Second cycle, with our own line now in the merge: nothing new to say.
        let merged =
            effective_settings(&[setting("classes", "mine", "2026-09-11T00:00:00Z", "me")]);
        let second = plan_settings_sync(
            &map(&[("classes", "mine")]),
            &merged,
            &first.applied,
            false,
            "2026-09-11T01:00:00Z",
            "me",
        );
        assert!(second.is_empty(), "{second:?}");
    }

    #[test]
    fn a_device_local_key_never_travels_in_either_direction() {
        let merged =
            effective_settings(&[setting("theme", "\"dark\"", "2026-09-11T00:00:00Z", "them")]);
        let plan = plan_settings_sync(
            &map(&[("theme", "\"light\"")]),
            &merged,
            &BTreeMap::new(),
            false,
            "2026-09-11T01:00:00Z",
            "me",
        );
        assert!(plan.is_empty(), "a device-local setting moved: {plan:?}");
    }

    #[test]
    fn the_loser_of_a_simultaneous_edit_converges_next_cycle() {
        // Both devices edited between two syncs, so both published. The tiebreak picked `aaa`.
        let merged = effective_settings(&[
            setting("classes", "from-me", "2026-09-11T00:00:00Z", "zzz"),
            setting("classes", "from-them", "2026-09-11T00:00:00Z", "aaa"),
        ]);
        // Our previous cycle recorded what we published as agreed.
        let plan = plan_settings_sync(
            &map(&[("classes", "from-me")]),
            &merged,
            &map(&[("classes", "from-me")]),
            false,
            "2026-09-11T02:00:00Z",
            "zzz",
        );
        assert!(plan.lines_to_append.is_empty(), "should stop arguing");
        assert_eq!(plan.values_to_apply["classes"], "from-them");
    }

    #[test]
    fn joining_an_established_folder_does_not_publish_this_devices_defaults() {
        // The case every new desktop is in: two phones have built a categorisation, this machine
        // has whatever the web UI wrote on first launch, and nothing has been agreed here yet.
        // Publishing would stamp `now` on the defaults and win everywhere.
        let merged = effective_settings(&[setting(
            "classes",
            "[\"their careful categories\"]",
            "2026-09-11T13:41:33Z",
            "phone",
        )]);
        let plan = plan_settings_sync(
            &map(&[("classes", "[\"factory defaults\"]")]),
            &merged,
            &BTreeMap::new(),
            true,
            "2026-09-11T21:00:00Z",
            "desktop",
        );
        assert!(
            plan.lines_to_append.is_empty(),
            "a joining device published over an established folder: {plan:?}"
        );
        assert_eq!(
            plan.values_to_apply["classes"],
            "[\"their careful categories\"]"
        );
        assert_eq!(plan.applied["classes"], "[\"their careful categories\"]");
    }

    #[test]
    fn joining_still_offers_a_key_nobody_else_has() {
        // Accepting what the folder holds is not the same as having nothing to say.
        let plan = plan_settings_sync(
            &map(&[("classes", "theirs"), ("startOfDay", "\"04:00\"")]),
            &effective_settings(&[setting(
                "classes",
                "theirs",
                "2026-09-11T00:00:00Z",
                "phone",
            )]),
            &BTreeMap::new(),
            true,
            "2026-09-11T21:00:00Z",
            "desktop",
        );
        assert_eq!(plan.lines_to_append.len(), 1);
        assert_eq!(plan.lines_to_append[0].key, "startOfDay");
    }

    #[test]
    fn joining_an_empty_folder_publishes_normally() {
        // A device joining a folder with nothing in it is not joining anything.
        let plan = plan_settings_sync(
            &map(&[("classes", "mine")]),
            &BTreeMap::new(),
            &BTreeMap::new(),
            true,
            "2026-09-11T21:00:00Z",
            "desktop",
        );
        assert_eq!(plan.lines_to_append.len(), 1);
        assert_eq!(plan.lines_to_append[0].value, "mine");
    }

    #[test]
    fn an_edit_after_joining_still_wins() {
        // Deferring on the first pass must not mean deferring forever.
        let merged = effective_settings(&[setting(
            "classes",
            "theirs",
            "2026-09-11T00:00:00Z",
            "phone",
        )]);
        let joined = plan_settings_sync(
            &map(&[("classes", "defaults")]),
            &merged,
            &BTreeMap::new(),
            true,
            "2026-09-11T21:00:00Z",
            "desktop",
        );
        // Next pass: the owner has edited the categories here.
        let plan = plan_settings_sync(
            &map(&[("classes", "edited here")]),
            &merged,
            &joined.applied,
            false,
            "2026-09-11T22:00:00Z",
            "desktop",
        );
        assert_eq!(plan.lines_to_append.len(), 1);
        assert_eq!(plan.lines_to_append[0].value, "edited here");
    }

    #[test]
    fn a_line_survives_a_round_trip() {
        let original = setting("classes", "{\"a\":1}", "2026-09-11T00:00:00Z", "me");
        let line = setting_to_json_line(&original);
        assert_eq!(parse_setting_line(&line), Some(original));
    }

    #[test]
    fn other_record_types_are_not_settings() {
        // `decisions.jsonl` and `settings.jsonl` are separate files, but a reader that confused
        // the two would silently drop or invent settings.
        assert!(parse_setting_line(r#"{"type":"decision","id":"X"}"#).is_none());
        assert!(parse_setting_line("not json").is_none());
        assert!(parse_setting_line(r#"{"type":"setting"}"#).is_none());
    }

    #[test]
    fn a_half_written_line_does_not_take_the_file_with_it() {
        // Syncthing can be interrupted mid-transfer; the rest of the file is still good.
        let body = "{\"type\":\"setting\",\"key\":\"classes\",\"value\":\"ok\",\
                    \"updated_at\":\"2026-09-11T00:00:00Z\",\"updated_by\":\"me\"}\n\
                    {\"type\":\"setti";
        let parsed = parse_settings(body);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].value, "ok");
    }
}
