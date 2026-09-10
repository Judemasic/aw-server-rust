//! ⑦ Smooth: round away the crumbs, in the view only.
//!
//! Roadmap 4.5. ⑥ [`crate::coalesce`] already glues neighbouring segments that attribute to the
//! same thing, but a stretch of one app is routinely interrupted by an 8-second flick to another
//! and comes back as three blocks instead of one. That is not what the day looked like, and on a
//! phone it is what makes a correct timeline look broken.
//!
//! **This is a display transform and nothing else.** It runs after the whole pipeline, on a copy,
//! and writes nothing: change the threshold and the day recomputes from the same stored events and
//! the same decisions, with nothing lost. The owner has already ruled on the data itself — *"do not
//! write any data"* — so absorbing a sliver may only change what is *drawn*, never what is kept.
//!
//! # The rules, in the precedence the roadmap sets
//!
//! 1. **The owner's answer outranks everything.** A segment carrying a `resolved_by` is never
//!    absorbed into one that does not, and never absorbs one.
//! 1b. **A question is never smoothed away.** A segment still asking (`unresolved`) neither absorbs
//!    nor is absorbed. Today the 60-second contention floor already means no sliver under the
//!    threshold can be `unresolved`, so this rule costs nothing — but it is the guarantee that
//!    matters most, so it is enforced rather than inferred from another rule's constant.
//! 2. **Noise floor, 5s, not user-facing.** Below it nothing is ever its own block: it joins its
//!    longer neighbour whether or not anything brackets it. The owner's real day contains
//!    one-second and 17-millisecond segments; that is watcher jitter, not a preference.
//! 3. **Went-and-came-back.** `A, B, A` with `B` under the threshold: `B` joins `A`. The strongest
//!    rule of the set — the bracket *is* the evidence that the stretch was really one stretch.
//! 5. **`A, B, C` with `B` short and no bracket stays literal.** Genuinely ambiguous; guessing is
//!    worse than leaving it.
//! 6. **A decided stretch never shatters.** Segments sharing one `resolved_by` join each other at
//!    any size, so one answer never draws as three blocks.
//! 7. **Absorb by a total order, not by scan direction.** A run of consecutive slivers is resolved
//!    shortest-first, ties broken by label then device then start — so the result does not depend
//!    on which end the list is read from (**R18**). A sliver always joins its **longer** neighbour,
//!    whichever rule admitted it, which is what makes raising the threshold monotone: a bigger
//!    number can only ever absorb more. See [`target_for`] for the day that proved it matters.
//! 8. **Off means literal.** A threshold of zero disables 3 and 6; only the noise floor survives.
//!
//! Rule 4 of the roadmap's list — *transit apps*, `A, Home, B` absorbing forward into `B` — is
//! deliberately **not** implemented. It needs the app to decide which apps are "transit", and the
//! owner's ruling on the launcher was that it does count and should be left alone. Choosing what
//! does not count is roadmap 4.6, by hand, not something inferred here.

use chrono::{DateTime, Duration, Utc};

use crate::{activity_label, Segment};

/// Default sliver threshold in seconds — the owner's answer, 2026-09-10 (*"okay 15s"*).
pub const DEFAULT_SLIVER_SECS: i64 = 15;

/// Rule 2. Not user-facing and not switchable: below this a segment is watcher jitter.
pub const NOISE_FLOOR_SECS: i64 = 5;

/// The two numbers ⑦ runs on. Build with [`SmoothOptions::from_sliver_secs`].
#[derive(Clone, Copy, Debug)]
pub struct SmoothOptions {
    /// Rules 3 and 6 apply below this. Zero turns them off and leaves the day literal (rule 8).
    pub sliver: Duration,
    /// Rule 2. Always applied, whatever `sliver` says.
    pub noise_floor: Duration,
}

impl SmoothOptions {
    pub fn from_sliver_secs(secs: i64) -> Self {
        Self {
            sliver: Duration::seconds(secs.max(0)),
            noise_floor: Duration::seconds(NOISE_FLOOR_SECS),
        }
    }
}

impl Default for SmoothOptions {
    fn default() -> Self {
        Self::from_sliver_secs(DEFAULT_SLIVER_SECS)
    }
}

/// What one absorption does: `sliver` disappears into `target`, which grows to cover it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Absorption {
    sliver: usize,
    target: usize,
}

/// Fold every absorbable sliver into a neighbour and return the shortened list.
///
/// Deterministic and independent of scan direction: each pass picks the single best candidate by a
/// total order (rule 7) and repeats until none is left. Idempotent — running it twice on its own
/// output changes nothing.
pub fn smooth(segments: Vec<Segment>, opts: SmoothOptions) -> Vec<Segment> {
    let mut segs = segments;
    // Every pass removes exactly one segment, so this cannot spin.
    while let Some(a) = next_absorption(&segs, &opts) {
        absorb(&mut segs, a);
    }
    // Absorbing a sliver out of `A, B, A` leaves two `A`s touching, which is ⑥'s job and not this
    // module's. Running it again here is what makes the promise the step was opened for -- one
    // stretch draws as one block -- and what makes `smooth` idempotent.
    crate::coalesce(segs)
}

/// The label this segment draws under — the owner's words when they gave any.
fn label_of(seg: &Segment) -> String {
    seg.label_override
        .clone()
        .unwrap_or_else(|| activity_label(&seg.foreground_slice().data))
}

/// Whether two segments may be joined at all, before any threshold is considered.
///
/// Rules 1 and 1b. `resolved_by` must be *equal*, which covers both directions of rule 1 at once:
/// two `None`s join freely, two halves of one decision join under rule 6, and an answered stretch
/// and an unanswered one never touch.
fn joinable(a: &Segment, b: &Segment) -> bool {
    !a.unresolved && !b.unresolved && a.resolved_by == b.resolved_by
}

/// Index of the neighbour before `i`, if it is contiguous and joinable.
fn prev_of(segs: &[Segment], i: usize) -> Option<usize> {
    let p = i.checked_sub(1)?;
    (segs[p].end == segs[i].start && joinable(&segs[p], &segs[i])).then_some(p)
}

/// Index of the neighbour after `i`, if it is contiguous and joinable.
fn next_of(segs: &[Segment], i: usize) -> Option<usize> {
    let n = i + 1;
    let after = segs.get(n)?;
    (after.start == segs[i].end && joinable(&segs[i], after)).then_some(n)
}

fn span(seg: &Segment) -> Duration {
    seg.end - seg.start
}

/// The neighbour with the most time in it, `prev` on a tie — so a sliver joins the stretch it is
/// most plausibly part of, and the choice never depends on list order.
fn longer(segs: &[Segment], prev: Option<usize>, next: Option<usize>) -> Option<usize> {
    match (prev, next) {
        (Some(p), Some(n)) if span(&segs[n]) > span(&segs[p]) => Some(n),
        (Some(p), _) => Some(p),
        (None, n) => n,
    }
}

/// Is segment `i` allowed to disappear into a neighbour at all?
///
/// This is the whole of the rule set. **Where** it goes is a separate question, answered by
/// [`longer`] and never by which rule said yes — see the note there.
fn absorbable(segs: &[Segment], i: usize, opts: &SmoothOptions, bracketed: bool) -> bool {
    let seg = &segs[i];

    // Rule 6 — inside one decision's window, at any size. `joinable` has already established that
    // a neighbour reached here carries the *same* decision id, so this can never merge two answers.
    if !opts.sliver.is_zero() && seg.resolved_by.is_some() {
        return true;
    }

    let dur = span(seg);

    // Rule 2 — the noise floor. No bracket needed: this is not a preference, it is jitter.
    if dur < opts.noise_floor {
        return true;
    }

    // Rule 3 — went and came back. Rule 5 is the `else`: short, unbracketed, and left literal.
    !opts.sliver.is_zero() && dur < opts.sliver && bracketed
}

/// Where segment `i` should go, if anywhere.
///
/// **The target never depends on which rule admitted the segment**, and that is not a detail. It
/// was, once: rule 3 returned `prev` because `A, B, A` reads as "B belongs to the A before it".
/// Measured on the owner's real day, that made the whole transform non-monotone — raising the
/// threshold from 0 to 5s produced *more* blocks, because a crumb that had been joining its longer
/// neighbour started joining its shorter one, and the merge that used to follow downstream no
/// longer did. A threshold that can make the day more shattered by being raised is not a setting
/// anyone can reason about. Both neighbours carry the same label whenever rule 3 fires, so joining
/// the longer one says exactly the same thing about the day and keeps the guarantee: raising the
/// number can only ever absorb more.
fn target_for(segs: &[Segment], i: usize, opts: &SmoothOptions) -> Option<usize> {
    let prev = prev_of(segs, i);
    let next = next_of(segs, i);

    let bracketed = match (prev, next) {
        (Some(p), Some(n)) => {
            let (a, b) = (&segs[p], &segs[n]);
            a.foreground_slice().device == b.foreground_slice().device && label_of(a) == label_of(b)
        }
        _ => false,
    };

    if !absorbable(segs, i, opts, bracketed) {
        return None;
    }
    longer(segs, prev, next)
}

/// The single best absorption to do next, by rule 7's total order: shortest first, then label, then
/// device uuid, then start. Every key is a value the pipeline already produces deterministically, so
/// two devices holding the same day pick the same one.
fn next_absorption(segs: &[Segment], opts: &SmoothOptions) -> Option<Absorption> {
    let mut best: Option<(i64, String, String, DateTime<Utc>, Absorption)> = None;
    for i in 0..segs.len() {
        let Some(target) = target_for(segs, i, opts) else {
            continue;
        };
        let seg = &segs[i];
        let key = (
            span(seg).num_milliseconds(),
            label_of(seg),
            seg.foreground_slice().device.clone(),
            seg.start,
            Absorption { sliver: i, target },
        );
        let better = match &best {
            None => true,
            Some(h) => (key.0, &key.1, &key.2, key.3) < (h.0, &h.1, &h.2, h.3),
        };
        if better {
            best = Some(key);
        }
    }
    best.map(|k| k.4)
}

/// Carry out one absorption: `target` grows over `sliver`, `sliver` disappears from the list.
///
/// The target keeps its own `active`, so it still resolves as itself and the resolution sheet asks
/// the same question it asked before. What the sliver contributed is recorded in
/// [`Segment::smoothed_seconds`] and [`Segment::absorbed_labels`] instead, so the detail panel can
/// say what was rounded away rather than letting it vanish.
fn absorb(segs: &mut Vec<Segment>, a: Absorption) {
    let sliver = segs.remove(a.sliver);
    let after = a.target > a.sliver;
    let t = if after { a.target - 1 } else { a.target };
    let label = label_of(&sliver);
    let seconds = span(&sliver).num_seconds();
    let carried = sliver.absorbed_labels;
    let carried_seconds = sliver.smoothed_seconds;

    let target = &mut segs[t];
    if after {
        target.start = sliver.start;
    } else {
        target.end = sliver.end;
    }
    target.smoothed_seconds += seconds + carried_seconds;

    let own = target
        .label_override
        .clone()
        .unwrap_or_else(|| activity_label(&target.active[target.foreground].data));
    for l in std::iter::once(label).chain(carried) {
        // A sliver of the same app as the block it joined is not worth naming — the noise floor
        // absorbs plenty of those, and listing "YouTube" under a YouTube block explains nothing.
        if l != own && !target.absorbed_labels.contains(&l) {
            target.absorbed_labels.push(l);
        }
    }
    target.absorbed_labels.sort();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ActiveSlice, SegmentState};
    use chrono::TimeZone;
    use serde_json::{json, Map, Value};

    fn ts(sec: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 10, 12, 0, 0).unwrap() + Duration::seconds(sec)
    }

    fn app(name: &str) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("app".to_string(), json!(name));
        m
    }

    /// One settled segment on one device.
    fn seg(a: i64, b: i64, device: &str, label: &str) -> Segment {
        Segment {
            start: ts(a),
            end: ts(b),
            state: SegmentState::Settled,
            active: vec![ActiveSlice {
                device: device.to_string(),
                bucket_id: format!("bucket-{device}"),
                data: app(label),
                source_start: ts(a),
                source_end: ts(b),
            }],
            absorbed_short_contention: false,
            foreground: 0,
            unresolved: false,
            resolved_by: None,
            auto_resolved: false,
            label_override: None,
            ignored: false,
            deliberate_background: Vec::new(),
            smoothed_seconds: 0,
            absorbed_labels: Vec::new(),
        }
    }

    fn shape(segs: &[Segment]) -> Vec<(i64, i64, String)> {
        segs.iter()
            .map(|s| {
                (
                    (s.start - ts(0)).num_seconds(),
                    (s.end - ts(0)).num_seconds(),
                    label_of(s),
                )
            })
            .collect()
    }

    #[test]
    fn a_bracketed_sliver_joins_the_stretch_around_it() {
        let segs = vec![
            seg(0, 600, "d1", "YouTube"),
            seg(600, 608, "d1", "One UI Home"),
            seg(608, 1200, "d1", "YouTube"),
        ];
        let out = smooth(segs, SmoothOptions::default());
        assert_eq!(shape(&out), vec![(0, 1200, "YouTube".to_string())]);
        assert_eq!(out[0].smoothed_seconds, 8);
        assert_eq!(out[0].absorbed_labels, vec!["One UI Home".to_string()]);
    }

    #[test]
    fn an_unbracketed_sliver_stays_literal() {
        let segs = vec![
            seg(0, 600, "d1", "YouTube"),
            seg(600, 608, "d1", "One UI Home"),
            seg(608, 1200, "d1", "Firefox"),
        ];
        let out = smooth(segs, SmoothOptions::default());
        assert_eq!(shape(&out).len(), 3);
    }

    #[test]
    fn the_noise_floor_takes_an_unbracketed_crumb_anyway() {
        let segs = vec![
            seg(0, 600, "d1", "YouTube"),
            seg(600, 602, "d1", "One UI Home"),
            seg(602, 1400, "d1", "Firefox"),
        ];
        let out = smooth(segs, SmoothOptions::default());
        // Two seconds is jitter; it joins the longer neighbour, which is Firefox.
        assert_eq!(
            shape(&out),
            vec![
                (0, 600, "YouTube".to_string()),
                (600, 1400, "Firefox".to_string())
            ]
        );
        assert_eq!(out[1].absorbed_labels, vec!["One UI Home".to_string()]);
    }

    #[test]
    fn off_still_keeps_the_noise_floor_and_nothing_else() {
        let segs = vec![
            seg(0, 600, "d1", "YouTube"),
            seg(600, 608, "d1", "One UI Home"),
            seg(608, 1200, "d1", "YouTube"),
            seg(1200, 1201, "d1", "Firefox"),
        ];
        let out = smooth(segs, SmoothOptions::from_sliver_secs(0));
        assert_eq!(
            shape(&out),
            vec![
                (0, 600, "YouTube".to_string()),
                (600, 608, "One UI Home".to_string()),
                (608, 1201, "YouTube".to_string()),
            ]
        );
    }

    #[test]
    fn a_gap_is_never_crossed() {
        let segs = vec![
            seg(0, 600, "d1", "YouTube"),
            seg(700, 702, "d1", "One UI Home"),
            seg(702, 1200, "d1", "YouTube"),
        ];
        let out = smooth(segs, SmoothOptions::default());
        // A 2s crumb is under the noise floor and has to go somewhere, but only forward: the gap at
        // 600-700 means the leading YouTube block is a different stretch and is never reached.
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].end, ts(600));
        assert_eq!(out[1].start, ts(700));
    }

    #[test]
    fn an_answer_is_never_absorbed_into_an_unanswered_neighbour() {
        let mut segs = vec![
            seg(0, 600, "d1", "YouTube"),
            seg(600, 605, "d1", "One UI Home"),
            seg(605, 1200, "d1", "YouTube"),
        ];
        segs[1].resolved_by = Some("d_1".to_string());
        let out = smooth(segs, SmoothOptions::default());
        assert_eq!(shape(&out).len(), 3);
    }

    #[test]
    fn a_question_is_never_smoothed_away() {
        let mut segs = vec![
            seg(0, 600, "d1", "YouTube"),
            seg(600, 603, "d1", "One UI Home"),
            seg(603, 1200, "d1", "YouTube"),
        ];
        segs[1].unresolved = true;
        segs[1].state = SegmentState::Contended;
        let out = smooth(segs, SmoothOptions::default());
        assert_eq!(shape(&out).len(), 3);
    }

    #[test]
    fn one_decision_never_draws_as_three_blocks() {
        let mut segs = vec![
            seg(0, 300, "d1", "YouTube"),
            seg(300, 900, "d1", "Firefox"),
            seg(900, 1200, "d1", "YouTube"),
        ];
        for s in &mut segs {
            s.resolved_by = Some("d_1".to_string());
        }
        let out = smooth(segs, SmoothOptions::default());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].start, ts(0));
        assert_eq!(out[0].end, ts(1200));
        assert_eq!(out[0].smoothed_seconds, 600);
    }

    #[test]
    fn a_run_of_slivers_absorbs_into_one_block() {
        let segs = vec![
            seg(0, 600, "d1", "YouTube"),
            seg(600, 604, "d1", "A"),
            seg(604, 610, "d1", "B"),
            seg(610, 613, "d1", "C"),
            seg(613, 1200, "d1", "YouTube"),
        ];
        let out = smooth(segs, SmoothOptions::default());
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].absorbed_labels,
            vec!["A".to_string(), "B".to_string(), "C".to_string()]
        );
        assert_eq!(out[0].smoothed_seconds, 13);
    }

    #[test]
    fn smoothing_is_idempotent() {
        let segs = vec![
            seg(0, 600, "d1", "YouTube"),
            seg(600, 608, "d1", "One UI Home"),
            seg(608, 1200, "d1", "YouTube"),
        ];
        let once = smooth(segs, SmoothOptions::default());
        let twice = smooth(once.clone(), SmoothOptions::default());
        assert_eq!(shape(&once), shape(&twice));
        assert_eq!(once[0].smoothed_seconds, twice[0].smoothed_seconds);
    }

    #[test]
    fn no_seconds_are_created_or_lost() {
        let segs = vec![
            seg(0, 600, "d1", "YouTube"),
            seg(600, 608, "d1", "One UI Home"),
            seg(608, 1200, "d1", "YouTube"),
            seg(1200, 1800, "d2", "Firefox"),
        ];
        let before: i64 = segs.iter().map(|s| span(s).num_seconds()).sum();
        let out = smooth(segs, SmoothOptions::default());
        let after: i64 = out.iter().map(|s| span(s).num_seconds()).sum();
        assert_eq!(before, after);
    }
}
