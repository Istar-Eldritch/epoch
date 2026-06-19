// This should compile to ERROR - schema_version must be an integer, not a string
#[event_data(schema_version = "not_an_int")]
pub enum MalformedSchemaVersionEvent { A }