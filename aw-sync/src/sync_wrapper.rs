use std::error::Error;
use std::fs;
use std::path::PathBuf;

use crate::sync::{sync_datastores, sync_run, AccessMethod, SyncMode, SyncSpec};
use aw_client_rust::blocking::AwClient;

pub fn pull_all(client: &AwClient) -> Result<(), Box<dyn Error>> {
    let hostnames = crate::util::get_remotes()?;
    for host in hostnames {
        pull(&host, client)?
    }
    Ok(())
}

pub fn pull(host: &str, client: &AwClient) -> Result<(), Box<dyn Error>> {
    client.wait_for_start()?;

    // Path to the sync folder
    // Sync folder is structured ./{hostname}/{device_id}/test.db
    let sync_root_dir = crate::dirs::get_sync_dir().map_err(|_| "Could not get sync dir")?;
    let sync_dir = sync_root_dir.join(host);
    let dbs = fs::read_dir(&sync_dir)?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .map(|entry| fs::read_dir(entry.path()))
        .filter_map(Result::ok)
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.path().is_file()
                && entry.path().extension().and_then(|os_str| os_str.to_str()) == Some("db")
        })
        .collect::<Vec<_>>();

    // if more than one db, warn and use the largest one
    if dbs.len() > 1 {
        warn!(
            "More than one db found in sync folder for host, choosing largest db {:?}",
            dbs
        );
    }

    let db = dbs
        .into_iter()
        .max_by_key(|entry| entry.metadata().map(|m| m.len()).unwrap_or(0))
        .ok_or_else(|| format!("No db found in sync folder {:?}", sync_dir))?;

    let sync_spec = SyncSpec {
        path: sync_dir.clone(),
        path_db: Some(db.path().clone()),
        buckets: None, // Sync all buckets by default
        start: None,
    };
    sync_run(client, &sync_spec, SyncMode::Pull)?;

    Ok(())
}

pub fn push(client: &AwClient) -> Result<(), Box<dyn Error>> {
    push_with_hostname(client, &client.hostname)
}

pub fn push_with_hostname(client: &AwClient, hostname: &str) -> Result<(), Box<dyn Error>> {
    let sync_dir = crate::dirs::get_sync_dir()
        .map_err(|_| "Could not get sync dir")?
        .join(hostname);

    let sync_spec = SyncSpec {
        path: sync_dir,
        path_db: None,
        buckets: None, // Sync all buckets by default
        start: None,
    };
    sync_run(client, &sync_spec, SyncMode::Push)?;

    Ok(())
}

/// Push local data to a per-device staging area.
///
/// On Android, each device writes to `<sync_dir>/<hostname>/<device_id>_test.db`
/// so multiple devices can coexist in the shared sync folder without overwriting
/// each other's files. This is Phase 1 of multi-device support.
pub fn push_with_hostname_and_device_id(
    client: &AwClient,
    hostname: &str,
    device_id: &str,
) -> Result<(), Box<dyn Error>> {
    let sync_dir = crate::dirs::get_sync_dir()
        .map_err(|_| "Could not get sync dir")?
        .join(hostname);

    // Create a device-specific staging directory
    let device_staging_dir = sync_dir.join(format!("{}_staging", device_id));
    fs::create_dir_all(&device_staging_dir)
        .map_err(|e| format!("Failed to create staging dir: {}", e))?;

    // Use the device-specific path as our local remote for push staging
    let sync_spec = SyncSpec {
        path: device_staging_dir,
        path_db: None,
        buckets: None,
        start: None,
    };
    sync_run(client, &sync_spec, SyncMode::Push)?;

    Ok(())
}

/// Pull data from ALL discovered remotes across all hostname directories.
/// This is the Android-compatible version of pull_all that works when multiple
/// devices share the same AW_SYNC_DIR folder (e.g., via Syncthing).
pub fn pull_all_from_all_hostnames(client: &AwClient) -> Result<(), Box<dyn Error>> {
    let hostnames = crate::util::get_remotes()?;
    for host in &hostnames {
        // Pull from ALL devices under this hostname directory
        // (could be multiple device databases if they share a hostname)
        pull_from_hostname(host, client)?;
    }
    Ok(())
}

/// Pull and merge data from all .db files under a single hostname directory.
fn pull_from_hostname(host: &str, client: &AwClient) -> Result<(), Box<dyn Error>> {
    let sync_root_dir = crate::dirs::get_sync_dir().map_err(|_| "Could not get sync dir")?;
    let sync_dir = sync_root_dir.join(host);

    // Find all device directories under this hostname
    let device_dirs: Vec<PathBuf> = fs::read_dir(&sync_dir)?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.path())
        .collect();

    if device_dirs.is_empty() {
        debug!("No device directories found under {host}");
        return Ok(());
    }

    // For each device directory, find its database files and pull from them
    for device_dir in &device_dirs {
        let db_files: Vec<PathBuf> = fs::read_dir(device_dir)?
            .filter_map(Result::ok)
            .filter(|entry| {
                entry.path().is_file()
                    && entry.path().extension().and_then(|e| e.to_str()) == Some("db")
            })
            .map(|entry| entry.path())
            .collect();

        for db_path in &db_files {
            // Skip the device's own database (we detect this by checking if it
            // is in the client's own staging area - handled by sync_datastores skip logic)
            let device_id = device_dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string();

            let sync_spec = SyncSpec {
                path: sync_dir.clone(),
                path_db: Some(db_path.clone()),
                buckets: None,
                start: None,
            };

            // sync_run handles the pull; it already has logic to skip synced-from-* buckets
            // and avoid re-syncing data that's already present.
            sync_run(client, &sync_spec, SyncMode::Pull)?;
        }
    }

    Ok(())
}
