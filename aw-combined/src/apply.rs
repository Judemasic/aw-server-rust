//! ④ Apply decisions — the step that turns an owner's answer into the timeline everyone sees.
//!
//! Runs between ③ classify and ⑤ provisional attribution, exactly where
//! `aw-android/docs/04_COMBINED_TIMELINE.md` §2 puts it, and on the **atomic** segments: coalescing
//! first would destroy what a decision attaches to.
//!
//! Two passes, in this order (§2.3):
//!
//! 1. **Exact** — a `scope: once` decision whose window covers this segment and which can act on it.
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
//!
//! **A windowed decision answers a stretch of time, not a cast of competitors** (roadmap 4.2a).
//! Until 4.2a a `once` decision also had to match the segment's signature exactly, which was wrong
//! in the case the owner meets most: `coalesce` glues neighbouring segments together whenever the
//! *winner* is unchanged, so a block on screen routinely contains several different casts. The
//! sheet reads its cast off the glued block, and a three-way cast matches none of the two-way
//! segments underneath — so the answer landed nowhere, silently, and every overlap where a device
//! switched app part-way through was unresolvable.
//!
//! The signature stays what a **rule** matches on: "whenever *these* things compete, X wins" really
//! is a statement about the cast. A `once` decision is a statement about the time.
//!
//! **And it can only settle time where the thing it picked was running.** A `foreground` pick names
//! one competitor; where that competitor is absent the owner has answered nothing, and the segment
//! goes on asking. The alternative — settling it anyway and letting ⑤ choose a winner — credits an
//! activity with time no watcher ever recorded, which is what **R11** forbids. `ignore` and
//! `relabel` name the *time* rather than a competitor, so those two do cover the whole window.

use std::collections::HashMap;

use crate::decision::{
    Decision, ForegroundPick, Participant, Signature, OUTCOME_FOREGROUND, OUTCOME_IGNORE,
    OUTCOME_RELABEL,
};
use crate::{activity_label, Segment, SegmentState};

/// Apply every decision that matches, to every segment it matches.
///
/// `roles` maps device uuid -> the role name a signature uses. See [`crate::PipelineInput`]: today
/// that is the device's hostname, which is what the recording device wrote.
pub(crate) fn apply(
    segments: &mut [Segment],
    decisions: &[Decision],
    roles: &HashMap<String, String>,
) {
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
        // Pass 1: the narrowest thing there is — a decision recorded over this very time.
        let mut exact: Option<&Decision> = None;
        for decision in decisions {
            // `once` only. A rule's window records where the owner *was* when they made it, not
            // what it applies to, and since 4.2a the windowed pass no longer checks the cast — so
            // letting a rule through here would settle whatever else happened to share that clock.
            // A rule still settles the block it was made on, through the pass below, where its cast
            // matches by construction and the view can say a rule did it (**R16**).
            if decision.is_rule() {
                continue;
            }
            let Some((start, end)) = decision.window_bounds() else {
                continue;
            };
            if start > seg.start || end < seg.end {
                continue;
            }
            if !can_act_on(seg, decision, roles) {
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
            None => {
                let keys = segment_match_keys(seg, roles);
                match keys
                    .iter()
                    .find_map(|k| best_rule.get(k))
                    .filter(|d| can_act_on(seg, d, roles))
                {
                    Some(d) => (*d, true),
                    None => continue,
                }
            }
        };
        resolve(seg, decision, by_rule, roles);
    }
}

/// Whether this decision has anything to say about this segment.
///
/// A `foreground` pick can only settle a segment the picked activity is actually part of; see the
/// module note. An outcome this build does not recognise says nothing about any segment — §8 says
/// ignore what we do not understand — and it must not consume precedence over a rule that *is*
/// understood, either.
fn can_act_on(seg: &Segment, decision: &Decision, roles: &HashMap<String, String>) -> bool {
    match decision.resolution.outcome.as_str() {
        OUTCOME_FOREGROUND => decision
            .resolution
            .foreground
            .as_ref()
            .and_then(|pick| pick_index(seg, pick, roles))
            .is_some(),
        OUTCOME_RELABEL | OUTCOME_IGNORE => true,
        _ => false,
    }
}

/// Where the picked activity sits among this segment's slices, if it is here at all.
///
/// uuid first, role second: a decision synced from a peer names the device it saw, and a rule that
/// outlived a replaced device only has the role left.
///
/// ⚠️ **The role is only consulted for a device we have never heard of.** Every Android device
/// reports its own hostname as `localhost` (`gethostname()` on the embedded server), so a peer's
/// decision names *itself* `localhost` — the one string that means a different device on every
/// machine it is read on. Falling back on it whenever the uuid missed made a peer credit **its own**
/// activity with the owner's pick, and the two devices then disagreed about the same seconds, which
/// is exactly what **R18** forbids. Found on hardware in 4.2a: the S25U left an 8-second tail
/// asking, the tablet settled the same tail in favour of itself.
///
/// A device we know about and simply cannot find in this segment is an answer of "not here", not a
/// reason to guess. The fallback goes on existing for the case it was written for — a rule that
/// outlived the device that made it, whose uuid nothing in this day has ever seen.
fn pick_index(
    seg: &Segment,
    pick: &ForegroundPick,
    roles: &HashMap<String, String>,
) -> Option<usize> {
    if let Some(i) = seg
        .active
        .iter()
        .position(|s| s.device == pick.device_uuid && activity_label(&s.data) == pick.app)
    {
        return Some(i);
    }
    if roles.contains_key(&pick.device_uuid) {
        return None; // a device this day knows; it is simply not in this segment
    }
    seg.active.iter().position(|s| {
        role_of(&s.device, roles) == pick.device_role && activity_label(&s.data) == pick.app
    })
}

/// Every signature key this segment could have been recorded under. **Rules only** — a windowed
/// decision no longer matches on the cast (see the module note).
///
/// Normally one: the participants, canonically ordered, with the same fields the sheet writes.
///
/// **Two when the roles are named.** Records written before the server learned to resolve hostnames
/// carry the device *uuid* in `device_role`, because the hostname map was never populated and both
/// ends fell back to the uuid identically. Those decisions are correct and the owner made them on
/// purpose; matching only the new spelling would make them silently stop applying, and a resolved
/// block quietly going back to asking is worse than never having resolved it. So the uuid form is
/// tried too. It costs one extra string compare and it can be dropped once no such records remain
/// — there is no way to know when that is, so it stays.
///
/// Duplicates collapse within a key. A heartbeat-split event puts one device's app into `active`
/// twice, which would otherwise produce a two-participant signature for what the owner was shown as
/// one row — and no decision they made would ever match it again.
fn segment_match_keys(seg: &Segment, roles: &HashMap<String, String>) -> Vec<String> {
    let named = signature_of(seg, |uuid| role_of(uuid, roles));
    let by_uuid = signature_of(seg, |uuid| uuid.to_string());
    if named == by_uuid {
        vec![named]
    } else {
        vec![named, by_uuid]
    }
}

fn signature_of(seg: &Segment, role: impl Fn(&str) -> String) -> String {
    let mut participants: Vec<Participant> = Vec::new();
    for slice in &seg.active {
        let participant = Participant {
            device_role: role(&slice.device),
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

/// Carry one decision onto one segment. Only ever reached after [`can_act_on`] said yes.
fn resolve(seg: &mut Segment, decision: &Decision, by_rule: bool, roles: &HashMap<String, String>) {
    match decision.resolution.outcome.as_str() {
        OUTCOME_FOREGROUND => {
            let Some(pick) = decision.resolution.foreground.as_ref() else {
                return; // a `foreground` outcome with nothing picked is not an answer
            };
            let Some(i) = pick_index(seg, pick, roles) else {
                return; // not running here; `can_act_on` has already kept us out of this case
            };
            seg.foreground = i;
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
