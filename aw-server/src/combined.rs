//! Roadmap 3.4 — the datastore adapter for [`aw_combined`].
//!
//! `aw-combined` is pure by design: no datastore, no file I/O, no clock. This module is the layer
//! that reads a [`Datastore`], decides which buckets are *activity* and which are *idle*, runs the
//! pipeline, and shapes the result for a view. It is the only place that knows both halves.
//!
//! **Deliberately not inside `android/`.** That module is `#[cfg(target_os = "android")]`, so a
//! desktop `cargo check` never type-checks a line of it — and this repo's local check is a desktop
//! check (`scripts/check-local.sh` says so in as many words). Keeping the logic here means the part
//! that can actually be wrong is verified before an APK is ever built; the JNI wrapper stays a
//! string-in, string-out shell.
//!
//! See `aw-android/docs/04_COMBINED_TIMELINE.md` §1–2.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};

use aw_combined::{
    activity_label, coalesce, compute_segments, default_min_contention, merge_decisions,
    parse_line, questions, resolve_bucket_device, smooth, synced_from_hostname, BucketEvents,
    NotCountedRule, PipelineInput, Question, QuestionOptions, Segment, SharedRecord, SmoothOptions,
};
use aw_datastore::Datastore;
use aw_models::Event;

/// The bucket type that counts as activity.
///
/// Only `currentwindow`. `web.tab.current` is deliberately excluded even though a device may have
/// one: it overlaps the window bucket for the same instants, so including it would put a browser
/// *tab* and its *window* in contention with each other on one device and let a tab title win the
/// combined track. Web data stays in the raw per-device view. Revisit if the owner asks for
/// tab-level detail in the combined track.
const ACTIVITY_TYPE: &str = "currentwindow";

/// Bucket types that say *what* the owner was doing, and never *which device was counting*.
///
/// These are the buckets [`ACTIVITY_TYPE`]'s doc comment declines to put into contention, and for
/// the same reason: a browser tab overlaps its own window for the same instants, so a tab that
/// competed with a window would let a tab title win the combined track. That argument is about
/// *contention* only. It was read as "the combined day cannot answer these at all", which is why
/// the browser, editor and clock panels were marked unavailable there -- the owner's whole day was
/// in the app and the question "what was I reading?" had no answer on the one screen that shows
/// the whole day.
///
/// So they are read, and they are never activity. Each one's events are intersected with the time
/// its *own* device actually counted for, which is the part a per-device view cannot do: a phone's
/// browsing while the desktop held the foreground is real browsing that is not part of the
/// combined day, and a per-device browser panel has always counted it anyway.
const DETAIL_TYPES: [&str; 3] = [
    "web.tab.current",
    "app.editor.activity",
    "general.stopwatch",
];

/// The clock, which is masked differently -- see [`detail_rows`].
const STOPWATCH_TYPE: &str = "general.stopwatch";

/// How many rows of one detail type the response will carry.
///
/// A day of browsing is thousands of distinct URLs and a panel shows ten. The cap is generous
/// rather than tight because the view groups these again -- URLs into domains, files into projects
/// -- and a cap applied before grouping would quietly drop the tail of a domain that is large only
/// once its URLs are added together. Rows are cut from the short end, so what is lost is always
/// the least of it.
const MAX_DETAIL_ROWS: usize = 500;

/// The bucket type carrying AFK status. Absent on Android — `aw-watcher-android` only records
/// while the screen is on and in use — but a synced desktop peer's afk bucket arrives through the
/// same datastore, and `aw-combined`'s idle contract wants only the genuinely-idle events.
const AFK_TYPE: &str = "afkstatus";
const AFK_STATUS_KEY: &str = "status";
const AFK_STATUS_IDLE: &str = "afk";

/// What [`combined_timeline`] needs beyond the datastore. `hostname_to_uuid` cannot be read here:
/// it lives in the Syncthing folder behind Android's SAF, which only Kotlin can open.
pub struct TimelineRequest {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub own_device: String,
    pub hostname_to_uuid: HashMap<String, String>,
    /// This machine's hostname, for naming the *own* device where a peer is named by the
    /// `-synced-from-<peer>` suffix in its bucket ids. Without it the one device whose events carry
    /// no suffix — this one — would be the only one left showing a uuid.
    pub own_hostname: String,
    /// Roadmap 4.5 — the sliver threshold ⑦ smooths at, in seconds. `None` uses the default (15s);
    /// `Some(0)` is "off", which means literal. A **display** setting and therefore a per-request
    /// parameter rather than stored state: change the number and the day recomputes from the same
    /// events and the same decisions, with nothing written and nothing lost.
    pub sliver_secs: Option<i64>,
}

/// A human label for one event's `data`, for a track row.
///
/// Thin wrapper on [`aw_combined::activity_label`]: step ④ has to reproduce this string exactly to
/// match a decision's signature against a segment, so the definition lives in the pipeline crate and
/// this module borrows it rather than keeping a second copy that could drift.
fn label(data: &Map<String, Value>) -> String {
    activity_label(data)
}

/// Datastore key prefix for the owner's decisions and tombstones (roadmap 4.2).
///
/// **Why the key-value store and not a file.** The canonical, cross-device copy of a decision is a
/// line in `decisions.jsonl` in the Syncthing folder, which on Android only Kotlin can open (SAF).
/// The server therefore keeps its own copy in the datastore and Kotlin carries lines both ways at
/// sync time — exactly the shape the settings sync already has (roadmap 2.3), for exactly the same
/// reason. On a desktop, where there is no shared folder, the datastore copy simply *is* the store.
///
/// Values are stored in **one canonical spelling**: the record's JSON with its keys sorted, which
/// is what `serde_json` produces here (its `Map` is a `BTreeMap` — no `preserve_order` feature).
/// Storing whatever bytes arrived was the first design, and it was worse: a decision made on the
/// phone and the same decision arriving from the tablet would sit in the two devices' files spelled
/// two ways. The merge keys on `id` and would not care, but every dump of the file, every diff and
/// any future byte-level compaction would. One normal form, applied on the way in, and both devices
/// hold the same line. Unknown fields survive it — §8 — because nothing is parsed into a struct.
pub const DECISION_KEY_PREFIX: &str = "combined.decision.";

/// Every decision and tombstone this device holds, parsed.
///
/// Keys are visited in sorted order so the list is stable (**R18**) — though the merge itself is
/// order-independent by construction, so this is belt and braces.
pub fn stored_records(ds: &Datastore) -> Result<Vec<SharedRecord>, String> {
    let stored = ds
        .get_key_values(&format!("{DECISION_KEY_PREFIX}%"))
        .map_err(|e| format!("could not read decisions: {e:?}"))?;
    let mut keys: Vec<&String> = stored.keys().collect();
    keys.sort();
    Ok(keys
        .into_iter()
        .filter_map(|k| parse_line(&stored[k]))
        .collect())
}

/// Store one decision or tombstone, keyed by its id. Returns the id.
///
/// Idempotent: the same record written twice overwrites itself, which is what makes the sync's
/// "import everything the shared folder has" pass cheap and safe to repeat.
pub fn store_record(ds: &Datastore, raw: &str) -> Result<String, String> {
    let id = match parse_line(raw) {
        Some(SharedRecord::Decision(d)) => d.id,
        Some(SharedRecord::Tombstone(t)) => t.id,
        _ => return Err("not a decision or tombstone record".to_string()),
    };
    let canonical = serde_json::from_str::<Value>(raw)
        .map(|v| v.to_string())
        .map_err(|e| format!("{id} is not JSON: {e}"))?;
    // The id becomes part of a datastore key, and a ULID is `[0-9A-Z]` after a short prefix. Refuse
    // anything else rather than let a crafted id reach out of this key space.
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(format!("`id` has characters that cannot be a key: {id}"));
    }
    ds.set_key_value(&format!("{DECISION_KEY_PREFIX}{id}"), &canonical)
        .map_err(|e| format!("could not store {id}: {e:?}"))?;
    Ok(id)
}

/// True for an event that represents an *idle* period, per `aw-combined`'s idle contract.
fn is_idle(event: &Event) -> bool {
    matches!(
        event.data.get(AFK_STATUS_KEY),
        Some(Value::String(s)) if s == AFK_STATUS_IDLE
    )
}

/// A pipeline input carrying only what [`resolve_bucket_device`] reads, for resolving a bucket's
/// device *before* the real input exists. The map is deliberately whatever the caller passed and no
/// more: this call is what fills the rest in, so seeding it from itself would be circular.
fn probe(req: &TimelineRequest) -> PipelineInput {
    PipelineInput {
        own_device: req.own_device.clone(),
        hostname_to_uuid: req.hostname_to_uuid.clone(),
        activity: Vec::new(),
        idle: Vec::new(),
        min_contention: default_min_contention(),
        decisions: Vec::new(),
        not_counted: Vec::new(),
    }
}

/// Datastore key holding aw-webui's whole categorisation. One value, a JSON array of categories —
/// the same setting the category editor posts and the same one settings sync carries between
/// devices (roadmap 2.3).
const CLASSES_KEY: &str = "settings.classes";

/// The "never count this" rules, read off the owner's categories (roadmap 4.6).
///
/// A category opts in with `data.not_counted: true`. Reading them here rather than taking them as a
/// request parameter is what makes every screen agree: the day view, the combined timeline and the
/// day's own total all read one server answer, so none of them can be excluding a different set
/// from the others — which is exactly how 4.4d's two-day bug happened.
///
/// **A broken rule is skipped, never fatal.** `classes` is edited by hand in Settings and can hold
/// a regex that does not compile. Refusing to draw the day because one rule is malformed would take
/// the whole screen away over a typo; the other rules still apply and the bad one is logged.
///
/// Children inherit nothing: a category's rule matches or it does not, and a child that should also
/// be excluded says so itself. That mirrors how categorisation already works — a child is not
/// matched by its parent's rule — so the owner has one model to hold, not two.
fn not_counted_rules(ds: &Datastore) -> Vec<NotCountedRule> {
    let Ok(raw) = ds.get_key_value(CLASSES_KEY) else {
        return Vec::new(); // no categorisation stored yet
    };
    let Ok(Value::Array(classes)) = serde_json::from_str::<Value>(&raw) else {
        log::warn!("{CLASSES_KEY} is not a JSON array; no exclusions applied");
        return Vec::new();
    };
    let mut rules = Vec::new();
    for class in &classes {
        if class.pointer("/data/not_counted") != Some(&Value::Bool(true)) {
            continue;
        }
        let Some(regex) = class.pointer("/rule/regex").and_then(Value::as_str) else {
            continue; // a `type: none` rule matches nothing; nothing to exclude
        };
        let ignore_case = class.pointer("/rule/ignore_case") == Some(&Value::Bool(true));
        let select_keys = class
            .pointer("/rule/select_keys")
            .and_then(Value::as_array)
            .map(|keys| {
                keys.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .filter(|keys: &Vec<String>| !keys.is_empty());
        match NotCountedRule::new(regex, ignore_case, select_keys) {
            Ok(rule) => rules.push(rule),
            Err(e) => log::warn!("not-counted rule `{regex}` does not compile, skipping it: {e}"),
        }
    }
    rules
}

/// Read one day (or any range) out of the datastore and return the combined track, the per-device
/// tracks, and the totals, as JSON.
///
/// Bucket ids are visited in sorted order so the JSON is byte-identical across runs (**R18**); the
/// pipeline itself is order-independent, but the per-device tracks are assembled here.
pub fn combined_timeline(ds: &Datastore, req: &TimelineRequest) -> Result<Value, String> {
    let buckets = ds
        .get_buckets()
        .map_err(|e| format!("could not list buckets: {e:?}"))?;

    let mut ids: Vec<&String> = buckets.keys().collect();
    ids.sort();

    let mut activity: Vec<BucketEvents> = Vec::new();
    let mut idle: Vec<BucketEvents> = Vec::new();
    // Read in the same pass, kept apart from `activity` so nothing about contention changes.
    let mut detail: Vec<(String, BucketEvents)> = Vec::new();
    for id in ids {
        let bucket = &buckets[id];
        let is_activity = bucket._type == ACTIVITY_TYPE;
        let is_afk = bucket._type == AFK_TYPE;
        let is_detail = DETAIL_TYPES.contains(&bucket._type.as_str());
        if !is_activity && !is_afk && !is_detail {
            continue;
        }
        let events = ds
            .get_events(id, Some(req.start), Some(req.end), None)
            .map_err(|e| format!("could not read events from {id}: {e:?}"))?;
        if is_activity {
            activity.push(BucketEvents {
                bucket_id: id.clone(),
                events,
            });
        } else if is_afk {
            let events = events.into_iter().filter(is_idle).collect();
            idle.push(BucketEvents {
                bucket_id: id.clone(),
                events,
            });
        } else {
            detail.push((
                bucket._type.clone(),
                BucketEvents {
                    bucket_id: id.clone(),
                    events,
                },
            ));
        }
    }

    // Peers name themselves in their bucket ids (`-synced-from-<peer>`), which is a name every
    // device reads identically — unlike a local nickname. Fill in from there whatever the caller
    // did not supply, so a decision's `device_role` is a hostname and not a uuid
    // (`04_COMBINED_TIMELINE.md` §3). Roadmap 4.2 shipped without this and every recorded role was
    // a uuid: correct, because both sides fell back the same way, but a rule keyed on it could
    // never outlive the device that made it, which is the whole point of a role.
    let mut hostname_to_uuid = req.hostname_to_uuid.clone();
    for bucket in &activity {
        if let Some(peer) = synced_from_hostname(&bucket.bucket_id) {
            let device = resolve_bucket_device(&bucket.events, &bucket.bucket_id, &probe(req));
            hostname_to_uuid.entry(peer.to_string()).or_insert(device);
        }
    }
    if !req.own_hostname.is_empty() {
        hostname_to_uuid
            .entry(req.own_hostname.clone())
            .or_insert_with(|| req.own_device.clone());
    }

    let input = PipelineInput {
        own_device: req.own_device.clone(),
        hostname_to_uuid,
        activity,
        idle,
        // D15/Q1's default. Not a setting yet — nothing in the app exposes one, and 3.4 is about
        // seeing the track at all. Wire it to a preference when Phase 4 gives it a home.
        min_contention: default_min_contention(),
        // ④'s input. Merged here rather than stored merged, because the merge's answer changes
        // whenever a peer's file arrives, and a stored answer would go stale silently (R18).
        decisions: merge_decisions(&stored_records(ds)?),
        // ②b. Read from the owner's categories, not from the request: see `not_counted_rules`.
        not_counted: not_counted_rules(ds),
    };

    // Per-device tracks are built from the *raw* events, before idle subtraction and before
    // segmentation: R11 says the per-device rows are unmodified truth, always available underneath
    // the combined track for comparison. They use `resolve_bucket_device` so origin is decided by
    // exactly the same rule (R19) the pipeline uses.
    let devices = device_tracks(&input);

    // ⑥ then ⑦: coalesce glues identical neighbours, smoothing rounds away what is left over.
    // Both are presentation, both are recomputed on every read, and neither touches the datastore.
    let segments = smooth(
        coalesce(compute_segments(input.clone())),
        SmoothOptions::from_sliver_secs(
            req.sliver_secs.unwrap_or(aw_combined::DEFAULT_SLIVER_SECS),
        ),
    );
    // Time the owner said was nobody's counts toward nothing — that is what "I was away" means.
    // Summed as durations and truncated **once**. Per-block `seconds` truncates per block, so
    // adding those up loses a second every time smoothing joins two blocks whose sub-second parts
    // both round down -- which showed up as the day's total shrinking by 1s at a 60s threshold.
    let combined_seconds: i64 = segments
        .iter()
        // `counted_span`, never `end - start`: ⑥ and ⑦ are allowed to draw a block over a hole a
        // few milliseconds or a second or two wide, and are not allowed to count it (roadmap 4.5c).
        //
        // No `filter` on `ignored` any more, because after 4.5d the block is the wrong unit to ask.
        // Smoothing may draw an excluded sliver inside a counted block and a counted sliver inside
        // an excluded one, so what counts is a property of the *shares*; `counted_span` sums exactly
        // the ones that count and nothing else, in both directions.
        .fold(chrono::Duration::zero(), |acc, s| acc + s.counted_span())
        .num_seconds();

    // The mirror of `combined_seconds`: everything an exclusion rule (or an "I was away" answer)
    // said counts toward nothing. Roadmap 4.6b's panel could only ever say "not measured on this
    // page" on the combined day, because the day's own response never carried this figure -- so an
    // exclusion the owner set was invisible in exactly the view that draws it.
    let excluded_seconds: i64 = segments
        .iter()
        .fold(chrono::Duration::zero(), |acc, s| acc + s.uncounted_span())
        .num_seconds();

    // ⑧ Roadmap 4.11 — what the owner is actually *asked*, which is not one thing per block. Runs
    // over the smoothed list because that is the list the view draws and the indices refer to it.
    let asked = questions(&segments, QuestionOptions::default());
    // Reverse index, so the view can go from a block it is drawing to the question it belongs to
    // without scanning every question for every block.
    let mut question_of: HashMap<usize, usize> = HashMap::new();
    for (qi, q) in asked.iter().enumerate() {
        for &b in &q.blocks {
            question_of.insert(b, qi);
        }
    }

    Ok(json!({
        "start": req.start,
        "end": req.end,
        "own_device": req.own_device,
        "combined_seconds": combined_seconds,
        "excluded_seconds": excluded_seconds,
        "combined": segments
            .iter()
            .enumerate()
            .map(|(i, seg)| combined_row(seg, question_of.get(&i).copied()))
            .collect::<Vec<_>>(),
        // One entry per thing the owner is asked about, in the order it happened. The count the
        // header shows is this list's length, *not* the number of shaded blocks -- those two were
        // the same number until 4.11 and the difference is the whole step.
        "questions": asked
            .iter()
            .map(|q| question_row(q, &segments))
            .collect::<Vec<_>>(),
        "devices": devices,
        // What the owner was reading, editing and timing during the stretches that counted.
        // Keyed by bucket type, because the view already knows what a `web.tab.current` event
        // holds and there is no gain in re-teaching the server.
        "details": detail_rows(&detail, &input, &counted_intervals(&segments), req),
    }))
}

/// One winning activity's `data`, minus the tagging this crate added.
///
/// Returned whole rather than as a hand-picked set of keys: the combined track's shortcoming has
/// always been that a segment "carries an app label and nothing finer", and picking three keys
/// today only moves the same wall three keys further out. What a panel can do with a field is the
/// view's business.
fn detail(data: &Map<String, Value>) -> Map<String, Value> {
    let mut out = data.clone();
    out.remove(aw_combined::EVENT_ORIGIN_KEY);
    out
}

/// The `data` of the activity that held this block longest — [`Segment::dominant_share`], falling
/// back to the foreground slice for a segment that never went through ⑥.
fn dominant_data(seg: &Segment) -> &Map<String, Value> {
    seg.dominant_share()
        .map(|sh| &sh.data)
        .unwrap_or_else(|| &seg.foreground_slice().data)
}

/// What else was running behind the winner, deduplicated by `(device, label)`.
///
/// Deduplication became necessary in 4.5c. ⑥ now merges two blocks of the same app whose screens
/// differ, and the merged block's `active` is the union of both — so the same device appears twice
/// with the same label, and one of those is the winner. Listing it would have the view say the
/// owner's app was running in the background behind itself.
fn background_rows(seg: &Segment) -> Vec<Value> {
    let fg = seg.foreground_slice();
    let own = (fg.device.as_str(), label(&fg.data));
    let mut seen: Vec<(String, String)> = Vec::new();
    let mut out = Vec::new();
    for s in seg.background_slices() {
        let row = (s.device.clone(), label(&s.data));
        if (row.0.as_str(), row.1.clone()) == own || seen.contains(&row) {
            continue;
        }
        seen.push(row.clone());
        out.push(json!({ "device": row.0, "label": row.1 }));
    }
    out
}

/// One question, as the resolution sheet needs it (roadmap 4.11).
///
/// `seconds` is the *answerable* time — the sum of the contended blocks' counted spans — and is
/// deliberately smaller than `end - start`, which also covers the settled slivers the run was drawn
/// across. The two are both here because they answer different things: the span says what to shade,
/// and the seconds say how much time an answer will move. Reporting the span as the time would
/// promise the owner seconds a watcher never recorded (**R11**).
///
/// `competitors` is already longest-first with the offered winner leading; the view renders it in
/// the order it arrives rather than sorting again, so two devices cannot disagree about the order
/// they put the same day's answers in (**R18**).
fn question_row(q: &Question, segments: &[Segment]) -> Value {
    json!({
        "start": q.start,
        "end": q.end,
        "seconds": q.unresolved_seconds(segments),
        "blocks": q.blocks,
        "competitors": q
            .competitors
            .iter()
            .map(|c| json!({
                "device": c.device,
                "label": c.label,
                // How long this activity actually ran, across the whole run and counting an
                // overlap once — not the window's length, which used to be handed to every
                // competitor alike and so told the owner nothing.
                "seconds": c.seconds,
                "is_foreground": c.is_foreground,
            }))
            .collect::<Vec<_>>(),
    })
}

/// One row of the combined track: what counted, and whether the view must shade it (**R8**).
fn combined_row(seg: &Segment, question: Option<usize>) -> Value {
    let fg = seg.foreground_slice();
    json!({
        // Index into the response's `questions`, or null for a block nobody is being asked about.
        // Every block carrying one shades as part of one region and answers with one decision.
        "question": question,
        "start": seg.start,
        "end": seg.end,
        // What a watcher actually recorded inside this block, which is not the same as its span:
        // see `Segment::bridged_ms`. The span is `start`..`end` and is what gets drawn.
        "seconds": seg.counted_span().num_seconds(),
        "bridged_seconds": chrono::Duration::milliseconds(seg.bridged_ms).num_seconds(),
        // Time drawn inside this block that counts toward nothing (roadmap 4.5d). Non-zero only
        // where smoothing put a share of the other kind inside it.
        "uncounted_seconds": seg.uncounted_span().num_milliseconds() as f64 / 1000.0,
        // The owner's own words win over the app name when they gave any (`outcome: relabel`).
        "label": seg.label_override.clone().unwrap_or_else(|| label(&fg.data)),
        "device": fg.device,
        "state": seg.state,
        "unresolved": seg.unresolved,
        "absorbed_short_contention": seg.absorbed_short_contention,
        "resolved_by": seg.resolved_by,
        "auto_resolved": seg.auto_resolved,
        // What the winning activity was, beyond its name (roadmap 4.4i). On Android that is
        // `classname`, the screen inside the app, which is the only per-screen detail the
        // platform gives -- and which the combined day had no way to show because this row
        // carried a label and nothing else.
        //
        // **The dominant one, with the rest in `shares`.** Until 4.5c this was exact for a
        // different reason -- ⑥ would only glue two blocks whose winning `data` was equal, so a
        // block could not span two screens. That exactness is what made the day draw four
        // Photos blocks in a row where the owner had used Photos once, so ⑥ now merges on the
        // app and the screens move into `shares` with the time each held. `detail` names the
        // one that held longest; nothing is lost, and `shares` is what a per-screen panel
        // should sum. The origin tag is stripped because it is bookkeeping about where the
        // event came from, not about what the owner was doing, and `device` already says it.
        "detail": detail(dominant_data(seg)),
        // Every distinct screen (or window title) inside this block and how long it held.
        // Fractional seconds on purpose: a block's shares have to add back up to its own
        // `seconds`, and truncating each one would lose a second per share per block.
        "shares": seg
            .foreground_shares
            .iter()
            .map(|sh| json!({
                "detail": detail(&sh.data),
                "label": label(&sh.data),
                "seconds": sh.ms as f64 / 1000.0,
                // Roadmap 4.5d: a share inside a counted block that still counts toward nothing,
                // or the reverse. The view marks it rather than hiding it -- the owner has to be
                // able to see that the launcher second inside this stretch of Photos is not in
                // the total, or the exclusion they set becomes invisible.
                "not_counted": sh.not_counted,
            }))
            .collect::<Vec<_>>(),
        "ignored": seg.ignored,
        // Roadmap 4.6: ignored because a rule says this never counts, rather than because the
        // owner answered "I was away". The view says different things about the two.
        "not_counted": seg.not_counted,
        "excluded_labels": seg.excluded_labels,
        "relabelled": seg.label_override.is_some(),
        "deliberate_background": seg.deliberate_background,
        // Roadmap 4.5. What ⑦ rounded into this block, so the view can say so.
        "smoothed_seconds": seg.smoothed_seconds,
        "absorbed_labels": seg.absorbed_labels,
        "background": background_rows(seg),
    })
}

/// A half-open span of time, as the mask uses it.
type Span = (DateTime<Utc>, DateTime<Utc>);

/// For each device, the stretches of the day the combined track gave it, merged and sorted.
///
/// This is the whole point of putting browser and editor data on the combined day rather than
/// leaving it to the per-device pages. A per-device browser panel counts every second the browser
/// was in front on *that* device, including the hours the combined day awarded to a different
/// device entirely. Intersecting with the mask answers the question the combined day actually
/// asks: not "what did this device's browser see today", but "what was I reading, during the time
/// that counted as mine".
///
/// **Blocks, not shares.** A block whose `counted_span` is zero contributes nothing; anything else
/// contributes its whole span. Shares carry the `not_counted` flag at a finer grain (4.5d) but
/// carry no times of their own, so a sliver of excluded time that ⑦ drew inside a counted block is
/// inside the mask. That sliver is bounded by the smoothing threshold -- seconds, against a day --
/// and the alternative is to give shares timestamps they do not have. Worth revisiting if
/// smoothing ever runs at minutes.
fn counted_intervals(segments: &[Segment]) -> HashMap<String, Vec<Span>> {
    let mut by_device: HashMap<String, Vec<Span>> = HashMap::new();
    for seg in segments {
        if seg.counted_span().num_milliseconds() <= 0 {
            continue;
        }
        by_device
            .entry(seg.foreground_slice().device.clone())
            .or_default()
            .push((seg.start, seg.end));
    }
    for spans in by_device.values_mut() {
        spans.sort();
        // Merge touching and overlapping spans, so an event crossing a block boundary is not
        // counted once per block it touches.
        let mut merged: Vec<Span> = Vec::with_capacity(spans.len());
        for (start, end) in spans.drain(..) {
            match merged.last_mut() {
                Some(last) if start <= last.1 => {
                    if end > last.1 {
                        last.1 = end;
                    }
                }
                _ => merged.push((start, end)),
            }
        }
        *spans = merged;
    }
    by_device
}

/// Milliseconds of `event` that fall inside `mask`.
fn overlap_ms(event: &Event, mask: &[Span]) -> i64 {
    let start = event.timestamp;
    let end = start + event.duration;
    let mut total = 0;
    for (ms, me) in mask {
        // The mask is sorted, so once a span starts after this event ends, so does every one after.
        if *ms >= end {
            break;
        }
        let from = if *ms > start { *ms } else { start };
        let to = if *me < end { *me } else { end };
        if to > from {
            total += (to - from).num_milliseconds();
        }
    }
    total
}

/// The browser, editor and clock rows for a combined day, keyed by bucket type.
///
/// Grouped by the event's whole `data` rather than by a chosen key. Grouping by domain, by file or
/// by language is the view's business -- it has to group them anyway, since the same URL arrives
/// from two devices as two rows -- and picking keys here would mean the server deciding, per
/// watcher, which fields matter. A watcher this code has never heard of then reaches the day whole
/// rather than gutted, which is the same reasoning `detail` on a block already follows.
///
/// **The clock is not masked.** A stopwatch event is not a measurement of a device: it is the
/// owner starting and stopping a timer, over a span they chose, and there is no sense in which
/// another device winning the foreground makes ten minutes of it not have happened. Clipping it to
/// the requested range is right -- that is what the day is -- and masking it is not.
fn detail_rows(
    buckets: &[(String, BucketEvents)],
    input: &PipelineInput,
    mask: &HashMap<String, Vec<Span>>,
    req: &TimelineRequest,
) -> Value {
    // type -> canonical data -> (data, milliseconds)
    let mut by_type: HashMap<&str, HashMap<String, (Map<String, Value>, i64)>> = HashMap::new();
    let day: [Span; 1] = [(req.start, req.end)];

    for (bucket_type, bucket) in buckets {
        let device = resolve_bucket_device(&bucket.events, &bucket.bucket_id, input);
        let empty: Vec<Span> = Vec::new();
        let spans: &[Span] = if bucket_type == STOPWATCH_TYPE {
            &day
        } else {
            mask.get(&device).unwrap_or(&empty)
        };
        if spans.is_empty() {
            continue; // this device never held the foreground today
        }
        let rows = by_type.entry(bucket_type.as_str()).or_default();
        for event in &bucket.events {
            let ms = overlap_ms(event, spans);
            if ms <= 0 {
                continue;
            }
            let data = detail(&event.data);
            // `Map` is a `BTreeMap` here, so its `to_string` is already one canonical spelling --
            // the same property `store_record` relies on.
            let key = Value::Object(data.clone()).to_string();
            let row = rows.entry(key).or_insert_with(|| (data, 0));
            row.1 += ms;
        }
    }

    let mut out = Map::new();
    for (bucket_type, rows) in by_type {
        let mut rows: Vec<(Map<String, Value>, i64)> = rows.into_values().collect();
        // Longest first, and ties broken by the data itself so two identical durations do not
        // swap places between two reads of the same day (**R18**).
        rows.sort_by(|a, b| {
            b.1.cmp(&a.1).then_with(|| {
                Value::Object(a.0.clone())
                    .to_string()
                    .cmp(&Value::Object(b.0.clone()).to_string())
            })
        });
        rows.truncate(MAX_DETAIL_ROWS);
        out.insert(
            bucket_type.to_string(),
            Value::Array(
                rows.into_iter()
                    .map(|(data, ms)| {
                        json!({
                            "data": data,
                            // Fractional, for the same reason a block's shares are: a day is
                            // hundreds of rows and truncating each one loses minutes.
                            "seconds": ms as f64 / 1000.0,
                        })
                    })
                    .collect::<Vec<_>>(),
            ),
        );
    }
    Value::Object(out)
}

/// The raw per-device rows, sorted by device uuid so the output is stable (**R18**).
fn device_tracks(input: &PipelineInput) -> Vec<Value> {
    // device -> (rows, total seconds). Rows keep bucket order, which is sorted-id order.
    let mut by_device: HashMap<String, Vec<Value>> = HashMap::new();
    let mut totals: HashMap<String, i64> = HashMap::new();
    for bucket in &input.activity {
        let device = resolve_bucket_device(&bucket.events, &bucket.bucket_id, input);
        for event in &bucket.events {
            let seconds = event.duration.num_seconds();
            if seconds <= 0 {
                continue; // the unlock watcher's zero-width heartbeats would draw nothing
            }
            *totals.entry(device.clone()).or_insert(0) += seconds;
            by_device.entry(device.clone()).or_default().push(json!({
                "start": event.timestamp,
                "end": event.timestamp + event.duration,
                "seconds": seconds,
                "label": label(&event.data),
            }));
        }
    }

    // hostname_to_uuid is a lookup map; invert it once so a row can show a name, not a uuid.
    let mut uuid_to_hostname: HashMap<&str, &str> = HashMap::new();
    for (hostname, uuid) in &input.hostname_to_uuid {
        uuid_to_hostname.insert(uuid, hostname);
    }

    let mut out: Vec<String> = by_device.keys().cloned().collect();
    out.sort();
    out.into_iter()
        .map(|device| {
            let mut rows = by_device.remove(&device).unwrap_or_default();
            rows.sort_by(|a, b| a["start"].to_string().cmp(&b["start"].to_string()));
            json!({
                "device": device,
                "hostname": uuid_to_hostname.get(device.as_str()),
                "is_own": device == input.own_device,
                "total_seconds": totals.get(&device).copied().unwrap_or(0),
                "events": rows,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn data(app: &str) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("app".to_string(), json!(app));
        m
    }

    #[test]
    fn label_prefers_app_then_title_then_url() {
        assert_eq!(label(&data("YouTube")), "YouTube");
        let mut m = Map::new();
        m.insert("title".to_string(), json!("A page"));
        assert_eq!(label(&m), "A page");
        assert_eq!(label(&Map::new()), "(unknown)");
    }

    #[test]
    fn label_skips_empty_strings() {
        let mut m = Map::new();
        m.insert("app".to_string(), json!(""));
        m.insert("title".to_string(), json!("fallback"));
        assert_eq!(label(&m), "fallback");
    }

    fn at(minute: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-11T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
            + chrono::Duration::minutes(minute)
    }

    fn event(from: i64, to: i64, data: Map<String, Value>) -> Event {
        Event {
            id: None,
            timestamp: at(from),
            duration: chrono::Duration::minutes(to - from),
            data,
        }
    }

    fn page(url: &str) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("url".to_string(), json!(url));
        m
    }

    #[test]
    fn overlap_counts_only_the_part_inside_the_mask() {
        let mask = vec![(at(10), at(20))];
        // Straddling the start, straddling the end, wholly inside, wholly outside.
        assert_eq!(overlap_ms(&event(5, 15, page("a")), &mask), 5 * 60_000);
        assert_eq!(overlap_ms(&event(15, 25, page("a")), &mask), 5 * 60_000);
        assert_eq!(overlap_ms(&event(12, 18, page("a")), &mask), 6 * 60_000);
        assert_eq!(overlap_ms(&event(30, 40, page("a")), &mask), 0);
    }

    #[test]
    fn overlap_adds_up_across_separate_stretches() {
        // A browser left open across a gap the combined day gave to another device: both ends
        // count, the middle does not.
        let mask = vec![(at(0), at(10)), (at(20), at(30))];
        assert_eq!(overlap_ms(&event(0, 30, page("a")), &mask), 20 * 60_000);
    }

    #[test]
    fn overlap_of_a_zero_length_event_is_zero() {
        let mask = vec![(at(0), at(10))];
        assert_eq!(overlap_ms(&event(5, 5, page("a")), &mask), 0);
    }

    fn input_for(own: &str) -> PipelineInput {
        PipelineInput {
            own_device: own.to_string(),
            hostname_to_uuid: HashMap::new(),
            activity: Vec::new(),
            idle: Vec::new(),
            min_contention: default_min_contention(),
            decisions: Vec::new(),
            not_counted: Vec::new(),
        }
    }

    fn request() -> TimelineRequest {
        TimelineRequest {
            start: at(0),
            end: at(60),
            own_device: "me".to_string(),
            hostname_to_uuid: HashMap::new(),
            own_hostname: "my-host".to_string(),
            sliver_secs: None,
        }
    }

    fn browser_bucket(id: &str, events: Vec<Event>) -> (String, BucketEvents) {
        (
            "web.tab.current".to_string(),
            BucketEvents {
                bucket_id: id.to_string(),
                events,
            },
        )
    }

    #[test]
    fn a_page_is_counted_only_while_its_device_held_the_day() {
        // Half an hour of one page, of which the combined day gave this device ten minutes.
        let buckets = vec![browser_bucket(
            "aw-watcher-web-firefox_my-host",
            vec![event(0, 30, page("https://example.com/a"))],
        )];
        let mut mask = HashMap::new();
        mask.insert("me".to_string(), vec![(at(5), at(15))]);

        let out = detail_rows(&buckets, &input_for("me"), &mask, &request());
        let rows = out["web.tab.current"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["data"]["url"], json!("https://example.com/a"));
        assert_eq!(rows[0]["seconds"], json!(600.0));
    }

    #[test]
    fn a_device_that_never_counted_contributes_no_browsing() {
        // The point of the mask: a phone browsing while the desktop held the day is real
        // browsing that is not part of this day, and the per-device panel counts it anyway.
        let buckets = vec![browser_bucket(
            "aw-watcher-web-firefox_their-host-synced-from-their-host",
            vec![event(0, 30, page("https://example.com/a"))],
        )];
        let mut mask = HashMap::new();
        mask.insert("me".to_string(), vec![(at(0), at(60))]);

        let out = detail_rows(&buckets, &input_for("me"), &mask, &request());
        assert!(
            out.get("web.tab.current").is_none(),
            "a device with no counted time still reached the day: {out}"
        );
    }

    #[test]
    fn the_same_page_from_two_devices_becomes_one_row() {
        let mut input = input_for("me");
        input
            .hostname_to_uuid
            .insert("their-host".to_string(), "them".to_string());
        let buckets = vec![
            browser_bucket("aw-watcher-web-firefox", vec![event(0, 10, page("same"))]),
            browser_bucket(
                "aw-watcher-web-firefox-synced-from-their-host",
                vec![event(20, 25, page("same"))],
            ),
        ];
        let mut mask = HashMap::new();
        mask.insert("me".to_string(), vec![(at(0), at(10))]);
        mask.insert("them".to_string(), vec![(at(20), at(25))]);

        let out = detail_rows(&buckets, &input, &mask, &request());
        let rows = out["web.tab.current"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "two devices, two rows: {rows:?}");
        assert_eq!(rows[0]["seconds"], json!(900.0));
    }

    #[test]
    fn rows_come_back_longest_first() {
        let buckets = vec![browser_bucket(
            "aw-watcher-web-firefox",
            vec![
                event(0, 5, page("short")),
                event(5, 25, page("long")),
                event(25, 35, page("middle")),
            ],
        )];
        let mut mask = HashMap::new();
        mask.insert("me".to_string(), vec![(at(0), at(60))]);

        let out = detail_rows(&buckets, &input_for("me"), &mask, &request());
        let urls: Vec<&str> = out["web.tab.current"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["data"]["url"].as_str().unwrap())
            .collect();
        assert_eq!(urls, vec!["long", "middle", "short"]);
    }

    #[test]
    fn the_clock_is_not_masked() {
        // A stopwatch event is the owner starting and stopping a timer, not a measurement of a
        // device. Another device winning the foreground does not un-happen ten minutes of it.
        let mut data = Map::new();
        data.insert("label".to_string(), json!("reading"));
        data.insert("running".to_string(), json!(false));
        let buckets = vec![(
            "general.stopwatch".to_string(),
            BucketEvents {
                bucket_id: "aw-stopwatch".to_string(),
                events: vec![event(0, 10, data)],
            },
        )];
        // Nobody counted anything at all today.
        let out = detail_rows(&buckets, &input_for("me"), &HashMap::new(), &request());
        let rows = out["general.stopwatch"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["seconds"], json!(600.0));
    }

    #[test]
    fn the_clock_is_still_clipped_to_the_day() {
        let mut data = Map::new();
        data.insert("label".to_string(), json!("reading"));
        let buckets = vec![(
            "general.stopwatch".to_string(),
            BucketEvents {
                bucket_id: "aw-stopwatch".to_string(),
                // A timer running from before this day into it.
                events: vec![event(-30, 10, data)],
            },
        )];
        let out = detail_rows(&buckets, &input_for("me"), &HashMap::new(), &request());
        assert_eq!(out["general.stopwatch"][0]["seconds"], json!(600.0));
    }

    #[test]
    fn the_origin_tag_does_not_split_a_row() {
        // Two devices' copies of the same page differ by the tag this crate added, which is
        // bookkeeping about where an event came from and not about what the owner was doing.
        let mut theirs = page("same");
        theirs.insert(aw_combined::EVENT_ORIGIN_KEY.to_string(), json!("them"));
        let buckets = vec![
            browser_bucket("aw-watcher-web-firefox", vec![event(0, 10, page("same"))]),
            browser_bucket("aw-watcher-web-other", vec![event(10, 20, theirs)]),
        ];
        let mut mask = HashMap::new();
        mask.insert("me".to_string(), vec![(at(0), at(10))]);
        mask.insert("them".to_string(), vec![(at(10), at(20))]);

        let out = detail_rows(&buckets, &input_for("me"), &mask, &request());
        assert_eq!(out["web.tab.current"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn is_idle_only_matches_afk_status() {
        let mk = |v: Value| {
            let mut m = Map::new();
            m.insert(AFK_STATUS_KEY.to_string(), v);
            Event {
                id: None,
                timestamp: Utc::now(),
                duration: chrono::Duration::seconds(1),
                data: m,
            }
        };
        assert!(is_idle(&mk(json!("afk"))));
        assert!(!is_idle(&mk(json!("not-afk"))));
        assert!(!is_idle(&mk(json!(42))));
        assert!(!is_idle(&Event {
            id: None,
            timestamp: Utc::now(),
            duration: chrono::Duration::seconds(1),
            data: Map::new(),
        }));
    }
}
