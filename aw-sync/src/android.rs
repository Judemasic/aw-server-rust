use std::panic::{self, catch_unwind, AssertUnwindSafe};

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
            .with_max_level(log::LevelFilter::from_level(match verbosity {
                0 => log::Level::Error,
                1 => log::Level::Warn,
                2 => log::Level::Info,
                3 => log::Level::Debug,
                _ => log::Level::Trace,
            }))
            .with_tag("aw-sync"),
    );
}

/// Helper function to convert Rust string to Java string
fn rust_string_to_jstring(env: &JNIEnv, s: String) -> jstring {
    let output = env.new_string(s).expect("Couldn't create java string!");
    output.into_raw()
}

/// Helper function to get AwClient from port
fn get_client(port: i32) -> Result<AwClient, String> {
    let host = "127.0.0.1";
    AwClient::new(host, port as u16, "aw-sync-android")
        .map_err(|e| format!("Failed to create client: {}", e))
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
