/// Basic syncing for ActivityWatch
/// Based on: https://github.com/ActivityWatch/aw-server/pull/50
///
/// This does not handle any direct peer interaction/connections/networking, it works as a "bring your own folder synchronizer".
///
/// It manages a sync-folder by syncing the aw-server datastore with a copy/staging datastore in the folder (one for each host).
/// The sync folder is then synced with remotes using Syncthing/Dropbox/whatever.
extern crate chrono;
extern crate reqwest;
extern crate serde_json;

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use aw_client_rust::blocking::AwClient;
use chrono::{DateTime, Duration, Utc};

use aw_datastore::{Datastore, DatastoreError};
use aw_models::{Bucket, Event};

#[cfg(feature = "cli")]
use clap::ValueEnum;

use crate::accessmethod::AccessMethod;

pub use aw_models::EVENT_ORIGIN_KEY;

#[derive(PartialEq, Eq, Copy, Clone)]
#[cfg_attr(feature = "cli", derive(ValueEnum))]
pub enum SyncMode {
    Push,
    Pull,
    Both,
}

#[derive(Debug)]
pub struct SyncSpec {
    /// Path of sync folder
    pub path: PathBuf,
    /// Path of sync db
    /// If None, will use all
    pub path_db: Option<PathBuf>,
    /// Bucket IDs to sync
    pub buckets: Option<Vec<String>>,
    /// Start of time range to sync
    pub start: Option<DateTime<Utc>>,
}

impl Default for SyncSpec {
    fn default() -> Self {
        // TODO: Better default path
        let path = Path::new("/tmp/aw-sync").to_path_buf();
        SyncSpec {
            path,
            path_db: None,
            buckets: None,
            start: None,
        }
    }
}

/// Performs a single sync pass
pub fn sync_run(
    client: &AwClient,
    sync_spec: &SyncSpec,
    mode: SyncMode,
) -> Result<(), Box<dyn Error>> {
    let info = client.get_info()?;

    // FIXME: Here it is assumed that the device_id for the local server is the one used by
    // aw-server-rust, which is not necessarily true (aw-server-python has seperate device_id).
    // Therefore, this may sometimes fail to pick up the correct local datastore.
    let device_id = info.device_id.as_str();

    // FIXME: Bad device_id assumption?
    let ds_localremote = setup_local_remote(sync_spec.path.as_path(), device_id)?;
    let remote_dbfiles = crate::util::find_remotes_nonlocal(
        sync_spec.path.as_path(),
        device_id,
        sync_spec.path_db.as_ref(),
    );

    // Log if remotes found
    // TODO: Only log remotes of interest
    if !remote_dbfiles.is_empty() {
        info!(
            "Found {} remote db files: {:?}",
            remote_dbfiles.len(),
            remote_dbfiles
        );
    }

    // TODO: Check for compatible remote db version before opening
    // Each remote is paired with the device it belongs to, because the only place that is
    // knowable is here, where the file's path is still in hand: the directory containing the
    // database is named after the device that wrote it (`origin_from_db_path`).
    let ds_remotes: Vec<(Option<String>, Datastore)> = remote_dbfiles
        .iter()
        .map(|p| {
            let origin = crate::util::origin_from_db_path(p);
            if origin.is_none() {
                warn!("Cannot tell which device {p:?} belongs to; importing it untagged");
            }
            (origin, create_datastore(p.as_path()))
        })
        .collect();

    if !ds_remotes.is_empty() {
        info!(
            "Found {} remote datastores: {:?}",
            ds_remotes.len(),
            ds_remotes
        );
    }

    // Pull
    if mode == SyncMode::Pull || mode == SyncMode::Both {
        info!("Pulling...");
        for (origin, ds_from) in &ds_remotes {
            sync_datastores(ds_from, client, false, origin.as_deref(), sync_spec);
        }
    }

    // Push local server buckets to sync folder
    if mode == SyncMode::Push || mode == SyncMode::Both {
        info!("Pushing...");
        sync_datastores(client, &ds_localremote, true, Some(device_id), sync_spec);
    }

    // Close open database connections
    for (_, ds_from) in &ds_remotes {
        ds_from.close();
    }
    ds_localremote.close();

    // Dropping also works to close the database connections, weirdly enough.
    // Probably because once the database is dropped, the thread will stop,
    // and then the Connection will be dropped, which closes the connection.
    std::mem::drop(ds_remotes);
    std::mem::drop(ds_localremote);

    // NOTE: Will fail if db connections not closed (as it will open them again)
    //list_buckets(&client, sync_spec.path.as_path());

    Ok(())
}

#[allow(dead_code)]
pub fn list_buckets(client: &AwClient) -> Result<(), Box<dyn Error>> {
    let sync_directory = crate::dirs::get_sync_dir().map_err(|_| "Could not get sync dir")?;
    let sync_directory = sync_directory.as_path();
    let info = client.get_info()?;

    // FIXME: Incorrect device_id assumption?
    let device_id = info.device_id.as_str();
    let ds_localremote = setup_local_remote(sync_directory, device_id)?;

    let remote_dbfiles = crate::util::find_remotes_nonlocal(sync_directory, device_id, None);
    info!("Found remotes: {:?}", remote_dbfiles);

    // TODO: Check for compatible remote db version before opening
    let ds_remotes: Vec<Datastore> = remote_dbfiles
        .iter()
        .map(|p| p.as_path())
        .map(create_datastore)
        .collect();

    log_buckets(client);
    log_buckets(&ds_localremote);
    for ds_from in &ds_remotes {
        log_buckets(ds_from);
    }

    Ok(())
}

fn setup_local_remote(path: &Path, device_id: &str) -> Result<Datastore, Box<dyn Error>> {
    // FIXME: Don't run twice if already exists
    fs::create_dir_all(path)?;

    let remotedir = path.join(device_id);
    fs::create_dir_all(&remotedir)?;

    let dbfile = remotedir.join("test.db");

    // Print a message if dbfile doesn't already exist
    if !dbfile.exists() {
        info!("Creating new database file: {}", dbfile.display());
    }

    let ds_localremote = create_datastore(&dbfile);
    Ok(ds_localremote)
}

pub fn create_datastore(path: &Path) -> Datastore {
    let pathstr = path.as_os_str().to_str().unwrap();
    Datastore::new(pathstr.to_string(), false)
}

/// Returns the sync-destination bucket for a given bucket, creates it if it doesn't exist.
fn get_or_create_sync_bucket(
    bucket_from: &Bucket,
    ds_to: &dyn AccessMethod,
    is_push: bool,
) -> Bucket {
    // On pull/import: derive the origin from $aw.sync.origin metadata (preferred) or the
    // hostname (legacy fallback for buckets that predate the metadata field).  On push-staging
    // the bucket keeps its original ID and we do NOT stamp $aw.sync.origin — staging copies
    // should not look like synced-from-remote buckets.
    let (new_id, sync_origin) = if is_push {
        (bucket_from.id.clone(), None)
    } else {
        let orig_bucketid = bucket_from.id.split("-synced-from-").next().unwrap();
        let fallback = serde_json::to_value(&bucket_from.hostname).unwrap();
        let origin = bucket_from
            .data
            .get("$aw.sync.origin")
            .unwrap_or(&fallback)
            .as_str()
            .unwrap()
            .to_string();
        (
            format!("{orig_bucketid}-synced-from-{origin}"),
            Some(origin),
        )
    };

    // The metadata this bucket should carry, derived fresh from the source every time.
    let mut bucket_want = bucket_from.clone();
    bucket_want.id = new_id.clone();
    // Only stamp $aw.sync.origin on pull/import.  The derived origin already handles
    // the legacy case: hostname is used when the source bucket has no metadata field.
    if let Some(origin) = sync_origin {
        bucket_want
            .data
            .insert("$aw.sync.origin".to_string(), serde_json::json!(origin));
    } else {
        // Push path: strip any stale $aw.sync.origin that bucket_from may carry
        // (e.g. if it was previously imported by a pull).  Staging copies must
        // never look like synced-from-remote buckets.
        bucket_want.data.remove("$aw.sync.origin");
    }

    match ds_to.get_bucket(new_id.as_str()) {
        Ok(bucket) => {
            // An existing bucket used to be returned untouched, which froze whatever
            // metadata it had the first time it was written.  A device that corrected its
            // own name could then never correct what it had already staged, and its peers
            // went on reading the stale name as the origin of the data.  So refresh the
            // descriptive fields from the source whenever they have drifted.  Events,
            // creation time and id are not involved.
            if bucket_metadata_differs(&bucket, &bucket_want) {
                info!(
                    "Refreshing stale metadata on bucket {} (hostname {:?} -> {:?})",
                    new_id, bucket.hostname, bucket_want.hostname
                );
                match ds_to.update_bucket(&bucket_want) {
                    Ok(()) => match ds_to.get_bucket(new_id.as_str()) {
                        Ok(bucket) => bucket,
                        Err(e) => panic!("{e:?}"),
                    },
                    Err(e) => {
                        // A refused refresh is cosmetic: the bucket still holds the right
                        // events under the right id.  Carry on with the stale copy rather
                        // than aborting a sync over a name.
                        warn!("Could not refresh metadata on bucket {new_id}: {e:?}");
                        bucket
                    }
                }
            } else {
                bucket
            }
        }
        Err(DatastoreError::NoSuchBucket(_)) => {
            ds_to.create_bucket(&bucket_want).unwrap();
            match ds_to.get_bucket(new_id.as_str()) {
                Ok(bucket) => bucket,
                Err(e) => panic!("{e:?}"),
            }
        }
        Err(e) => panic!("{e:?}"),
    }
}

/// Whether the descriptive metadata of `have` has drifted from what `want` says it
/// should be.  Deliberately ignores everything that is not descriptive — id, created,
/// last_updated, events and the row id — so an otherwise identical bucket is not
/// rewritten on every sync pass.
fn bucket_metadata_differs(have: &Bucket, want: &Bucket) -> bool {
    have.hostname != want.hostname
        || have.client != want.client
        || have._type != want._type
        || have.data != want.data
}

/// Number of events fetched per page in the chunked-fetch loop in `sync_one`.
/// Reduced in tests so multi-page paths can be exercised with a small event count.
#[cfg(not(test))]
const BATCH_SIZE: usize = 5000;
#[cfg(test)]
const BATCH_SIZE: usize = 5;

/// Whether a bucket holds data synced from another host, rather than data
/// collected on this host.
///
/// The `-synced-from-<origin>` ID suffix is the marker, matching how
/// `get_or_create_sync_bucket` builds and parses these IDs. Note that
/// `$aw.sync.origin` cannot be used here: it is written on push-staging as well
/// as on import (see the FIXME on `sync_datastores`), so it is set on a host's
/// own exported buckets too and would make every bucket look second-hand.
///
/// `-synced-from-` is a **reserved token** in aw-sync's ID grammar, not merely a
/// convention: `get_or_create_sync_bucket` splits on it to recover the original
/// ID, so a first-hand bucket whose own ID contained it would already have that
/// ID truncated on import, independently of this check. Treating it as a marker
/// therefore adds no new failure mode. Issue #649 tracks moving provenance to
/// bucket metadata, which removes the dependency on the ID string entirely.
fn is_synced_bucket(bucket: &Bucket) -> bool {
    bucket.id.contains("-synced-from-")
}

/// Syncs all buckets from `ds_from` to `ds_to` with `-synced` appended to the ID of the destination bucket.
///
/// Buckets that were themselves synced from another host are skipped in both
/// directions, so data is only ever exchanged first-hand.
///
/// is_push: a bool indicating if we're pushing local buckets to the sync dir
///          (as opposed to pulling from remotes)
/// src_did: the device the source data belongs to. On push that is this device; on import it is
///          the peer whose directory the database was read from, and every event copied in is
///          tagged with it ([`EVENT_ORIGIN_KEY`]) so the combined timeline can tell whose
///          activity is whose without re-deriving it from bucket ids.
pub fn sync_datastores(
    ds_from: &dyn AccessMethod,
    ds_to: &dyn AccessMethod,
    is_push: bool,
    src_did: Option<&str>,
    sync_spec: &SyncSpec,
) {
    // FIXME: "-synced" should only be appended when synced to the local database, not to the
    // staging area for local buckets.
    info!("Syncing {:?} to {:?}", ds_from, ds_to);

    let mut buckets_from: Vec<Bucket> = ds_from
        .get_buckets()
        .unwrap()
        .iter_mut()
        // Never sync a bucket that is itself a copy synced from another host.
        // A host must only ever offer data it collected itself. Without this,
        // HOSTA's buckets reach HOSTB, are re-exported by HOSTB's next push, and
        // come back to HOSTA as `<bucket>_HOSTA-synced-from-HOSTA` — a duplicate
        // of the local bucket, so /timeline renders every event twice.
        // See https://github.com/orgs/ActivityWatch/discussions/1373
        .filter(|tup| {
            if is_synced_bucket(&tup.1) {
                debug!(" - Skipping already-synced bucket '{}'", tup.1.id);
                false
            } else {
                true
            }
        })
        // Only filter buckets if specific bucket IDs are provided
        .filter(|tup| {
            let bucket = &tup.1;
            if let Some(buckets) = &sync_spec.buckets {
                // If "*" is in the buckets list or no buckets specified, sync all buckets
                if buckets.iter().any(|b_id| b_id == "*") || buckets.is_empty() {
                    true
                } else {
                    buckets.iter().any(|b_id| b_id == &bucket.id)
                }
            } else {
                // By default, sync all buckets
                true
            }
        })
        .map(|tup| {
            // TODO: Refuse to sync buckets without hostname/device ID set, or if set to 'unknown'
            if tup.1.hostname == "unknown" {
                // This used to be `src_did.unwrap()`, which was a panic waiting on the import
                // path: src_did was always None there. Import now knows the source device, but a
                // missing one must still not take the process down over one malformed bucket.
                match src_did {
                    Some(did) => {
                        warn!(
                            " ! Bucket hostname/device ID was invalid, setting to device ID/hostname"
                        );
                        tup.1.hostname = did.to_string();
                    }
                    None => warn!(
                        " ! Bucket hostname/device ID was invalid and the source device is unknown"
                    ),
                }
            }
            tup.1.clone()
        })
        .collect();

    // Log warning for buckets requested but not found
    if let Some(buckets) = &sync_spec.buckets {
        for b_id in buckets {
            if !buckets_from.iter().any(|b| b.id == *b_id) {
                error!(" ! Bucket \"{}\" not found in source datastore", b_id);
            }
        }
    }

    // Sync buckets in order of most recently updated
    buckets_from.sort_by_key(|b| b.metadata.end);

    // Nothing is stamped on the way out. A staging copy is data this device is offering as its
    // own first-hand record, and an origin tag on it would come back from a peer as provenance
    // it never had.
    let origin = if is_push { None } else { src_did };

    for bucket_from in buckets_from {
        let bucket_to = get_or_create_sync_bucket(&bucket_from, ds_to, is_push);
        sync_one(ds_from, ds_to, bucket_from, bucket_to, sync_spec, origin);
    }
}

/// Record on an imported event which device collected it (roadmap 3.1).
///
/// An existing [`EVENT_ORIGIN_KEY`] is left alone rather than overwritten: if a tag is ever
/// present already it is a truer statement of where the event came from than the directory this
/// particular copy was read out of.
fn tag_origin(event: &mut Event, origin: Option<&str>) {
    let Some(origin) = origin else { return };
    event
        .data
        .entry(EVENT_ORIGIN_KEY.to_string())
        .or_insert_with(|| serde_json::json!(origin));
}

/// Syncs a single bucket from one datastore to another
fn sync_one(
    ds_from: &dyn AccessMethod,
    ds_to: &dyn AccessMethod,
    bucket_from: Bucket,
    bucket_to: Bucket,
    sync_spec: &SyncSpec,
    origin: Option<&str>,
) {
    let eventcount_to_old = ds_to.get_event_count(bucket_to.id.as_str()).unwrap();
    info!(" ⟳  Syncing bucket '{}'", bucket_to.id);

    // Sync events
    // FIXME: This should use bucket_to.metadata.end, but it doesn't because it doesn't work
    // for empty buckets (Should be None, is Some(unknown_time))
    // let resume_sync_at = bucket_to.metadata.end;
    let most_recent_events = ds_to
        .get_events(bucket_to.id.as_str(), None, None, Some(1))
        .unwrap();
    // If the destination bucket already has events, resume from where it left off.
    // Otherwise (first sync of this bucket), fall back to sync_spec.start, if specified.
    let resume_sync_at = most_recent_events
        .first()
        .map(|e| e.timestamp + e.duration)
        .or(sync_spec.start);

    if let Some(resume_time) = resume_sync_at {
        info!("   + Resuming at {:?}", resume_time);
    } else {
        info!("   + Starting from beginning");
    }

    // Fetch events in bounded chunks to avoid OOM on devices with limited RAM (e.g. Android).
    // get_events returns events in descending order (newest first), so we paginate backwards
    // using the `end` parameter. Each chunk is written to `ds_to` as soon as it is fetched,
    // so peak memory is O(BATCH_SIZE), not O(total events in the bucket).
    //
    // Each chunk is reversed before writing so events are inserted oldest-first (matching the
    // insertion order in the source DB). This preserves consistent ID assignment across source
    // and destination, which the sync tests rely on.
    //
    // Heartbeat semantics at the resume boundary: we use heartbeat() for the globally-oldest
    // new event ONLY in the single-page case (pages_written == 0 when is_last_fetch fires).
    // In that case dest's "last event" is still the pre-sync resume-boundary row, so
    // heartbeat() can correctly merge an adjacent new event into it.
    //
    // In the multi-page case (pages_written > 0), newer pages have already been inserted and
    // dest's "last event" is no longer the resume boundary — heartbeat() would compare against
    // the wrong row and skip the merge anyway. Inserting the oldest event directly is correct.
    let mut fetch_end: Option<DateTime<Utc>> = None;
    let mut events_sent = 0usize;
    let mut pages_written = 0u32;

    loop {
        let raw = ds_from
            .get_events(
                bucket_from.id.as_str(),
                resume_sync_at,
                fetch_end,
                Some(BATCH_SIZE as u64),
            )
            .unwrap();

        if raw.is_empty() {
            break;
        }

        // Fewer events than requested means there's nothing older left to fetch.
        let is_last_fetch = raw.len() < BATCH_SIZE;

        let mut chunk: Vec<Event> = raw
            .into_iter()
            .map(|mut e| {
                // Unset ID on events, as they are not globally unique
                e.id = None;
                // Tag before anything writes it, so every path below -- paged insert, last-page
                // insert and the boundary heartbeat -- carries the tag without having to
                // remember to.
                tag_origin(&mut e, origin);
                e
            })
            .collect();

        if !is_last_fetch {
            // chunk is in DESC order (newest first); chunk.last() = oldest in this (full) page.
            // Naively setting the next `end` to `oldest.timestamp - 1ns` silently drops events
            // if the page happens to end mid-run of same-timestamp events: anything else at
            // that exact timestamp would fall outside the next page's range. Guard against that
            // by dropping ALL trailing events at `boundary_ts` from this page and leaving them
            // for the next fetch (whose `end = boundary_ts` is inclusive, so it re-fetches
            // the whole tied run at once).
            //
            // Note: we must drop the boundary event itself, not just its duplicates. Keeping
            // one copy in this chunk while also setting `fetch_end = Some(boundary_ts)` (inclusive)
            // would cause that event to be fetched again next page, producing a duplicate row.
            let boundary_ts = chunk.last().unwrap().timestamp;
            if chunk.first().unwrap().timestamp != boundary_ts {
                // Safe to pop all boundary_ts events: the `if` guard ensures at least one
                // earlier event (with a different timestamp) remains in the chunk.
                while chunk.last().map_or(false, |e| e.timestamp == boundary_ts) {
                    chunk.pop();
                }
                fetch_end = Some(boundary_ts);
            } else {
                // Pathological case: every event in this full page shares the exact same
                // timestamp, so we can't tell where the tied run ends without an unbounded
                // query. This can't occur with AW's event model in practice (activity records
                // span seconds+) — accept the page as-is rather than looping forever.
                fetch_end = Some(boundary_ts - Duration::nanoseconds(1));
            }

            // Reverse to ASC order (oldest first) before inserting.
            chunk.reverse();
            events_sent += chunk.len();
            pages_written += 1;
            for batch in chunk.chunks(BATCH_SIZE) {
                print!("({}/…)\r", events_sent);
                ds_to
                    .insert_events(bucket_to.id.as_str(), batch.to_vec())
                    .unwrap();
            }
        } else {
            // Last (oldest) page: process oldest-first to preserve ID ordering.
            chunk.reverse(); // chunk is now ASC (oldest first)

            // Use heartbeat() for the oldest event only in the single-page case:
            // (Note: at the first import after origin tagging was introduced, the resume-boundary
            // row is untagged and the new event is tagged, so their data differ and heartbeat
            // inserts instead of extending. That costs one extra row at the seam, once.)
            // dest's "last event" is still the pre-sync resume-boundary row, so heartbeat()
            // can correctly merge an adjacent new event into it (delta=0.0 → exact adjacency).
            // In multi-page syncs, newer pages are already in dest, so heartbeat() would
            // compare against the wrong row — insert directly instead.
            if !chunk.is_empty() && pages_written == 0 {
                let oldest = chunk.remove(0);
                ds_to.heartbeat(bucket_to.id.as_str(), oldest, 0.0).unwrap();
                events_sent += 1;
            }

            // Insert the remaining events from the last page in ASC order.
            if !chunk.is_empty() {
                events_sent += chunk.len();
                for batch in chunk.chunks(BATCH_SIZE) {
                    print!("({}/…)\r", events_sent);
                    ds_to
                        .insert_events(bucket_to.id.as_str(), batch.to_vec())
                        .unwrap();
                }
            }

            break;
        }
    }

    let eventcount_to_new = ds_to.get_event_count(bucket_to.id.as_str()).unwrap();
    let new_events_count = eventcount_to_new - eventcount_to_old;
    assert!(new_events_count >= 0);
    if new_events_count > 0 {
        // The origin is named here rather than only in the code because it is the only way to
        // check 3.1 on a device: the tag lands inside event data, which nothing in the UI is
        // guaranteed to surface, and app-private storage is not readable over adb.
        match origin {
            Some(origin) => {
                info!("  = Synced {new_events_count} new events, tagged origin {origin}")
            }
            None => info!("  = Synced {new_events_count} new events"),
        }
    } else {
        info!("  ✓ Already up to date!");
    }
}

fn log_buckets(ds: &dyn AccessMethod) {
    // Logs all buckets and some metadata for a given datastore
    let buckets = ds.get_buckets().unwrap();
    info!("Buckets in {:?}:", ds);
    for bucket in buckets.values() {
        info!(" - {}", bucket.id.as_str());
        info!(
            "   eventcount: {:?}",
            ds.get_event_count(bucket.id.as_str()).unwrap()
        );
    }
}
