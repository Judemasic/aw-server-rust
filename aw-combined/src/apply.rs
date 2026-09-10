//! ④ Apply decisions — the step that turns an owner's answer into the timeline everyone sees.
//!
//! Runs between ③ classify and ⑤ provisional attribution, exactly where
//! `aw-android/docs/04_COMBINED_TIMELINE.md` §2 puts it, and on the **atomic** segments: coalescing
//! first would destroy what a decision attaches to.
//!
//! Two passes, in this order (§2.3):
//!
//! 1. **Exact** — a decision whose window covers this segment and whose signature matches it.
//! 2. **Rule** — a `scope: always` decision whose signature matches, wherever it was recorded.
//!
//! Exact beats rule, so a one-off correction overrides a standing rule without deleting it. A
//! segment resolved by a rule is flagged [`Segment::auto_resolved`] so the view can say *"resolved
//! by your rule"* and offer to revoke it (**R16**).
//!
//! **Covers, not equals.** §2.3 says "a decision recorded for this specific window", and 4.1's sheet
//! resolves a *coalesced* block — a run of atomic segments. Requiring the window to equal the
//! segment would therefore match nothing at all. Covering is the reading that makes the recorded
//! window mean what the owner saw when they answered.

use std::collections::HashMap;

use crate::decision::{Decision, Participant, Signature, OUTCOME_FOREGROUND, OUTCOME_IGNORE, OUTCOME_RELABEL};
use crate::{activity_label, Segment, SegmentState};

/// Apply every decision that matches, to every segment it matches.
///
/// `roles` maps device uuid -> the role name a signature uses. See [`crate::PipelineInput`]: today
/// that is the device's hostname, which is what the recording device wrote.
pub(crate) fn apply(segments: &mut [Segment], decisions: &[Decision], roles: &HashMap<String, String>) {
    if decisions.is_empty() {
        return;
    }
    // Rules are signature-keyed and window-free, so their best candidate can be chosen once for the
    // whole day rather than rescanned per segment.
    let mut best_rule: HashMap<String, &Decision> = HashMap::new();
    for decision in decisions.iter().filter(|d| d.is_rule()) {
        let key = decision.signature.match_key();
        match best_rule.get(&key) {
            Some(held) if !crate::decision::wins(decision, held) => {}
            _ => {
                best_rule.insert(key, decision);
            }
        }
    }

    for seg in segments.iter_mut() {
        let key = segment_match_key(seg, roles);

        // Pass 1: the narrowest thing there is — a decision recorded over this very time.
        let mut exact: Option<&Decision> = None;
        for decision in decisions {
            if decision.signature.match_key() != key {
                continue;
            }
            let Some((start, end)) = decision.window_bounds() else {
                continue;
            };
            if start > seg.start || end < seg.end {
                continue;
            }
            // Two different windows can both cover one segment (a rule recorded over an hour, a
            // correction recorded over ten minutes inside it). The merge never compares these —
            // it only ever groups within one window — so the same precedence is applied here.
            exact = match exact {
                Some(held) if !crate::decision::wins(decision, held) => Some(held),
                _ => Some(decision),
            };
        }

        let (decision, by_rule) = match exact {
            Some(d) => (d, false),
            None => match best_rule.get(&key) {
                Some(d) => (*d, true),
                None => continue,
            },
        };
        resolve(seg, decision, by_rule, roles);
    }
}

/// The signature this segment would be recorded under: every competing device/app, canonically
/// ordered, with the same fields the sheet writes.
///
/// Duplicates collapse. A heartbeat-split event puts one device's app into `active` twice, which
/// would otherwise produce a two-participant signature for what the owner was shown as one row —
/// and no decision they made would ever match it again.
fn segment_match_key(seg: &Segment, roles: &HashMap<String, String>) -> String {
    let mut participants: Vec<Participant> = Vec::new();
    for slice in &seg.active {
        let participant = Participant {
            device_role: role_of(&slice.device, roles),
            device_uuid: slice.device.clone(),
            app: activity_label(&slice.data),
            // Not surfaced by the pipeline yet; the sheet writes null for the same reason. When
            // categories arrive, both sides start filling this in together or neither does.
            category: String::new(),
        };
        if !participants
            .iter()
            .any(|p| p.device_role == participant.device_role && p.app == participant.app)
        {
            participants.push(participant);
        }
    }
    Signature::of(participants).match_key()
}

/// The role name a signature uses for a device: its hostname when we know one, else its uuid — the
/// same fallback the recording device applies, so the two agree on an untagged peer.
fn role_of(uuid: &str, roles: &HashMap<String, String>) -> String {
    roles.get(uuid).cloned().unwrap_or_else(|| uuid.to_string())
}

/// Carry one decision onto one segment.
///
/// An outcome this build does not recognise resolves **nothing**: §8 says ignore what we do not
/// understand, and quietly settling a segment on the strength of a word we cannot read would hide
/// an open question rather than answer it.
fn resolve(seg: &mut Segment, decision: &Decision, by_rule: bool, roles: &HashMap<String, String>) {
    match decision.resolution.outcome.as_str() {
        OUTCOME_FOREGROUND => {
            let Some(pick) = decision.resolution.foreground.as_ref() else {
                return; // a `foreground` outcome with nothing picked is not an answer
            };
            // uuid first, role second: a decision synced from a peer names the device it saw, and a
            // rule that outlived a replaced device only has the role left.
            let index = seg
                .active
                .iter()
                .position(|s| s.device == pick.device_uuid && activity_label(&s.data) == pick.app)
                .or_else(|| {
                    seg.active.iter().position(|s| {
                        role_of(&s.device, roles) == pick.device_role
                            && activity_label(&s.data) == pick.app
                    })
                });
            match index {
                Some(i) => seg.foreground = i,
                // The picked activity is not in this segment. That is possible for a rule matched
                // on a signature whose apps are present but whose *pick* names a device no longer
                // here. Leave the winner to ⑤ rather than crediting the wrong slice, but still
                // record that the question was answered.
                None => {}
            }
        }
        OUTCOME_RELABEL => {
            seg.label_override = decision.resolution.label.clone();
        }
        OUTCOME_IGNORE => {
            seg.ignored = true;
        }
        _ => return,
    }
    seg.state = SegmentState::Settled;
    seg.resolved_by = Some(decision.id.clone());
    seg.auto_resolved = by_rule;
    seg.deliberate_background = decision.resolution.deliberate_background.clone();
}
