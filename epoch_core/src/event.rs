use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Event definition
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event<D>
where
    D: EventData,
{
    /// Event ID
    pub id: Uuid,

    /// The sequence number of the envent. Used to check for sync issues
    pub sequence_number: u64,

    /// Event type
    ///
    /// The type of this event in PascalCase, like `OrganisationCreated` or `StudyCurated`
    pub event_type: String,

    /// The ID of the creator of this event
    pub actor_id: Option<Uuid>,

    /// Purger subject ID
    ///
    /// Will be `None` if event is not purged
    pub purger_id: Option<Uuid>,

    /// Event data
    ///
    /// If the event has been purged, this will be `None` for security/compliance reasons - the data
    /// must be deleted from both the event log and the aggregate tables. Check the `purged_at` or
    /// `purger_id` fields to check the purge status.
    pub data: Option<D>,

    /// The time at which this event was created
    pub created_at: DateTime<Utc>,

    /// The time at which this event was purged, if any
    pub purged_at: Option<DateTime<Utc>>,
}

/// An event's data payload
pub trait EventData: Serialize + Sized + Clone {
    /// Get the event type/identifier in PascalCase like `UserCreated` or `PasswordChanged`
    fn event_type(&self) -> &'static str;

    /// Creates a new event from the values of a different event but replacing its data
    fn into_event<E: EventData>(self, e: Event<E>) -> Event<Self> {
        Event {
            event_type: self.event_type().to_string(),
            data: Some(self),
            id: e.id,
            actor_id: e.actor_id,
            purger_id: e.purger_id,
            purged_at: e.purged_at,
            created_at: e.created_at,
            sequence_number: e.sequence_number,
        }
    }
}

/// Error returned when an event cannot be converted from one type to another.
#[derive(Debug, thiserror::Error)]
#[error("Can't convert enum variant {0} into subset-enum {1}")]
pub struct EnumConversionError(String, String);

impl EnumConversionError {
    /// Constructor for a new EnumConversionError.
    pub fn new(origina_enum_variant: String, subenum: String) -> Self {
        EnumConversionError(origina_enum_variant, subenum)
    }
}

// EOF
