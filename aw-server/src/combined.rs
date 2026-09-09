//! Roadmap 3.4 — the datastore adapter for [`aw_combined`].
//!
//! `aw-combined` is pure by design: no datastore, no file I/O, no clock. This module is the layer
//! that reads a [`Datastore`], decides which buckets are *activity* and which are *idle*, runs the
//! pipeline, and shapes the result for a view. It is the only place that knows both halves.
//!
//! **Deliberately not inside `android/`.** That module is `#[cfg(target_os = "android")]`, so a
//! desktop `cargo check` never type-checks a line of it — and this repo's local check is a desktop
//! check (`scripts/check-local.sh` says so in as many words). Keeping the logic here means the part
//! that can actually be wrong is verified before an APK is ever built; the JNI wrapper stays a
//! string-in, string-out shell.
//!
//! See `aw-android/docs/04_COMBINED_TIMELINE.md` §1–2.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};

use aw_combined::{
    coalesce, compute_segments, default_min_contention, resolve_bucket_device, BucketEvents,
    PipelineInput, Segment,
};
use aw_datastore::Datastore;
use aw_models::Event;

/// The bucket type that counts as activity.
///
/// Only `currentwindow`. `web.tab.current` is deliberately excluded even though a device may have
/// one: it overlaps the window bucket for the same instants, so including it would put a browser
/// *tab* and its *window* in contention with each other on one device and let a tab title win the
/// combined track. Web data stays in the raw per-device view. Revisit if the owner asks for
/// tab-level detail in the combined track.
const ACTIVITY_TYPE: &str = "currentwindow";

/// The bucket type carrying AFK status. Absent on Android — `aw-watcher-android` only records
/// while the screen is on and in use — but a synced desktop peer's afk bucket arrives through the
/// same datastore, and `aw-combined`'s idle contract wants only the genuinely-idle events.
const AFK_TYPE: &str = "afkstatus";
const AFK_STATUS_KEY: &str = "status";
const AFK_STATUS_IDLE: &str = "afk";

/// What [`combined_timeline`] needs beyond the datastore. `hostname_to_uuid` cannot be read here:
/// it lives in the Syncthing folder behind Android's SAF, which only Kotlin can open.
pub struct TimelineRequest {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub own_device: String,
    pub hostname_to_uuid: HashMap<String, String>,
}

/// A human label for one event's `data`, for a track row. Falls back rather than failing (**R19**).
fn label(data: &Map<String, Value>) -> String {
    for key in ["app", "title", "url"] {
        if let Some(Value::String(s)) = data.get(key) {
            if !s.is_empty() {
                return s.clone();
            }
        }
    }
    "(unknown)".to_string()
}

/// True for an event that represents an *idle* period, per `aw-combined`'s idle contract.
fn is_idle(event: &Event) -> bool {
    matches!(
        event.data.get(AFK_STATUS_KEY),
        Some(Value::String(s)) if s == AFK_STATUS_IDLE
    )
}

/// Read one day (or any range) out of the datastore and return the combined track, the per-device
/// tracks, and the totals, as JSON.
///
/// Bucket ids are visited in sorted order so the JSON is byte-identical across runs (**R18**); the
/// pipeline itself is order-independent, but the per-device tracks are assembled here.
pub fn combined_timeline(ds: &Datastore, req: &TimelineRequest) -> Result<Value, String> {
    let buckets = ds
        .get_buckets()
        .map_err(|e| format!("could not list buckets: {e:?}"))?;

    let mut ids: Vec<&String> = buckets.keys().collect();
    ids.sort();

    let mut activity: Vec<BucketEvents> = Vec::new();
    let mut idle: Vec<BucketEvents> = Vec::new();
    for id in ids {
        let bucket = &buckets[id];
        let is_activity = bucket._type == ACTIVITY_TYPE;
        let is_afk = bucket._type == AFK_TYPE;
        if !is_activity && !is_afk {
            continue;
        }
        let events = ds
            .get_events(id, Some(req.start), Some(req.end), None)
            .map_err(|e| format!("could not read events from {id}: {e:?}"))?;
        if is_activity {
            activity.push(BucketEvents { bucket_id: id.clone(), events });
        } else {
            let events = events.into_iter().filter(is_idle).collect();
            idle.push(BucketEvents { bucket_id: id.clone(), events });
        }
    }

    let input = PipelineInput {
        own_device: req.own_device.clone(),
        hostname_to_uuid: req.hostname_to_uuid.clone(),
        activity,
        idle,
        // D15/Q1's default. Not a setting yet — nothing in the app exposes one, and 3.4 is about
        // seeing the track at all. Wire it to a preference when Phase 4 gives it a home.
        min_contention: default_min_contention(),
    };

    // Per-device tracks are built from the *raw* events, before idle subtraction and before
    // segmentation: R11 says the per-device rows are unmodified truth, always available underneath
    // the combined track for comparison. They use `resolve_bucket_device` so origin is decided by
    // exactly the same rule (R19) the pipeline uses.
    let devices = device_tracks(&input);

    let segments = coalesce(compute_segments(input.clone()));
    let combined_seconds: i64 = segments
        .iter()
        .map(|s| (s.end - s.start).num_seconds())
        .sum();

    Ok(json!({
        "start": req.start,
        "end": req.end,
        "own_device": req.own_device,
        "combined_seconds": combined_seconds,
        "combined": segments.iter().map(combined_row).collect::<Vec<_>>(),
        "devices": devices,
    }))
}

/// One row of the combined track: what counted, and whether the view must shade it (**R8**).
fn combined_row(seg: &Segment) -> Value {
    let fg = seg.foreground_slice();
    json!({
        "start": seg.start,
        "end": seg.end,
        "seconds": (seg.end - seg.start).num_seconds(),
        "label": label(&fg.data),
        "device": fg.device,
        "state": seg.state,
        "unresolved": seg.unresolved,
        "absorbed_short_contention": seg.absorbed_short_contention,
        "background": seg
            .background_slices()
            .map(|s| json!({ "device": s.device, "label": label(&s.data) }))
            .collect::<Vec<_>>(),
    })
}

/// The raw per-device rows, sorted by device uuid so the output is stable (**R18**).
fn device_tracks(input: &PipelineInput) -> Vec<Value> {
    // device -> (rows, total seconds). Rows keep bucket order, which is sorted-id order.
    let mut by_device: HashMap<String, Vec<Value>> = HashMap::new();
    let mut totals: HashMap<String, i64> = HashMap::new();
    for bucket in &input.activity {
        let device = resolve_bucket_device(&bucket.events, &bucket.bucket_id, input);
        for event in &bucket.events {
            let seconds = event.duration.num_seconds();
            if seconds <= 0 {
                continue; // the unlock watcher's zero-width heartbeats would draw nothing
            }
            *totals.entry(device.clone()).or_insert(0) += seconds;
            by_device.entry(device.clone()).or_default().push(json!({
                "start": event.timestamp,
                "end": event.timestamp + event.duration,
                "seconds": seconds,
                "label": label(&event.data),
            }));
        }
    }

    // hostname_to_uuid is a lookup map; invert it once so a row can show a name, not a uuid.
    let mut uuid_to_hostname: HashMap<&str, &str> = HashMap::new();
    for (hostname, uuid) in &input.hostname_to_uuid {
        uuid_to_hostname.insert(uuid, hostname);
    }

    let mut out: Vec<String> = by_device.keys().cloned().collect();
    out.sort();
    out.into_iter()
        .map(|device| {
            let mut rows = by_device.remove(&device).unwrap_or_default();
            rows.sort_by(|a, b| a["start"].to_string().cmp(&b["start"].to_string()));
            json!({
                "device": device,
                "hostname": uuid_to_hostname.get(device.as_str()),
                "is_own": device == input.own_device,
                "total_seconds": totals.get(&device).copied().unwrap_or(0),
                "events": rows,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn data(app: &str) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("app".to_string(), json!(app));
        m
    }

    #[test]
    fn label_prefers_app_then_title_then_url() {
        assert_eq!(label(&data("YouTube")), "YouTube");
        let mut m = Map::new();
        m.insert("title".to_string(), json!("A page"));
        assert_eq!(label(&m), "A page");
        assert_eq!(label(&Map::new()), "(unknown)");
    }

    #[test]
    fn label_skips_empty_strings() {
        let mut m = Map::new();
        m.insert("app".to_string(), json!(""));
        m.insert("title".to_string(), json!("fallback"));
        assert_eq!(label(&m), "fallback");
    }

    #[test]
    fn is_idle_only_matches_afk_status() {
        let mk = |v: Value| {
            let mut m = Map::new();
            m.insert(AFK_STATUS_KEY.to_string(), v);
            Event { id: None, timestamp: Utc::now(), duration: chrono::Duration::seconds(1), data: m }
        };
        assert!(is_idle(&mk(json!("afk"))));
        assert!(!is_idle(&mk(json!("not-afk"))));
        assert!(!is_idle(&mk(json!(42))));
        assert!(!is_idle(&Event {
            id: None,
            timestamp: Utc::now(),
            duration: chrono::Duration::seconds(1),
            data: Map::new(),
        }));
    }
}
