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
use rocket::serde::json::Value;
use rocket::State;

use crate::combined::{combined_timeline, TimelineRequest};
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

/// `GET /api/0/combined/timeline?start=…&end=…&hostnames=…`
///
/// `hostnames` is an optional JSON object mapping a peer's hostname to its device uuid. It exists
/// only as a **fallback**: [`aw_combined::resolve_bucket_device`] prefers the `$aw.origin.device`
/// tag written at merge (roadmap 3.1), and only falls back to the `-synced-from-<peer>` suffix in
/// the bucket id for databases holding pre-3.1 events. On Android the real map lives in the
/// Syncthing folder behind SAF, which only Kotlin can open — hence a parameter rather than a read.
/// Omitted, an untagged peer bucket shows its hostname where a uuid would go, which is legible.
#[get("/timeline?<start>&<end>&<hostnames>")]
pub fn timeline(
    start: String,
    end: String,
    hostnames: Option<String>,
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
}
