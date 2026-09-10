//! Roadmap 3.5 — the combined timeline over HTTP.
//!
//! [`crate::combined`] already does the work; this is the second way to call it. The first is the
//! JNI entry point in `android/mod.rs`, which exists because 3.4 drew the timeline with a native
//! Android `View`.
//!
//! **Why a REST route as well.** The owner's requirement for the combined timeline is that it works
//! on **PC, tablet and phone**, and that it looks like aw-webui's Activity view. A native Android
//! `View` cannot ever satisfy the first half of that — there is no Android on a PC — so the view is
//! being rebuilt in aw-webui (Vue), which on Android renders in the WebView against this same
//! embedded server and on a PC renders in a browser. aw-webui speaks HTTP and cannot call JNI, so
//! it needs this. See `aw-android/docs/06_ROADMAP.md` §3.5.
//!
//! The JNI entry point stays for now: 3.4's native screen still ships until the Vue view replaces
//! it, and deleting a working screen before its replacement exists would leave the owner with none.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use rocket::http::Status;
use rocket::serde::json::{Json, Value};
use rocket::State;

use aw_combined::synced_from_hostname;

use crate::combined::{combined_timeline, store_record, stored_records, TimelineRequest};
use crate::endpoints::{HttpErrorJson, ServerState};

/// Parse one RFC 3339 timestamp, naming which parameter was wrong. The client is a browser sending
/// `Date#toISOString()`, so a failure here is a bug in the caller, not user input — say which.
fn parse_ts(name: &str, raw: &str) -> Result<DateTime<Utc>, HttpErrorJson> {
    DateTime::parse_from_rfc3339(raw)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| {
            HttpErrorJson::new(
                Status::BadRequest,
                format!("`{name}` is not an RFC 3339 timestamp: {e}"),
            )
        })
}

/// The name this device records for **itself** in a decision it makes.
///
/// ⚠️ **Not `gethostname()`.** The embedded server on Android answers `localhost` on every device,
/// and `localhost` is the one name that means something different everywhere it is read: a decision
/// made on the phone saying `device_role: "localhost"` reads on the tablet as *the tablet*. 4.2a hit
/// this on hardware — the phone left an eight-second tail asking and the tablet settled the same
/// tail in favour of itself, which is precisely the disagreement **R18** forbids. `apply` was
/// hardened against acting on it, but a name that lies is still a name that lies: it is written into
/// every `scope: always` rule's signature, and a rule keyed on `localhost` can never mean the same
/// thing on two devices.
///
/// The real name is already in the database. The Android watcher creates its buckets with the
/// hostname Kotlin derives from `Settings.Global.DEVICE_NAME` (`DeviceHostname.kt`), which is the
/// same name a peer reads off the `-synced-from-<peer>` suffix — so both ends of a decision agree.
/// Read it back from the device's **own** buckets, the ones without that suffix.
///
/// `gethostname()` stays as the fallback: on a desktop it is a real name, and on a fresh Android
/// install with no local buckets yet there is nothing better to say. Disagreeing local buckets fall
/// back too — several names means we do not know ours, and guessing is how this went wrong before.
fn own_hostname(state: &ServerState) -> String {
    let from_buckets = state.datastore.get_buckets().ok().and_then(|buckets| {
        own_name_from(buckets.iter().map(|(id, b)| (id.as_str(), b.hostname.as_str())))
    });
    from_buckets.unwrap_or_else(|| {
        gethostname::gethostname()
            .into_string()
            .unwrap_or_else(|_| String::new())
    })
}

/// The one name this device's own buckets agree on, if there is one.
///
/// `localhost` and `unknown` are discarded rather than returned: both are what a component says
/// when it does *not* know the name, and neither identifies a device to a peer. Split out from
/// [`own_hostname`] so the rule can be tested without standing up a datastore.
fn own_name_from<'a>(buckets: impl Iterator<Item = (&'a str, &'a str)>) -> Option<String> {
    let mut names: Vec<String> = buckets
        .filter(|(id, _)| synced_from_hostname(id).is_none())
        .map(|(_, hostname)| hostname.trim().to_string())
        .filter(|h| !h.is_empty() && h != "localhost" && h != "unknown")
        .collect();
    names.sort_unstable();
    names.dedup();
    match names.len() {
        1 => names.pop(),
        _ => None,
    }
}

/// `GET /api/0/combined/timeline?start=…&end=…&hostnames=…&sliver=…`
///
/// `hostnames` is an optional JSON object mapping a peer's hostname to its device uuid. It exists
/// only as a **fallback**: [`aw_combined::resolve_bucket_device`] prefers the `$aw.origin.device`
/// tag written at merge (roadmap 3.1), and only falls back to the `-synced-from-<peer>` suffix in
/// the bucket id for databases holding pre-3.1 events. On Android the real map lives in the
/// Syncthing folder behind SAF, which only Kotlin can open — hence a parameter rather than a read.
/// Omitted, an untagged peer bucket shows its hostname where a uuid would go, which is legible.
///
/// `sliver` is roadmap 4.5's smoothing threshold in seconds — how short a stretch has to be before
/// it stops being its own block. Omitted it is 15s; `0` is off, and the day is drawn literally.
/// A query parameter and not a stored setting on purpose: smoothing is a *view* transform, so the
/// number arrives with the request and the day recomputes, with nothing written either way.
#[get("/timeline?<start>&<end>&<hostnames>&<sliver>")]
pub fn timeline(
    start: String,
    end: String,
    hostnames: Option<String>,
    sliver: Option<i64>,
    state: &State<ServerState>,
) -> Result<Value, HttpErrorJson> {
    let start = parse_ts("start", &start)?;
    let end = parse_ts("end", &end)?;
    if end <= start {
        return Err(HttpErrorJson::new(
            Status::BadRequest,
            format!("`end` ({end}) must be after `start` ({start})"),
        ));
    }

    let hostname_to_uuid: HashMap<String, String> = match hostnames.as_deref() {
        None | Some("") => HashMap::new(),
        Some(raw) => serde_json::from_str(raw).map_err(|e| {
            HttpErrorJson::new(
                Status::BadRequest,
                format!("`hostnames` is not a JSON object of hostname->uuid: {e}"),
            )
        })?,
    };

    let req = TimelineRequest {
        start,
        end,
        // The server already knows which device it is; unlike the JNI path there is nothing for a
        // caller to get wrong here, so it is deliberately not a parameter.
        own_device: state.device_id.clone(),
        hostname_to_uuid,
        own_hostname: own_hostname(state),
        // Clamped rather than rejected: a bad number here cannot corrupt anything -- nothing is
        // written -- and refusing the whole day over a display preference would be the worse
        // failure. An hour is far past any threshold that still means "sliver".
        sliver_secs: sliver.map(|s| s.clamp(0, 3600)),
    };

    combined_timeline(&state.datastore, &req)
        .map_err(|e| HttpErrorJson::new(Status::InternalServerError, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ts_accepts_a_browser_iso_string() {
        // What `new Date().toISOString()` produces, which is what the Vue view will send.
        let t = parse_ts("start", "2026-09-09T00:00:00.000Z").unwrap();
        assert_eq!(t.to_rfc3339(), "2026-09-09T00:00:00+00:00");
    }

    #[test]
    fn parse_ts_keeps_the_instant_when_an_offset_is_given() {
        let t = parse_ts("start", "2026-09-09T02:00:00+02:00").unwrap();
        assert_eq!(t.to_rfc3339(), "2026-09-09T00:00:00+00:00");
    }

    #[test]
    fn parse_ts_names_the_offending_parameter() {
        let err = parse_ts("end", "not a date").unwrap_err();
        let msg = serde_json::to_string(&err).unwrap();
        assert!(msg.contains("`end`"), "message should name the parameter: {msg}");
    }

    #[test]
    fn own_name_is_what_this_devices_own_watchers_call_it() {
        // What an S25U's database actually looks like: its own watchers, plus the tablet's buckets
        // arriving over sync. Only the first kind says anything about who *this* device is.
        let name = own_name_from(
            [
                ("aw-watcher-android-test", "jude_s_s25_ultra"),
                ("aw-watcher-android-unlock", "jude_s_s25_ultra"),
                ("aw-watcher-android-test-synced-from-galaxy_tab_s9", "galaxy_tab_s9"),
            ]
            .into_iter(),
        );
        assert_eq!(name.as_deref(), Some("jude_s_s25_ultra"));
    }

    #[test]
    fn own_name_refuses_the_names_that_mean_i_do_not_know() {
        // The bug this function exists for: `localhost` is what the embedded server calls itself on
        // every Android device, so recording it in a decision makes the phone's answer read on the
        // tablet as being about the tablet. Better to fall back than to write a name that lies.
        assert_eq!(own_name_from([("aw-server", "localhost")].into_iter()), None);
        assert_eq!(own_name_from([("aw-watcher-android-test", "unknown")].into_iter()), None);
        assert_eq!(own_name_from([("aw-watcher-android-test", "  ")].into_iter()), None);
    }

    #[test]
    fn own_name_does_not_guess_when_its_own_buckets_disagree() {
        let name = own_name_from(
            [
                ("aw-watcher-android-test", "jude_s_s25_ultra"),
                ("aw-stopwatch", "an_older_name"),
            ]
            .into_iter(),
        );
        assert_eq!(name, None);
    }

    #[test]
    fn own_name_is_none_before_any_watcher_has_run() {
        assert_eq!(own_name_from([].into_iter()), None);
    }
}

/// `GET /api/0/combined/decisions`
///
/// Every decision and tombstone this device holds, as the stored lines themselves — canonical JSON,
/// keys sorted (see [`crate::combined::store_record`]). The caller that matters is the Android sync
/// (roadmap 4.2), which copies these lines into `decisions.jsonl` in the Syncthing folder, so what
/// comes out of here has to be exactly what should go into that file.
#[get("/decisions")]
pub fn decisions_get(state: &State<ServerState>) -> Result<Value, HttpErrorJson> {
    let stored = state
        .datastore
        .get_key_values(&format!("{}%", crate::combined::DECISION_KEY_PREFIX))
        .map_err(|e| {
            HttpErrorJson::new(
                Status::InternalServerError,
                format!("could not read decisions: {e:?}"),
            )
        })?;
    let mut keys: Vec<&String> = stored.keys().collect();
    keys.sort();
    let lines: Vec<&str> = keys.into_iter().map(|k| stored[k].as_str()).collect();
    Ok(serde_json::json!({ "decisions": lines }).into())
}

/// `POST /api/0/combined/decisions`
///
/// Body: one decision or tombstone record, exactly as `05_DATA_MODEL.md` §4 describes it. Storing
/// it is what makes the next timeline read come back resolved (**R26**) — the pipeline applies
/// whatever is stored, so there is no second "recompute" call to make and none to forget.
///
/// Accepts a record whose `id` already exists and overwrites it. That is not a merge: the merge
/// (`aw_combined::merge_decisions`) happens at read time across every device's records, and a
/// repeated id is by definition the same decision, not a competing one.
#[post("/decisions", data = "<record>")]
pub fn decisions_post(
    record: Json<Value>,
    state: &State<ServerState>,
) -> Result<Value, HttpErrorJson> {
    // Serialised from the parsed body rather than taken as a string: Rocket has already validated
    // that it is JSON, and `store_record` normalises it anyway.
    let raw = record.0.to_string();
    let id = store_record(&state.datastore, &raw)
        .map_err(|e| HttpErrorJson::new(Status::BadRequest, e))?;
    Ok(serde_json::json!({ "success": true, "id": id }).into())
}

/// `GET /api/0/combined/decisions/effective`
///
/// What actually applies, after the deterministic merge across every device's records
/// (`05_DATA_MODEL.md` §4.2) — revoked decisions dropped, one winner per window and signature.
/// The view uses it to name the rule that auto-resolved a segment; a human reads it to see why the
/// timeline looks the way it does.
#[get("/decisions/effective")]
pub fn decisions_effective(state: &State<ServerState>) -> Result<Value, HttpErrorJson> {
    let records = stored_records(&state.datastore)
        .map_err(|e| HttpErrorJson::new(Status::InternalServerError, e))?;
    let merged = aw_combined::merge_decisions(&records);
    Ok(serde_json::json!({ "decisions": merged }).into())
}
