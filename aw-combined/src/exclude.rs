//! ②b Exclude — take activity the owner said never counts out of the competition.
//!
//! Roadmap 4.6. The owner's words: *"do not count One UI Home"* — **not** "tap this block and it
//! stops counting", which is a different (and also wanted) thing. This is the standing version: a
//! rule, written the way a category rule is written, that removes an activity from the day
//! wherever it appears, on every device, forever, until the rule is changed.
//!
//! **The rules are category rules.** A category carries `data.not_counted: true` and everything it
//! matches stops counting. That is not a shortcut: the owner asked for "a rule like the
//! categories", the category editor already writes regexes and tests them against real events,
//! `classes` already syncs between devices (roadmap 2.3), and it is the same setting aw-webui uses
//! on a PC. A second rule language would have needed its own editor, its own sync and its own
//! matcher that had to agree with this one forever.
//!
//! **Where it runs, and why there.** Between ② segment and ③ classify. A launcher sitting on the
//! phone while the tablet is genuinely in use is *contention today* — the pipeline has two devices
//! awake and has to ask the owner which counted. Removing the excluded slice before classification
//! means that question is never asked, which is most of what the owner is buying: the launcher was
//! never a competitor.
//!
//! **Nothing is deleted (R11).** A segment left with no counted activity keeps every slice it had
//! and is marked [`Segment::ignored`] plus [`Segment::not_counted`], so the block still draws — the
//! owner asked for excluded time to stay visible, muted, rather than vanish — and the day's total
//! leaves it out exactly the way an "I was away" decision does. Change the rule and the day
//! recomputes with the time back.
//!
//! ⚠️ **A rule beats a per-block decision here**, which is the opposite of ④'s "exact beats rule".
//! Exclusion runs first and a decision cannot act on a slice that is no longer in the segment. That
//! is the honest behaviour for now — a standing "this never counts" really should not be overridden
//! silently by a decision made before the rule existed — but it means the way to count one excluded
//! block again is to narrow the rule, not to tap the block. Revisit if it bites.

use std::sync::Arc;

use fancy_regex::Regex;
use serde_json::{Map, Value};

use crate::Segment;

/// One "never count this" rule, compiled.
///
/// Mirrors `aw-transform`'s `RegexRule` on purpose, down to the `(?i)` prefix trick and the
/// `select_keys` semantics: the same category rule has to mean the same thing to the per-device
/// query (which uses that type) and to the combined pipeline (which uses this one). Two readings of
/// one rule would show the owner two different days.
#[derive(Clone, Debug)]
pub struct NotCountedRule {
    regex: Arc<Regex>,
    select_keys: Option<Vec<String>>,
}

impl NotCountedRule {
    pub fn new(
        regex: &str,
        ignore_case: bool,
        select_keys: Option<Vec<String>>,
    ) -> Result<Self, fancy_regex::Error> {
        // An empty select_keys would silently match nothing at all, which reads to the owner as
        // "my rule does not work" with nothing to see. Refuse it, as aw-transform does.
        if let Some(keys) = &select_keys {
            if keys.is_empty() {
                return Err(fancy_regex::Error::ParseError(
                    0,
                    fancy_regex::ParseError::GeneralParseError(
                        "select_keys must not be empty".to_string(),
                    ),
                ));
            }
        }
        // fancy-regex has no case-insensitive builder flag, so it goes in the pattern.
        let full = if ignore_case {
            format!("(?i){regex}")
        } else {
            regex.to_string()
        };
        Ok(NotCountedRule {
            regex: Arc::new(Regex::new(&full)?),
            select_keys,
        })
    }

    /// Whether this rule matches one activity's `data`.
    ///
    /// With `select_keys`, only those fields are tested; without, every string field is — the same
    /// two cases the category matcher has. Keys are visited in the map's own (sorted) order, so the
    /// answer never depends on which device wrote the event (**R18**).
    pub fn matches(&self, data: &Map<String, Value>) -> bool {
        let hit = |value: &Value| match value.as_str() {
            Some(s) => self.regex.is_match(s).unwrap_or(false),
            None => false,
        };
        match &self.select_keys {
            Some(keys) => keys.iter().filter_map(|k| data.get(k)).any(hit),
            None => data.values().any(hit),
        }
    }
}

/// Pull excluded activity out of every segment.
///
/// Two outcomes, and the difference matters to the owner:
///
///  - **Some counted activity remains.** The excluded slices are dropped from the competition and
///    the block goes on counting, now attributed to whatever really was in use. Their labels are
///    kept in [`Segment::excluded_labels`] so the view can say what was taken out — a block whose
///    competitor silently disappeared would be indistinguishable from one that never had one.
///  - **Nothing counted remains.** Every slice is kept so the block still draws and still names
///    what it was, and it is marked ignored and [`Segment::not_counted`]: the time counts toward
///    nothing, the same way an "I was away" answer does.
pub(crate) fn exclude(segments: &mut [Segment], rules: &[NotCountedRule]) {
    if rules.is_empty() {
        return;
    }
    for seg in segments.iter_mut() {
        let excluded: Vec<bool> = seg
            .active
            .iter()
            .map(|slice| rules.iter().any(|r| r.matches(&slice.data)))
            .collect();
        if !excluded.iter().any(|e| *e) {
            continue;
        }

        let mut labels: Vec<String> = seg
            .active
            .iter()
            .zip(&excluded)
            .filter(|(_, ex)| **ex)
            .map(|(slice, _)| crate::activity_label(&slice.data))
            .collect();
        labels.sort();
        labels.dedup();
        seg.excluded_labels = labels;

        if excluded.iter().all(|e| *e) {
            seg.ignored = true;
            seg.not_counted = true;
            continue;
        }

        let mut keep = excluded.iter().map(|ex| !ex);
        seg.active.retain(|_| keep.next().unwrap_or(true));
    }
}
