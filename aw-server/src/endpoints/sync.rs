//! The API behind the desktop's sync page.
//!
//! Four things a person setting sync up for the first time needs: where the folder is, what is in
//! it, a way to change both, and a button that runs one pass now so they find out whether it works
//! before walking away. See [`crate::sync_setup`] for why this drives the `aw-sync` binary.

use rocket::http::Status;
use rocket::serde::json::Json;
use rocket::State;
use serde::{Deserialize, Serialize};

use crate::endpoints::{HttpErrorJson, ServerState};
use crate::sync_setup;

/// Everything the page draws, in one request.
///
/// One request rather than three, because the three answers are only meaningful together: a folder
/// with no peers means something different depending on whether sync has ever run, and a last-run
/// failure means something different depending on whether the folder exists. A page that fetched
/// them separately could render a coherent-looking screen out of three moments.
#[derive(Serialize)]
pub struct SyncStatus {
    /// The folder this device syncs through.
    sync_dir: String,
    /// Whether it is there yet. The most common way this is set up wrong, and the one thing a
    /// person can fix in a file manager in five seconds once told.
    sync_dir_exists: bool,
    /// The folder used when nothing has been chosen, so the page can offer "put it back".
    default_sync_dir: String,
    /// Whether the background pass runs.
    enabled: bool,
    /// This device's own uuid, which is the directory name it writes under.
    own_device_id: String,
    /// Every database in the folder, this device's own flagged rather than hidden: seeing your own
    /// appear is how you learn your half is working.
    peers: Vec<sync_setup::Peer>,
    /// How the last pass went, if there has been one since this server started.
    last_run: Option<sync_setup::LastRun>,
    /// False when `aw-sync` is not installed beside this server, which is a real state -- a server
    /// built on its own genuinely cannot sync -- and one the page must say plainly rather than
    /// discover when the button fails.
    can_sync: bool,
}

#[get("/")]
pub fn sync_status(state: &State<ServerState>) -> Json<SyncStatus> {
    let dir = sync_setup::sync_dir(&state.datastore);
    Json(SyncStatus {
        sync_dir: dir.to_string_lossy().to_string(),
        sync_dir_exists: dir.is_dir(),
        default_sync_dir: sync_setup::default_sync_dir().to_string_lossy().to_string(),
        enabled: sync_setup::sync_enabled(&state.datastore),
        own_device_id: state.device_id.clone(),
        peers: sync_setup::peers(&dir, &state.device_id),
        last_run: sync_setup::last_run(),
        can_sync: sync_setup::aw_sync_binary().is_some(),
    })
}

#[derive(Deserialize)]
pub struct SyncConfig {
    /// Absent means "leave the folder alone", which is what the on/off switch sends.
    dir: Option<String>,
    /// Absent means "leave the switch alone", which is what the folder form sends.
    enabled: Option<bool>,
}

/// Change the folder, the switch, or both.
///
/// Both in one call, and both optional, so the page never has to make two requests that could half
/// succeed -- a device left syncing into a folder it was just moved away from is worse than either
/// change failing.
#[post("/", data = "<config>")]
pub fn sync_configure(
    state: &State<ServerState>,
    config: Json<SyncConfig>,
) -> Result<Json<SyncStatus>, HttpErrorJson> {
    if let Some(dir) = &config.dir {
        sync_setup::set_sync_dir(&state.datastore, dir)
            .map_err(|e| HttpErrorJson::new(Status::BadRequest, e))?;
    }
    if let Some(enabled) = config.enabled {
        sync_setup::set_sync_enabled(&state.datastore, enabled)
            .map_err(|e| HttpErrorJson::new(Status::InternalServerError, e))?;
    }
    Ok(sync_status(state))
}

/// Run one pass now, and answer with the whole status.
///
/// Answering with the status rather than just the result is what lets the page show the new peer
/// list in the same breath as "synced 190 events": the first sync is exactly the moment the folder
/// changes, and a page that had to ask again would show a stale list for as long as that took.
///
/// **Not an error response when the sync fails.** A failed pass is a normal outcome the page
/// renders -- the folder is not shared yet, Syncthing has not finished, a peer wrote a file we
/// cannot read -- and an HTTP error would push it into an error handler that says less than the
/// message does.
#[post("/run")]
pub fn sync_now(state: &State<ServerState>) -> Json<SyncStatus> {
    let profile = crate::config::get_profile().to_string();
    sync_setup::run_once(&state.datastore, &profile, &state.device_id, true);
    sync_status(state)
}
