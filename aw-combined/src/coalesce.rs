//! ⑥ Coalesce: merge adjacent segments that attribute to the same thing, so the day view is not a
//! wall of 30-second slivers.
//!
//! See `aw-android/docs/04_COMBINED_TIMELINE.md` §2. **Presentation step, and lossy:** the merged
//! segment's `active` is the union of its parts', so per-instant background detail is gone.
//! [`crate::compute_segments`] does **not** call this — the pipeline stays lossless by default and
//! the view opts in. Phase 4 keys decisions on the atomic segments, so coalescing before ④ would
//! destroy what decisions attach to.
//!
//! # What changed in roadmap 4.5c
//!
//! Two things this step got wrong, both found by looking at the owner's real day rather than at the
//! code, and both of which made the day draw as repeats of the same app back to back:
//!
//! 1. **It required the two blocks to *touch exactly*.** Watchers do not hand over a seamless day:
//!    between one event's end and the next event's start there is routinely a hole of a few
//!    milliseconds. Of 364 adjacent pairs on the measured day, 168 had a hole — 8ms, 33ms, 13ms —
//!    and every one of them defeated `prev.end == next.start`. Worse, it defeated ⑦ as well: a
//!    sliver with a 33ms hole on one side has no neighbour, so the sliver threshold did **nothing
//!    at all**, and 0s, 15s and 60s produced identical days. A hole under [`crate::jitter_gap`] is
//!    now no hole.
//! 2. **It required the winning activity's whole `data` map to be equal**, which means the same
//!    *screen* on Android and the same *window title* on a desktop — so a stretch of Photos drew as
//!    four blocks the moment the owner opened a story and came back. The identity is now the
//!    activity's **label** (plus its device): one app, one block. The finer detail is not discarded,
//!    it moves into [`ForegroundShare`]s, with the time each one held, so a per-screen panel still
//!    gets exact numbers out of a coarser block.
//!
//! Bridging a hole means the merged block's *span* covers time no watcher recorded. That is allowed
//! for drawing and not for counting, so the bridged milliseconds are added up in
//! [`Segment::bridged_ms`] and subtracted back out by [`Segment::counted_span`].

use crate::{activity_label, ActiveSlice, ForegroundShare, Segment};

/// Give every segment its starting share — its own winning activity, for its whole span — unless it
/// already has one.
///
/// ② cannot do this: `foreground` is not known until ⑤ has run. Both ⑥ and ⑦ call it on entry, so a
/// caller may run either one first, or ⑦ alone in a test, and the invariant (shares sum to the
/// counted span) holds from the first line either way.
pub(crate) fn seed_shares(segments: &mut [Segment]) {
    for seg in segments.iter_mut() {
        if seg.foreground_shares.is_empty() {
            let ms = (seg.end - seg.start).num_milliseconds() - seg.bridged_ms;
            // The block's own verdict becomes the share's, once, here. From this point on it is the
            // *share* that knows whether its milliseconds count, which is what lets ⑦ move one into
            // a block of the other kind without changing any total. See `ForegroundShare`.
            let not_counted = seg.ignored || seg.not_counted;
            seg.foreground_shares.push(ForegroundShare {
                data: seg.foreground_slice().data.clone(),
                ms,
                not_counted,
            });
        }
    }
}

/// Longest first, ties broken by the activity's own label and then by its serialised fields, so two
/// devices holding the same day produce the same order (**R18**).
pub(crate) fn sort_shares(shares: &mut [ForegroundShare]) {
    shares.sort_by(|a, b| {
        b.ms.cmp(&a.ms)
            // A share that counts outranks one that does not at equal length, so a block absorbing a
            // same-length excluded sliver is still named after the part of it that counted.
            .then_with(|| a.not_counted.cmp(&b.not_counted))
            .then_with(|| activity_label(&a.data).cmp(&activity_label(&b.data)))
            .then_with(|| {
                // Last-resort tiebreak so the order is total. `Map` is a `BTreeMap` here (no
                // `preserve_order` feature), so its serialisation is key-sorted and stable.
                let (x, y) = (
                    serde_json::to_string(&a.data).unwrap_or_default(),
                    serde_json::to_string(&b.data).unwrap_or_default(),
                );
                x.cmp(&y)
            })
    });
}

/// Merge adjacent segments whose provisional attribution is identical.
///
/// `prev` and `next` merge iff **all** hold:
///
/// - they are contiguous *within jitter*: `0 <= next.start - prev.end <= jitter_gap()` (a real gap
///   never merges, and an overlap — which ② cannot produce — never merges either);
/// - `prev.foreground_slice().device == next.foreground_slice().device` **and** the two winning
///   activities carry the same [`activity_label`] — one app on one device is one block, however many
///   screens or window titles it went through inside;
/// - `prev.state == next.state`;
/// - `prev.unresolved == next.unresolved`;
/// - `prev.absorbed_short_contention == next.absorbed_short_contention`;
/// - every field ④ writes is equal — same decision id, same rule flag, same relabel, same
///   `ignored`, same deliberate-background set;
/// - every field ②b writes is equal, so a block a rule emptied never merges with one it did not.
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
    let mut segments = segments;
    seed_shares(&mut segments);
    let mut out: Vec<Segment> = Vec::with_capacity(segments.len());
    for seg in segments {
        match out.last_mut() {
            Some(prev) if mergeable(prev, &seg) => merge_into(prev, seg),
            _ => out.push(seg),
        }
    }
    out
}

/// Whether two blocks are close enough to be treated as touching. See [`crate::JITTER_GAP_MS`].
pub(crate) fn adjacent(prev: &Segment, next: &Segment) -> bool {
    let hole = next.start - prev.end;
    hole >= chrono::Duration::zero() && hole <= crate::jitter_gap()
}

fn mergeable(prev: &Segment, next: &Segment) -> bool {
    adjacent(prev, next)
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
        && prev.not_counted == next.not_counted
        && prev.excluded_labels == next.excluded_labels
        && prev.deliberate_background == next.deliberate_background
        && {
            let (p, n) = (prev.foreground_slice(), next.foreground_slice());
            p.device == n.device && activity_label(&p.data) == activity_label(&n.data)
        }
}

/// Identity of a slice for dedup: everything but the source span.
fn key(s: &ActiveSlice) -> (&str, &str, &serde_json::Map<String, serde_json::Value>) {
    (&s.device, &s.bucket_id, &s.data)
}

fn merge_into(prev: &mut Segment, next: Segment) {
    // Remember which slice was foreground so we can find it again after the union is rebuilt.
    let fg = prev.foreground_slice().clone();

    // A hole between the two is bridged by the merge -- the block now draws over it -- so it is
    // recorded rather than counted. See `Segment::bridged_ms`.
    prev.bridged_ms += (next.start - prev.end).num_milliseconds();
    prev.bridged_ms += next.bridged_ms;

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

    // The two blocks are the same app but not necessarily the same screen, so the shares are summed
    // per distinct activity rather than replaced. This is the whole reason the merge is allowed to
    // be coarser than it was: nothing about what was on screen is lost by it.
    for share in next.foreground_shares {
        match prev
            .foreground_shares
            .iter_mut()
            .find(|e| e.data == share.data && e.not_counted == share.not_counted)
        {
            Some(e) => e.ms += share.ms,
            None => prev.foreground_shares.push(share),
        }
    }
    sort_shares(&mut prev.foreground_shares);

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
