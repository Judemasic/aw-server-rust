//! Desktop sync, as something a person can set up from a page.
//!
//! Sync on the desktop has always been a background process with no face: `aw-sync` on a timer,
//! configured by a line in a TOML file that a new user has no reason to know exists. The phone has
//! a screen for it -- pick a folder, turn it on, tap Sync Now -- and the desktop had nothing, so
//! "install the exe and start syncing" was not a thing anyone could do. This module is what the
//! page talks to.
//!
//! **It drives the `aw-sync` binary rather than linking it.** `aw-sync` already depends on
//! `aw-server` (it reads this crate's `dirs` and `config`), so a dependency back the other way
//! would be a cycle. Running the binary that ships beside us in the same install is not a
//! workaround for that so much as the honest shape: `aw-sync` is a program with its own profile
//! resolution, its own logging and its own exit codes, and reproducing those in-process would mean
//! two implementations of sync that can disagree.
//!
//! **Desktop only.** On Android the app owns sync: `SyncInterface.kt` copies the tree in and out
//! through SAF, which is the only way to reach a shared folder on Android 11+, and Rust cannot do
//! it. Everything here is compiled out there.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use aw_datastore::Datastore;

/// Where this device's sync folder is, in the datastore's key-value store.
///
/// **Deliberately not under `settings.`**, which is the prefix the settings API namespaces into
/// and the prefix settings sync carries between devices. A sync folder is the one setting that
/// must *not* travel: it names a path on this machine, and a phone that adopted a PC's
/// `C:\Users\...` would be pointed at nothing. aw-android draws the same line, and says so --
/// `SharedFolder.kt` keeps the sync folder URI out of shared settings on purpose (**R28**).
const SYNC_DIR_KEY: &str = "sync.dir";

/// Whether the background pass runs. Device-local for the same reason as the folder: one machine
/// being paused is not a statement about any other.
const SYNC_ENABLED_KEY: &str = "sync.enabled";

/// How often the background pass runs.
///
/// Fifteen minutes, matching what aw-android's scheduler uses, so two devices sharing a folder
/// converge on the same rhythm rather than one of them holding data twice as long as the other.
const SYNC_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// How far below the sync root a peer's database may sit.
///
/// Two, for the reason `aw-sync`'s own `find_remotes` gives: a desktop writes
/// `<root>/<device id>/test.db` and an Android device writes
/// `<root>/<hostname>/<device id>/test.db`. This is a *reporting* copy of that walk -- the page
/// shows what is in the folder before any sync has run, which is the difference between "your
/// phone is not sharing yet" and "sync is broken", and a person setting this up for the first
/// time needs to be able to tell those apart.
const MAX_PEER_DEPTH: usize = 2;

/// The default folder, when nothing has been chosen.
///
/// Under `Documents/`, not at the top of the home directory: the home directory belongs to the
/// person, not to the applications running on it, and this is a folder its owner has to open,
/// point Syncthing at, and still recognise a year from now. The *name* matches what aw-android
/// uses, which is what lets a phone and a PC meet without either being reconfigured -- Syncthing
/// pairs folders by what the people setting them up agreed to call them.
pub fn default_sync_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Documents")
        .join("ActivityWatch-sync")
}

/// The folder this device syncs through: what was chosen, or the default.
pub fn sync_dir(ds: &Datastore) -> PathBuf {
    match ds.get_key_value(SYNC_DIR_KEY) {
        Ok(raw) => match serde_json::from_str::<String>(&raw) {
            Ok(s) if !s.trim().is_empty() => PathBuf::from(s),
            // Stored by hand, or by a version that wrote a bare string. Take it as it came rather
            // than silently falling back to a different folder than the one the owner named.
            _ if !raw.trim().is_empty() => PathBuf::from(raw.trim()),
            _ => default_sync_dir(),
        },
        Err(_) => default_sync_dir(),
    }
}

/// Whether the background pass is on. Off until somebody turns it on: this writes a device's
/// activity into a folder other machines read, and that is not a thing to start by default.
pub fn sync_enabled(ds: &Datastore) -> bool {
    matches!(
        ds.get_key_value(SYNC_ENABLED_KEY).ok().as_deref(),
        Some("true")
    )
}

/// What one peer left in the shared folder.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Peer {
    /// The hostname directory, where there is one. Absent for a database written flat at the root
    /// of the folder, which is the layout a desktop's `setup_local_remote` used to produce.
    pub hostname: Option<String>,
    /// The device's uuid -- the name of the directory the database sits in, which is what
    /// `origin_from_db_path` reads it as.
    pub device_id: String,
    /// When that device last wrote. The single most useful number on the page: a peer whose
    /// database is four days old is a peer whose Syncthing has stopped, and nothing else on the
    /// screen would say so.
    pub last_modified: Option<DateTime<Utc>>,
    pub size_bytes: u64,
    /// True for this machine's own database, which is in the folder but is not a peer.
    pub is_own: bool,
}

/// Every *device* in the shared folder, this device's own included and flagged.
///
/// **One row per device, not one per file**, and that is not tidying -- a device genuinely has
/// several databases in the folder. Pulling from a peer has a side effect: `sync_run` calls
/// `setup_local_remote(<peer hostname>, our_device_id)`, which creates
/// `<peer hostname>/<our device id>/test.db`. So after syncing with two phones this PC's own id
/// appears four times: once flat, once under its own hostname, and once under each phone's.
/// `SyncInterface.kt` documents the same effect from the Android side, where it was observed
/// producing four directories for two devices. Harmless for sync -- every file still has exactly
/// one writer, the id in its path -- but a page that listed files would tell its owner they have
/// four devices, and the count would grow as the square of the real one.
///
/// The surviving row is the most recently written, which is also the one that answers the question
/// the page is asking: *when did this device last put something here*.
///
/// Errors are swallowed per entry rather than failing the whole scan: this reads a directory that
/// other machines are writing into while it reads, so a half-copied file or one Syncthing has
/// locked is an ordinary event and must not take the page down.
pub fn peers(dir: &Path, own_device_id: &str) -> Vec<Peer> {
    let mut found = Vec::new();
    collect_peers(dir, MAX_PEER_DEPTH, &[], own_device_id, &mut found);

    let mut by_device: HashMap<String, Peer> = HashMap::new();
    for peer in found {
        match by_device.get_mut(&peer.device_id) {
            // Newest wins. Sizes are *not* summed: the copies are copies, and adding them would
            // report a device as several times larger than anything it ever wrote.
            Some(existing) if peer.last_modified > existing.last_modified => *existing = peer,
            Some(_) => {}
            None => {
                by_device.insert(peer.device_id.clone(), peer);
            }
        }
    }

    let mut out: Vec<Peer> = by_device.into_values().collect();
    out.sort_by(|a, b| {
        b.is_own
            .cmp(&a.is_own)
            .then_with(|| a.hostname.cmp(&b.hostname))
            .then_with(|| a.device_id.cmp(&b.device_id))
    });
    out
}

fn collect_peers(
    dir: &Path,
    depth: usize,
    trail: &[String],
    own_device_id: &str,
    out: &mut Vec<Peer>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if path.is_dir() {
            // Syncthing's own bookkeeping, and the copies it leaves when a folder is removed.
            // Walking into them would report `.stfolder` as a device.
            if name.starts_with('.') {
                continue;
            }
            if depth > 0 {
                let mut next = trail.to_vec();
                next.push(name);
                collect_peers(&path, depth - 1, &next, own_device_id, out);
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("db") {
            // The directory holding a database *is* the device that wrote it. One lying loose at
            // the root belongs to nobody and is not reported as a device -- the same rule
            // `find_remotes` applies when deciding what to sync.
            let Some(device_id) = trail.last() else {
                continue;
            };
            let meta = entry.metadata().ok();
            out.push(Peer {
                hostname: if trail.len() > 1 {
                    Some(trail[trail.len() - 2].clone())
                } else {
                    None
                },
                device_id: device_id.clone(),
                last_modified: meta
                    .as_ref()
                    .and_then(|m| m.modified().ok())
                    .map(DateTime::<Utc>::from),
                size_bytes: meta.as_ref().map(|m| m.len()).unwrap_or(0),
                is_own: device_id == own_device_id,
            });
        }
    }
}

/// The outcome of one pass, kept so the page can say what happened without running another.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LastRun {
    pub at: DateTime<Utc>,
    pub ok: bool,
    /// One line, for a person: either what it did or why it could not.
    pub message: String,
    /// Whether a person pressed the button or the timer came round. A failure the owner is
    /// watching for deserves different treatment from one at 3am, and the page can say which.
    pub manual: bool,
}

static LAST_RUN: Mutex<Option<LastRun>> = Mutex::new(None);

/// Held for the duration of a pass, so two of them cannot run at once. A person pressing the
/// button while the timer fires would otherwise have two processes writing the same database.
static RUNNING: Mutex<()> = Mutex::new(());

pub fn last_run() -> Option<LastRun> {
    LAST_RUN.lock().ok().and_then(|g| g.clone())
}

fn record(run: LastRun) {
    if let Ok(mut guard) = LAST_RUN.lock() {
        *guard = Some(run);
    }
}

/// Where `aw-sync` is, if it is anywhere we can see.
///
/// Beside this executable is the answer in every shipped layout -- the installer puts the whole
/// suite in one directory and aw-qt discovers modules by looking there. A dev build is the same
/// shape, `target/<profile>/`, so this works without a special case for it. Returning `None` is a
/// real answer the page shows, rather than an error: a server built without the rest of the suite
/// beside it genuinely cannot sync, and saying so beats a spawn failure.
pub fn aw_sync_binary() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let name = if cfg!(windows) {
        "aw-sync.exe"
    } else {
        "aw-sync"
    };
    let candidate = dir.join(name);
    candidate.is_file().then_some(candidate)
}

/// Run one sync pass, start to finish, and remember how it went.
///
/// Blocking on purpose. A pass over a day's events takes well under a second against a local
/// server, and the page's button is a thing a person is *watching*: handing back a job id they
/// then have to poll would be more machinery and less information.
pub fn run_once(ds: &Datastore, profile: &str, manual: bool) -> LastRun {
    let started = Utc::now();
    let finish = |ok: bool, message: String| {
        let run = LastRun {
            at: Utc::now(),
            ok,
            message,
            manual,
        };
        record(run.clone());
        run
    };

    let Ok(_guard) = RUNNING.try_lock() else {
        // Not a failure: the thing the caller asked for is already happening.
        return LastRun {
            at: started,
            ok: true,
            message: "A sync was already running; this request joined it.".to_string(),
            manual,
        };
    };

    let Some(binary) = aw_sync_binary() else {
        return finish(
            false,
            "aw-sync is not installed beside this server, so this build cannot sync.".to_string(),
        );
    };

    let dir = sync_dir(ds);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return finish(false, format!("Could not create {}: {e}", dir.display()));
    }

    let output = std::process::Command::new(&binary)
        .arg("--profile")
        .arg(profile)
        .arg("--sync-dir")
        .arg(&dir)
        .arg("sync")
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            finish(true, summarise(&stderr))
        }
        Ok(out) => {
            // aw-sync logs to stderr, so a failure's reason is there rather than in stdout.
            let stderr = String::from_utf8_lossy(&out.stderr);
            let last = stderr
                .lines()
                .filter(|l| !l.trim().is_empty())
                .next_back()
                .unwrap_or("aw-sync failed with no output")
                .to_string();
            finish(false, last)
        }
        Err(e) => finish(false, format!("Could not start aw-sync: {e}")),
    }
}

/// Turn a pass's log into the one sentence a page should show.
///
/// Counting the events it moved, rather than quoting the last line, because the last line of a
/// successful run is bookkeeping about closing a database and says nothing a person wants. A run
/// that moved nothing says so plainly: "up to date" is the most common outcome and reads as
/// success, where an empty message reads as a failure that forgot to explain itself.
fn summarise(log: &str) -> String {
    let mut events = 0u64;
    let mut buckets = 0u32;
    for line in log.lines() {
        if let Some(rest) = line.split("Synced ").nth(1) {
            if let Some(count) = rest.split_whitespace().next() {
                if let Ok(n) = count.parse::<u64>() {
                    if n > 0 {
                        events += n;
                        buckets += 1;
                    }
                }
            }
        }
    }
    if events == 0 {
        "Already up to date.".to_string()
    } else {
        format!(
            "Synced {events} event{} across {buckets} bucket{}.",
            if events == 1 { "" } else { "s" },
            if buckets == 1 { "" } else { "s" }
        )
    }
}

/// Store the folder this device syncs through.
///
/// The path is taken as given rather than normalised into something prettier: a person who typed a
/// path should see the path they typed when they come back to the page, and a folder on a network
/// share or behind a junction is not ours to rewrite. It is created, though -- a folder named but
/// not existing is the single most likely way this is set up wrong, and Syncthing cannot be
/// pointed at a directory that is not there.
pub fn set_sync_dir(ds: &Datastore, dir: &str) -> Result<PathBuf, String> {
    let trimmed = dir.trim();
    if trimmed.is_empty() {
        return Err("A sync folder needs a path.".to_string());
    }
    let path = PathBuf::from(trimmed);
    if !path.is_absolute() {
        // A relative path means something different depending on where the server was started
        // from, which is not a thing a person setting this up can reason about.
        return Err("The sync folder has to be a full path.".to_string());
    }
    std::fs::create_dir_all(&path).map_err(|e| format!("Could not create {trimmed}: {e}"))?;
    ds.set_key_value(SYNC_DIR_KEY, &serde_json::to_string(trimmed).unwrap())
        .map_err(|e| format!("Could not save the sync folder: {e:?}"))?;
    Ok(path)
}

/// Turn the background pass on or off.
pub fn set_sync_enabled(ds: &Datastore, enabled: bool) -> Result<(), String> {
    ds.set_key_value(SYNC_ENABLED_KEY, if enabled { "true" } else { "false" })
        .map_err(|e| format!("Could not save the sync setting: {e:?}"))
}

/// Run a pass every [`SYNC_INTERVAL`] for as long as sync is switched on.
///
/// The switch is read at the top of each round rather than captured when the thread starts, so
/// turning sync off from the page stops it without a restart -- and turning it on starts it,
/// because the thread keeps waiting rather than exiting.
///
/// Deliberately *not* a first pass on startup. A machine that has just booted is a machine whose
/// clock, network and Syncthing may all still be settling, and an immediate pass would be the one
/// most likely to fail and the least likely to be noticed. The page's button is there for anyone
/// who does not want to wait.
#[cfg(not(target_os = "android"))]
pub fn spawn_daemon(ds: Datastore, profile: String) {
    std::thread::spawn(move || loop {
        std::thread::sleep(SYNC_INTERVAL);
        if !sync_enabled(&ds) {
            continue;
        }
        let run = run_once(&ds, &profile, false);
        if run.ok {
            log::info!("Scheduled sync: {}", run.message);
        } else {
            log::warn!("Scheduled sync failed: {}", run.message);
        }
    });
}

/// Seconds since a file was last written, for the page's "how stale is this peer" figure.
#[allow(dead_code)]
fn age_secs(modified: SystemTime) -> u64 {
    SystemTime::now()
        .duration_since(modified)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aw-syncsetup-{}-{}-{}",
            tag,
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"x").unwrap();
    }

    /// Backdate a file's mtime, so "newest wins" can be tested rather than raced.
    ///
    /// Done through the file handle rather than by pulling in the `filetime` crate: one test
    /// does not justify a dependency, and `File::set_modified` has been stable since 1.75.
    fn filetime_set(path: &Path, when: SystemTime) {
        let f = fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(when).unwrap();
    }

    #[test]
    fn finds_a_desktop_and_a_phone_in_the_same_folder() {
        // The two layouts that actually appear in a shared folder: a desktop writes flat, an
        // Android device one level deeper under its hostname.
        let root = scratch("both");
        touch(&root.join("desktop-uuid").join("test.db"));
        touch(
            &root
                .join("jude_s_s25_ultra")
                .join("phone-uuid")
                .join("test.db"),
        );

        let found = peers(&root, "nobody");
        let ids: Vec<&str> = found.iter().map(|p| p.device_id.as_str()).collect();
        assert!(ids.contains(&"desktop-uuid"), "{found:?}");
        assert!(ids.contains(&"phone-uuid"), "{found:?}");

        let phone = found.iter().find(|p| p.device_id == "phone-uuid").unwrap();
        assert_eq!(phone.hostname.as_deref(), Some("jude_s_s25_ultra"));
        let desktop = found
            .iter()
            .find(|p| p.device_id == "desktop-uuid")
            .unwrap();
        assert_eq!(desktop.hostname, None);

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn one_device_is_one_row_however_many_copies_it_left() {
        // The owner's real folder, after this PC had synced with two phones: its own id sits
        // flat at the root, under its own hostname, and under each phone's hostname -- because
        // pulling from a peer calls `setup_local_remote(<peer hostname>, our id)`. Four files,
        // one device. Listing files would have told them they own four computers.
        let root = scratch("copies");
        for parent in [
            root.join("me"),
            root.join("Judes-Desktop").join("me"),
            root.join("jude_s_s25_ultra").join("me"),
            root.join("jude_s_tab_s10_fe").join("me"),
        ] {
            touch(&parent.join("test.db"));
        }
        touch(&root.join("jude_s_s25_ultra").join("phone").join("test.db"));

        let found = peers(&root, "me");
        assert_eq!(found.len(), 2, "one row per device, got {found:?}");
        assert!(found[0].is_own);
        assert_eq!(found[0].device_id, "me");
        assert_eq!(found[1].device_id, "phone");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_surviving_row_is_the_newest_write() {
        // Which is the one that answers what the page asks: when did this device last put
        // something here.
        let root = scratch("newest");
        let old = root.join("stale-host").join("them").join("test.db");
        let new = root.join("live-host").join("them").join("test.db");
        touch(&old);
        touch(&new);
        // Push the two apart, since two writes in the same millisecond would make this a
        // coin toss rather than a test.
        let long_ago = SystemTime::now() - Duration::from_secs(60 * 60 * 24);
        filetime_set(&old, long_ago);

        let found = peers(&root, "me");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].hostname.as_deref(), Some("live-host"));

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn this_device_is_marked_rather_than_hidden() {
        // It is in the folder and the page should show it -- seeing your own database appear is
        // how you know your half is working -- but it is not a peer.
        let root = scratch("own");
        touch(&root.join("me").join("test.db"));
        touch(&root.join("them").join("test.db"));

        let found = peers(&root, "me");
        assert_eq!(found.len(), 2);
        assert!(found[0].is_own, "own device should sort first: {found:?}");
        assert_eq!(found[0].device_id, "me");
        assert!(!found[1].is_own);

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn syncthings_own_folders_are_not_devices() {
        // `.stfolder` and the `.stfolder.removed-*` copies it leaves behind are bookkeeping.
        let root = scratch("stfolder");
        touch(&root.join(".stfolder").join("marker.db"));
        touch(&root.join(".stfolder.removed-20260902").join("x.db"));
        touch(&root.join("them").join("test.db"));

        let found = peers(&root, "me");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].device_id, "them");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_database_loose_at_the_root_belongs_to_nobody() {
        let root = scratch("loose");
        touch(&root.join("stray.db"));

        assert!(peers(&root, "me").is_empty());

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_folder_that_is_not_there_is_no_peers_rather_than_an_error() {
        // What the page shows before anything has been set up, which must not be a crash.
        assert!(peers(Path::new("/definitely/not/here/at/all"), "me").is_empty());
    }

    #[test]
    fn the_default_folder_is_not_the_top_of_the_home_directory() {
        let dir = default_sync_dir();
        assert!(dir.ends_with("ActivityWatch-sync"));
        assert_eq!(
            dir.parent().and_then(|p| p.file_name()),
            Some(std::ffi::OsStr::new("Documents"))
        );
    }

    #[test]
    fn a_summary_counts_what_moved() {
        let log = "\
[INFO] Syncing bucket 'aw-watcher-window'
[INFO]   = Synced 184 new events
[INFO] Syncing bucket 'aw-watcher-afk'
[INFO]   = Synced 6 new events, tagged origin abc
[INFO] DB Worker thread finished";
        assert_eq!(summarise(log), "Synced 190 events across 2 buckets.");
    }

    #[test]
    fn a_pass_that_moved_nothing_says_so() {
        // The most common outcome, and it has to read as success rather than as a blank.
        assert_eq!(
            summarise("[INFO] Pushing local data"),
            "Already up to date."
        );
        assert_eq!(summarise("   = Synced 0 new events"), "Already up to date.");
    }

    #[test]
    fn one_event_is_not_pluralised() {
        assert_eq!(
            summarise("= Synced 1 new events"),
            "Synced 1 event across 1 bucket."
        );
    }

    #[test]
    fn a_relative_folder_is_refused() {
        // It would mean a different directory depending on where the server was started.
        let ds = Datastore::new_in_memory(false);
        let err = set_sync_dir(&ds, "ActivityWatch-sync").unwrap_err();
        assert!(err.contains("full path"), "{err}");
        assert!(set_sync_dir(&ds, "   ").is_err());
    }

    #[test]
    fn a_chosen_folder_comes_back_as_chosen() {
        let ds = Datastore::new_in_memory(false);
        let root = scratch("chosen");
        set_sync_dir(&ds, root.to_str().unwrap()).unwrap();
        assert_eq!(sync_dir(&ds), root);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn sync_is_off_until_somebody_turns_it_on() {
        // This writes a device's activity into a folder other machines read. Not a default.
        let ds = Datastore::new_in_memory(false);
        assert!(!sync_enabled(&ds));
        set_sync_enabled(&ds, true).unwrap();
        assert!(sync_enabled(&ds));
        set_sync_enabled(&ds, false).unwrap();
        assert!(!sync_enabled(&ds));
    }

    #[test]
    fn an_unset_folder_is_the_default_one() {
        let ds = Datastore::new_in_memory(false);
        assert_eq!(sync_dir(&ds), default_sync_dir());
    }
}
