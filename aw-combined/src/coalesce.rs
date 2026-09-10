//! ⑥ Coalesce: merge adjacent segments that attribute to the same thing, so the day view is not a
//! wall of 30-second slivers.
//!
//! See `aw-android/docs/04_COMBINED_TIMELINE.md` §2. **Presentation step, and lossy:** the merged
//! segment's `active` is the union of its parts', so per-instant background detail is gone.
//! [`crate::compute_segments`] does **not** call this — the pipeline stays lossless by default and
//! the view opts in. Phase 4 keys decisions on the atomic segments, so coalescing before ④ would
//! destroy what decisions attach to.

use crate::{ActiveSlice, Segment};

/// Merge adjacent segments whose provisional attribution is identical.
///
/// `prev` and `next` merge iff **all** hold:
///
/// - `prev.end == next.start` (contiguous — a gap never merges);
/// - `prev.foreground_slice().device == next.foreground_slice().device` **and** their `data` maps
///   are equal (this is what §2 means by "identical attribution" — keying on the whole `active` vec
///   would merge nothing, because heartbeat-split events carry different source spans);
/// - `prev.state == next.state`;
/// - `prev.unresolved == next.unresolved`;
/// - `prev.absorbed_short_contention == next.absorbed_short_contention`;
/// - every field ④ writes is equal — same decision id, same rule flag, same relabel, same
///   `ignored`, same deliberate-background set.
///
/// ⑦'s smoothing bookkeeping (`smoothed_seconds`, `absorbed_labels`) is **not** compared: it is a
/// footnote about what was rounded away, and two otherwise-identical stretches must not be held
/// apart by one. The merged segment sums the seconds and unions the labels.
///
/// The merged segment takes `start` from `prev`, `end` from `next`, carries the flags over, and its
/// `active` is the union of both slice lists deduplicated by `(device, bucket_id, data)` and
/// re-sorted by `(device, bucket_id)`. A slice present in both with different source spans is kept
/// once, with the **earlier** `source_start` and the **later** `source_end` — the merged slice
/// describes the whole merged span. `foreground` is recomputed as the index of `prev`'s foreground
/// slice within that union.
pub fn coalesce(segments: Vec<Segment>) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::with_capacity(segments.len());
    for seg in segments {
        match out.last_mut() {
            Some(prev) if mergeable(prev, &seg) => merge_into(prev, seg),
            _ => out.push(seg),
        }
    }
    out
}

fn mergeable(prev: &Segment, next: &Segment) -> bool {
    prev.end == next.start
        && prev.state == next.state
        && prev.unresolved == next.unresolved
        && prev.absorbed_short_contention == next.absorbed_short_contention
        // Two stretches the owner answered separately stay separate, even when the same activity
        // won both: the view names the decision that settled a block, and merging them would put
        // one id on time the other decision covers.
        && prev.resolved_by == next.resolved_by
        && prev.auto_resolved == next.auto_resolved
        && prev.label_override == next.label_override
        && prev.ignored == next.ignored
        && prev.deliberate_background == next.deliberate_background
        && {
            let (p, n) = (prev.foreground_slice(), next.foreground_slice());
            p.device == n.device && p.data == n.data
        }
}

/// Identity of a slice for dedup: everything but the source span.
fn key(s: &ActiveSlice) -> (&str, &str, &serde_json::Map<String, serde_json::Value>) {
    (&s.device, &s.bucket_id, &s.data)
}

fn merge_into(prev: &mut Segment, next: Segment) {
    // Remember which slice was foreground so we can find it again after the union is rebuilt.
    let fg = prev.foreground_slice().clone();

    // ⑦'s bookkeeping is additive, not an identity: two blocks that each swallowed a crumb are
    // still mergeable, and the merged block swallowed both. Deliberately not part of `mergeable`
    // for that reason -- comparing them would keep two identical stretches apart over a footnote.
    prev.smoothed_seconds += next.smoothed_seconds;
    for l in next.absorbed_labels {
        if !prev.absorbed_labels.contains(&l) {
            prev.absorbed_labels.push(l);
        }
    }
    prev.absorbed_labels.sort();

    let mut active = std::mem::take(&mut prev.active);
    for s in next.active {
        match active.iter_mut().find(|e| key(e) == key(&s)) {
            Some(e) => {
                e.source_start = e.source_start.min(s.source_start);
                e.source_end = e.source_end.max(s.source_end);
            }
            None => active.push(s),
        }
    }
    active.sort_by(|x, y| {
        x.device
            .cmp(&y.device)
            .then_with(|| x.bucket_id.cmp(&y.bucket_id))
    });

    prev.foreground = active
        .iter()
        .position(|e| key(e) == key(&fg))
        .expect("prev's foreground slice is always in the union");
    prev.active = active;
    prev.end = next.end;
}
