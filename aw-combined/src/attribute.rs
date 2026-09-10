//! ⑤ Provisional attribution: for every segment, pick the single slice that *counts* — so the day
//! totals correctly (R6) even though no decision exists yet (R17) — and flag whether the view must
//! still shade it (R8).
//!
//! See `aw-android/docs/04_COMBINED_TIMELINE.md` §2.4. This is **provisional**: it changes the
//! number a segment contributes to a day total, never whether the segment is still an open question.
//! A `Contended` segment stays `unresolved` here; only a Phase 4 decision (step ④) clears that.

use crate::{Segment, SegmentState};

/// Set [`Segment::foreground`] and [`Segment::unresolved`] on every segment.
///
/// `foreground` is the index of the winning slice under this **total order**, applied in sequence:
///
/// 1. longest `source_end - source_start` wins — R17 rule 1, the *substance*: the phone's hour-long
///    YouTube session beats the tablet's fifteen-minute Kindle dip. This is only a real rule
///    because ② records each slice's originating (post-idle) span; every slice covers the segment
///    itself exactly, so the segment's own length never discriminates.
/// 2. tie → lowest `device` lexicographically (`04` §2.4 rule 2);
/// 3. tie → lowest `bucket_id` — rule 2 cannot separate one device's two overlapping buckets;
/// 4. tie → lowest `serde_json::to_string(&data)` — `data` is not part of ①'s sort key, so without
///    this the winner could depend on input order and break R18. `serde_json::Value` implements no
///    `Ord`, so the canonical string stands in; `Map` is a `BTreeMap` here (no `preserve_order`
///    feature), so that string is stable.
///
/// `unresolved` is simply `state == Contended`. A segment demoted by the short-contention pass is
/// `Settled`, so `unresolved == false` — it is never shaded and never asked about — but it still
/// goes through the same pick above, so its time is credited to its longest-running activity rather
/// than lost. That is the answer to "whose time is a demoted segment's": nobody's by decision, the
/// longest activity's by placement.
pub(crate) fn attribute(segments: &mut [Segment]) {
    for seg in segments.iter_mut() {
        let foreground = (0..seg.active.len())
            .max_by(|&i, &j| {
                let (x, y) = (&seg.active[i], &seg.active[j]);
                let dx = x.source_end - x.source_start;
                let dy = y.source_end - y.source_start;
                // `max_by` keeps the *last* maximum on a tie, so order the comparison to make the
                // lexicographically-smaller candidate compare Greater and thus win.
                dx.cmp(&dy)
                    .then_with(|| y.device.cmp(&x.device))
                    .then_with(|| y.bucket_id.cmp(&x.bucket_id))
                    .then_with(|| {
                        let sx = serde_json::to_string(&x.data).unwrap_or_default();
                        let sy = serde_json::to_string(&y.data).unwrap_or_default();
                        sy.cmp(&sx)
                    })
            })
            .expect("every segment has at least one slice (② never emits an empty one)");
        // ④ may already have named the winner. This step is *provisional* — it fills the gap where
        // no decision exists — so it never overrules one that does.
        if seg.foreground == usize::MAX {
            seg.foreground = foreground;
        }
        seg.unresolved = seg.state == SegmentState::Contended;
    }
}
