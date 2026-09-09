//! Combined-timeline pipeline, first half: normalise ①, segment ②, classify ③.
//!
//! Turns every device's post-sync events into a list of non-overlapping **atomic** segments, each
//! labelled [`SegmentState::Settled`] (0-1 device active) or [`SegmentState::Contended`] (>=2
//! devices active). Idle time is subtracted before the contention test, and contention shorter than
//! [`PipelineInput::min_contention`] is absorbed back into `Settled` (roadmap D15/Q1).
//!
//! This crate is pure: no datastore access, no file I/O, no clock reads. Callers pass events in.
//! Attribution (which activity "wins" a contended segment) is roadmap 3.3; decisions are Phase 4.
//!
//! See `aw-android/docs/04_COMBINED_TIMELINE.md` §2.
//!
//! # Scaling
//!
//! [`compute_segments`] is **O(n²)** in the event count: [`segment`] tests every interval against
//! every boundary. Measured on desktop (release, 3 devices): 3k events 18 ms, 6k 52 ms, 15k 253 ms,
//! 30k 870 ms. A single day is a few thousand events, so this is comfortable for the day view; a
//! week or month view would need a sweep line that keeps a running active-set instead of rescanning
//! (roadmap 3.4 onward).

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use serde_json::{Map, Value};

use aw_models::Event;

mod attribute;
mod classify;
mod coalesce;
mod normalise;
mod segment;

pub use coalesce::coalesce;
pub use normalise::resolve_device;

pub use aw_models::EVENT_ORIGIN_KEY;

/// One bucket's events, local or imported. `bucket_id` is kept because it is the fallback route to
/// the origin device for events imported before roadmap 3.1 (see [`normalise`], rule 2).
#[derive(Clone, Debug)]
pub struct BucketEvents {
    pub bucket_id: String,
    pub events: Vec<Event>,
}

/// Everything [`compute_segments`] needs. Built by the caller from its datastore; this crate never
/// touches one.
#[derive(Clone, Debug)]
pub struct PipelineInput {
    /// This device's UUID. Its own events carry no origin tag and are attributed to it.
    pub own_device: String,
    /// hostname -> device UUID, built from `devices/<uuid>/meta.json`. Lookup only, never iterated,
    /// so it never enters an output ordering (R18).
    pub hostname_to_uuid: HashMap<String, String>,
    /// Activity buckets (currentwindow-type), local and `-synced-from-*` alike.
    pub activity: Vec<BucketEvents>,
    /// Idle buckets. **Contract: every event here is an idle period.** The caller filters by status
    /// (e.g. aw-watcher-afk `status == "afk"`); this crate knows no AFK schema. Empty on Android,
    /// where the watcher only records while the screen is on and in use. Origin is resolved by the
    /// same rule as `activity`.
    pub idle: Vec<BucketEvents>,
    /// Minimum contention duration (roadmap D15/Q1). Use [`default_min_contention`].
    pub min_contention: Duration,
}

/// Default for [`PipelineInput::min_contention`] in seconds (roadmap D15/Q1).
pub const DEFAULT_MIN_CONTENTION_SECS: i64 = 60;

/// Default minimum contention duration: contended runs shorter than this are absorbed into
/// `Settled` (roadmap D15/Q1).
pub fn default_min_contention() -> Duration {
    Duration::seconds(DEFAULT_MIN_CONTENTION_SECS)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SegmentState {
    Settled,
    Contended,
}

/// One device's activity covering a segment. Classification counts *distinct* [`ActiveSlice::device`]
/// values, so a device with two overlapping activity buckets contributes two slices but one device.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ActiveSlice {
    pub device: String,
    pub bucket_id: String,
    pub data: Map<String, Value>,
    /// Start of the *originating* activity interval this slice was cut from — **after** idle
    /// subtraction, so idle time never inflates a device's claim. Not the segment's own start.
    pub source_start: DateTime<Utc>,
    /// End of that interval. `source_end - source_start` is the duration provisional attribution's
    /// rule 1 compares (R17): the longest-running originating activity wins the segment, not the
    /// segment's own (identical for every slice) length.
    pub source_end: DateTime<Utc>,
}

/// An atomic segment: a maximal time span over which the set of covering [`ActiveSlice`]s does not
/// change. Boundaries come from *every* device's app changes, so adjacent segments are never merged
/// here (coalescing on identical attribution is roadmap 3.3/3.4).
///
/// **Invariant:** `state == Settled` does **not** imply <=1 active device. A short-contention run
/// demoted by the minimum-duration pass is `Settled` with `absorbed_short_contention == true` and
/// keeps every slice. Consumers wanting "was there really only one device here?" must inspect
/// [`Segment::active`], not [`Segment::state`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Segment {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub state: SegmentState,
    /// Every device-activity covering this segment, sorted by `(device, bucket_id)`. May hold more
    /// than one slice per device.
    pub active: Vec<ActiveSlice>,
    /// True when this segment held >=2 distinct devices but its contended run was shorter than
    /// `min_contention` and was demoted to `Settled`.
    pub absorbed_short_contention: bool,
    /// Index into [`Segment::active`] of the slice that counts as **foreground** (R6: exactly one
    /// activity is foreground at any instant). Set by ⑤ provisional attribution. Every segment has
    /// at least one slice, so this is always a valid index. `usize::MAX` before ⑤ has run.
    pub foreground: usize,
    /// True when this segment is contended and no decision has resolved it, so the view shades it
    /// (R8). Provisional attribution changes *which* activity counts; it never clears the shading
    /// (`04` §2.4). A short-contention segment demoted to `Settled` has `unresolved == false`.
    pub unresolved: bool,
}

impl Segment {
    /// The slice ⑤ picked as foreground. Panics only if called before attribution has run.
    pub fn foreground_slice(&self) -> &ActiveSlice {
        &self.active[self.foreground]
    }

    /// Every slice other than the foreground one, in `active` order.
    pub fn background_slices(&self) -> impl Iterator<Item = &ActiveSlice> {
        self.active
            .iter()
            .enumerate()
            .filter(move |(i, _)| *i != self.foreground)
            .map(|(_, s)| s)
    }
}

/// Run ① normalise, ② segment, ③ classify, ⑤ provisional attribution. Deterministic: identical
/// input (in any event/bucket order) yields byte-identical output (R18).
///
/// Step ④ (apply decisions) is Phase 4 and slots in between ③ and ⑤. Step ⑥ ([`coalesce`]) is a
/// separate opt-in call — the pipeline stays lossless by default so Phase 4 can attach decisions
/// to the atomic segments.
pub fn compute_segments(input: PipelineInput) -> Vec<Segment> {
    let intervals = normalise::normalise(&input);
    let mut segments = segment::segment(&intervals);
    classify::classify(&mut segments, input.min_contention);
    attribute::attribute(&mut segments);
    segments
}
