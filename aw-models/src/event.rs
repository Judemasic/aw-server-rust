use chrono::DateTime;
use chrono::Duration;
use chrono::Utc;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Map;
use serde_json::Value;

use crate::duration::DurationSerialization;
use crate::TimeInterval;

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct Event {
    /// An unique id for this event.
    /// Will be assigned once the event has reached the servers datastore.
    ///
    /// **WARNING:** If you set the ID and insert the event to the server it will replace the previous
    /// event with that ID. Only do this if you are completely sure what you are doing.
    pub id: Option<i64>,
    /// An rfc3339 timestamp which represents the start of the event
    pub timestamp: DateTime<Utc>,
    /// Duration of the event as a floating point number in seconds.
    /// Appended to the timestamp it can represent the end of the event
    /// Maximum precision is nanoseconds.
    #[serde(with = "DurationSerialization", default = "default_duration")]
    #[schemars(with = "f64")]
    pub duration: Duration,
    /// Can contain any arbitrary JSON data that represents the value of the event.
    /// All events in a bucket should follow the format of it's respective bucket-type.
    pub data: Map<String, Value>,
}

impl Event {
    pub fn new(timestamp: DateTime<Utc>, duration: Duration, data: Map<String, Value>) -> Self {
        Event {
            id: None,
            timestamp,
            duration,
            data,
        }
    }
    pub fn calculate_endtime(&self) -> DateTime<Utc> {
        self.timestamp + chrono::Duration::nanoseconds(self.duration.num_nanoseconds().unwrap())
    }
    pub fn interval(&self) -> TimeInterval {
        TimeInterval::new(self.timestamp, self.calculate_endtime())
    }
}

impl PartialEq for Event {
    fn eq(&self, other: &Event) -> bool {
        !(self.timestamp != other.timestamp
            || self.duration != other.duration
            || self.data != other.data)
    }
}

impl Default for Event {
    fn default() -> Self {
        Event {
            id: None,
            timestamp: Utc::now(),
            duration: Duration::seconds(0),
            data: serde_json::Map::new(),
        }
    }
}

fn default_duration() -> Duration {
    Duration::seconds(0)
}

/// Event data key holding the UUID of the device an imported event was collected on
/// (roadmap 3.1, `05_DATA_MODEL.md` §6).
///
/// Written only on import, and only onto the *copy* in this device's datastore -- the originating
/// device's own events are never touched (**R11**). Its value is a device UUID, deliberately not a
/// hostname: hostnames are display names that can be changed and are already truncated in places
/// (roadmap 1.10), while `devices/<uuid>/meta.json` and every decision signature key on the UUID.
///
/// Distinct from the bucket-level `$aw.sync.origin`, which holds a *hostname* and exists to build
/// the `-synced-from-<host>` bucket id. Two keys because they answer two questions; sharing one
/// name for two kinds of value is how the pair would eventually be misread.
///
/// Written by aw-sync at merge time (roadmap 3.1); read by aw-combined (roadmap 3.2).
pub const EVENT_ORIGIN_KEY: &str = "$aw.origin.device";

#[test]
fn test_event() {
    use serde_json::json;

    let e = Event {
        id: None,
        timestamp: Utc::now(),
        duration: Duration::seconds(1),
        data: json_map! {"test": json!(1)},
    };
    debug!("event: {:?}", e);
}
