//! ⑧ Question: what the owner is actually asked, and in what order the answers are offered.
//!
//! Roadmap 4.11. Everything before this step is about *time* — which seconds belong to whom. This
//! step is about the **owner's attention**, which is a different and much scarcer resource, and the
//! two had been conflated: the view asked one question per contended block, so the number of times
//! the owner was interrupted was an artefact of how finely ② had cut the day.
//!
//! # The measurement that forced it
//!
//! 4.5c's ⑥ and 7b69009's `excluded_labels` fix both attacked the same complaint — *"the same two
//! apps over seconds"* — and both helped, 70 questions down to 42 and then to 5 on the day measured
//! here. But the complaint survived, because neither one could touch its actual cause. Read off the
//! owner's phone, 2026-09-11, after both fixes had shipped:
//!
//! ```text
//! 22:31:53 -> 22:33:30    96s  contended  UNRESOLVED  Emby  vs  ActivityWatch
//! 22:33:30 -> 22:33:37     7s  settled                Emby
//! 22:33:37 -> 22:51:53  1095s  contended  UNRESOLVED  Emby  vs  ActivityWatch
//! 22:51:53 -> 22:51:55     1s  settled                Emby  (peer blinked to Interpreter)
//! 22:51:55 -> 23:00:00   485s  contended  UNRESOLVED  Emby  vs  ActivityWatch
//! 23:00:00 -> 23:00:16    15s  settled                Emby
//! 23:00:16 -> 23:10:07   591s  contended  UNRESOLVED  Emby  vs  ActivityWatch
//! ```
//!
//! That is **one** thing that happened — Emby on the phone against ActivityWatch on the S25U, from
//! 22:31 to 23:10 — and the owner is asked about it **four times**, because the peer's activity
//! blinked out for seven seconds, then for one, then for fifteen. Answering one leaves three. Of
//! the five questions on that whole day, four were this one overlap.
//!
//! ⑦ [`crate::smooth`] cannot fix it and must not: its rule 1b says *a question is never smoothed
//! away*, which is the guarantee that an unanswered overlap can never be hidden by a display
//! setting. Absorbing those settled slivers would also be wrong on the merits — during those seven
//! seconds the peer genuinely was not running, and a pick must never be credited with seconds no
//! watcher recorded (**R11**).
//!
//! So the blocks stay exactly as they are. What changes is that **asking is no longer per block**.
//! A *question* is a maximal run of contended blocks that are plainly the same competition, and the
//! decision it produces spans the run; ④ [`crate::apply`] already settles only the sub-segments
//! where the picked activity was actually running, which is what makes a run-wide window safe. That
//! machinery landed in 4.2a for a different reason and is exactly what is needed here.
//!
//! # What holds a run together
//!
//! Two contended blocks with nothing but settled blocks between them are the same question iff:
//!
//! - they attribute to the same **foreground** — same device, same [`activity_label`]. A different
//!   provisional winner is a genuinely different competition, and rolling the two together would
//!   offer the owner a pick that was never on the table for half the span.
//! - the settled stretch between them is no longer than [`QuestionOptions::gap`]. A blink is not the
//!   end of a competition; an hour of one device alone is.
//!
//! The gap is **not** ⑦'s sliver threshold and is deliberately larger (60s against 15s). They answer
//! different questions: ⑦'s is *"is this its own block on screen?"*, which is about what a day looks
//! like, and this one is *"is this still the same thing happening?"*, which is about what a person
//! would call one event. Fifteen seconds was already too small for the day above.
//!
//! # The order the competitors are offered in
//!
//! Longest first, by each competitor's **own** running time — not by the window's.
//!
//! Every participant used to be handed the segment's duration, on the reasoning that a contention
//! segment is by construction a window where the whole set was active, so any per-participant
//! figure would be the same figure. True of one atomic segment, and useless: it made every option
//! in the sheet carry an identical number, so the number told the owner nothing and the order was
//! whatever `(device, bucket_id)` happened to sort to.
//!
//! Over a *run* the figures differ, and the owner asked for exactly this, in exactly these words:
//!
//! > *"Youtube from 1 to 3, game from 1 to 2.5, work from 1.5 to 2 [...] they should be ascending
//! > youtube game work from the longest to the shortest"*
//!
//! — YouTube 2h, game 1h30, work 30m. Note what that implies: the figure is the competitor's whole
//! originating run (`source_start`..`source_end`), **not** its overlap with the run window. YouTube
//! is called the longest even though the contention ends at 2.5. That is the same measure
//! provisional attribution's rule 1 (**R17**) compares to pick a winner, so the list now reads in
//! the order the winner was chosen in, and the offered winner is always first — the answer the
//! owner is most likely to want is under the thumb.
//!
//! Ties break on label then device, so two devices holding the same day offer the same order
//! (**R18**).

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};

use crate::{activity_label, Segment};

/// Default run gap in seconds — how long one device may hold the day alone before the competition
/// either side of it counts as two competitions rather than one.
pub const DEFAULT_QUESTION_GAP_SECS: i64 = 60;

/// The one number ⑧ runs on.
#[derive(Clone, Copy, Debug)]
pub struct QuestionOptions {
    /// Settled time longer than this ends a run. Zero means every contended block is its own
    /// question, which is the behaviour every release before 4.11 had.
    pub gap: Duration,
}

impl QuestionOptions {
    pub fn from_gap_secs(secs: i64) -> Self {
        Self {
            gap: Duration::seconds(secs.max(0)),
        }
    }
}

impl Default for QuestionOptions {
    fn default() -> Self {
        Self::from_gap_secs(DEFAULT_QUESTION_GAP_SECS)
    }
}

/// One activity that was running during a question, and for how long.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Competitor {
    pub device: String,
    pub label: String,
    /// The union of this activity's originating intervals across the run, in seconds. Unions
    /// rather than sums: a heartbeat-split event is one stretch of one app, and counting it twice
    /// would put it above a rival it did not actually outlast.
    pub seconds: i64,
    /// True for the run's provisional winner — the activity that counts unless the owner says
    /// otherwise. Exactly one competitor carries it.
    pub is_foreground: bool,
}

/// One thing the owner is asked about.
#[derive(Clone, Debug)]
pub struct Question {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// Indices into the segment list this was built from, in order. Never empty. The view shades
    /// these as one region and the decision covers `start`..`end`.
    pub blocks: Vec<usize>,
    /// Longest first. Never empty, and always at least two entries — a block with one competitor
    /// is not contended.
    pub competitors: Vec<Competitor>,
}

impl Question {
    /// Seconds of *unresolved* time this question covers — the sum of its blocks' counted spans,
    /// not `end - start`, which also covers the settled slivers bridged over.
    pub fn unresolved_seconds(&self, segments: &[Segment]) -> i64 {
        self.blocks
            .iter()
            .filter_map(|&i| segments.get(i))
            .fold(Duration::zero(), |acc, s| acc + s.counted_span())
            .num_seconds()
    }
}

/// Group the still-unanswered blocks of a day into the questions actually worth asking.
///
/// Input is the segment list as the view has it — after ⑥ and ⑦ — because the indices handed back
/// are indices into exactly that list. Blocks that are not `unresolved` are never in a question:
/// an answered overlap is not a question, and a settled block never was one.
pub fn questions(segments: &[Segment], opts: QuestionOptions) -> Vec<Question> {
    let mut out: Vec<Question> = Vec::new();
    // The index of the previous unresolved block, and the foreground identity it ran under. Kept
    // rather than re-read from `out` so the "nothing but settled blocks between" condition is a
    // property of the walk instead of a second scan.
    let mut prev: Option<(usize, String, String)> = None;

    for (i, seg) in segments.iter().enumerate() {
        if !seg.unresolved {
            continue;
        }
        let fg = seg.foreground_slice();
        let ident = (fg.device.clone(), activity_label(&fg.data));

        let joins = match &prev {
            Some((j, dev, lab)) => {
                *dev == ident.0
                    && *lab == ident.1
                    && seg.start - segments[*j].end <= opts.gap
                    // A negative difference cannot come out of ②, but ⑦ may draw a block over a
                    // hole; guard rather than rely on it, since a negative always compares <= gap.
                    && seg.start >= segments[*j].end
            }
            None => false,
        };

        if joins {
            let q = out.last_mut().expect("`prev` is set only after a push");
            q.end = seg.end;
            q.blocks.push(i);
        } else {
            out.push(Question {
                start: seg.start,
                end: seg.end,
                blocks: vec![i],
                competitors: Vec::new(),
            });
        }
        prev = Some((i, ident.0, ident.1));
    }

    for q in out.iter_mut() {
        q.competitors = competitors_of(segments, q);
    }
    out
}

/// Every activity that ran during a question, longest first.
fn competitors_of(segments: &[Segment], q: &Question) -> Vec<Competitor> {
    // `BTreeMap` and not a `HashMap`: the spans are unioned in an order that must not depend on
    // hash seeding, and the final sort's tiebreak reads the key (R18).
    let mut spans: BTreeMap<(String, String), Vec<(DateTime<Utc>, DateTime<Utc>)>> = BTreeMap::new();
    // The run's winner, taken from the first block; every block in a run shares it by construction.
    let first = &segments[q.blocks[0]];
    let fg = first.foreground_slice();
    let winner = (fg.device.clone(), activity_label(&fg.data));

    for &i in &q.blocks {
        for slice in &segments[i].active {
            spans
                .entry((slice.device.clone(), activity_label(&slice.data)))
                .or_default()
                .push((slice.source_start, slice.source_end));
        }
    }

    let mut out: Vec<Competitor> = spans
        .into_iter()
        .map(|((device, label), mut ivs)| {
            ivs.sort();
            let seconds = union_seconds(&ivs);
            let is_foreground = (device.clone(), label.clone()) == winner;
            Competitor {
                device,
                label,
                seconds,
                is_foreground,
            }
        })
        .collect();

    out.sort_by(|a, b| {
        b.seconds
            .cmp(&a.seconds)
            // The winner outranks an equally long rival, so the offered answer is still first when
            // two devices ran the same app for exactly as long — which is not hypothetical, it is
            // what a phone and a tablet showing the same video look like.
            .then_with(|| b.is_foreground.cmp(&a.is_foreground))
            .then_with(|| a.label.cmp(&b.label))
            .then_with(|| a.device.cmp(&b.device))
    });
    out
}

/// Total seconds covered by a sorted interval list, counting overlap once.
fn union_seconds(sorted: &[(DateTime<Utc>, DateTime<Utc>)]) -> i64 {
    let mut total = Duration::zero();
    let mut cur: Option<(DateTime<Utc>, DateTime<Utc>)> = None;
    for &(s, e) in sorted {
        match cur {
            Some((cs, ce)) if s <= ce => cur = Some((cs, ce.max(e))),
            Some((cs, ce)) => {
                total = total + (ce - cs);
                cur = Some((s, e));
            }
            None => cur = Some((s, e)),
        }
    }
    if let Some((cs, ce)) = cur {
        total = total + (ce - cs);
    }
    total.num_seconds()
}
