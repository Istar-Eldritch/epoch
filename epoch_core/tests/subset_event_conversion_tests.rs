//! Integration tests for subset event conversion using `to_subset_event_ref()`.
//!
//! These tests verify that:
//! 1. The `#[subset_enum]` macro generates correct `TryFrom<&D>` implementations
//! 2. `to_subset_event_ref()` correctly converts events to subset types
//! 3. Only matched variant fields are cloned (not the entire enum)

use epoch_core::event::{EnumConversionError, Event, EventData};
use epoch_derive::subset_enum;
use uuid::Uuid;

// ============================================================================
// Test Event Types
// ============================================================================

/// Application-wide event enum with multiple variants
#[subset_enum(UserEvent, UserCreated, UserUpdated)]
#[subset_enum(OrderEvent, OrderCreated, OrderShipped)]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ApplicationEvent {
    UserCreated { name: String, email: String },
    UserUpdated { name: String },
    UserDeleted,
    OrderCreated { product: String, quantity: u32 },
    OrderShipped { tracking_number: String },
    OrderCancelled,
}

impl EventData for ApplicationEvent {
    fn event_type(&self) -> &'static str {
        match self {
            ApplicationEvent::UserCreated { .. } => "UserCreated",
            ApplicationEvent::UserUpdated { .. } => "UserUpdated",
            ApplicationEvent::UserDeleted => "UserDeleted",
            ApplicationEvent::OrderCreated { .. } => "OrderCreated",
            ApplicationEvent::OrderShipped { .. } => "OrderShipped",
            ApplicationEvent::OrderCancelled => "OrderCancelled",
        }
    }
}

impl EventData for UserEvent {
    fn event_type(&self) -> &'static str {
        match self {
            UserEvent::UserCreated { .. } => "UserCreated",
            UserEvent::UserUpdated { .. } => "UserUpdated",
        }
    }
}

impl EventData for OrderEvent {
    fn event_type(&self) -> &'static str {
        match self {
            OrderEvent::OrderCreated { .. } => "OrderCreated",
            OrderEvent::OrderShipped { .. } => "OrderShipped",
        }
    }
}

// ============================================================================
// Basic Conversion Tests
// ============================================================================

#[test]
fn to_subset_event_ref_converts_matching_variant() {
    let event = Event::<ApplicationEvent>::builder()
        .stream_id(Uuid::new_v4())
        .event_type("UserCreated".to_string())
        .data(Some(ApplicationEvent::UserCreated {
            name: "Alice".to_string(),
            email: "alice@example.com".to_string(),
        }))
        .build()
        .unwrap();

    let user_event: Event<UserEvent> = event.to_subset_event_ref().unwrap();

    assert!(matches!(
        user_event.data,
        Some(UserEvent::UserCreated { ref name, ref email })
        if name == "Alice" && email == "alice@example.com"
    ));
}

#[test]
fn to_subset_event_ref_fails_for_non_matching_variant() {
    let event = Event::<ApplicationEvent>::builder()
        .stream_id(Uuid::new_v4())
        .event_type("UserDeleted".to_string())
        .data(Some(ApplicationEvent::UserDeleted))
        .build()
        .unwrap();

    let result: Result<Event<UserEvent>, EnumConversionError> = event.to_subset_event_ref();

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.to_string().contains("UserDeleted"));
}

#[test]
fn to_subset_event_ref_preserves_event_metadata() {
    let stream_id = Uuid::new_v4();
    let actor_id = Uuid::new_v4();

    let event = Event::<ApplicationEvent>::builder()
        .id(Uuid::new_v4())
        .stream_id(stream_id)
        .stream_version(42)
        .event_type("OrderCreated".to_string())
        .actor_id(actor_id)
        .data(Some(ApplicationEvent::OrderCreated {
            product: "Widget".to_string(),
            quantity: 5,
        }))
        .build()
        .unwrap();

    let order_event: Event<OrderEvent> = event.to_subset_event_ref().unwrap();

    assert_eq!(order_event.id, event.id);
    assert_eq!(order_event.stream_id, stream_id);
    assert_eq!(order_event.stream_version, 42);
    assert_eq!(order_event.actor_id, Some(actor_id));
}

#[test]
fn to_subset_event_ref_handles_none_data() {
    let event = Event::<ApplicationEvent>::builder()
        .stream_id(Uuid::new_v4())
        .event_type("UserCreated".to_string())
        .data(None)
        .build()
        .unwrap();

    let user_event: Event<UserEvent> = event.to_subset_event_ref().unwrap();

    assert!(user_event.data.is_none());
}

// ============================================================================
// Tests for different subset enums from the same source
// ============================================================================

#[test]
fn different_subsets_convert_different_variants() {
    let user_event = Event::<ApplicationEvent>::builder()
        .stream_id(Uuid::new_v4())
        .event_type("UserCreated".to_string())
        .data(Some(ApplicationEvent::UserCreated {
            name: "Bob".to_string(),
            email: "bob@example.com".to_string(),
        }))
        .build()
        .unwrap();

    let order_event = Event::<ApplicationEvent>::builder()
        .stream_id(Uuid::new_v4())
        .event_type("OrderCreated".to_string())
        .data(Some(ApplicationEvent::OrderCreated {
            product: "Gadget".to_string(),
            quantity: 3,
        }))
        .build()
        .unwrap();

    // UserEvent can convert user events but not order events
    assert!(user_event.to_subset_event_ref::<UserEvent>().is_ok());
    assert!(user_event.to_subset_event_ref::<OrderEvent>().is_err());

    // OrderEvent can convert order events but not user events
    assert!(order_event.to_subset_event_ref::<OrderEvent>().is_ok());
    assert!(order_event.to_subset_event_ref::<UserEvent>().is_err());
}

// ============================================================================
// Comparison with owned conversion
// ============================================================================

#[test]
fn to_subset_event_and_to_subset_event_ref_produce_same_result() {
    let event = Event::<ApplicationEvent>::builder()
        .stream_id(Uuid::new_v4())
        .event_type("UserUpdated".to_string())
        .data(Some(ApplicationEvent::UserUpdated {
            name: "Charlie".to_string(),
        }))
        .build()
        .unwrap();

    let ref_result: Event<UserEvent> = event.to_subset_event_ref().unwrap();
    let owned_result: Event<UserEvent> = event.to_subset_event().unwrap();

    assert_eq!(ref_result.data, owned_result.data);
    assert_eq!(ref_result.id, owned_result.id);
    assert_eq!(ref_result.stream_id, owned_result.stream_id);
}

// ============================================================================
// Unit variant tests
// ============================================================================

/// Subset with unit variants
#[subset_enum(StatusEvent, Started, Stopped)]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SystemEvent {
    Started,
    Stopped,
    Error { message: String },
}

impl EventData for SystemEvent {
    fn event_type(&self) -> &'static str {
        match self {
            SystemEvent::Started => "Started",
            SystemEvent::Stopped => "Stopped",
            SystemEvent::Error { .. } => "Error",
        }
    }
}

impl EventData for StatusEvent {
    fn event_type(&self) -> &'static str {
        match self {
            StatusEvent::Started => "Started",
            StatusEvent::Stopped => "Stopped",
        }
    }
}

#[test]
fn to_subset_event_ref_handles_unit_variants() {
    let event = Event::<SystemEvent>::builder()
        .stream_id(Uuid::new_v4())
        .event_type("Started".to_string())
        .data(Some(SystemEvent::Started))
        .build()
        .unwrap();

    let status_event: Event<StatusEvent> = event.to_subset_event_ref().unwrap();

    assert_eq!(status_event.data, Some(StatusEvent::Started));
}

#[test]
fn to_subset_event_ref_rejects_excluded_unit_variant() {
    let event = Event::<SystemEvent>::builder()
        .stream_id(Uuid::new_v4())
        .event_type("Error".to_string())
        .data(Some(SystemEvent::Error {
            message: "oops".to_string(),
        }))
        .build()
        .unwrap();

    let result: Result<Event<StatusEvent>, _> = event.to_subset_event_ref();

    assert!(result.is_err());
}

// ============================================================================
// Tuple variant tests
// ============================================================================

/// Subset with tuple variants
#[subset_enum(PositionEvent, Moved, Jumped)]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum GameEvent {
    Moved(i32, i32),
    Jumped(i32),
    Died,
}

impl EventData for GameEvent {
    fn event_type(&self) -> &'static str {
        match self {
            GameEvent::Moved(_, _) => "Moved",
            GameEvent::Jumped(_) => "Jumped",
            GameEvent::Died => "Died",
        }
    }
}

impl EventData for PositionEvent {
    fn event_type(&self) -> &'static str {
        match self {
            PositionEvent::Moved(_, _) => "Moved",
            PositionEvent::Jumped(_) => "Jumped",
        }
    }
}

#[test]
fn to_subset_event_ref_handles_tuple_variants() {
    let event = Event::<GameEvent>::builder()
        .stream_id(Uuid::new_v4())
        .event_type("Moved".to_string())
        .data(Some(GameEvent::Moved(10, 20)))
        .build()
        .unwrap();

    let position_event: Event<PositionEvent> = event.to_subset_event_ref().unwrap();

    assert_eq!(position_event.data, Some(PositionEvent::Moved(10, 20)));
}

#[test]
fn to_subset_event_ref_rejects_excluded_tuple_variant() {
    let event = Event::<GameEvent>::builder()
        .stream_id(Uuid::new_v4())
        .event_type("Died".to_string())
        .data(Some(GameEvent::Died))
        .build()
        .unwrap();

    let result: Result<Event<PositionEvent>, _> = event.to_subset_event_ref();

    assert!(result.is_err());
}

// ============================================================================
// Identity conversion tests (EventType == ED)
// ============================================================================

/// For identity conversion, we need a manual TryFrom<&Self> impl
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SimpleEvent {
    Created,
    Updated { value: i32 },
}

impl EventData for SimpleEvent {
    fn event_type(&self) -> &'static str {
        match self {
            SimpleEvent::Created => "Created",
            SimpleEvent::Updated { .. } => "Updated",
        }
    }
}

impl TryFrom<&SimpleEvent> for SimpleEvent {
    type Error = EnumConversionError;

    fn try_from(value: &SimpleEvent) -> Result<Self, Self::Error> {
        Ok(value.clone())
    }
}

#[test]
fn to_subset_event_ref_works_with_identity_conversion() {
    let event = Event::<SimpleEvent>::builder()
        .stream_id(Uuid::new_v4())
        .event_type("Updated".to_string())
        .data(Some(SimpleEvent::Updated { value: 42 }))
        .build()
        .unwrap();

    // Identity conversion: SimpleEvent -> SimpleEvent
    let result: Event<SimpleEvent> = event.to_subset_event_ref().unwrap();

    assert_eq!(result.data, Some(SimpleEvent::Updated { value: 42 }));
}
