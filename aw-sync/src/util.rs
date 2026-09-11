use std::error::Error;
use std::ffi::OsStr;
use std::fs;
use std::fs::File;
use std::io::Read;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

pub struct ServerConfig {
    pub port: u16,
    pub api_key: Option<String>,
}

impl ServerConfig {
    pub fn default_for(testing: bool) -> Self {
        Self {
            port: if testing { 5666 } else { 5600 },
            api_key: None,
        }
    }
}

/// Returns the settings aw-sync needs from the selected aw-server config.
///
/// Also used on Android: the embedded server writes `config.toml` under
/// `filesDir`, and `get_client()` in `android.rs` must send the same
/// `[auth].api_key` or `/api/0/buckets` returns 401 (aw-android#247).
pub fn get_server_config(
    testing: bool,
    config_override: Option<&Path>,
) -> Result<ServerConfig, Box<dyn Error>> {
    let path = match config_override {
        Some(path) => path.to_path_buf(),
        None => crate::dirs::get_server_config_path(testing)
            .map_err(|_| "Could not get aw-server config path")?,
    };
    let default = ServerConfig::default_for(testing);
    if !path.exists() {
        return Ok(default);
    }

    let mut contents = String::new();
    File::open(path)?.read_to_string(&mut contents)?;
    let value: toml::Value = toml::from_str(&contents)?;
    let port = value
        .get("port")
        .and_then(|v| v.as_integer())
        .and_then(|v| u16::try_from(v).ok())
        .unwrap_or(default.port);
    let api_key = value
        .get("auth")
        .and_then(|a| a.get("api_key"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);

    Ok(ServerConfig { port, api_key })
}

/// Local config must never be read for a caller-selected remote target.
#[cfg(not(target_os = "android"))]
pub fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

/// Add URL brackets around bare IPv6 literals.
#[cfg(not(target_os = "android"))]
pub fn host_for_url(host: &str) -> String {
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V6(_)) => format!("[{host}]"),
        _ => host.to_string(),
    }
}

#[cfg(all(test, not(target_os = "android")))]
mod tests {
    use super::{
        find_remotes_nonlocal, get_server_config, host_for_url, is_loopback_host,
        origin_from_db_path,
    };
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A scratch directory that cleans up after itself, so these tests leave no sync folders behind.
    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aw-sync-{}-{}-{}",
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

    fn touch_db(path: &PathBuf) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"").unwrap();
    }

    /// The shape a phone actually leaves in the shared folder, against the shape a desktop does.
    ///
    /// Android copies its tree out as `<root>/<hostname>/<device id>/test.db`, a desktop writes
    /// `<root>/<device id>/test.db`. Reading only one level meant a PC sharing a folder with a
    /// phone found nothing at all, while the phone found the PC -- sync that looks fine from one
    /// end and is empty at the other.
    #[test]
    fn finds_peers_at_both_depths() {
        let root = scratch_dir("depths");
        let desktop = root.join("desktop-uuid").join("test.db");
        let phone = root
            .join("jude_s_s25_ultra")
            .join("phone-uuid")
            .join("test.db");
        touch_db(&desktop);
        touch_db(&phone);

        let found = find_remotes_nonlocal(&root, "nobody", None);
        assert!(found.contains(&desktop), "desktop layout missed: {found:?}");
        assert!(found.contains(&phone), "android layout missed: {found:?}");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn leaves_out_this_device_at_either_depth() {
        let root = scratch_dir("own");
        let own_flat = root.join("me").join("test.db");
        let own_nested = root.join("my-host").join("me").join("test.db");
        let theirs = root.join("them").join("test.db");
        touch_db(&own_flat);
        touch_db(&own_nested);
        touch_db(&theirs);

        let found = find_remotes_nonlocal(&root, "me", None);
        assert_eq!(
            found,
            vec![theirs.clone()],
            "own remote came back: {found:?}"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ignores_a_database_lying_loose_at_the_root() {
        // The directory holding a database *is* the device that wrote it, so one with no
        // directory cannot be attributed to anybody.
        let root = scratch_dir("loose");
        touch_db(&root.join("stray.db"));
        let real = root.join("them").join("test.db");
        touch_db(&real);

        let found = find_remotes_nonlocal(&root, "me", None);
        assert_eq!(found, vec![real.clone()]);

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn does_not_follow_a_shared_folder_all_the_way_down() {
        // The folder belongs to whoever else is in it too; a database four levels down is
        // somebody else's business, not a peer.
        let root = scratch_dir("deep");
        touch_db(&root.join("a").join("b").join("c").join("test.db"));

        let found = find_remotes_nonlocal(&root, "me", None);
        assert!(found.is_empty(), "followed too deep: {found:?}");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn reads_port_and_api_key_from_config_override() {
        let config_path = std::env::temp_dir().join(format!(
            "aw-sync-config-{}-{}.toml",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(
            &config_path,
            "port = 5611\n[auth]\napi_key = \"custom-key\"\n",
        )
        .unwrap();

        let config = get_server_config(false, Some(&config_path)).unwrap();

        fs::remove_file(config_path).unwrap();
        assert_eq!(config.port, 5611);
        assert_eq!(config.api_key.as_deref(), Some("custom-key"));
    }

    #[test]
    fn missing_config_override_uses_defaults() {
        let config_path = std::env::temp_dir().join(format!(
            "missing-aw-sync-config-{}.toml",
            std::process::id()
        ));
        let _ = fs::remove_file(&config_path);

        let production = get_server_config(false, Some(&config_path)).unwrap();
        let testing = get_server_config(true, Some(&config_path)).unwrap();

        assert_eq!(production.port, 5600);
        assert!(production.api_key.is_none());
        assert_eq!(testing.port, 5666);
        assert!(testing.api_key.is_none());
    }

    #[test]
    fn commented_or_empty_api_key_is_absent() {
        let config_path = std::env::temp_dir().join(format!(
            "aw-sync-config-commented-{}-{}.toml",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(
            &config_path,
            "port = 5600\n[auth]\n#api_key = \"secret\"\napi_key = \"\"\n",
        )
        .unwrap();

        let config = get_server_config(false, Some(&config_path)).unwrap();
        fs::remove_file(config_path).unwrap();
        assert!(config.api_key.is_none());
    }

    #[test]
    fn recognizes_only_loopback_hosts() {
        for host in ["127.0.0.1", "127.0.0.2", "::1", "localhost", "LOCALHOST"] {
            assert!(is_loopback_host(host));
        }
        for host in ["example.com", "192.0.2.1", "localhost.example.com"] {
            assert!(!is_loopback_host(host));
        }
    }

    #[test]
    fn brackets_bare_ipv6_hosts_for_urls() {
        assert_eq!(host_for_url("::1"), "[::1]");
        assert_eq!(host_for_url("2001:db8::1"), "[2001:db8::1]");
        assert_eq!(host_for_url("127.0.0.1"), "127.0.0.1");
        assert_eq!(host_for_url("localhost"), "localhost");
    }

    #[test]
    fn origin_is_the_directory_holding_the_db() {
        let path = std::path::Path::new("/sync/jude_s_s25_ultra/9f0c-uuid/test.db");
        assert_eq!(origin_from_db_path(path).as_deref(), Some("9f0c-uuid"));
    }

    #[test]
    fn origin_is_none_when_there_is_no_containing_directory() {
        // A bare filename's parent is the empty path -- there is no device name to be had, and
        // inventing one would mislabel every event in the file.
        assert_eq!(origin_from_db_path(std::path::Path::new("test.db")), None);
    }
}

/// Check if a directory contains a .db file
fn contains_db_file(dir: &std::path::Path) -> bool {
    fs::read_dir(dir)
        .ok()
        .map(|entries| {
            entries.filter_map(Result::ok).any(|entry| {
                entry
                    .path()
                    .extension()
                    .map(|ext| ext == "db")
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// Check if a directory contains a subdirectory that contains a .db file
fn contains_subdir_with_db_file(dir: &std::path::Path) -> bool {
    fs::read_dir(dir)
        .ok()
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .any(|entry| entry.path().is_dir() && contains_db_file(&entry.path()))
        })
        .unwrap_or(false)
}

/// Return all remotes in the sync folder
/// Only returns folders that match ./{host}/{device_id}/*.db
// TODO: share logic with find_remotes and find_remotes_nonlocal
pub fn get_remotes() -> Result<Vec<String>, Box<dyn Error>> {
    let sync_root_dir = crate::dirs::get_sync_dir()?;
    fs::create_dir_all(&sync_root_dir)?;
    let hostnames = fs::read_dir(sync_root_dir)?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir() && contains_subdir_with_db_file(&entry.path()))
        .filter_map(|entry| {
            entry
                .path()
                .file_name()
                .and_then(|os_str| os_str.to_str().map(String::from))
        })
        .collect();
    info!("Found remotes: {:?}", hostnames);
    Ok(hostnames)
}

/// The device UUID a remote database belongs to, taken from the directory containing it.
///
/// The shared folder is laid out `<sync root>/<hostname>/<device uuid>/<file>.db` (see
/// `sync_wrapper::pull`), so the parent directory *is* the device that wrote the file. Reading
/// origin from the path at merge time is what lets a device leave its own events completely
/// untouched when it exports them (**R11**, `05_DATA_MODEL.md` §6) -- nothing is stamped at
/// capture time, and the importer works it out from where the file sat.
///
/// Returns `None` rather than guessing when there is no usable parent directory name. An event
/// that arrives untagged can still be attributed from its bucket; an event tagged with the wrong
/// device cannot be told apart from a correct one.
pub fn origin_from_db_path(path: &Path) -> Option<String> {
    path.parent()?
        .file_name()
        .and_then(OsStr::to_str)
        .filter(|name| !name.is_empty())
        .map(String::from)
}

/// How far below the sync root a peer's database may sit.
///
/// Two, because the two devices that share this folder do not nest it the same way. A desktop
/// `setup_local_remote` writes `<root>/<device uuid>/test.db`, while an Android device copies its
/// tree out through SAF as `<root>/<hostname>/<device uuid>/test.db` -- `SyncInterface.kt`
/// documents that extra level, and it is what the phone actually puts in the folder. Scanning
/// exactly one level, as this did, meant a PC sharing a folder with a phone saw *nothing*: it
/// looked for `<root>/<hostname>/*.db` and found only another directory. The phone could read the
/// PC, the PC could not read the phone, and sync looked like it was working from one end.
///
/// Bounded rather than a full walk: the folder is shared, so an unbounded recursion would follow
/// whatever anyone else put in it, and every layout either device writes is covered by two.
const MAX_REMOTE_DEPTH: usize = 2;

/// Returns a list of all remote dbs, at either of the depths a peer may have written one.
///
/// A `.db` sitting loose at the very root is deliberately not a remote: the directory holding a
/// database *is* the device that wrote it (see `origin_from_db_path`), and the root belongs to
/// nobody. That was true of the single-level scan this replaces, and events that cannot be
/// attributed are worse than events not read.
fn find_remotes(sync_directory: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut dbs = Vec::new();
    let entries = match fs::read_dir(sync_directory) {
        Ok(entries) => entries,
        Err(e) => return Err(e),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_remotes(&path, MAX_REMOTE_DEPTH - 1, &mut dbs);
        }
    }
    dbs.sort();
    Ok(dbs)
}

/// Collect `*.db` files up to `depth` directories below `dir`.
///
/// Unreadable entries are skipped rather than panicking: this reads a folder other devices write
/// into, so a half-written directory or one Syncthing has locked is an ordinary event, and it must
/// not take down a sync that could still carry everything else.
fn collect_remotes(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if depth > 0 {
                collect_remotes(&path, depth - 1, out);
            }
        } else if path.extension().unwrap_or_else(|| OsStr::new("")) == "db" {
            out.push(path);
        }
    }
}

/// Returns a list of all remotes, excluding local ones
pub fn find_remotes_nonlocal(
    sync_directory: &Path,
    device_id: &str,
    sync_db: Option<&PathBuf>,
) -> Vec<PathBuf> {
    let remotes_all = find_remotes(sync_directory).unwrap();
    remotes_all
        .into_iter()
        // Filter out own remote
        .filter(|path| {
            !(path
                .clone()
                .into_os_string()
                .into_string()
                .unwrap()
                .contains(device_id))
        })
        // If sync_db is Some, return only remotes in that path
        .filter(|path| {
            if let Some(sync_db) = sync_db {
                path.starts_with(sync_db)
            } else {
                true
            }
        })
        .collect()
}
