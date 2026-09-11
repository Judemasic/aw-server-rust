//! The desktop's half of the shared folder: settings and decisions, not just events.
//!
//! `aw-sync` moves *events*. It has never touched the shared folder's other two files, because
//! until now the only devices that had them were Android ones, where Kotlin does this work
//! (`SharedStore.kt`, `SharedSettings.kt`, `SharedFolder.kt`). The consequence on a mixed set of
//! devices was quiet and bad: two phones agreed about categories, not-counted rules and resolved
//! overlaps, and the PC agreed with nobody. Events synced; the rules for interpreting them did
//! not, so the same day totalled differently depending on which device was asked.
//!
//! This is the missing half. The decision logic is in `aw_combined::settings`, ported from the
//! Kotlin and tested there; everything here is files and the datastore.
//!
//! **R20: every file has exactly one writer.** This device appends only to
//! `devices/<our uuid>/`, and reads everyone else's. That is what keeps Syncthing from producing
//! `.sync-conflict-*` copies, and it is not negotiable -- a second writer would be discovered as
//! data loss weeks later.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use aw_combined::{
    effective_settings, is_shared_setting_key, parse_line, parse_settings, plan_settings_sync,
    setting_to_json_line, SharedRecord,
};
use aw_datastore::Datastore;

use crate::combined::{store_record, DECISION_KEY_PREFIX};

const DEVICES_DIR: &str = "devices";
const SETTINGS_FILE: &str = "settings.jsonl";
const DECISIONS_FILE: &str = "decisions.jsonl";
const META_FILE: &str = "meta.json";
const VERSION_FILE: &str = "VERSION";

/// The shared-folder schema this build speaks. Must match `SHARED_SCHEMA_VERSION` in the Kotlin.
const SCHEMA_VERSION: u32 = 1;

/// aw-webui's settings live under this prefix in the datastore; the shared file names them without
/// it. `endpoints::settings` owns the mapping, and this has to agree with it.
const SETTINGS_PREFIX: &str = "settings.";

/// What this device last agreed with its peers, per key.
///
/// **Device-local**, and the piece that makes the whole algorithm work: "the owner changed this
/// here" and "a peer changed it and we have not applied it yet" both look like `local != merged`,
/// and only a comparison against what was last agreed tells them apart. On Android this lives in
/// `AWPreferences`; here it is a datastore key outside the `settings.` namespace, so it is never
/// itself mistaken for a setting and never shared.
const APPLIED_KEY: &str = "sync.settings.applied";

/// What one pass did, for the page and the log.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SharedSyncReport {
    /// Settings published from this device.
    pub settings_published: usize,
    /// Settings taken from a peer and written here.
    pub settings_applied: usize,
    /// Decisions and tombstones imported from peers.
    pub decisions_imported: usize,
    /// Decisions and tombstones this device put into the folder.
    pub decisions_published: usize,
    /// Things that went wrong without stopping the pass -- an unreadable peer file, a folder we
    /// could not write. Reported rather than thrown: one bad peer must not stop the others.
    pub problems: Vec<String>,
}

impl SharedSyncReport {
    pub fn moved_nothing(&self) -> bool {
        self.settings_published == 0
            && self.settings_applied == 0
            && self.decisions_imported == 0
            && self.decisions_published == 0
    }

    /// One sentence for a person, or `None` when there is nothing worth saying.
    pub fn summary(&self) -> Option<String> {
        if self.moved_nothing() {
            return None;
        }
        let mut parts = Vec::new();
        if self.settings_applied > 0 {
            parts.push(format!(
                "took {} setting(s) from peers",
                self.settings_applied
            ));
        }
        if self.settings_published > 0 {
            parts.push(format!("shared {} setting(s)", self.settings_published));
        }
        if self.decisions_imported > 0 {
            parts.push(format!("imported {} decision(s)", self.decisions_imported));
        }
        if self.decisions_published > 0 {
            parts.push(format!("shared {} decision(s)", self.decisions_published));
        }
        Some(parts.join(", "))
    }
}

/// Whether the folder's `VERSION` is one this build understands.
///
/// A folder with no `VERSION` is a folder nothing has claimed yet -- fine, and we write ours. A
/// folder claiming a *newer* schema is refused: appending lines a newer reader will interpret
/// differently is how two devices quietly disagree, and stopping is the honest answer. An older
/// one we accept, since every version so far has been additive.
fn check_version(root: &Path) -> Result<(), String> {
    let path = root.join(VERSION_FILE);
    match fs::read_to_string(&path) {
        Ok(raw) => match raw.trim().parse::<u32>() {
            Ok(v) if v <= SCHEMA_VERSION => Ok(()),
            Ok(v) => Err(format!(
                "This shared folder is version {v}; this build speaks {SCHEMA_VERSION}. \
                 Update ActivityWatch on this computer before syncing."
            )),
            Err(_) => Err(format!("{} is not a version number", path.display())),
        },
        Err(_) => {
            // Nothing has claimed the folder. Claim it, and do not treat a write failure as fatal:
            // a read-only folder can still be read.
            let _ = fs::write(&path, format!("{SCHEMA_VERSION}\n"));
            Ok(())
        }
    }
}

fn devices_dir(root: &Path) -> PathBuf {
    root.join(DEVICES_DIR)
}

fn our_dir(root: &Path, own_device_id: &str) -> PathBuf {
    devices_dir(root).join(own_device_id)
}

/// Every device directory in the folder, ours included.
fn device_dirs(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(devices_dir(root)) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.push(path);
            }
        }
    }
    // Sorted so a pass reads devices in the same order every time (**R18**).
    out.sort();
    out
}

/// Read one file, treating "not there" as empty.
///
/// A peer that has never written settings has no `settings.jsonl`, which is a normal state and not
/// an error. A file we cannot read *is* worth reporting, since it usually means a permission
/// problem that will not fix itself.
fn read_shared_file(path: &Path, problems: &mut Vec<String>) -> String {
    match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            problems.push(format!("Could not read {}: {e}", path.display()));
            String::new()
        }
    }
}

/// Append lines to one of our own files, creating it and its directory if needed.
fn append_lines(path: &Path, lines: &[String]) -> Result<(), String> {
    if lines.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Could not create {}: {e}", parent.display()))?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("Could not open {}: {e}", path.display()))?;
    for line in lines {
        writeln!(file, "{line}").map_err(|e| format!("Could not write {}: {e}", path.display()))?;
    }
    Ok(())
}

/// This device's shared settings, as the datastore holds them.
fn local_settings(ds: &Datastore) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if let Ok(stored) = ds.get_key_values(&format!("{SETTINGS_PREFIX}%")) {
        for (key, value) in stored {
            if let Some(bare) = key.strip_prefix(SETTINGS_PREFIX) {
                if is_shared_setting_key(bare) {
                    out.insert(bare.to_string(), value);
                }
            }
        }
    }
    out
}

fn read_applied(ds: &Datastore) -> BTreeMap<String, String> {
    ds.get_key_value(APPLIED_KEY)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Carry settings both ways.
fn sync_settings(ds: &Datastore, root: &Path, own_device_id: &str, report: &mut SharedSyncReport) {
    let mut all = Vec::new();
    for dir in device_dirs(root) {
        let text = read_shared_file(&dir.join(SETTINGS_FILE), &mut report.problems);
        all.extend(parse_settings(&text));
    }
    let merged = effective_settings(&all);
    let local = local_settings(ds);
    let applied = read_applied(ds);
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

    // Nothing agreed yet means this device is joining, and a joining device accepts before it
    // publishes -- see plan_settings_sync. Read from the datastore rather than tracked in memory,
    // so a restarted server does not think it is joining all over again.
    let joining = applied.is_empty();
    let plan = plan_settings_sync(&local, &merged, &applied, joining, &now, own_device_id);

    // Apply first, publish second. If the process dies between the two, the worst case is that we
    // accepted a peer's value and have not yet said so -- next pass republishes. The other order
    // could leave us having announced a value we do not hold.
    for (key, value) in &plan.values_to_apply {
        match ds.set_key_value(&format!("{SETTINGS_PREFIX}{key}"), value) {
            Ok(()) => report.settings_applied += 1,
            Err(e) => report
                .problems
                .push(format!("Could not apply setting {key}: {e:?}")),
        }
    }

    let lines: Vec<String> = plan
        .lines_to_append
        .iter()
        .map(setting_to_json_line)
        .collect();
    let published = lines.len();
    match append_lines(&our_dir(root, own_device_id).join(SETTINGS_FILE), &lines) {
        Ok(()) => report.settings_published += published,
        Err(e) => {
            report.problems.push(e);
            // Do not record as agreed what we failed to publish: next pass must try again.
            return;
        }
    }

    if let Ok(encoded) = serde_json::to_string(&plan.applied) {
        if let Err(e) = ds.set_key_value(APPLIED_KEY, &encoded) {
            report
                .problems
                .push(format!("Could not remember the settings state: {e:?}"));
        }
    }
}

/// The id and author of a decision or tombstone line, if it is one.
fn record_id_and_author(line: &str) -> Option<(String, String)> {
    match parse_line(line)? {
        SharedRecord::Decision(d) => Some((d.id, d.created_by)),
        SharedRecord::Tombstone(t) => Some((t.id, t.created_by)),
        SharedRecord::Unknown(_) => None,
    }
}

fn record_id(line: &str) -> Option<String> {
    record_id_and_author(line).map(|(id, _)| id)
}

/// Carry decisions and tombstones both ways.
///
/// Decisions are append-only and identified by a ULID, so this needs no memory of what it did last
/// time -- unlike settings, where "did the owner change it here, or has a peer's change not
/// arrived yet?" can only be answered by remembering. Here the question is only ever *is this id
/// present*, which both sides can answer from what they hold right now. Running it twice in a row
/// publishes and imports nothing the second time.
///
/// **Only records this device authored are published** (**R20**: one writer per file). A peer's
/// decision that reached us through the datastore is never written into our file -- it already has
/// a home, in the file its author owns, and copying it would put the same record in two files that
/// two devices then keep appending to. `planDecisionSync` in `SharedStore.kt` draws the same line,
/// and this originally did not: it republished every record it had imported, which on the machine
/// this was written for meant 118 of the phones' decisions copied into the desktop's file on the
/// very first pass.
fn sync_decisions(ds: &Datastore, root: &Path, own_device_id: &str, report: &mut SharedSyncReport) {
    let ours_path = our_dir(root, own_device_id).join(DECISIONS_FILE);

    // Every id anywhere in the folder, ours included. Checking only our own file would republish a
    // record we authored but that reached the folder through some other route.
    let mut shared_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut peer_lines: Vec<String> = Vec::new();
    for dir in device_dirs(root) {
        let is_ours = dir.file_name().and_then(|n| n.to_str()) == Some(own_device_id);
        let text = read_shared_file(&dir.join(DECISIONS_FILE), &mut report.problems);
        for line in text.lines() {
            let trimmed = line.trim();
            // A line we cannot read is left where it is: not published, not imported, not deleted.
            // It is either a newer build's record or a half-written transfer, and both outlast us.
            let Some(id) = record_id(trimmed) else {
                continue;
            };
            shared_ids.insert(id);
            if !is_ours {
                peer_lines.push(trimmed.to_string());
            }
        }
    }

    // Pull. `store_record` is idempotent -- the same record written twice overwrites itself -- so
    // this is cheap and safe to repeat, but only records we do not already hold are counted.
    for line in &peer_lines {
        let Some(id) = record_id(line) else { continue };
        let held = ds
            .get_key_value(&format!("{DECISION_KEY_PREFIX}{id}"))
            .is_ok();
        match store_record(ds, line) {
            Ok(_) if !held => report.decisions_imported += 1,
            Ok(_) => {}
            Err(e) => report.problems.push(e),
        }
    }

    // Push: records *we authored* that the folder does not have yet.
    let stored = match ds.get_key_values(&format!("{DECISION_KEY_PREFIX}%")) {
        Ok(stored) => stored,
        Err(e) => {
            report
                .problems
                .push(format!("Could not read decisions: {e:?}"));
            return;
        }
    };
    let mut keys: Vec<&String> = stored.keys().collect();
    keys.sort(); // stable output (**R18**)
    let mut to_append = Vec::new();
    for key in keys {
        let line = &stored[key];
        if let Some((id, author)) = record_id_and_author(line) {
            if author == own_device_id && !shared_ids.contains(&id) {
                to_append.push(line.clone());
            }
        }
    }
    let count = to_append.len();
    match append_lines(&ours_path, &to_append) {
        Ok(()) => report.decisions_published += count,
        Err(e) => report.problems.push(e),
    }
}

/// Announce this device in the folder, so the others can name it.
///
/// Replaced wholesale rather than merged: we are its only writer (**R20**).
fn write_meta(root: &Path, own_device_id: &str, hostname: &str, report: &mut SharedSyncReport) {
    let meta = serde_json::json!({
        "device_uuid": own_device_id,
        "display_name": hostname,
        // What this device *is*, which is how a phone's UI can say "your PC" rather than a uuid.
        "role": "desktop",
        "platform": std::env::consts::OS,
        "app_version": crate::version::version_string(),
        "last_seen": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "schema_version": SCHEMA_VERSION,
    });
    let dir = our_dir(root, own_device_id);
    if let Err(e) = fs::create_dir_all(&dir) {
        report
            .problems
            .push(format!("Could not create {}: {e}", dir.display()));
        return;
    }
    if let Err(e) = fs::write(
        dir.join(META_FILE),
        serde_json::to_string_pretty(&meta).unwrap_or_default(),
    ) {
        report
            .problems
            .push(format!("Could not write meta.json: {e}"));
    }
}

/// Carry settings and decisions between this device and the shared folder.
///
/// Runs after `aw-sync` has moved events, so a pass leaves the folder consistent: the events and
/// the rules for reading them arrive together rather than a quarter of an hour apart.
pub fn sync_shared_store(
    ds: &Datastore,
    root: &Path,
    own_device_id: &str,
    hostname: &str,
) -> Result<SharedSyncReport, String> {
    if !root.is_dir() {
        return Err(format!("{} is not a folder", root.display()));
    }
    check_version(root)?;

    let mut report = SharedSyncReport::default();
    write_meta(root, own_device_id, hostname, &mut report);
    sync_settings(ds, root, own_device_id, &mut report);
    sync_decisions(ds, root, own_device_id, &mut report);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aw-sharedstore-{}-{}-{}",
            tag,
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn peer_settings(root: &Path, uuid: &str, body: &str) {
        let dir = devices_dir(root).join(uuid);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(SETTINGS_FILE), body).unwrap();
    }

    fn our_settings_body(root: &Path, uuid: &str) -> String {
        fs::read_to_string(our_dir(root, uuid).join(SETTINGS_FILE)).unwrap_or_default()
    }

    #[test]
    fn a_peers_categories_arrive_in_the_datastore() {
        // The hole this module exists to close: a category set on the phone never reached the PC.
        let root = scratch("pull");
        let ds = Datastore::new_in_memory(false);
        peer_settings(
            &root,
            "phone",
            "{\"type\":\"setting\",\"key\":\"classes\",\"value\":\"[1,2]\",\
             \"updated_at\":\"2026-09-11T00:00:00Z\",\"updated_by\":\"phone\"}\n",
        );

        let report = sync_shared_store(&ds, &root, "desktop", "Judes-Desktop").unwrap();
        assert_eq!(report.settings_applied, 1, "{report:?}");
        assert_eq!(ds.get_key_value("settings.classes").unwrap(), "[1,2]");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn our_own_categories_reach_the_folder() {
        let root = scratch("push");
        let ds = Datastore::new_in_memory(false);
        ds.set_key_value("settings.classes", "[\"mine\"]").unwrap();

        let report = sync_shared_store(&ds, &root, "desktop", "Judes-Desktop").unwrap();
        assert_eq!(report.settings_published, 1, "{report:?}");
        let body = our_settings_body(&root, "desktop");
        assert!(body.contains("\"key\":\"classes\""), "{body}");
        assert!(body.contains("\"updated_by\":\"desktop\""), "{body}");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_second_pass_with_no_edits_says_nothing_twice() {
        // The file is append-only and shared. A pass that republished everything every fifteen
        // minutes would grow it without bound and make every peer re-read it.
        let root = scratch("idempotent");
        let ds = Datastore::new_in_memory(false);
        ds.set_key_value("settings.classes", "[\"mine\"]").unwrap();

        sync_shared_store(&ds, &root, "desktop", "host").unwrap();
        let after_first = our_settings_body(&root, "desktop");
        let second = sync_shared_store(&ds, &root, "desktop", "host").unwrap();

        assert_eq!(second.settings_published, 0, "{second:?}");
        assert_eq!(our_settings_body(&root, "desktop"), after_first);

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_device_local_setting_is_left_where_it_is() {
        let root = scratch("local");
        let ds = Datastore::new_in_memory(false);
        ds.set_key_value("settings.theme", "\"dark\"").unwrap();

        let report = sync_shared_store(&ds, &root, "desktop", "host").unwrap();
        assert_eq!(report.settings_published, 0);
        assert!(!our_settings_body(&root, "desktop").contains("theme"));

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_decision_from_a_peer_is_imported_but_not_republished() {
        // R20: one writer per file. A peer's decision already has a home in the file its author
        // owns, and copying it into ours would put the same record in two files that two devices
        // then keep appending to.
        let root = scratch("decisions");
        let ds = Datastore::new_in_memory(false);
        let line =
            "{\"type\":\"decision\",\"id\":\"01ABCDEF\",\"created_at\":\"2026-09-11T00:00:00Z\",\
                    \"created_by\":\"phone\",\"window\":{\"start\":\"2026-09-11T09:00:00Z\",\
                    \"end\":\"2026-09-11T09:10:00Z\"},\"signature\":{\"participants\":[]},\
                    \"resolution\":{\"outcome\":\"foreground\"}}";
        let dir = devices_dir(&root).join("phone");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(DECISIONS_FILE), format!("{line}\n")).unwrap();

        let report = sync_shared_store(&ds, &root, "desktop", "host").unwrap();
        assert_eq!(report.decisions_imported, 1, "{report:?}");
        assert_eq!(
            report.decisions_published, 0,
            "a peer's decision was copied into our file: {report:?}"
        );
        assert!(ds
            .get_key_value(&format!("{DECISION_KEY_PREFIX}01ABCDEF"))
            .is_ok());
        assert!(
            !our_dir(&root, "desktop").join(DECISIONS_FILE).exists(),
            "we should not have written a decisions file at all"
        );

        // And nothing is imported twice.
        let again = sync_shared_store(&ds, &root, "desktop", "host").unwrap();
        assert_eq!(again.decisions_imported, 0, "{again:?}");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn our_own_decision_is_published() {
        let root = scratch("ourdecision");
        let ds = Datastore::new_in_memory(false);
        let line =
            "{\"type\":\"decision\",\"id\":\"01OURS\",\"created_at\":\"2026-09-11T00:00:00Z\",\
                    \"created_by\":\"desktop\",\"window\":{\"start\":\"2026-09-11T09:00:00Z\",\
                    \"end\":\"2026-09-11T09:10:00Z\"},\"signature\":{\"participants\":[]},\
                    \"resolution\":{\"outcome\":\"foreground\"}}";
        store_record(&ds, line).unwrap();

        let report = sync_shared_store(&ds, &root, "desktop", "host").unwrap();
        assert_eq!(report.decisions_published, 1, "{report:?}");

        // Not a second time.
        let again = sync_shared_store(&ds, &root, "desktop", "host").unwrap();
        assert_eq!(again.decisions_published, 0, "{again:?}");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_newer_folder_is_refused_rather_than_appended_to() {
        // Appending lines a newer reader interprets differently is how two devices quietly
        // disagree. Stopping is the honest answer.
        let root = scratch("version");
        fs::write(root.join(VERSION_FILE), "99\n").unwrap();
        let ds = Datastore::new_in_memory(false);

        let err = sync_shared_store(&ds, &root, "desktop", "host").unwrap_err();
        assert!(err.contains("version 99"), "{err}");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_unclaimed_folder_is_claimed() {
        let root = scratch("claim");
        let ds = Datastore::new_in_memory(false);
        sync_shared_store(&ds, &root, "desktop", "host").unwrap();
        assert_eq!(
            fs::read_to_string(root.join(VERSION_FILE)).unwrap().trim(),
            SCHEMA_VERSION.to_string()
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn this_device_announces_itself() {
        // So a phone's UI can say "your PC" rather than 36 characters of hex.
        let root = scratch("meta");
        let ds = Datastore::new_in_memory(false);
        sync_shared_store(&ds, &root, "desktop-uuid", "Judes-Desktop").unwrap();

        let meta = fs::read_to_string(our_dir(&root, "desktop-uuid").join(META_FILE)).unwrap();
        assert!(
            meta.contains("\"display_name\": \"Judes-Desktop\""),
            "{meta}"
        );
        assert!(meta.contains("\"role\": \"desktop\""), "{meta}");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_folder_that_is_not_there_is_an_error_not_a_panic() {
        let ds = Datastore::new_in_memory(false);
        assert!(sync_shared_store(&ds, Path::new("/nope/nope"), "d", "h").is_err());
    }

    #[test]
    fn an_unreadable_peer_does_not_stop_the_others() {
        let root = scratch("badpeer");
        let ds = Datastore::new_in_memory(false);
        // A directory where a file should be: readable as an entry, not as a file.
        fs::create_dir_all(devices_dir(&root).join("broken").join(SETTINGS_FILE)).unwrap();
        peer_settings(
            &root,
            "good",
            "{\"type\":\"setting\",\"key\":\"classes\",\"value\":\"[3]\",\
             \"updated_at\":\"2026-09-11T00:00:00Z\",\"updated_by\":\"good\"}\n",
        );

        let report = sync_shared_store(&ds, &root, "desktop", "host").unwrap();
        assert_eq!(report.settings_applied, 1, "{report:?}");
        assert!(
            !report.problems.is_empty(),
            "the bad peer should be reported"
        );

        fs::remove_dir_all(&root).ok();
    }
}
