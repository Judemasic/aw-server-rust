// Based On the following guide from Mozilla:
//   https://mozilla.github.io/firefox-browser-architecture/experiments/2017-09-21-rust-on-android.html

extern crate android_logger;

use std::ffi::{CStr, CString};
use std::os::raw::c_char;

use crate::device_id;
use crate::dirs;

use android_logger::Config;
use rocket::serde::json::json;

#[no_mangle]
pub extern "C" fn rust_greeting(to: *const c_char) -> *mut c_char {
    let c_str = unsafe { CStr::from_ptr(to) };
    let recipient = match c_str.to_str() {
        Err(_) => "there",
        Ok(string) => string,
    };

    CString::new("Hello ".to_owned() + recipient + " (from Rust!)")
        .unwrap()
        .into_raw()
}

#[cfg(target_os = "android")]
#[allow(non_snake_case)]
pub mod android {
    extern crate jni;

    use self::jni::objects::{JClass, JString};
    use self::jni::sys::{jdouble, jint, jstring};
    use self::jni::JNIEnv;
    use super::*;

    use std::path::PathBuf;

    use crate::endpoints;
    use crate::endpoints::ServerState;
    use aw_client_rust::classes::{classes_from_settings_str, default_classes};
    use aw_client_rust::queries::{
        build_android_canonical_events, AndroidQueryParams, QueryParamsBase,
    };
    use aw_datastore::Datastore;
    use aw_models::{Bucket, Event, TimeInterval};

    static mut DATASTORE: Option<Datastore> = None;

    unsafe fn openDatastore() -> Datastore {
        match DATASTORE {
            Some(ref ds) => ds.clone(),
            None => {
                let db_dir = dirs::db_path("default")
                    .expect("Failed to get db path")
                    .to_str()
                    .unwrap()
                    .to_string();
                DATASTORE = Some(Datastore::new(db_dir, false));
                openDatastore()
            }
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn Java_net_activitywatch_android_RustInterface_greeting(
        env: JNIEnv,
        _: JClass,
        java_pattern: JString,
    ) -> jstring {
        // Our Java companion code might pass-in "world" as a string, hence the name.
        let world = rust_greeting(
            env.get_string(java_pattern)
                .expect("invalid pattern string")
                .as_ptr(),
        );
        // Retake pointer so that we can use it below and allow memory to be freed when it goes out of scope.
        let world_ptr = CString::from_raw(world);
        let output = env
            .new_string(world_ptr.to_str().unwrap())
            .expect("Couldn't create java string!");

        output.into_raw()
    }

    unsafe fn jstring_to_string(env: &JNIEnv, string: JString) -> String {
        let jstr = env.get_string(string).expect("Failed to get Java string");
        jstr.into()
    }

    unsafe fn string_to_jstring(env: &JNIEnv, string: String) -> jstring {
        env.new_string(string)
            .expect("Couldn't create java string")
            .into_raw()
    }

    unsafe fn create_error_object(env: &JNIEnv, msg: String) -> jstring {
        let obj = json!({ "error": &msg });
        string_to_jstring(&env, obj.to_string())
    }

    #[no_mangle]
    pub unsafe extern "C" fn Java_net_activitywatch_android_RustInterface_startServer(
        env: JNIEnv,
        _: JClass,
    ) {
        info!("Starting server...");
        start_server();
        info!("Server exited");
    }

    #[rocket::main]
    async fn start_server() {
        info!("Building server state...");

        // FIXME: Why is unsafe needed here? Can we get rid of it?
        unsafe {
            let server_state: ServerState = endpoints::ServerState {
                datastore: openDatastore(),
                asset_resolver: endpoints::AssetResolver::new(None),
                device_id: device_id::get_device_id(),
            };
            info!("Using server_state:: device_id: {}", server_state.device_id);

            let mut server_config = crate::config::create_config("default", None);
            server_config.port = 5600;

            endpoints::build_rocket(server_state, server_config)
                .launch()
                .await;
        }
    }

    static mut INITIALIZED: bool = false;

    #[no_mangle]
    pub unsafe extern "C" fn Java_net_activitywatch_android_RustInterface_initialize(
        env: JNIEnv,
        _: JClass,
    ) {
        if !INITIALIZED {
            android_logger::init_once(
                Config::default()
                    .with_max_level(log::LevelFilter::Info) // limit log level
                    .with_tag("aw-server-rust"), // logs will show under mytag tag
                                                 //.with_filter( // configure messages for specific crate
                                                 //    FilterBuilder::new()
                                                 //        .parse("debug,hello::crate=error")
                                                 //        .build())
            );
            // Default panic hook writes to stderr, which Android discards
            // (ActivityWatch/aw-android#220). log_panics routes them through
            // android_logger so they appear in logcat.
            log_panics::init();
            info!("Initializing aw-server-rust...");
            debug!("Redirected aw-server-rust stdout/stderr to logcat");
        } else {
            info!("Already initialized");
        }
        INITIALIZED = true;

        // Without this it might not work due to weird error probably arising from Rust optimizing away the JNIEnv:
        //  JNI DETECTED ERROR IN APPLICATION: use of deleted weak global reference
        string_to_jstring(&env, "test".to_string());
    }

    #[no_mangle]
    pub unsafe extern "C" fn Java_net_activitywatch_android_RustInterface_setDataDir(
        env: JNIEnv,
        _: JClass,
        java_dir: JString,
    ) {
        let path = &jstring_to_string(&env, java_dir);
        debug!("Setting android data dir as {}", path);
        dirs::set_android_data_dir(path);
    }

    /// Report the Android app's release version from `/api/0/info` instead of
    /// the aw-server-rust package version, which is the version of a component
    /// rather than of the app the user installed.
    #[no_mangle]
    pub unsafe extern "C" fn Java_net_activitywatch_android_RustInterface_setVersionOverride(
        env: JNIEnv,
        _: JClass,
        java_version: JString,
    ) {
        let version = &jstring_to_string(&env, java_version);
        debug!("Setting reported version to {}", version);
        crate::version::set_version_override(version);
    }

    #[no_mangle]
    pub unsafe extern "C" fn Java_net_activitywatch_android_RustInterface_getBuckets(
        env: JNIEnv,
        _: JClass,
    ) -> jstring {
        let buckets = openDatastore().get_buckets().unwrap();
        string_to_jstring(&env, json!(buckets).to_string())
    }

    #[no_mangle]
    pub unsafe extern "C" fn Java_net_activitywatch_android_RustInterface_createBucket(
        env: JNIEnv,
        _: JClass,
        java_bucket: JString,
    ) -> jstring {
        let bucket = jstring_to_string(&env, java_bucket);
        let bucket_json: Bucket = match serde_json::from_str(&bucket) {
            Ok(json) => json,
            Err(err) => return create_error_object(&env, err.to_string()),
        };
        match openDatastore().create_bucket(&bucket_json) {
            Ok(()) => string_to_jstring(&env, "Bucket successfully created".to_string()),
            Err(e) => create_error_object(
                &env,
                format!("Something went wrong when trying to create bucket: {:?}", e),
            ),
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn Java_net_activitywatch_android_RustInterface_heartbeat(
        env: JNIEnv,
        _: JClass,
        java_bucket_id: JString,
        java_event: JString,
        java_pulsetime: jdouble,
    ) -> jstring {
        let bucket_id = jstring_to_string(&env, java_bucket_id);
        let event = jstring_to_string(&env, java_event);
        let pulsetime = java_pulsetime as f64;
        let event_json: Event = match serde_json::from_str(&event) {
            Ok(json) => json,
            Err(err) => return create_error_object(&env, err.to_string()),
        };
        match openDatastore().heartbeat(&bucket_id, event_json, pulsetime) {
            Ok(_) => string_to_jstring(&env, "Heartbeat successfully received".to_string()),
            Err(e) => create_error_object(
                &env,
                format!(
                    "Something went wrong when trying to send heartbeat: {:?}",
                    e
                ),
            ),
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn Java_net_activitywatch_android_RustInterface_getEvents(
        env: JNIEnv,
        _: JClass,
        java_bucket_id: JString,
        java_limit: jint,
    ) -> jstring {
        let bucket_id = jstring_to_string(&env, java_bucket_id);
        let limit = java_limit as u64;
        match openDatastore().get_events(&bucket_id, None, None, Some(limit)) {
            Ok(events) => string_to_jstring(&env, json!(events).to_string()),
            Err(e) => create_error_object(
                &env,
                format!("Something went wrong when trying to get events: {:?}", e),
            ),
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn Java_net_activitywatch_android_RustInterface_migrateHostname(
        env: JNIEnv,
        _: JClass,
        hostname: JString,
    ) -> jstring {
        let hostname = jstring_to_string(&env, hostname);
        if hostname.is_empty() {
            return create_error_object(&env, "hostname must not be empty".to_string());
        }
        match openDatastore().migrate_hostname(&hostname) {
            Ok(count) => {
                string_to_jstring(&env, format!("Migrated hostname for {} bucket(s)", count))
            }
            Err(e) => create_error_object(&env, format!("Failed to migrate hostname: {:?}", e)),
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn Java_net_activitywatch_android_RustInterface_migrateAndroidBucketName(
        env: JNIEnv,
        _: JClass,
    ) -> jstring {
        match openDatastore().rename_bucket("aw-android-test", "aw-android") {
            Ok(()) => string_to_jstring(
                &env,
                "Renamed bucket 'aw-android-test' to 'aw-android'".to_string(),
            ),
            Err(e) => create_error_object(&env, format!("Failed to rename bucket: {:?}", e)),
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn Java_net_activitywatch_android_RustInterface_migrateWatcherAndroidBucketNames(
        env: JNIEnv,
        _: JClass,
    ) -> jstring {
        match openDatastore().migrate_test_bucket_names() {
            Ok(count) => string_to_jstring(
                &env,
                format!("Migrated {} 'aw-watcher-android-test' bucket(s)", count),
            ),
            Err(e) => create_error_object(
                &env,
                format!("Failed to migrate watcher bucket names: {:?}", e),
            ),
        }
    }

    /// Return a raw settings JSON value (the datastore body, matching GET /api/0/settings/<key>).
    /// Missing or invalid keys return the JSON literal `null`.
    ///
    /// Widget/worker code must use this instead of unauthenticated HTTP: Android
    /// enables API-key auth by default, so GET /api/0/settings/... from the
    /// widget process 401s and silently falls back to defaults.
    #[no_mangle]
    pub unsafe extern "C" fn Java_net_activitywatch_android_RustInterface_getSetting(
        env: JNIEnv,
        _: JClass,
        java_key: JString,
    ) -> jstring {
        let key = jstring_to_string(&env, java_key);
        // Match GET /api/0/settings/<key>: dots are valid (nested-looking
        // keys like "foo.bar" store as settings.foo.bar). Reject empty keys
        // and path/NUL bytes so JNI cannot smuggle a lookup the HTTP router
        // would never pass through.
        if key.is_empty() || key.contains('/') || key.contains('\\') || key.contains('\0') {
            return string_to_jstring(&env, "null".to_string());
        }
        let setting_key = match crate::endpoints::settings_datastore_key(&key) {
            Ok(k) => k,
            Err(_) => return string_to_jstring(&env, "null".to_string()),
        };
        match openDatastore().get_key_value(&setting_key) {
            Ok(value) => string_to_jstring(&env, value),
            Err(_) => string_to_jstring(&env, "null".to_string()),
        }
    }

    /// Every stored setting, as one JSON object keyed the way `GET /api/0/settings` keys them
    /// (the `settings.` prefix stripped).
    ///
    /// **Values are the stored bodies verbatim, as strings** -- `{"startOfDay": "\"04:00\"",
    /// "classes": "[{...}]"} `-- deliberately unlike the HTTP endpoint, which parses them. The
    /// settings sync (roadmap 2.3) copies a value from one device to another and compares it
    /// against what it last applied; parsing and re-serialising on the way through would reformat
    /// it (key order above all) and make two devices holding the same setting disagree about
    /// whether it had changed. Verbatim strings propagate byte for byte and compare exactly.
    ///
    /// Only settings the user has actually saved appear here -- the datastore holds nothing for a
    /// key still sitting at aw-webui's built-in default. That is what makes it safe for the
    /// settings sync (roadmap 2.3) to publish everything it finds: it publishes choices, never
    /// defaults that merely happen to be this build's.
    ///
    /// JNI rather than HTTP for the reason `getSetting` gives above: API-key auth is on by default
    /// on Android, so an unauthenticated GET from a worker 401s and quietly reads as "no settings".
    #[no_mangle]
    pub unsafe extern "C" fn Java_net_activitywatch_android_RustInterface_getSettings(
        env: JNIEnv,
        _: JClass,
    ) -> jstring {
        match openDatastore().get_key_values("settings.%") {
            Ok(settings) => {
                let mut map = serde_json::Map::new();
                for (key, value) in settings.iter() {
                    let stripped = key.strip_prefix("settings.").unwrap_or(key).to_string();
                    map.insert(stripped, serde_json::Value::String(value.clone()));
                }
                string_to_jstring(&env, serde_json::Value::Object(map).to_string())
            }
            Err(e) => create_error_object(&env, format!("Failed to read settings: {:?}", e)),
        }
    }

    /// Every decision and tombstone this device holds, as a JSON array of the stored lines
    /// (roadmap 4.2).
    ///
    /// The lines, not re-serialised records, for the reason `getSettings` gives about setting
    /// values: the sync copies these into `decisions.jsonl` in the Syncthing folder, and Kotlin
    /// rebuilding them through `JSONObject` would spell them a third way. Storage already put them
    /// in one canonical form (`crate::combined::store_record`), so what leaves here is what every
    /// device holding this decision holds.
    ///
    /// JNI rather than HTTP for the same reason as the settings pair: API-key auth is on by default
    /// on Android, so an unauthenticated GET from a sync worker 401s and reads as "no decisions".
    #[no_mangle]
    pub unsafe extern "C" fn Java_net_activitywatch_android_RustInterface_getDecisions(
        env: JNIEnv,
        _: JClass,
    ) -> jstring {
        match openDatastore().get_key_values(&format!(
            "{}%",
            crate::combined::DECISION_KEY_PREFIX
        )) {
            Ok(stored) => {
                let mut keys: Vec<&String> = stored.keys().collect();
                keys.sort();
                let lines: Vec<serde_json::Value> = keys
                    .into_iter()
                    .map(|k| serde_json::Value::String(stored[k].clone()))
                    .collect();
                string_to_jstring(&env, serde_json::Value::Array(lines).to_string())
            }
            Err(e) => create_error_object(&env, format!("Failed to read decisions: {:?}", e)),
        }
    }

    /// Store decisions and tombstones a peer wrote, as a JSON array of raw lines (roadmap 4.2).
    ///
    /// Keyed by each record's `id`, so importing the whole shared folder every cycle costs a
    /// rewrite of what is already there rather than a duplicate — the sync can be dumb and
    /// repeatable, which is what makes it correct.
    ///
    /// Returns `{"success": true, "stored": n, "skipped": n}`. A line this build cannot read is
    /// skipped, not fatal: `05_DATA_MODEL.md` §8 says a record we do not understand is ignored, and
    /// one bad line must not stop the other devices' decisions from arriving.
    #[no_mangle]
    pub unsafe extern "C" fn Java_net_activitywatch_android_RustInterface_putDecisions(
        env: JNIEnv,
        _: JClass,
        java_lines: JString,
    ) -> jstring {
        let raw = jstring_to_string(&env, java_lines);
        let lines: Vec<String> = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => return create_error_object(&env, format!("not a JSON array of lines: {e}")),
        };
        let ds = openDatastore();
        let (mut stored, mut skipped) = (0u32, 0u32);
        for line in &lines {
            match crate::combined::store_record(&ds, line) {
                Ok(_) => stored += 1,
                Err(_) => skipped += 1,
            }
        }
        string_to_jstring(
            &env,
            json!({ "success": true, "stored": stored, "skipped": skipped }).to_string(),
        )
    }

    /// Write one setting, matching `POST /api/0/settings/<key>`.
    ///
    /// `java_value` is the raw JSON body -- `"fun"` with its quotes for a string, `[...]` for
    /// `classes` -- and is rejected unless it parses, so a malformed value cannot be stored where
    /// aw-webui would later fail to read it.
    ///
    /// Returns `{"success": true}` or an object with an `error`.
    #[no_mangle]
    pub unsafe extern "C" fn Java_net_activitywatch_android_RustInterface_setSetting(
        env: JNIEnv,
        _: JClass,
        java_key: JString,
        java_value: JString,
    ) -> jstring {
        let key = jstring_to_string(&env, java_key);
        // The same guard `getSetting` applies, for the same reason: JNI must not be able to reach
        // a key the HTTP router would refuse to route to.
        if key.is_empty() || key.contains('/') || key.contains('\\') || key.contains('\0') {
            return create_error_object(&env, "invalid settings key".to_string());
        }
        let setting_key = match crate::endpoints::settings_datastore_key(&key) {
            Ok(k) => k,
            Err(msg) => return create_error_object(&env, msg.to_string()),
        };
        let value = jstring_to_string(&env, java_value);
        if serde_json::from_str::<serde_json::Value>(&value).is_err() {
            return create_error_object(&env, format!("value for {} is not valid JSON", key));
        }
        match openDatastore().set_key_value(&setting_key, &value) {
            Ok(()) => string_to_jstring(&env, json!({ "success": true }).to_string()),
            Err(e) => create_error_object(&env, format!("Failed to write {}: {:?}", key, e)),
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn Java_net_activitywatch_android_RustInterface_query(
        env: JNIEnv,
        _: JClass,
        java_query: JString,
        java_timeperiods: JString,
    ) -> jstring {
        let query_code = jstring_to_string(&env, java_query);
        let timeperiods_str = jstring_to_string(&env, java_timeperiods);
        let timeperiods: Vec<TimeInterval> = match serde_json::from_str(&timeperiods_str) {
            Ok(json) => json,
            Err(err) => return create_error_object(&env, err.to_string()),
        };

        let datastore = openDatastore();
        let mut results = Vec::new();

        for interval in &timeperiods {
            let result = match aw_query::query(&query_code, interval, &datastore) {
                Ok(data) => data,
                Err(e) => {
                    return create_error_object(
                        &env,
                        format!("Something went wrong when trying to query: {:?}", e),
                    )
                }
            };
            results.push(result);
        }

        string_to_jstring(&env, json!(results).to_string())
    }

    #[no_mangle]
    pub unsafe extern "C" fn Java_net_activitywatch_android_RustInterface_androidQuery(
        env: JNIEnv,
        _: JClass,
        java_timeperiods: JString,
    ) -> jstring {
        let timeperiods_str = jstring_to_string(&env, java_timeperiods);

        let timeperiods: Vec<TimeInterval> = match serde_json::from_str(&timeperiods_str) {
            Ok(json) => json,
            Err(err) => return create_error_object(&env, err.to_string()),
        };

        // Hardcoded bucket ID
        let bid_android = "aw-watcher-android".to_string();

        // Read classes from the datastore directly. Do NOT fetch them over HTTP:
        // Android enables API-key auth by default, and androidQuery runs from the
        // widget process which does not send a Bearer token. The previous
        // AwClient GET /api/0/settings/classes path 401'd (or failed if the
        // HTTP server wasn't up) and silently fell back to default_classes(),
        // which is why the homescreen widget disagreed with the Activity view
        // on per-category time while totals still matched.
        // See ActivityWatch/aw-android#142.
        let datastore = openDatastore();
        let classes = match datastore.get_key_value("settings.classes") {
            Ok(raw) => {
                info!("Loaded classes from datastore settings.classes");
                classes_from_settings_str(&raw)
            }
            Err(_) => {
                info!("settings.classes unset or unreadable, using default classes");
                default_classes()
            }
        };

        // Build canonical Android query
        let params = AndroidQueryParams {
            base: QueryParamsBase {
                bid_browsers: Vec::new(),
                classes,
                filter_classes: Vec::new(),
                filter_afk: true,
                include_audible: true,
            },
            bid_android,
        };
        let query_code = format!(
            r#"{}
duration = sum_durations(events);
cat_events = sort_by_duration(merge_events_by_keys(events, ["$category"]));
RETURN = {{"events": events, "duration": duration, "cat_events": cat_events}};"#,
            build_android_canonical_events(&params)
        );

        let mut results = Vec::new();

        for interval in &timeperiods {
            let result = match aw_query::query(&query_code, interval, &datastore) {
                Ok(data) => data,
                Err(e) => {
                    return create_error_object(
                        &env,
                        format!("Something went wrong when trying to query: {:?}", e),
                    )
                }
            };
            results.push(result);
        }

        string_to_jstring(&env, json!(results).to_string())
    }
}
