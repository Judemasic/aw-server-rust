//! Phase 4 — the decision record, and the deterministic merge across devices.
//!
//! A decision is *data about* a contention, never an edit to the events underneath it (**R11**).
//! Every device appends its own decisions to its own `decisions.jsonl` in the shared folder, reads
//! every device's file, and merges them here. The merge is the thing that makes three devices agree
//! without coordinating (**R18**), so it is written to the letter of
//! `aw-android/docs/05_DATA_MODEL.md` §4.2 — including the three rules that section records as
//! having been pinned down by the Kotlin implementation (roadmap 2.2).
//!
//! **This module is a second implementation of a merge Kotlin already has**
//! (`SharedStore.kt::mergeDecisions`). That is deliberate, not an oversight: Kotlin merges to decide
//! what to *publish and carry between files*, Rust merges to decide what to *apply to a timeline*,
//! and the two run on different sides of the JNI boundary. They must agree exactly, which is why
//! the field-by-field semantics below (empty string for a missing or null value, `Instant::MIN` for
//! an unparseable timestamp, `id` as the last tiebreak) mirror `SharedStore.kt` deliberately rather
//! than being written the way a fresh Rust module would want to write them.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

/// A decision that applies only to the window it was recorded for.
pub const SCOPE_ONCE: &str = "once";
/// A decision that applies to every segment with the same signature (**R16**).
pub const SCOPE_ALWAYS: &str = "always";

/// `resolution.outcome`: this device/app is what counted.
pub const OUTCOME_FOREGROUND: &str = "foreground";
/// `resolution.outcome`: neither competitor is right; the owner typed a label.
pub const OUTCOME_RELABEL: &str = "relabel";
/// `resolution.outcome`: the owner was away; this time counts as nothing.
pub const OUTCOME_IGNORE: &str = "ignore";

/// Field separator inside a signature's match key. Both are ASCII control characters that cannot
/// occur in an app name or a hostname, so no escaping is needed — same choice as `SharedStore.kt`.
const UNIT_SEP: char = '\u{1F}';
const RECORD_SEP: char = '\u{1E}';

/// One device's side of a contention (`04_COMBINED_TIMELINE.md` §3).
///
/// `device_uuid` is provenance and is deliberately **not** part of [`Signature::match_key`]:
/// matching on `device_role` is what lets a rule outlive replacing a device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Participant {
    pub device_role: String,
    pub device_uuid: String,
    pub app: String,
    pub category: String,
}

/// What was competing, canonically ordered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Signature {
    pub participants: Vec<Participant>,
}

impl Signature {
    /// Sort on construction, so a stored line is canonical too and not just the key derived from it.
    pub fn of(mut participants: Vec<Participant>) -> Self {
        participants.sort_by(|a, b| {
            a.device_role
                .cmp(&b.device_role)
                .then_with(|| a.app.cmp(&b.app))
                .then_with(|| a.category.cmp(&b.category))
                .then_with(|| a.device_uuid.cmp(&b.device_uuid))
        });
        Signature { participants }
    }

    /// The rule key: role/app/category only, sorted. No uuids, and no ordering left to the writer.
    pub fn match_key(&self) -> String {
        let mut parts: Vec<String> = self
            .participants
            .iter()
            .map(|p| format!("{}{UNIT_SEP}{}{UNIT_SEP}{}", p.device_role, p.app, p.category))
            .collect();
        parts.sort();
        parts.join(&RECORD_SEP.to_string())
    }
}

/// The chosen device+app when the outcome is `foreground`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ForegroundPick {
    pub device_role: String,
    /// Kept even though `SharedStore.kt` drops it: applying a decision has to find the *slice* it
    /// picked, and a uuid identifies one where a role may name two devices in a future layout.
    pub device_uuid: String,
    pub app: String,
}

/// What the owner decided.
///
/// `outcome` stays a plain string rather than an enum, for the reason `05_DATA_MODEL.md` §8 gives:
/// a newer build may record an outcome this one has never heard of, and the rule is to ignore what
/// we do not understand — not to drop the line, and not to guess.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Resolution {
    pub outcome: String,
    pub foreground: Option<ForegroundPick>,
    pub label: Option<String>,
    pub deliberate_background: Vec<String>,
}

/// The interval a decision applies to. Kept as the ISO-8601 strings we read *and* as parsed
/// instants: the strings are what the grouping key uses (two devices must group identically even
/// on a timestamp neither can parse), the instants are what covers-this-segment tests need.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TimeWindow {
    pub start: String,
    pub end: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Decision {
    pub id: String,
    pub created_at: String,
    pub created_by: String,
    pub window: TimeWindow,
    pub signature: Signature,
    pub resolution: Resolution,
    pub scope: String,
}

impl Decision {
    /// The window as instants, or `None` if either end is unparseable. A decision whose window
    /// cannot be read can still be a *rule* (`scope: always` matches on signature alone); it simply
    /// can never match a specific window.
    pub fn window_bounds(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        let start = parse_instant(&self.window.start)?;
        let end = parse_instant(&self.window.end)?;
        Some((start, end))
    }

    pub fn is_rule(&self) -> bool {
        self.scope == SCOPE_ALWAYS
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Tombstone {
    pub id: String,
    pub created_at: String,
    pub created_by: String,
    pub revokes: String,
}

/// One line of a `.jsonl` file.
///
/// [`SharedRecord::Unknown`] is a feature, not a parse failure: §8 requires that a line we do not
/// understand is ignored rather than dropped, so an older build cannot destroy a newer one's data.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum SharedRecord {
    Decision(Decision),
    Tombstone(Tombstone),
    Unknown(String),
}

/// Read one JSONL body. Blank lines carry nothing and are dropped; everything else becomes a record.
pub fn parse_records(text: &str) -> Vec<SharedRecord> {
    text.lines().filter_map(parse_line).collect()
}

/// Read one line. `None` only for a blank line.
pub fn parse_line(line: &str) -> Option<SharedRecord> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let json: Value = match serde_json::from_str(trimmed) {
        Ok(v @ Value::Object(_)) => v,
        // Not JSON, or JSON that is not an object: keep it verbatim. A half-written line from an
        // interrupted Syncthing transfer is not ours to delete.
        _ => return Some(SharedRecord::Unknown(trimmed.to_string())),
    };
    let record = match str_field(&json, "type").as_str() {
        "decision" => parse_decision(&json).map(SharedRecord::Decision),
        "tombstone" => parse_tombstone(&json).map(SharedRecord::Tombstone),
        _ => None,
    };
    Some(record.unwrap_or_else(|| SharedRecord::Unknown(trimmed.to_string())))
}

/// A string field, with `SharedStore.kt`'s `optString` semantics: missing, null, or a non-string
/// all read as the empty string. Two implementations that disagreed here would group differently.
fn str_field(json: &Value, key: &str) -> String {
    json.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

fn parse_decision(json: &Value) -> Option<Decision> {
    let id = str_field(json, "id");
    if id.is_empty() {
        return None;
    }
    let window = json.get("window")?;
    let scope = match str_field(json, "scope") {
        s if s.is_empty() => SCOPE_ONCE.to_string(),
        s => s,
    };
    Some(Decision {
        id,
        created_at: str_field(json, "created_at"),
        created_by: str_field(json, "created_by"),
        window: TimeWindow {
            start: str_field(window, "start"),
            end: str_field(window, "end"),
        },
        signature: parse_signature(json.get("signature")),
        resolution: parse_resolution(json.get("resolution")),
        scope,
    })
}

fn parse_signature(json: Option<&Value>) -> Signature {
    let participants = json
        .and_then(|s| s.get("participants"))
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|p| p.is_object())
                .map(|p| Participant {
                    device_role: str_field(p, "device_role"),
                    device_uuid: str_field(p, "device_uuid"),
                    app: str_field(p, "app"),
                    category: str_field(p, "category"),
                })
                .collect()
        })
        .unwrap_or_default();
    Signature::of(participants)
}

fn parse_resolution(json: Option<&Value>) -> Resolution {
    let json = match json {
        Some(j) if j.is_object() => j,
        _ => {
            return Resolution {
                outcome: String::new(),
                foreground: None,
                label: None,
                deliberate_background: Vec::new(),
            }
        }
    };
    Resolution {
        outcome: str_field(json, "outcome"),
        foreground: json.get("foreground").filter(|f| f.is_object()).map(|f| ForegroundPick {
            device_role: str_field(f, "device_role"),
            device_uuid: str_field(f, "device_uuid"),
            app: str_field(f, "app"),
        }),
        label: Some(str_field(json, "label")).filter(|l| !l.is_empty()),
        deliberate_background: json
            .get("deliberate_background")
            .and_then(|b| b.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn parse_tombstone(json: &Value) -> Option<Tombstone> {
    let id = str_field(json, "id");
    let revokes = str_field(json, "revokes");
    // A tombstone that revokes nothing is not a tombstone. Falling back to Unknown keeps the line
    // without letting an empty `revokes` match every decision that also lost its id.
    if id.is_empty() || revokes.is_empty() {
        return None;
    }
    Some(Tombstone {
        id,
        created_at: str_field(json, "created_at"),
        created_by: str_field(json, "created_by"),
        revokes,
    })
}

/// The effective decisions across every device (`05_DATA_MODEL.md` §4.2):
///
/// 1. concatenate every device's lines (the caller does that);
/// 2. drop any decision revoked by a tombstone, **whichever file the tombstone came from**;
/// 3. group by `(window, signature match key)`;
/// 4. within a group keep the highest `created_at`; ties break on lowest `created_by`, then lowest
///    `id`.
///
/// Plus the two rules `SharedStore.kt` pinned down and §4.2 now records: the same `id` seen twice
/// is one decision, and an unparseable `created_at` loses to every parseable one while still
/// comparing deterministically against another broken one (by raw string).
///
/// The result is sorted by window then id, so the *list* is order-independent, not just its
/// contents.
pub fn merge_decisions(records: &[SharedRecord]) -> Vec<Decision> {
    let revoked: HashSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            SharedRecord::Tombstone(t) => Some(t.revokes.as_str()),
            _ => None,
        })
        .collect();

    let mut seen_ids: HashSet<&str> = HashSet::new();
    let mut groups: Vec<(String, Decision)> = Vec::new();
    for record in records {
        let decision = match record {
            SharedRecord::Decision(d) => d,
            _ => continue,
        };
        if !seen_ids.insert(decision.id.as_str()) {
            continue; // a duplicated line is one decision, not two votes
        }
        if revoked.contains(decision.id.as_str()) {
            continue;
        }
        let key = format!(
            "{}\u{1D}{}\u{1D}{}",
            decision.window.start,
            decision.window.end,
            decision.signature.match_key()
        );
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, held)) => {
                if wins(decision, held) {
                    *held = decision.clone();
                }
            }
            None => groups.push((key, decision.clone())),
        }
    }

    let mut out: Vec<Decision> = groups.into_iter().map(|(_, d)| d).collect();
    out.sort_by(|a, b| {
        a.window
            .start
            .cmp(&b.window.start)
            .then_with(|| a.window.end.cmp(&b.window.end))
            .then_with(|| a.id.cmp(&b.id))
    });
    out
}

/// Does `challenger` beat `held`? Newest `created_at`, then lowest `created_by`, then lowest `id`.
///
/// Public because applying decisions needs the same precedence when two *different* windows both
/// cover one segment — a case merging never sees, since it only ever compares within one window.
pub fn wins(challenger: &Decision, held: &Decision) -> bool {
    let (ca, ha) = (parse_instant(&challenger.created_at), parse_instant(&held.created_at));
    match (ca, ha) {
        (Some(c), Some(h)) if c != h => return c > h,
        (Some(_), None) => return true,
        (None, Some(_)) => return false,
        _ => {}
    }
    // Equal instants, or two unparseable timestamps: fall back to the raw string, then the
    // document's tiebreaks. Guessing at a broken clock would put arrival order back in (R18).
    match challenger.created_at.cmp(&held.created_at) {
        std::cmp::Ordering::Greater => return true,
        std::cmp::Ordering::Less => return false,
        std::cmp::Ordering::Equal => {}
    }
    match challenger.created_by.cmp(&held.created_by) {
        std::cmp::Ordering::Less => return true,
        std::cmp::Ordering::Greater => return false,
        std::cmp::Ordering::Equal => {}
    }
    challenger.id < held.id
}

fn parse_instant(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(id: &str, created_at: &str, created_by: &str, app: &str, scope: &str) -> String {
        format!(
            r#"{{"id":"{id}","type":"decision","created_at":"{created_at}","created_by":"{created_by}",
               "window":{{"start":"2026-09-10T10:00:00Z","end":"2026-09-10T10:30:00Z"}},
               "signature":{{"participants":[
                 {{"device_role":"phone","device_uuid":"a","app":"YouTube","category":null}},
                 {{"device_role":"tablet","device_uuid":"b","app":"Kindle","category":null}}]}},
               "resolution":{{"outcome":"foreground","foreground":{{"device_role":"tablet","device_uuid":"b","app":"{app}"}},
                 "label":null,"deliberate_background":[]}},
               "scope":"{scope}"}}"#
        )
        .replace('\n', "")
    }

    #[test]
    fn match_key_is_order_independent() {
        let a = Signature::of(vec![
            Participant { device_role: "phone".into(), device_uuid: "a".into(), app: "YT".into(), category: String::new() },
            Participant { device_role: "tablet".into(), device_uuid: "b".into(), app: "Kindle".into(), category: String::new() },
        ]);
        let b = Signature::of(vec![
            Participant { device_role: "tablet".into(), device_uuid: "z".into(), app: "Kindle".into(), category: String::new() },
            Participant { device_role: "phone".into(), device_uuid: "y".into(), app: "YT".into(), category: String::new() },
        ]);
        assert_eq!(a.match_key(), b.match_key(), "uuids are provenance, not key");
    }

    #[test]
    fn null_category_reads_as_empty_string() {
        let r = parse_line(&line("d_1", "2026-09-10T11:00:00Z", "a", "Kindle", "once")).unwrap();
        match r {
            SharedRecord::Decision(d) => {
                assert!(d.signature.participants.iter().all(|p| p.category.is_empty()))
            }
            _ => panic!("expected a decision"),
        }
    }

    #[test]
    fn newest_wins_then_lowest_author_then_lowest_id() {
        let recs: Vec<SharedRecord> = [
            line("d_2", "2026-09-10T11:00:00Z", "b", "Kindle", "once"),
            line("d_1", "2026-09-10T12:00:00Z", "z", "Kindle", "once"),
        ]
        .iter()
        .filter_map(|l| parse_line(l))
        .collect();
        let merged = merge_decisions(&recs);
        assert_eq!(merged.len(), 1, "same window and signature is one group");
        assert_eq!(merged[0].id, "d_1", "newest created_at wins");

        // Same instant: lowest created_by.
        let recs: Vec<SharedRecord> = [
            line("d_2", "2026-09-10T11:00:00Z", "z", "Kindle", "once"),
            line("d_1", "2026-09-10T11:00:00Z", "b", "Kindle", "once"),
        ]
        .iter()
        .filter_map(|l| parse_line(l))
        .collect();
        assert_eq!(merge_decisions(&recs)[0].created_by, "b");
    }

    #[test]
    fn merge_is_input_order_independent() {
        let mut recs: Vec<SharedRecord> = [
            line("d_2", "2026-09-10T11:00:00Z", "b", "Kindle", "once"),
            line("d_1", "2026-09-10T12:00:00Z", "z", "Kindle", "once"),
            line("d_3", "2026-09-10T09:00:00Z", "c", "Kindle", "once"),
        ]
        .iter()
        .filter_map(|l| parse_line(l))
        .collect();
        let forward = merge_decisions(&recs);
        recs.reverse();
        assert_eq!(forward, merge_decisions(&recs));
    }

    #[test]
    fn a_duplicated_line_is_one_decision() {
        let dup = line("d_1", "2026-09-10T11:00:00Z", "a", "Kindle", "once");
        let recs: Vec<SharedRecord> = [dup.clone(), dup].iter().filter_map(|l| parse_line(l)).collect();
        assert_eq!(merge_decisions(&recs).len(), 1);
    }

    #[test]
    fn a_tombstone_from_another_file_revokes() {
        let mut recs: Vec<SharedRecord> =
            vec![parse_line(&line("d_1", "2026-09-10T11:00:00Z", "a", "Kindle", "once")).unwrap()];
        recs.push(
            parse_line(
                r#"{"id":"t_1","type":"tombstone","created_at":"2026-09-10T12:00:00Z","created_by":"b","revokes":"d_1"}"#,
            )
            .unwrap(),
        );
        assert!(merge_decisions(&recs).is_empty());
    }

    #[test]
    fn unparseable_created_at_loses_but_still_compares() {
        let good = parse_line(&line("d_1", "2026-09-10T11:00:00Z", "a", "Kindle", "once")).unwrap();
        let broken = parse_line(&line("d_2", "yesterday", "a", "Kindle", "once")).unwrap();
        let merged = merge_decisions(&[broken.clone(), good.clone()]);
        assert_eq!(merged[0].id, "d_1");
        assert_eq!(merge_decisions(&[good, broken])[0].id, "d_1");
    }

    #[test]
    fn a_line_we_do_not_understand_is_kept_verbatim() {
        let raw = r#"{"type":"prophecy","id":"p_1"}"#;
        assert_eq!(parse_line(raw), Some(SharedRecord::Unknown(raw.to_string())));
        assert_eq!(parse_line("not json"), Some(SharedRecord::Unknown("not json".to_string())));
        assert_eq!(parse_line("   "), None);
    }
}
