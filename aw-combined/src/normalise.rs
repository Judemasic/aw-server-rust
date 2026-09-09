//! ① Normalise: resolve each event's origin device, subtract idle, drop zero-width intervals,
//! return a deterministically ordered flat list.

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};

use aw_models::{Event, EVENT_ORIGIN_KEY};

use crate::{BucketEvents, PipelineInput};

/// A single device-activity span. Private to the crate: the public output type is [`crate::Segment`].
#[derive(Clone, Debug)]
pub(crate) struct Interval {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub device: String,
    pub bucket_id: String,
    pub data: Map<String, Value>,
}

/// Upstream's own marker (see `timelineLabels.ts`); the peer name follows its **first** occurrence.
const SYNCED_FROM: &str = "-synced-from-";

/// Resolve the origin device of one event. Never fails, never drops the event (R19):
///
/// 1. `data["$aw.origin.device"]` is a string -> use it (roadmap 3.1 tag).
/// 2. else `bucket_id` contains `-synced-from-<peer>` -> look `<peer>` up in `hostname_to_uuid`;
///    hit -> the UUID, miss -> the captured string verbatim. The miss branch is load-bearing:
///    upstream's stopwatch path names the peer by UUID, not hostname (roadmap 1.5), and an unknown
///    hostname stays visible rather than being merged into `own_device`.
/// 3. else (local bucket, no suffix, no tag) -> `own_device`.
///
/// Public so callers building *per-device* views (roadmap 3.4) attribute raw events by exactly the
/// same rule the pipeline does, rather than reimplementing it and drifting.
pub fn resolve_device(event: &Event, bucket_id: &str, input: &PipelineInput) -> String {
    if let Some(Value::String(uuid)) = event.data.get(EVENT_ORIGIN_KEY) {
        return uuid.clone();
    }
    if let Some(idx) = bucket_id.find(SYNCED_FROM) {
        let peer = &bucket_id[idx + SYNCED_FROM.len()..];
        return match input.hostname_to_uuid.get(peer) {
            Some(uuid) => uuid.clone(),
            None => peer.to_string(),
        };
    }
    input.own_device.clone()
}

/// Flatten buckets to intervals, resolving origin and dropping `end <= start` (Android watchers
/// emit zero-duration heartbeats, e.g. `aw-watcher-android-unlock`).
fn to_intervals(buckets: &[BucketEvents], input: &PipelineInput) -> Vec<Interval> {
    let mut out = Vec::new();
    for bucket in buckets {
        for event in &bucket.events {
            let start = event.timestamp;
            let end = event.timestamp + event.duration;
            if end <= start {
                continue;
            }
            out.push(Interval {
                start,
                end,
                device: resolve_device(event, &bucket.bucket_id, input),
                bucket_id: bucket.bucket_id.clone(),
                data: event.data.clone(),
            });
        }
    }
    out
}

/// Subtract idle `holes` from one interval. An idle period inside the interval splits it in two;
/// slivers where `end <= start` are dropped.
fn subtract(iv: &Interval, holes: &[(DateTime<Utc>, DateTime<Utc>)]) -> Vec<Interval> {
    let mut pieces = vec![(iv.start, iv.end)];
    for &(hs, he) in holes {
        let mut next = Vec::with_capacity(pieces.len() + 1);
        for (ps, pe) in pieces {
            if he <= ps || hs >= pe {
                next.push((ps, pe)); // no overlap
                continue;
            }
            if hs > ps {
                next.push((ps, hs));
            }
            if he < pe {
                next.push((he, pe));
            }
        }
        pieces = next;
    }
    pieces
        .into_iter()
        .filter(|(s, e)| e > s)
        .map(|(s, e)| Interval {
            start: s,
            end: e,
            device: iv.device.clone(),
            bucket_id: iv.bucket_id.clone(),
            data: iv.data.clone(),
        })
        .collect()
}

/// ① for the whole input. Result is sorted by `(start, end, device, bucket_id)` — a total order,
/// so the output is independent of input order (R18).
pub(crate) fn normalise(input: &PipelineInput) -> Vec<Interval> {
    let activity = to_intervals(&input.activity, input);
    let idle = to_intervals(&input.idle, input);

    let mut result = Vec::new();
    for iv in &activity {
        let holes: Vec<(DateTime<Utc>, DateTime<Utc>)> = idle
            .iter()
            .filter(|h| h.device == iv.device)
            .map(|h| (h.start, h.end))
            .collect();
        result.extend(subtract(iv, &holes));
    }

    result.sort_by(|a, b| {
        a.start
            .cmp(&b.start)
            .then(a.end.cmp(&b.end))
            .then_with(|| a.device.cmp(&b.device))
            .then_with(|| a.bucket_id.cmp(&b.bucket_id))
    });
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};
    use std::collections::HashMap;
    use serde_json::json;

    fn t(min: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 9, 14, 0, 0).unwrap() + Duration::minutes(min)
    }

    fn ev(start_min: i64, dur_min: i64, data: Map<String, Value>) -> Event {
        Event {
            id: None,
            timestamp: t(start_min),
            duration: Duration::minutes(dur_min),
            data,
        }
    }

    fn input(activity: Vec<BucketEvents>, idle: Vec<BucketEvents>) -> PipelineInput {
        PipelineInput {
            own_device: "own-uuid".to_string(),
            hostname_to_uuid: HashMap::new(),
            activity,
            idle,
            min_contention: crate::default_min_contention(),
        }
    }

    #[test]
    fn origin_tag_wins() {
        let mut data = Map::new();
        data.insert(EVENT_ORIGIN_KEY.to_string(), json!("tagged-uuid"));
        let inp = input(
            vec![BucketEvents {
                bucket_id: "aw-watcher-window_host-synced-from-somehost".to_string(),
                events: vec![ev(0, 10, data)],
            }],
            vec![],
        );
        assert_eq!(normalise(&inp)[0].device, "tagged-uuid");
    }

    #[test]
    fn synced_from_hostname_maps_to_uuid() {
        let mut inp = input(
            vec![BucketEvents {
                bucket_id: "aw-watcher-android-synced-from-jude_s_s25_ultra".to_string(),
                events: vec![ev(0, 10, Map::new())],
            }],
            vec![],
        );
        inp.hostname_to_uuid
            .insert("jude_s_s25_ultra".to_string(), "mapped-uuid".to_string());
        assert_eq!(normalise(&inp)[0].device, "mapped-uuid");
    }

    #[test]
    fn synced_from_miss_keeps_captured_string() {
        let inp = input(
            vec![BucketEvents {
                bucket_id: "aw-stopwatch-synced-from-ad0c6c34-d388-4ef0-b906-976bd760b22d"
                    .to_string(),
                events: vec![ev(0, 10, Map::new())],
            }],
            vec![],
        );
        assert_eq!(
            normalise(&inp)[0].device,
            "ad0c6c34-d388-4ef0-b906-976bd760b22d"
        );
    }

    #[test]
    fn local_bucket_is_own_device() {
        let inp = input(
            vec![BucketEvents {
                bucket_id: "aw-watcher-window_myhost".to_string(),
                events: vec![ev(0, 10, Map::new())],
            }],
            vec![],
        );
        assert_eq!(normalise(&inp)[0].device, "own-uuid");
    }

    #[test]
    fn zero_duration_dropped() {
        let inp = input(
            vec![BucketEvents {
                bucket_id: "aw-watcher-android-unlock_x".to_string(),
                events: vec![ev(0, 0, Map::new()), ev(5, 10, Map::new())],
            }],
            vec![],
        );
        assert_eq!(normalise(&inp).len(), 1);
    }

    #[test]
    fn idle_splits_interval() {
        let inp = input(
            vec![BucketEvents {
                bucket_id: "aw-watcher-window_myhost".to_string(),
                events: vec![ev(0, 60, Map::new())],
            }],
            vec![BucketEvents {
                bucket_id: "aw-watcher-afk_myhost".to_string(),
                events: vec![ev(20, 20, Map::new())],
            }],
        );
        let out = normalise(&inp);
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].start, out[0].end), (t(0), t(20)));
        assert_eq!((out[1].start, out[1].end), (t(40), t(60)));
    }
}
