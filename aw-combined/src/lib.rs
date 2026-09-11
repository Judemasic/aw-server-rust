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

mod apply;
mod attribute;
mod classify;
mod coalesce;
pub mod decision;
mod exclude;
mod normalise;
mod segment;
pub mod settings;
mod smooth;

pub use coalesce::coalesce;
pub use decision::{merge_decisions, parse_line, parse_records, Decision, SharedRecord};
pub use exclude::NotCountedRule;
pub use settings::{
    effective_settings, is_shared_setting_key, parse_settings, plan_settings_sync,
    setting_to_json_line, Setting, SettingsPlan,
};
pub use normalise::{resolve_bucket_device, synced_from_hostname};
pub use smooth::{smooth, SmoothOptions, DEFAULT_SLIVER_SECS, NOISE_FLOOR_SECS};

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
    /// Activity the owner has said never counts (roadmap 4.6), compiled from the categories
    /// flagged `data.not_counted`. Applied at ②b, before classification, so an excluded app is not
    /// even a competitor. Empty is the behaviour every earlier phase had.
    pub not_counted: Vec<NotCountedRule>,
    /// The owner's decisions, already merged across every device ([`merge_decisions`]). Step ④
    /// applies them; an empty vec is the Phase 3 behaviour, unchanged.
    ///
    /// **Order does not matter and must not matter** (R18): ④ picks between two candidates by the
    /// same precedence the merge uses, never by position.
    pub decisions: Vec<Decision>,
}

impl PipelineInput {
    /// device uuid -> the role name a decision's signature uses for it.
    ///
    /// Today that is the device's **hostname**, not `05_DATA_MODEL.md`'s "phone"/"tablet" role: the
    /// role lives in `devices/<uuid>/meta.json` behind Android's SAF, which neither this crate nor
    /// the web view can open. A hostname is the strongest identity both ends *can* agree on — it is
    /// already the shared folder's naming, and it is the same string on every device, which is what
    /// R18 actually requires. The cost is that a rule does not survive renaming a device; the
    /// alternative, the *local* display name the sheet used to write, did not even survive being
    /// read on a second device.
    pub fn roles_by_uuid(&self) -> HashMap<String, String> {
        self.hostname_to_uuid
            .iter()
            .map(|(hostname, uuid)| (uuid.clone(), hostname.clone()))
            .collect()
    }
}

/// Default for [`PipelineInput::min_contention`] in seconds (roadmap D15/Q1).
pub const DEFAULT_MIN_CONTENTION_SECS: i64 = 60;

/// Default minimum contention duration: contended runs shorter than this are absorbed into
/// `Settled` (roadmap D15/Q1).
pub fn default_min_contention() -> Duration {
    Duration::seconds(DEFAULT_MIN_CONTENTION_SECS)
}

/// How long a hole between two adjacent blocks may be before it counts as a real gap, in
/// milliseconds (roadmap 4.5c). Below it, ⑥ and ⑦ treat the two blocks as touching.
///
/// **Two seconds, and the number is evidence rather than taste.** On the owner's real day the holes
/// between adjacent blocks came in two clearly separated populations: watcher jitter, which is
/// everything from 0ms to about 1.9s and accounts for 314 of 364 adjacent pairs, and real absences,
/// which start at 8 seconds and run to hours. Nothing measured falls between 2 and 5 seconds. Two
/// seconds therefore catches the whole jitter population -- including the 1.008s hole between two
/// One UI Home blocks that the owner pointed at -- without reaching anything that could plausibly
/// have been time away, and it stays safely under [`NOISE_FLOOR_SECS`], so a bridged hole can never
/// be longer than a stretch the pipeline would have been willing to call a block of its own.
pub const JITTER_GAP_MS: i64 = 2_000;

/// [`JITTER_GAP_MS`] as a duration.
pub fn jitter_gap() -> Duration {
    Duration::milliseconds(JITTER_GAP_MS)
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

/// How long one distinct foreground activity held part of a block.
///
/// A segment straight out of ② has exactly one share: its own `data`, for its whole span. What
/// makes the type necessary is ⑥ [`coalesce`], which merges neighbouring blocks of the **same app**
/// even when the finer detail differs -- the screen inside the app on Android, the window title on
/// a desktop. A stretch of one app is one stretch of that app, and drawing it as four blocks is
/// what the owner saw on 2026-09-11 and called a repeat:
///
/// ```text
/// 08:17:52  9s  Photos  HomeActivity
/// 08:18:01  4s  Photos  StoryViewActivity
/// 08:18:05  4s  Photos  HomeActivity
/// ```
///
/// The detail is not thrown away to achieve that -- it moves here, with the time it held, so the
/// per-screen panels keep getting exact numbers out of a coarser block.
///
/// **Invariant:** a segment's shares always sum to exactly its counted time,
/// [`Segment::counted_span`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ForegroundShare {
    /// The winning activity's own fields, as ② cut them.
    pub data: Map<String, Value>,
    /// Milliseconds, deliberately not seconds. A day is hundreds of blocks and each of them carries
    /// sub-second parts; truncating per share and summing afterwards loses minutes over a day,
    /// which is the same mistake roadmap 4.5b had to undo once already for the day total.
    pub ms: i64,
    /// Whether these milliseconds are excluded from every total (roadmap 4.6a's *"do not count
    /// this"*), carried **per share** rather than per block.
    ///
    /// This is what lets roadmap 4.5d smooth without lying. The owner's instruction was that
    /// *"whether it is counted or not counted has nothing to do with the smoothing"* -- a two-second
    /// detour through the launcher should not split a stretch of Photos into two blocks. But the
    /// launcher's two seconds must still count toward nothing, or an exclusion the owner set would
    /// quietly stop working the moment smoothing moved it. Both hold at once only if the flag
    /// travels with the seconds instead of with the block: ⑦ may put an excluded share inside a
    /// counted block, and [`Segment::counted_span`] simply does not add it up.
    pub not_counted: bool,
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
    /// Id of the decision that resolved this segment (④), or `None` while it is still an open
    /// question. Set for every outcome, including `ignore` — "this counts as nothing" is an answer.
    pub resolved_by: Option<String>,
    /// True when [`Segment::resolved_by`] matched by *signature* (`scope: always`) rather than by
    /// this window, so the view can say which rule did it and offer to revoke it (**R16**).
    pub auto_resolved: bool,
    /// The owner's own words for this stretch, when they said neither competitor was right
    /// (`outcome: relabel`). Replaces the label shown; the time still counts to ⑤'s pick, because
    /// a relabel says *what* it was, not *whose* it was.
    pub label_override: Option<String>,
    /// True when the owner said they were away (`outcome: ignore`): the segment draws, but its
    /// seconds count toward no total.
    pub ignored: bool,
    /// True when it is a **rule** that stopped this block counting, not an answer to a question
    /// (roadmap 4.6). Always accompanied by `ignored`; kept apart from it so the view can say
    /// *"a rule excludes this"* and point at the rule, rather than *"you were away"*, which the
    /// owner never said.
    pub not_counted: bool,
    /// The activity labels ②b took out of this segment, sorted and deduplicated. Non-empty either
    /// when the block stopped counting entirely or when an excluded app was removed from a
    /// competition it used to be part of — an excluded competitor that simply vanished would leave
    /// the view unable to explain why a block it used to shade is now settled.
    pub excluded_labels: Vec<String>,
    /// Apps the owner ticked as deliberately running alongside the winner. Kept for the day view
    /// (**R6** still means one foreground) and for the rules engine R15 builds on this data.
    pub deliberate_background: Vec<String>,
    /// Seconds folded into this block from slivers ⑦ [`smooth`] rounded away (roadmap 4.5). Zero
    /// when nothing was. The seconds still *count* — the block's span grew to cover them — this is
    /// only how many of them came from somewhere else.
    pub smoothed_seconds: i64,
    /// The distinct labels of those slivers, sorted, excluding this block's own. Empty when nothing
    /// was rounded away. Kept so the view can say what was smoothed rather than let it vanish:
    /// rounding is a display setting, and a display setting that hides things silently is a lie.
    pub absorbed_labels: Vec<String>,
    /// Every distinct foreground activity inside this block and how long it held, longest first,
    /// ties broken by the activity's own label so the order is reproducible (**R18**). One entry
    /// until ⑥ merges something. See [`ForegroundShare`] for why this exists at all.
    pub foreground_shares: Vec<ForegroundShare>,
    /// Milliseconds inside this block's span that **no** source segment covered.
    ///
    /// A watcher does not hand over a seamless day: between one app's last event and the next
    /// app's first there is routinely a hole of a few milliseconds, and occasionally of a second or
    /// two while a transition animation runs and nothing is in the foreground. Measured on the
    /// owner's real day: of 364 adjacent pairs, 196 met exactly, 73 had a hole under 50ms, and 45
    /// more had one under 5 seconds. ⑥ and ⑦ treat a hole under [`jitter_gap`] as no hole at all
    /// (roadmap 4.5c) -- otherwise a day of one app draws as a hundred blocks, and the sliver
    /// threshold does nothing at all, because a sliver with a 33ms hole on one side has no
    /// neighbour to be absorbed into.
    ///
    /// Bridging one is a **drawing** decision, so the bridged milliseconds are recorded here and
    /// **never counted**: a block's span may cover time no watcher recorded, but its seconds may
    /// not. Over the owner's measured day the whole correction is 60 seconds out of 43,969.
    pub bridged_ms: i64,
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

    /// How much of this block **may be counted**: the time a watcher actually recorded, minus the
    /// holes ⑥ or ⑦ bridged to draw it as one block, minus any share an exclusion rule says counts
    /// toward nothing. **This, never `end - start`, is what may be added to a total.**
    ///
    /// Summed from the shares once ⑥ has seeded them, because after roadmap 4.5d a single block can
    /// hold both kinds of time: a stretch of Photos that swallowed a two-second launcher sliver
    /// draws as one block, and those two seconds are inside its span but in none of its totals. The
    /// shares always account for exactly `span - bridged_ms`, so before seeding — and only then —
    /// that subtraction is the same answer.
    pub fn counted_span(&self) -> Duration {
        if self.foreground_shares.is_empty() {
            return (self.end - self.start) - Duration::milliseconds(self.bridged_ms);
        }
        Duration::milliseconds(
            self.foreground_shares
                .iter()
                .filter(|sh| !sh.not_counted)
                .map(|sh| sh.ms)
                .sum(),
        )
    }

    /// The part of this block that counts toward nothing — the mirror of [`counted_span`], so the
    /// two plus `bridged_ms` always add back up to the drawn span.
    ///
    /// [`counted_span`]: Segment::counted_span
    pub fn uncounted_span(&self) -> Duration {
        Duration::milliseconds(
            self.foreground_shares
                .iter()
                .filter(|sh| sh.not_counted)
                .map(|sh| sh.ms)
                .sum(),
        )
    }

    /// The activity that held this block longest — what the view should name it by. `None` only
    /// before ⑥ has seeded the shares.
    pub fn dominant_share(&self) -> Option<&ForegroundShare> {
        self.foreground_shares.first()
    }
}

/// Run ① normalise, ② segment, ②b exclude, ③ classify, ④ apply decisions, ⑤ provisional
/// attribution. Deterministic: identical
/// input (in any event/bucket order) yields byte-identical output (R18).
///
/// Step ⑥ ([`coalesce`]) is a separate opt-in call — the pipeline stays lossless by default so Phase 4 can attach decisions
/// to the atomic segments.
pub fn compute_segments(input: PipelineInput) -> Vec<Segment> {
    let intervals = normalise::normalise(&input);
    let mut segments = segment::segment(&intervals);
    exclude::exclude(&mut segments, &input.not_counted);
    classify::classify(&mut segments, input.min_contention);
    apply::apply(&mut segments, &input.decisions, &input.roles_by_uuid());
    attribute::attribute(&mut segments);
    segments
}

/// A human label for one event's `data` — what the combined track calls this activity, and what a
/// decision's signature names it.
///
/// It lives here rather than in the view layer because ④ has to reproduce, exactly, the string the
/// resolution sheet wrote into a signature. Two spellings of "what app is this" would mean a
/// decision that never matches the segment it was made about. Falls back rather than failing
/// (**R19**).
pub fn activity_label(data: &Map<String, Value>) -> String {
    for key in ["app", "title", "url"] {
        if let Some(Value::String(s)) = data.get(key) {
            if !s.is_empty() {
                return s.clone();
            }
        }
    }
    "(unknown)".to_string()
}
