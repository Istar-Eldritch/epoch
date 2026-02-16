//! # Event Correlation & Causation Example
//!
//! This example demonstrates how Epoch tracks **correlation** and **causation**
//! metadata through a multi-aggregate saga workflow, and how to query events
//! using the correlation/causation APIs.
//!
//! ## Concepts
//!
//! - **Correlation ID**: A shared identifier tying together all events in a causal
//!   tree originating from the same user action. Set once at the entry point with
//!   `Command::with_correlation_id()` and automatically propagated through the chain.
//!
//! - **Causation ID**: Points to the specific event that directly caused this event
//!   to be produced. Set via `Command::caused_by(&event)` in saga handlers.
//!
//! ## Scenario
//!
//! A simplified order fulfillment flow:
//!
//! ```text
//! PlaceOrder command (correlation_id = trace_id)
//!   └─ OrderPlaced event (causation_id = None, correlation_id = trace_id)
//!        ├─ Saga dispatches NotifyPlacement command (caused_by OrderPlaced)
//!        │    └─ PlacementNotificationSent event (causation_id = OrderPlaced.id)
//!        └─ Saga dispatches ShipOrder command (caused_by OrderPlaced)
//!             └─ OrderShipped event (causation_id = OrderPlaced.id)
//!                  ├─ Saga dispatches ConfirmOrder command (caused_by OrderShipped)
//!                  │    └─ OrderConfirmed event (causation_id = OrderShipped.id)
//!                  └─ Saga dispatches NotifyShipment command (caused_by OrderShipped)
//!                       └─ ShipmentNotificationSent event (causation_id = OrderShipped.id)
//! ```
//!
//! All events are part of the causation chain, branching from OrderPlaced and OrderShipped.
//!
//! After the workflow completes, we query events using:
//! - `read_events_by_correlation_id()` — all events sharing the trace ID
//! - `trace_causation_chain()` — the direct causal path through a specific event

use async_trait::async_trait;
use epoch::prelude::*;
use epoch_mem::{InMemoryEventBus, InMemoryEventStore, InMemoryStateStore};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

// ============================================================================
// Commands
// ============================================================================

#[subset_enum(OrderCommand, PlaceOrder, ConfirmOrder)]
#[subset_enum(ShippingCommand, ShipOrder)]
#[subset_enum(NotificationCommand, NotifyPlacement, NotifyShipment)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AppCommand {
    PlaceOrder { items: Vec<String> },
    ShipOrder { order_id: Uuid },
    ConfirmOrder { order_id: Uuid },
    NotifyPlacement { order_id: Uuid },
    NotifyShipment { order_id: Uuid },
}

// ============================================================================
// Events
// ============================================================================

#[subset_enum(OrderEvent, OrderPlaced, OrderConfirmed)]
#[subset_enum(ShippingEvent, OrderShipped)]
#[subset_enum(NotificationEvent, PlacementNotificationSent, ShipmentNotificationSent)]
#[subset_enum(
    FulfillmentEvent,
    OrderPlaced,
    OrderShipped,
    OrderConfirmed,
    PlacementNotificationSent,
    ShipmentNotificationSent
)]
#[derive(Debug, Clone, Serialize, Deserialize, EventData)]
pub enum AppEvent {
    OrderPlaced { items: Vec<String> },
    OrderShipped { order_id: Uuid },
    OrderConfirmed { order_id: Uuid },
    PlacementNotificationSent { order_id: Uuid },
    ShipmentNotificationSent { order_id: Uuid },
}

// ============================================================================
// Order Aggregate
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderState {
    id: Uuid,
    items: Vec<String>,
    status: String,
    version: u64,
}

impl EventApplicatorState for OrderState {
    fn get_id(&self) -> &Uuid {
        &self.id
    }
}

impl AggregateState for OrderState {
    fn get_version(&self) -> u64 {
        self.version
    }
    fn set_version(&mut self, version: u64) {
        self.version = version;
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OrderError {
    #[error("Event build error: {0}")]
    EventBuild(#[from] EventBuilderError),
}

pub struct OrderAggregate {
    state_store: InMemoryStateStore<OrderState>,
    event_store: InMemoryEventStore<InMemoryEventBus<AppEvent>>,
}

impl EventApplicator<AppEvent> for OrderAggregate {
    type State = OrderState;
    type EventType = OrderEvent;
    type StateStore = InMemoryStateStore<OrderState>;
    type ApplyError = OrderError;

    fn get_state_store(&self) -> Self::StateStore {
        self.state_store.clone()
    }

    fn apply(
        &self,
        state: Option<Self::State>,
        event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, Self::ApplyError> {
        match event.data.as_ref().unwrap() {
            OrderEvent::OrderPlaced { items } => Ok(Some(OrderState {
                id: event.stream_id,
                items: items.clone(),
                status: "placed".to_string(),
                version: 0,
            })),
            OrderEvent::OrderConfirmed { .. } => {
                let mut s = state.unwrap();
                s.status = "confirmed".to_string();
                Ok(Some(s))
            }
        }
    }
}

#[async_trait]
impl Aggregate<AppEvent> for OrderAggregate {
    type CommandData = AppCommand;
    type CommandCredentials = ();
    type Command = OrderCommand;
    type AggregateError = OrderError;
    type EventStore = InMemoryEventStore<InMemoryEventBus<AppEvent>>;

    fn get_event_store(&self) -> Self::EventStore {
        self.event_store.clone()
    }

    async fn handle_command(
        &self,
        _state: &Option<Self::State>,
        command: Command<Self::Command, Self::CommandCredentials>,
    ) -> Result<Vec<Event<AppEvent>>, Self::AggregateError> {
        match command.data {
            OrderCommand::PlaceOrder { items } => {
                let event = AppEvent::OrderPlaced { items }
                    .into_builder()
                    .stream_id(command.aggregate_id)
                    .build()?;
                Ok(vec![event])
            }
            OrderCommand::ConfirmOrder { order_id } => {
                let event = AppEvent::OrderConfirmed { order_id }
                    .into_builder()
                    .stream_id(command.aggregate_id)
                    .build()?;
                Ok(vec![event])
            }
        }
    }
}

// ============================================================================
// Shipping Aggregate
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShippingState {
    id: Uuid,
    order_id: Uuid,
    shipped: bool,
    version: u64,
}

impl EventApplicatorState for ShippingState {
    fn get_id(&self) -> &Uuid {
        &self.id
    }
}

impl AggregateState for ShippingState {
    fn get_version(&self) -> u64 {
        self.version
    }
    fn set_version(&mut self, version: u64) {
        self.version = version;
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ShippingError {
    #[error("Event build error: {0}")]
    EventBuild(#[from] EventBuilderError),
}

pub struct ShippingAggregate {
    state_store: InMemoryStateStore<ShippingState>,
    event_store: InMemoryEventStore<InMemoryEventBus<AppEvent>>,
}

impl EventApplicator<AppEvent> for ShippingAggregate {
    type State = ShippingState;
    type EventType = ShippingEvent;
    type StateStore = InMemoryStateStore<ShippingState>;
    type ApplyError = ShippingError;

    fn get_state_store(&self) -> Self::StateStore {
        self.state_store.clone()
    }

    fn apply(
        &self,
        _state: Option<Self::State>,
        event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, Self::ApplyError> {
        match event.data.as_ref().unwrap() {
            ShippingEvent::OrderShipped { order_id } => Ok(Some(ShippingState {
                id: event.stream_id,
                order_id: *order_id,
                shipped: true,
                version: 0,
            })),
        }
    }
}

#[async_trait]
impl Aggregate<AppEvent> for ShippingAggregate {
    type CommandData = AppCommand;
    type CommandCredentials = ();
    type Command = ShippingCommand;
    type AggregateError = ShippingError;
    type EventStore = InMemoryEventStore<InMemoryEventBus<AppEvent>>;

    fn get_event_store(&self) -> Self::EventStore {
        self.event_store.clone()
    }

    async fn handle_command(
        &self,
        _state: &Option<Self::State>,
        command: Command<Self::Command, Self::CommandCredentials>,
    ) -> Result<Vec<Event<AppEvent>>, Self::AggregateError> {
        match command.data {
            ShippingCommand::ShipOrder { order_id } => {
                let event = AppEvent::OrderShipped { order_id }
                    .into_builder()
                    .stream_id(command.aggregate_id)
                    .build()?;
                Ok(vec![event])
            }
        }
    }
}

// ============================================================================
// Notification Aggregate
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationState {
    id: Uuid,
    order_id: Uuid,
    notified: bool,
    version: u64,
}

impl EventApplicatorState for NotificationState {
    fn get_id(&self) -> &Uuid {
        &self.id
    }
}

impl AggregateState for NotificationState {
    fn get_version(&self) -> u64 {
        self.version
    }
    fn set_version(&mut self, version: u64) {
        self.version = version;
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NotificationError {
    #[error("Event build error: {0}")]
    EventBuild(#[from] EventBuilderError),
}

pub struct NotificationAggregate {
    state_store: InMemoryStateStore<NotificationState>,
    event_store: InMemoryEventStore<InMemoryEventBus<AppEvent>>,
}

impl EventApplicator<AppEvent> for NotificationAggregate {
    type State = NotificationState;
    type EventType = NotificationEvent;
    type StateStore = InMemoryStateStore<NotificationState>;
    type ApplyError = NotificationError;

    fn get_state_store(&self) -> Self::StateStore {
        self.state_store.clone()
    }

    fn apply(
        &self,
        _state: Option<Self::State>,
        event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, Self::ApplyError> {
        match event.data.as_ref().unwrap() {
            NotificationEvent::PlacementNotificationSent { order_id } => {
                Ok(Some(NotificationState {
                    id: event.stream_id,
                    order_id: *order_id,
                    notified: true,
                    version: 0,
                }))
            }
            NotificationEvent::ShipmentNotificationSent { order_id } => {
                Ok(Some(NotificationState {
                    id: event.stream_id,
                    order_id: *order_id,
                    notified: true,
                    version: 0,
                }))
            }
        }
    }
}

#[async_trait]
impl Aggregate<AppEvent> for NotificationAggregate {
    type CommandData = AppCommand;
    type CommandCredentials = ();
    type Command = NotificationCommand;
    type AggregateError = NotificationError;
    type EventStore = InMemoryEventStore<InMemoryEventBus<AppEvent>>;

    fn get_event_store(&self) -> Self::EventStore {
        self.event_store.clone()
    }

    async fn handle_command(
        &self,
        _state: &Option<Self::State>,
        command: Command<Self::Command, Self::CommandCredentials>,
    ) -> Result<Vec<Event<AppEvent>>, Self::AggregateError> {
        match command.data {
            NotificationCommand::NotifyPlacement { order_id } => {
                let event = AppEvent::PlacementNotificationSent { order_id }
                    .into_builder()
                    .stream_id(command.aggregate_id)
                    .build()?;
                Ok(vec![event])
            }
            NotificationCommand::NotifyShipment { order_id } => {
                let event = AppEvent::ShipmentNotificationSent { order_id }
                    .into_builder()
                    .stream_id(command.aggregate_id)
                    .build()?;
                Ok(vec![event])
            }
        }
    }
}

// ============================================================================
// Fulfillment Saga (with causation tracking)
// ============================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum FulfillmentState {
    #[default]
    Pending,
    Shipped {
        order_id: Uuid,
    },
    Completed,
}

#[derive(Debug, thiserror::Error)]
pub enum FulfillmentError {
    #[error("Aggregate error: {0}")]
    Aggregate(String),
}

#[derive(epoch_derive::SubscriberId)]
#[subscriber_id(prefix = "saga")]
pub struct FulfillmentSaga {
    state_store: InMemoryStateStore<FulfillmentState>,
    order_aggregate: Arc<OrderAggregate>,
    shipping_aggregate: Arc<ShippingAggregate>,
    notification_aggregate: Arc<NotificationAggregate>,
}

#[async_trait]
impl Saga<AppEvent> for FulfillmentSaga {
    type State = FulfillmentState;
    type StateStore = InMemoryStateStore<FulfillmentState>;
    type SagaError = FulfillmentError;
    type EventType = FulfillmentEvent;

    fn get_state_store(&self) -> Self::StateStore {
        self.state_store.clone()
    }

    fn get_id_from_event(&self, event: &Event<Self::EventType>) -> Uuid {
        match event.data.as_ref().unwrap() {
            FulfillmentEvent::OrderPlaced { .. } => event.stream_id,
            FulfillmentEvent::OrderShipped { order_id } => *order_id,
            FulfillmentEvent::OrderConfirmed { order_id } => *order_id,
            FulfillmentEvent::PlacementNotificationSent { order_id } => *order_id,
            FulfillmentEvent::ShipmentNotificationSent { order_id } => *order_id,
        }
    }

    async fn handle_event(
        &self,
        state: Self::State,
        event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, Self::SagaError> {
        match (&state, event.data.as_ref().unwrap()) {
            // Step 1: OrderPlaced → dispatch NotifyPlacement and ShipOrder (both caused_by OrderPlaced)
            (FulfillmentState::Pending, FulfillmentEvent::OrderPlaced { .. }) => {
                println!(
                    "  🔗 Saga: OrderPlaced received, dispatching NotifyPlacement (caused_by event {})",
                    event.id
                );

                let notification_id = Uuid::new_v4();
                let notify_cmd = Command::new(
                    notification_id,
                    AppCommand::NotifyPlacement {
                        order_id: event.stream_id,
                    },
                    None,
                    None,
                )
                .caused_by(event); // ← Key: links causation chain

                self.notification_aggregate
                    .handle(notify_cmd)
                    .await
                    .map_err(|e| FulfillmentError::Aggregate(e.to_string()))?;

                println!(
                    "  🔗 Saga: OrderPlaced received, dispatching ShipOrder (caused_by event {})",
                    event.id
                );

                let shipping_id = Uuid::new_v4();
                let ship_cmd = Command::new(
                    shipping_id,
                    AppCommand::ShipOrder {
                        order_id: event.stream_id,
                    },
                    None,
                    None,
                )
                .caused_by(event); // ← Key: links causation chain

                self.shipping_aggregate
                    .handle(ship_cmd)
                    .await
                    .map_err(|e| FulfillmentError::Aggregate(e.to_string()))?;

                Ok(Some(FulfillmentState::Shipped {
                    order_id: event.stream_id,
                }))
            }

            // Step 2: OrderShipped → dispatch ConfirmOrder and NotifyShipment (both caused_by OrderShipped)
            (FulfillmentState::Shipped { order_id }, FulfillmentEvent::OrderShipped { .. }) => {
                println!(
                    "  🔗 Saga: OrderShipped received, dispatching ConfirmOrder (caused_by event {})",
                    event.id
                );

                let confirm_cmd = Command::new(
                    *order_id,
                    AppCommand::ConfirmOrder {
                        order_id: *order_id,
                    },
                    None,
                    None,
                )
                .caused_by(event); // ← Key: links causation chain

                self.order_aggregate
                    .handle(confirm_cmd)
                    .await
                    .map_err(|e| FulfillmentError::Aggregate(e.to_string()))?;

                println!(
                    "  🔗 Saga: OrderShipped received, dispatching NotifyShipment (caused_by event {})",
                    event.id
                );

                let notification_id = Uuid::new_v4();
                let notify_cmd = Command::new(
                    notification_id,
                    AppCommand::NotifyShipment {
                        order_id: *order_id,
                    },
                    None,
                    None,
                )
                .caused_by(event); // ← Key: links causation chain

                self.notification_aggregate
                    .handle(notify_cmd)
                    .await
                    .map_err(|e| FulfillmentError::Aggregate(e.to_string()))?;

                Ok(Some(FulfillmentState::Shipped {
                    order_id: *order_id,
                }))
            }

            // Step 3: OrderConfirmed → saga complete
            (_, FulfillmentEvent::OrderConfirmed { order_id }) => {
                println!("  ✅ Saga: OrderConfirmed received for {order_id}, saga complete");
                Ok(Some(FulfillmentState::Completed))
            }

            // Step 4: PlacementNotificationSent → just log it
            (_, FulfillmentEvent::PlacementNotificationSent { order_id }) => {
                println!("  📧 Saga: PlacementNotificationSent received for {order_id}");
                Ok(Some(state))
            }

            // Step 5: ShipmentNotificationSent → just log it
            (_, FulfillmentEvent::ShipmentNotificationSent { order_id }) => {
                println!("  📧 Saga: ShipmentNotificationSent received for {order_id}");
                Ok(Some(state))
            }

            // Catch-all
            _ => Ok(Some(state)),
        }
    }
}

// ============================================================================
// Main
// ============================================================================

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("═══════════════════════════════════════════════════════════════");
    println!("  Event Correlation & Causation Example");
    println!("═══════════════════════════════════════════════════════════════");
    println!();

    // -- Infrastructure --
    let bus: InMemoryEventBus<AppEvent> = InMemoryEventBus::new();
    let event_store = InMemoryEventStore::new(bus.clone());

    let order_aggregate = Arc::new(OrderAggregate {
        state_store: InMemoryStateStore::new(),
        event_store: event_store.clone(),
    });

    let shipping_aggregate = Arc::new(ShippingAggregate {
        state_store: InMemoryStateStore::new(),
        event_store: event_store.clone(),
    });

    let notification_aggregate = Arc::new(NotificationAggregate {
        state_store: InMemoryStateStore::new(),
        event_store: event_store.clone(),
    });

    let saga = FulfillmentSaga {
        state_store: InMemoryStateStore::new(),
        order_aggregate: order_aggregate.clone(),
        shipping_aggregate: shipping_aggregate.clone(),
        notification_aggregate: notification_aggregate.clone(),
    };

    bus.subscribe(SagaHandler::new(saga)).await?;

    // -- Execute workflow --
    let order_id = Uuid::new_v4();
    let trace_id = Uuid::new_v4(); // Our external trace/correlation ID

    println!("1️⃣  Placing order {order_id}");
    println!("   with correlation_id (trace) = {trace_id}");
    println!();

    // The entry-point command gets an explicit correlation ID
    let cmd = Command::new(
        order_id,
        AppCommand::PlaceOrder {
            items: vec!["Widget A".into(), "Widget B".into()],
        },
        None,
        None,
    )
    .with_correlation_id(trace_id);

    order_aggregate.handle(cmd).await?;

    // Wait for the async saga chain to complete
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    // ========================================================================
    // Query: read_events_by_correlation_id
    // ========================================================================
    println!();
    println!("═══════════════════════════════════════════════════════════════");
    println!("  Query: read_events_by_correlation_id({trace_id})");
    println!("═══════════════════════════════════════════════════════════════");
    println!();

    let correlated_events = event_store.read_events_by_correlation_id(trace_id).await?;

    println!(
        "  Found {} events sharing correlation_id {trace_id}:",
        correlated_events.len()
    );
    println!();
    println!(
        "  {:<4} {:<20} {:<38} {:<38}",
        "#", "Event Type", "Event ID", "Causation ID"
    );
    println!("  {}", "─".repeat(100));

    for (i, evt) in correlated_events.iter().enumerate() {
        let causation = evt
            .causation_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "(none — root)".to_string());
        println!(
            "  {:<4} {:<20} {:<38} {:<38}",
            i + 1,
            evt.event_type,
            evt.id,
            causation,
        );
    }

    // ========================================================================
    // Query: trace_causation_chain (from OrderShipped)
    // ========================================================================
    let shipped_event = correlated_events
        .iter()
        .find(|e| e.event_type == "OrderShipped");
    if let Some(shipped_event) = shipped_event {
        println!();
        println!("═══════════════════════════════════════════════════════════════");
        println!("  Query: trace_causation_chain({})", shipped_event.id);
        println!("  (tracing from event: {})", shipped_event.event_type);
        println!("═══════════════════════════════════════════════════════════════");
        println!();

        let chain = event_store.trace_causation_chain(shipped_event.id).await?;

        println!("  Causal chain ({} events):", chain.len());
        println!();
        for (i, evt) in chain.iter().enumerate() {
            let arrow = if i == 0 { "  ●" } else { "  └─▶" };
            let marker = if evt.id == shipped_event.id {
                " ◄── (target)"
            } else {
                ""
            };
            println!("  {arrow} {} [{}]{marker}", evt.event_type, evt.id);
        }
    }

    // ========================================================================
    // Query: trace_causation_chain (from OrderConfirmed)
    // ========================================================================
    let confirmed_event = correlated_events
        .iter()
        .find(|e| e.event_type == "OrderConfirmed");
    if let Some(confirmed_event) = confirmed_event {
        println!();
        println!("═══════════════════════════════════════════════════════════════");
        println!("  Query: trace_causation_chain({})", confirmed_event.id);
        println!("  (tracing from event: {})", confirmed_event.event_type);
        println!("═══════════════════════════════════════════════════════════════");
        println!();

        let chain = event_store
            .trace_causation_chain(confirmed_event.id)
            .await?;

        println!("  Causal chain ({} events):", chain.len());
        println!();
        for (i, evt) in chain.iter().enumerate() {
            let arrow = if i == 0 { "  ●" } else { "  └─▶" };
            let marker = if evt.id == confirmed_event.id {
                " ◄── (target)"
            } else {
                ""
            };
            println!("  {arrow} {} [{}]{marker}", evt.event_type, evt.id);
        }
    }

    println!();
    println!("═══════════════════════════════════════════════════════════════");
    println!("  Example completed successfully! 🎉");
    println!("═══════════════════════════════════════════════════════════════");

    Ok(())
}
