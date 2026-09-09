//! ③ Classify: mark each segment Settled/Contended, then absorb short contended *runs*.

use std::collections::HashSet;

use chrono::Duration;

use crate::{Segment, SegmentState};

fn distinct_devices(seg: &Segment) -> usize {
    seg.active
        .iter()
        .map(|s| s.device.as_str())
        .collect::<HashSet<_>>()
        .len()
}

/// 1. `Contended` iff a segment covers >=2 distinct devices, else `Settled`.
/// 2. Minimum-duration pass, applied to **runs**, not single segments: find each maximal run of
///    temporally contiguous `Contended` segments (`prev.end == next.start`; a `Settled` segment or
///    a time gap ends the run). If the run's *total* duration is `< min_contention`, demote every
///    segment in it to `Settled` and set `absorbed_short_contention`. Slices are left untouched for
///    roadmap 3.3. Exactly `min_contention` stays `Contended` (strictly-less-than test).
///
/// Runs, not individual segments, because segments are cut at every app change on every device: a
/// genuine long contention where either device switches app often is shredded into many sub-60s
/// atomic segments, and thresholding each separately would erase real contention (R7/R8). The
/// threshold is about short *episodes*, a property of the run.
pub(crate) fn classify(segments: &mut [Segment], min_contention: Duration) {
    for seg in segments.iter_mut() {
        seg.state = if distinct_devices(seg) >= 2 {
            SegmentState::Contended
        } else {
            SegmentState::Settled
        };
    }

    let mut i = 0;
    while i < segments.len() {
        if segments[i].state != SegmentState::Contended {
            i += 1;
            continue;
        }
        let run_start = i;
        let mut j = i + 1;
        while j < segments.len()
            && segments[j].state == SegmentState::Contended
            && segments[j].start == segments[j - 1].end
        {
            j += 1;
        }

        let total = segments[run_start..j]
            .iter()
            .fold(Duration::zero(), |acc, s| acc + (s.end - s.start));
        if total < min_contention {
            for seg in &mut segments[run_start..j] {
                seg.state = SegmentState::Settled;
                seg.absorbed_short_contention = true;
            }
        }
        i = j;
    }
}
