// This should ERROR - schema_version must be an integer literal, not a string
#[event_data(schema_version = "not_an_int")]
pub enum BadSchemaVersionEvent {
    A,
}

pub trait EventData: serde::Serialize + DeserializeOwned + Send + Sync {
    fn event_type(&self) -> &str;
}

unsafe impl EventData for BadSchemaVersionEvent {
    fn event_type(&self) -> &str {
        "BadSchemaVersionEvent"
    }
}