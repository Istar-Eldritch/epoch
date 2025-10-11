//! This module defines the `Event` struct and its associated `EventBuilder` for creating new events.
//! It also provides the `EventData` trait, which must be implemented by any data structure used as an
//! event's payload, and error types for event creation and conversion.

use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

/// Event definition
#[derive(Debug, Clone, PartialEq)]
pub struct Event<D>
where
    D: EventData,
{
    /// Event ID
    pub id: Uuid,

    /// The id of the entity this event aggregates to
    pub stream_id: Uuid,

    /// The version number of the envent. Used to check for sync issues
    pub stream_version: u64,

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

impl<D> Event<D>
where
    D: EventData,
{
    /// Creates a new `EventBuilder` instance.
    pub fn builder() -> EventBuilder<D> {
        EventBuilder::new()
    }

    /// Converts `self` into an `EventBuilder`.
    pub fn into_builder(self) -> EventBuilder<D> {
        self.into()
    }

    /// Transforms this event
    pub fn to_subset_event<ED>(&self) -> Result<Event<ED>, ED::Error>
    where
        ED: EventData + TryFrom<D>,
        ED::Error: Send + Sync,
    {
        let data = self
            .data
            .as_ref()
            .map(|d| ED::try_from(d.clone()))
            .transpose()?;
        Ok(Event {
            id: self.id,
            stream_id: self.stream_id,
            stream_version: self.stream_version,
            event_type: self.event_type.clone(),
            actor_id: self.actor_id,
            purger_id: self.purger_id,
            data,
            created_at: self.created_at,
            purged_at: self.purged_at,
        })
    }

    /// Transforms this event
    pub fn to_superset_event<ED>(&self) -> Event<ED>
    where
        ED: EventData,
        D: Into<ED>,
    {
        let data = self.data.as_ref().map(|d| d.clone().into());
        Event {
            id: self.id,
            stream_id: self.stream_id,
            stream_version: self.stream_version,
            event_type: self.event_type.clone(),
            actor_id: self.actor_id,
            purger_id: self.purger_id,
            data,
            created_at: self.created_at,
            purged_at: self.purged_at,
        }
    }
}

impl<D> From<Event<D>> for EventBuilder<D>
where
    D: EventData,
{
    fn from(event: Event<D>) -> Self {
        Self {
            id: Some(event.id),
            stream_id: Some(event.stream_id),
            stream_version: Some(event.stream_version),
            event_type: Some(event.event_type),
            actor_id: event.actor_id,
            purger_id: event.purger_id,
            data: event.data,
            created_at: Some(event.created_at),
            purged_at: event.purged_at,
        }
    }
}

impl<D> From<D> for EventBuilder<D>
where
    D: EventData,
{
    fn from(data: D) -> Self {
        Self {
            data: Some(data.clone()),
            event_type: Some(data.event_type().to_string()),
            ..EventBuilder::new()
        }
    }
}

/// Builder for `Event`
#[derive(Debug)]
pub struct EventBuilder<D>
where
    D: EventData,
{
    /// The event ID.
    pub id: Option<Uuid>,
    /// The stream ID
    pub stream_id: Option<Uuid>,
    /// The sequence number of the event.
    pub stream_version: Option<u64>,
    /// The event type.
    pub event_type: Option<String>,
    /// The ID of the creator of this event.
    pub actor_id: Option<Uuid>,
    /// The ID of the purger of this event.
    pub purger_id: Option<Uuid>,
    /// The event data.
    pub data: Option<D>,
    /// The time at which this event was created.
    pub created_at: Option<DateTime<Utc>>,
    /// The time at which this event was purged, if any.
    pub purged_at: Option<DateTime<Utc>>,
}

impl<D> Default for EventBuilder<D>
where
    D: EventData,
 {
    fn default() -> Self {
        Self::new()
    }
}

impl<D> EventBuilder<D>
where
    D: EventData,
{
    /// Creates a new `EventBuilder` instance with all fields set to `None`.
    pub fn new() -> Self {
        EventBuilder {
            id: None,
            stream_id: None,
            stream_version: None,
            event_type: None,
            actor_id: None,
            purger_id: None,
            data: None,
            created_at: None,
            purged_at: None,
        }
    }

    /// Sets the ID for the event.
    pub fn id(mut self, id: Uuid) -> Self {
        self.id = Some(id);
        self
    }

    /// Sets the stream ID for the event.
    pub fn stream_id(mut self, stream_id: Uuid) -> Self {
        self.stream_id = Some(stream_id);
        self
    }

    /// Sets the sequence number for the event.
    pub fn stream_version(mut self, sequence_number: u64) -> Self {
        self.stream_version = Some(sequence_number);
        self
    }

    /// Sets the event type for the event.
    pub fn event_type(mut self, event_type: String) -> Self {
        self.event_type = Some(event_type);
        self
    }

    /// Sets the actor ID for the event.
    pub fn actor_id(mut self, actor_id: Uuid) -> Self {
        self.actor_id = Some(actor_id);
        self
    }

    /// Sets the purger ID for the event.
    pub fn purger_id(mut self, purger_id: Uuid) -> Self {
        self.purger_id = Some(purger_id);
        self
    }

    /// Sets the data payload for the event.
    pub fn data<P: EventData>(self, data: Option<P>) -> EventBuilder<P> {
        EventBuilder {
            id: self.id,
            stream_id: self.stream_id,
            stream_version: self.stream_version,
            event_type: self.event_type,
            actor_id: self.actor_id,
            purger_id: self.purger_id,
            data,
            created_at: self.created_at,
            purged_at: self.purged_at,
        }
    }

    /// Sets the creation timestamp for the event.
    pub fn created_at(mut self, created_at: DateTime<Utc>) -> Self {
        self.created_at = Some(created_at);
        self
    }

    /// Sets the purged timestamp for the event.
    pub fn purged_at(mut self, purged_at: DateTime<Utc>) -> Self {
        self.purged_at = Some(purged_at);
        self
    }

    /// Builds the `Event` from the `EventBuilder`.
    ///
    /// # Errors
    ///
    /// Returns an error if `id`, `sequence_number`, `event_type`, or `created_at` are not set.
    pub fn build(self) -> Result<Event<D>, EventBuilderError> {
        Ok(Event {
            id: self.id.unwrap_or_else(Uuid::new_v4),
            stream_id: self.stream_id.ok_or(EventBuilderError::StreamIdMissing)?,
            stream_version: self.stream_version.unwrap_or(0),
            event_type: self.event_type.ok_or(EventBuilderError::EventTypeMissing)?,
            actor_id: self.actor_id,
            purger_id: self.purger_id,
            data: self.data,
            created_at: self.created_at.unwrap_or(Utc::now()),
            purged_at: self.purged_at,
        })
    }
}

/// An event's data payload
pub trait EventData: Serialize + serde::de::DeserializeOwned + Sized + Clone + Send {
    /// Get the event type/identifier in PascalCase like `UserCreated` or `PasswordChanged`
    fn event_type(&self) -> &'static str;

    /// Converts `self` into an `EventBuilder`.
    fn into_builder(self) -> EventBuilder<Self>
    where
        Self: Sized,
    {
        self.into()
    }
}

/// Error returned when an event cannot be converted from one type to another.
#[derive(Debug, thiserror::Error)]
#[error("Can't convert enum variant {0} into subset-enum {1}")]
pub struct EnumConversionError(String, String);

impl EnumConversionError {
    /// Creates a new `EnumConversionError`.
    ///
    /// # Arguments
    ///
    /// * `origina_enum_variant` - The name of the original enum variant that could not be converted.
    /// * `subenum` - The name of the sub-enum that the conversion was attempted into.
    pub fn new(origina_enum_variant: String, subenum: String) -> Self {
        EnumConversionError(origina_enum_variant, subenum)
    }
}

/// Errors that can occur when building an `Event`.
#[derive(Debug, thiserror::Error)]
pub enum EventBuilderError {
    /// The event ID is missing.
    #[error("Event Stream ID is required")]
    StreamIdMissing,
    /// The stream version is missing.
    #[error("Event Stream version is required")]
    StreamVersionMissing,
    /// The event type is missing.
    #[error("Event type is required")]
    EventTypeMissing,
}
