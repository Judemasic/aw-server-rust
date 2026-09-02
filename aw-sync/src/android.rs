use std::panic::{self, catch_unwind, AssertUnwindSafe};
use std::path::Path;

use aw_client_rust::blocking::AwClient;
use jni::objects::{JClass, JString};
use jni::sys::jstring;
use jni::JNIEnv;
use serde_json::json;

use crate::{pull, pull_all, push_with_hostname};
#[cfg(target_os = "android")]
use crate::pull_all_from_all_hostnames;
#[cfg(target_os = "android")]
use crate::push_with_hostname_and_device_id;

/// Initialize android_logger for aw-sync library.
/// Must be called before any other JNI functions that might use the log crate.
#[no_mangle]
pub extern "C" fn aw_sync_init_logging(verbosity: i32) {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(match verbosity {
                0 => log::LevelFilter::Error,
                1 => log::LevelFilter::Warn,
                2 => log::LevelFilter::Info,
                3 => log::LevelFilter::Debug,
                _ => log::LevelFilter::Trace,
            })
            .with_tag("aw-sync"),
    );
}

/// Helper function to convert Rust string to Java string
fn rust_string_to_jstring(env: &JNIEnv, s: String) -> jstring {
    let output = env.new_string(s).expect("Couldn't create java string!");
    output.into_raw()
}

/// Point this cdylib's `ANDROID_DATA_DIR` at the app's filesDir.
///
/// `libaw_sync.so` and `libaw_server.so` are separate cdylibs with separate statics, so
/// `RustInterface.setDataDir` updates only the server's copy. `SyncInterface.kt` sets
/// `XDG_DATA_HOME=$filesDir/data` before `loadLibrary`, so filesDir can be recovered from it --
/// which works on release, `.debug` and work-profile installs alike, and needs no new JNI symbol.
fn apply_android_data_dir_from_env() {
    let Ok(xdg_data) = std::env::var("XDG_DATA_HOME") else {
        return;
    };
    let Some(files_dir) = crate::dirs::files_dir_from_xdg_data_home(Path::new(&xdg_data)) else {
        warn!(
            "XDG_DATA_HOME={} is not $filesDir/data; leaving android data dir unchanged",
            xdg_data
        );
        return;
    };
    let path = files_dir.to_string_lossy();
    info!("android data dir from XDG_DATA_HOME: {}", path);
    aw_server::dirs::set_android_data_dir(&path);
}

/// Build a client for the embedded server, forwarding the API key.
///
/// The embedded server enables API-key auth whenever `config.toml` carries `[auth].api_key`, and
/// `AWPreferences.isDashboardAuthEnabled()` defaults to true -- so a key exists on first run.
/// A plain `AwClient::new()` here 401s on `GET /api/0/buckets`, which makes every sync report
/// success while transferring nothing (aw-android#247, aw-server-rust#666). Reading the key
/// requires the data dir to be correct first, hence the call above.
fn get_client(port: i32) -> Result<AwClient, String> {
    apply_android_data_dir_from_env();
    let host = "127.0.0.1";
    let api_key = match crate::util::get_server_config(false, None) {
        Ok(cfg) => {
            if cfg.api_key.is_some() {
                info!("using API key from config.toml for local client");
            }
            cfg.api_key
        }
        Err(e) => {
            warn!("failed to read server config for API key: {}", e);
            None
        }
    };
    AwClient::new_with_api_key(host, port as u16, "aw-sync-android", api_key)
        .map_err(|e| format!("Failed to create client: {}", e))
}
}

/// Pull sync data from all hosts in the sync directory
#[no_mangle]
pub extern "C" fn Java_net_activitywatch_android_SyncInterface_syncPullAll(
    mut env: JNIEnv,
    _class: JClass,
    port: i32,
    hostname: JString,
) -> jstring {
    let hostname_str: String = match env.get_string(&hostname) {
        Ok(s) => s.into(),
        Err(e) => {
            let error_msg = format!("Failed to get hostname: {}", e);
            error!("syncPullAll: {}", error_msg);
            return rust_string_to_jstring(
                &env,
                json!({
                    "success": false,
                    "error": error_msg
                })
                .to_string(),
            );
        }
    };

    let result: Result<String, String> = (|| {
        let client = get_client(port)?;
        pull_all(&client).map_err(|e| format!("Sync pull failed: {}", e))?;
        Ok(json!({
            "success": true,
            "message": "Successfully pulled from all hosts"
        })
        .to_string())
    })();

    match result {
        Ok(msg) => rust_string_to_jstring(&env, msg),
        Err(e) => {
            error!("syncPullAll error: {}", e);
            let error_msg: &str = &e;
            let error_json = json!({
                "success": false,
                "error": error_msg
            })
            .to_string();
            rust_string_to_jstring(&env, error_json)
        }
    }
}

/// Pull sync data from a specific host
#[no_mangle]
pub extern "C" fn Java_net_activitywatch_android_SyncInterface_syncPull(
    mut env: JNIEnv,
    _class: JClass,
    port: i32,
    hostname: JString,
) -> jstring {
    let result: Result<String, String> = (|| {
        let client = get_client(port)?;
        let hostname_str: String = env
            .get_string(&hostname)
            .map_err(|e| format!("Failed to get hostname string: {}", e))?
            .into();

        pull(&hostname_str, &client).map_err(|e| format!("Sync pull failed: {}", e))?;

        Ok(json!({
            "success": true,
            "message": format!("Successfully pulled from host: {}", hostname_str)
        })
        .to_string())
    })();

    match result {
        Ok(msg) => rust_string_to_jstring(&env, msg),
        Err(e) => {
            error!("syncPull error: {}", e);
            let error_msg: &str = &e;
            let error_json = json!({
                "success": false,
                "error": error_msg
            })
            .to_string();
            rust_string_to_jstring(&env, error_json)
        }
    }
}

/// Push local sync data to the sync directory
#[no_mangle]
pub extern "C" fn Java_net_activitywatch_android_SyncInterface_syncPush(
    mut env: JNIEnv,
    _class: JClass,
    port: i32,
    hostname: JString,
) -> jstring {
    let hostname_str: String = match env.get_string(&hostname) {
        Ok(s) => s.into(),
        Err(e) => {
            let error_msg = format!("Failed to get hostname: {}", e);
            error!("syncPush: {}", error_msg);
            return rust_string_to_jstring(
                &env,
                json!({
                    "success": false,
                    "error": error_msg
                })
                .to_string(),
            );
        }
    };

    let result: Result<String, String> = (|| {
        let client = get_client(port)?;
        push_with_hostname(&client, &hostname_str)
            .map_err(|e| format!("Sync push failed: {}", e))?;
        Ok(json!({
            "success": true,
            "message": "Successfully pushed local data"
        })
        .to_string())
    })();

    match result {
        Ok(msg) => rust_string_to_jstring(&env, msg),
        Err(e) => {
            error!("syncPush error: {}", e);
            let error_msg: &str = &e;
            let error_json = json!({
                "success": false,
                "error": error_msg
            })
            .to_string();
            rust_string_to_jstring(&env, error_json)
        }
    }
}

/// Perform full sync (pull from all hosts, then push local data)
#[no_mangle]
pub extern "C" fn Java_net_activitywatch_android_SyncInterface_syncBoth(
    mut env: JNIEnv,
    _class: JClass,
    port: i32,
    hostname: JString,
) -> jstring {
    let hostname_str: String = match env.get_string(&hostname) {
        Ok(s) => s.into(),
        Err(e) => {
            let error_msg = format!("Failed to get hostname: {}", e);
            error!("syncBoth: {}", error_msg);
            return rust_string_to_jstring(
                &env,
                json!({
                    "success": false,
                    "error": error_msg
                })
                .to_string(),
            );
        }
    };

    let result: Result<String, String> = (|| {
        let client = get_client(port)?;

        pull_all(&client).map_err(|e| format!("Pull phase failed: {}", e))?;

        push_with_hostname(&client, &hostname_str)
            .map_err(|e| format!("Push phase failed: {}", e))?;

        Ok(json!({
            "success": true,
            "message": "Successfully completed full sync"
        })
        .to_string())
    })();

    match result {
        Ok(msg) => rust_string_to_jstring(&env, msg),
        Err(e) => {
            error!("syncBoth error: {}", e);
            let error_msg: &str = &e;
            let error_json = json!({
                "success": false,
                "error": error_msg
            })
            .to_string();
            rust_string_to_jstring(&env, error_json)
        }
    }
}

/// Get the sync directory path
#[no_mangle]
pub extern "C" fn Java_net_activitywatch_android_SyncInterface_getSyncDir(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let result = crate::dirs::get_sync_dir();

    match result {
        Ok(path) => {
            let path_str = path.to_string_lossy().to_string();
            let response = json!({
                "success": true,
                "path": path_str
            })
            .to_string();
            rust_string_to_jstring(&env, response)
        }
        Err(e) => {
            let error_json = json!({
                "success": false,
                "error": format!("Failed to get sync dir: {}", e)
            })
            .to_string();
            rust_string_to_jstring(&env, error_json)
        }
    }
}

/// Push local data using per-device staging path (Android only).
/// Each device writes its staging area to a separate subdirectory so multiple devices
/// can share the same AW_SYNC_DIR without overwriting each other's files.
#[no_mangle]
pub extern "C" fn Java_net_activitywatch_android_SyncInterface_syncPushWithDeviceId(
    mut env: JNIEnv,
    _class: JClass,
    port: i32,
    hostname: JString,
    device_id: JString,
) -> jstring {
    // Wrap in catch_unwind to prevent Rust panics from SIGABRT-crashing the JNI
    let result = catch_unwind(AssertUnwindSafe(|| {
        let hostname_str: String = match env.get_string(&hostname) {
            Ok(s) => s.into(),
            Err(e) => {
                let error_msg = format!("Failed to get hostname: {}", e);
                return json!({"success": false, "error": error_msg}).to_string();
            }
        };

        let device_id_str: String = match env.get_string(&device_id) {
            Ok(s) => s.into(),
            Err(e) => {
                let error_msg = format!("Failed to get device_id: {}", e);
                return json!({"success": false, "error": error_msg}).to_string();
            }
        };

        let result: Result<String, String> = (|| {
            let client = get_client(port)?;
            push_with_hostname_and_device_id(&client, &hostname_str, &device_id_str)
                .map_err(|e| format!("Sync push failed: {}", e))?;
            Ok(json!({
                "success": true,
                "message": "Successfully pushed local data with per-device staging"
            })
            .to_string())
        })();

        match result {
            Ok(msg) => msg,
            Err(e) => json!({"success": false, "error": format!("{}", e)}).to_string(),
        }
    }));

    match result {
        Ok(json_str) => rust_string_to_jstring(&env, json_str),
        Err(panic_err) => {
            let msg = format!("RUST PANIC in syncPushWithDeviceId: {:?}", panic_err);
            error!("{}", msg);
            rust_string_to_jstring(
                &env,
                json!({"success": false, "error": msg}).to_string(),
            )
        }
    }
}

/// Pull from ALL hostname directories in the shared sync folder (Android only).
/// This discovers remotes across all device hostnames instead of just one.
#[no_mangle]
pub extern "C" fn Java_net_activitywatch_android_SyncInterface_syncPullAllFromAllHostnames(
    mut env: JNIEnv,
    _class: JClass,
    port: i32,
) -> jstring {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let result: Result<String, String> = (|| {
            let client = get_client(port)?;
            pull_all_from_all_hostnames(&client)
                .map_err(|e| format!("Sync pull failed: {}", e))?;
            Ok(json!({
                "success": true,
                "message": "Successfully pulled from all hostnames"
            })
            .to_string())
        })();

        match result {
            Ok(msg) => msg,
            Err(e) => json!({"success": false, "error": format!("{}", e)}).to_string(),
        }
    }));

    match result {
        Ok(json_str) => rust_string_to_jstring(&env, json_str),
        Err(panic_err) => {
            let msg = format!("RUST PANIC in syncPullAllFromAllHostnames: {:?}", panic_err);
            error!("{}", msg);
            rust_string_to_jstring(
                &env,
                json!({"success": false, "error": msg}).to_string(),
            )
        }
    }
}

/// Return this device's identity, as the embedded server sees it.
///
/// `aw-server` mints a persisted UUID v4 on first run (`aw-server/src/device_id.rs`) and
/// `setup_local_remote` names each device's directory in the shared sync folder from it. Kotlin
/// needs the same value -- to recognise which directory under the shared folder is its own, and to
/// key per-device shared state -- so it is read from the running server rather than minted a
/// second time. Two independently generated identities would have to be kept in correspondence
/// forever, and the `.db` path already commits to this one.
#[no_mangle]
pub extern "C" fn Java_net_activitywatch_android_SyncInterface_getDeviceId(
    env: JNIEnv,
    _class: JClass,
    port: i32,
) -> jstring {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let result: Result<String, String> = (|| {
            let client = get_client(port)?;
            let info = client
                .get_info()
                .map_err(|e| format!("Failed to read server info: {}", e))?;
            Ok(json!({
                "success": true,
                "device_id": info.device_id,
                "hostname": info.hostname,
            })
            .to_string())
        })();

        match result {
            Ok(msg) => msg,
            Err(e) => json!({"success": false, "error": e}).to_string(),
        }
    }));

    match result {
        Ok(json_str) => rust_string_to_jstring(&env, json_str),
        Err(panic_err) => {
            let msg = format!("RUST PANIC in getDeviceId: {:?}", panic_err);
            error!("{}", msg);
            rust_string_to_jstring(&env, json!({"success": false, "error": msg}).to_string())
        }
    }
}
