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
//! 3b. **The detour may be several apps long** (roadmap 4.5c, the owner's own example:
//!    `YouTube, Photos, Gallery, YouTube`). The bracket is any activity appearing on *both* sides of
//!    the sliver with nothing but slivers in between — see [`bracket_idents`] for why it is phrased
//!    that way rather than as "the nearest real block on each side", which measured non-monotone.
//!    Under this rule a sliver may only join a neighbour carrying the bracket's own activity, so a
//!    run fills from its ends inward instead of collapsing into a block the day never had.
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
//! # Holes are not gaps (roadmap 4.5c)
//!
//! None of the above did anything at all on real data until 4.5c, and the reason was one `==`.
//! Neighbours were found with `prev.end == next.start`, and a watcher's events do not meet exactly:
//! 168 of 364 adjacent pairs on the owner's measured day had a hole of a few milliseconds between
//! them, which meant the sliver in the middle had *no neighbour* to be absorbed into. Setting the
//! threshold to 0, 15 or 60 seconds produced byte-identical days. A hole under
//! [`crate::JITTER_GAP_MS`] is now treated as no hole, here and in ⑥, and the milliseconds bridged
//! that way are recorded in [`Segment::bridged_ms`] so that they are drawn over without being
//! counted.
//!
//! Rule 4 of the roadmap's list — *transit apps*, `A, Home, B` absorbing forward into `B` — is
//! deliberately **not** implemented. It needs the app to decide which apps are "transit", and the
//! owner's ruling on the launcher was that it does count and should be left alone. Choosing what
//! does not count is roadmap 4.6, by hand, not something inferred here.

use chrono::{DateTime, Duration, Utc};

use crate::{activity_label, coalesce::adjacent, coalesce::seed_shares, Segment};

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
    // ⑥ normally did this already; doing it here too means ⑦ can be run on its own in a test and the
    // share invariant still holds.
    seed_shares(&mut segs);
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
///
/// **`ignored` and `not_counted` must match as well, and that is not belt-and-braces.** A block a
/// *rule* emptied (roadmap 4.6a) carries `ignored` with **no** `resolved_by` at all — the owner
/// answered no question about it — so `resolved_by` equality alone reads it as freely joinable with
/// any ordinary settled block beside it. Found by measuring: with the owner's `One UI Home` rule
/// live, 2026-09-10 moved **28 seconds** out of the not-counted total and into the day's, because a
/// counted sliver was absorbed into an excluded block and vice versa. Smoothing is a drawing
/// transform and may never move a second across the line between counting and not counting.
fn joinable(a: &Segment, b: &Segment) -> bool {
    !a.unresolved
        && !b.unresolved
        && a.resolved_by == b.resolved_by
        && a.ignored == b.ignored
        && a.not_counted == b.not_counted
}

/// Index of the neighbour before `i`, if it is contiguous and joinable.
///
/// Contiguous *within jitter* — see [`crate::JITTER_GAP_MS`]. Exact equality was the bug that made
/// this whole module a no-op on real data: a 33ms hole left a sliver with no neighbour at all, so
/// there was nothing for it to be absorbed into and the threshold changed nothing.
fn prev_of(segs: &[Segment], i: usize) -> Option<usize> {
    let p = i.checked_sub(1)?;
    (adjacent(&segs[p], &segs[i]) && joinable(&segs[p], &segs[i])).then_some(p)
}

/// Index of the neighbour after `i`, if it is contiguous and joinable.
fn next_of(segs: &[Segment], i: usize) -> Option<usize> {
    let n = i + 1;
    let after = segs.get(n)?;
    (adjacent(&segs[i], after) && joinable(&segs[i], after)).then_some(n)
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
/// The activity of one block, as the bracket test compares it: device plus label.
fn ident(seg: &Segment) -> (String, String) {
    (seg.foreground_slice().device.clone(), label_of(seg))
}

/// Every activity that flanks this sliver on **both** sides with nothing but slivers in between.
///
/// This is rule 3 as the owner asked for it on 2026-09-11: *"the smoothing should take youtube,
/// photos, gallery, then youtube — if photos and gallery are less than the smoothing then take
/// them"*. The rule used to compare only the two immediate neighbours, so `YouTube, Photos, Gallery,
/// YouTube` smoothed to nothing at all: neither sliver was bracketed by a matching pair, and both
/// fell through to rule 5 and stayed literal. The bracket is the evidence that the stretch was really
/// one stretch, and it is no weaker for the detour having touched two apps instead of one.
///
/// # Why "any flanking pair" and not "the nearest real block on each side"
///
/// The obvious reading of *"look past the run"* is to walk outward to the first block that is **not**
/// a sliver and compare those two anchors. That was built first, and it is **not monotone in the
/// threshold** — measured on the owner's real day, 2026-09-10 came back as 282 blocks at 15s and
/// **290 at 60s**. Raising the number made the day more shattered, which is the one property this
/// module is not allowed to lose (see rule 7).
///
/// The reason is that "which block is an anchor" itself depends on the threshold. At 15s,
/// `A(20s), b(10s), A(20s)` has two `A` anchors and `b` is absorbed. At 60s both `A`s are *themselves*
/// slivers, so the walk goes straight past them looking for something bigger, finds two different
/// apps out there, and the bracket that existed at 15s is gone.
///
/// Collecting **every** position reachable across slivers, and asking whether any activity appears on
/// both sides, fixes that by construction: raising the threshold only ever *adds* reachable
/// positions, so a bracket that held at a lower threshold still holds. Each hop uses
/// [`prev_of`]/[`next_of`], so a real gap or somebody's decision still ends the walk — a run may not
/// be crossed over a hole or over an answer.
fn bracket_idents(segs: &[Segment], i: usize, opts: &SmoothOptions) -> Vec<(String, String)> {
    // Walk one way, taking every block reached and stopping *after* the first non-sliver: it can be
    // one end of a bracket, but nothing beyond it is reachable across slivers.
    let side = |back: bool| -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut at = i;
        loop {
            let Some(step) = (if back {
                prev_of(segs, at)
            } else {
                next_of(segs, at)
            }) else {
                return out;
            };
            out.push(ident(&segs[step]));
            if span(&segs[step]) >= opts.sliver {
                return out;
            }
            at = step;
        }
    };
    let (before, after) = (side(true), side(false));
    before.into_iter().filter(|b| after.contains(b)).collect()
}

/// Where segment `i` should go, if anywhere — `None` meaning it stays a block of its own.
///
/// **For rules 2 and 6 the target is simply the longer neighbour**, and that the target does not
/// depend on which rule admitted the segment is not a detail. It did once: rule 3 returned `prev`
/// because `A, B, A` reads as "B belongs to the A before it". Measured on the owner's real day, that
/// made the whole transform non-monotone — raising the threshold from 0 to 5s produced *more*
/// blocks, because a crumb that had been joining its longer neighbour started joining its shorter
/// one, and the merge that used to follow downstream no longer did.
///
/// **Rule 3 is the exception, and has to be.** Once the bracket may be a whole run away
/// ([`bracket_idents`]), a sliver in the middle of a run has slivers on both sides, and the longer of
/// *those* is the wrong place: `A, b1(14s), b2(10s), b3(14s), A` would put `b2` into `b1`, and
/// `b1+b2` at 24s is then over the threshold and stuck — leaving `A, b1(24s), A`, a block of an app
/// the day never had for that long. So under rule 3 a sliver may only join a neighbour that **carries
/// the bracket's own activity**, and otherwise waits: the run fills from its ends inward, every piece
/// of it lands in the bracketing app, and since both ends carry that same activity, which end it
/// lands in says the same thing about the day.
fn target_for(segs: &[Segment], i: usize, opts: &SmoothOptions) -> Option<usize> {
    let seg = &segs[i];
    let prev = prev_of(segs, i);
    let next = next_of(segs, i);

    // Rule 6 — inside one decision's window, at any size. `joinable` has already established that
    // a neighbour reached here carries the *same* decision id, so this can never merge two answers.
    if !opts.sliver.is_zero() && seg.resolved_by.is_some() {
        return longer(segs, prev, next);
    }

    // Rule 2 — the noise floor. No bracket needed: this is not a preference, it is jitter.
    let dur = span(seg);
    if dur < opts.noise_floor {
        return longer(segs, prev, next);
    }

    // Rule 8, and rule 5 as the fall-through: short, unbracketed, and left exactly as it is.
    if opts.sliver.is_zero() || dur >= opts.sliver {
        return None;
    }

    // Rule 3, and rule 3b in the `filter`.
    let bracket = bracket_idents(segs, i, opts);
    if bracket.is_empty() {
        return None;
    }
    let eligible: Vec<usize> = [prev, next]
        .into_iter()
        .flatten()
        .filter(|&j| bracket.contains(&ident(&segs[j])))
        .collect();
    // The longest of them, first one on a tie, which is `prev` — the same preference [`longer`] has,
    // for the same reason: the choice must not depend on which end the list is read from (**R18**).
    eligible
        .into_iter()
        .max_by_key(|&j| (span(&segs[j]).num_milliseconds(), std::cmp::Reverse(j)))
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
    let counted = sliver.counted_span();
    let seconds = counted.num_seconds();
    let carried = sliver.absorbed_labels;
    let carried_seconds = sliver.smoothed_seconds;
    let sliver_bridged = sliver.bridged_ms;
    let (sliver_start, sliver_end) = (sliver.start, sliver.end);

    let target = &mut segs[t];
    // Whatever hole sat between the two is bridged by the absorption, exactly as in ⑥: the block
    // draws over it and does not count it. `Segment::bridged_ms` says why.
    let hole = if after {
        target.start - sliver_end
    } else {
        sliver_start - target.end
    };
    target.bridged_ms += hole.num_milliseconds() + sliver_bridged;
    if after {
        target.start = sliver.start;
    } else {
        target.end = sliver.end;
    }
    target.smoothed_seconds += seconds + carried_seconds;

    // The sliver's time now counts as the block that swallowed it -- that is what smoothing *is* --
    // so it joins the target's own share rather than arriving as a share of its own. Keeping it
    // separate would put a screen in the per-screen panel for an app that is not the block's app.
    let own_data = target.foreground_slice().data.clone();
    let add = counted.num_milliseconds();
    match target
        .foreground_shares
        .iter_mut()
        .find(|e| e.data == own_data)
    {
        Some(e) => e.ms += add,
        // Only reachable if a caller skipped `seed_shares`; recorded rather than dropped.
        None => target.foreground_shares.push(crate::ForegroundShare {
            data: own_data,
            ms: add,
        }),
    }

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
            not_counted: false,
            excluded_labels: Vec::new(),
            deliberate_background: Vec::new(),
            smoothed_seconds: 0,
            absorbed_labels: Vec::new(),
            foreground_shares: Vec::new(),
            bridged_ms: 0,
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
