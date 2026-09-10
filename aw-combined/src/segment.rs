//! ② Segment: boundary sweep over the normalised intervals.

use chrono::{DateTime, Utc};

use crate::normalise::Interval;
use crate::{ActiveSlice, Segment, SegmentState};

/// Cut the timeline at every interval endpoint and emit one segment per non-empty gap-free slice.
///
/// Adjacent segments are **not** merged: §2.1 calls these *atomic*, and a boundary from any
/// device's app change is a real boundary. Merging on "same device set" would collapse two
/// consecutive activities on one device and throw one `data` map away. Coalescing on *identical
/// attribution* is roadmap 3.3/3.4.
pub(crate) fn segment(intervals: &[Interval]) -> Vec<Segment> {
    if intervals.is_empty() {
        return Vec::new();
    }

    let mut bounds: Vec<DateTime<Utc>> = Vec::with_capacity(intervals.len() * 2);
    for iv in intervals {
        bounds.push(iv.start);
        bounds.push(iv.end);
    }
    bounds.sort();
    bounds.dedup();

    let mut segments = Vec::new();
    for pair in bounds.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        // Half-open [a, b): an interval covers it iff it spans the whole slice.
        let mut active: Vec<ActiveSlice> = intervals
            .iter()
            .filter(|iv| iv.start <= a && b <= iv.end)
            .map(|iv| ActiveSlice {
                device: iv.device.clone(),
                bucket_id: iv.bucket_id.clone(),
                data: iv.data.clone(),
                // The originating interval's post-idle span, *not* `a`/`b`: rule 1 (R17) compares
                // how long each device's activity actually ran, and every slice here covers the
                // segment exactly, so `a`/`b` would make every candidate equal and rule 1 dead.
                source_start: iv.start,
                source_end: iv.end,
            })
            .collect();
        if active.is_empty() {
            continue; // a gap between activity
        }
        // Stable sort; the input order from ① is already a total order, so this is reproducible.
        active.sort_by(|x, y| {
            x.device
                .cmp(&y.device)
                .then_with(|| x.bucket_id.cmp(&y.bucket_id))
        });
        segments.push(Segment {
            start: a,
            end: b,
            state: SegmentState::Settled, // set by ③
            active,
            absorbed_short_contention: false,
            foreground: usize::MAX, // set by ④ or ⑤
            unresolved: false,      // set by ⑤
            resolved_by: None,      // set by ④
            auto_resolved: false,   // set by ④
            label_override: None,   // set by ④
            ignored: false,         // set by ④
            deliberate_background: Vec::new(), // set by ④
        });
    }
    segments
}
