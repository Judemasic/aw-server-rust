#[macro_use]
extern crate log;
extern crate chrono;
extern crate serde;
extern crate serde_json;

mod sync;
pub use sync::create_datastore;
pub use sync::EVENT_ORIGIN_KEY;
pub use sync::sync_datastores;
pub use sync::sync_run;
pub use sync::SyncSpec;

mod sync_wrapper;
pub use sync_wrapper::{pull, pull_all};
pub use sync_wrapper::pull_all_from_all_hostnames;
pub use sync_wrapper::{push, push_with_hostname};
pub use sync_wrapper::push_with_hostname_and_device_id;

mod accessmethod;
pub use accessmethod::AccessMethod;

mod dirs;
mod util;

#[cfg(target_os = "android")]
pub mod android;
