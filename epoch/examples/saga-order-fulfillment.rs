//! # Saga Example: Order Fulfillment
//!
//! This example demonstrates the Saga pattern in Epoch, showing how to coordinate
//! long-running business processes across multiple aggregates.
//!
//! ## What is a Saga?
//!
//! A saga is a pattern for managing distributed transactions across multiple aggregates.
//! Instead of using database transactions, sagas coordinate a sequence of local transactions,
//! with each step triggering the next through events.
//!
//! ## The Order Fulfillment Scenario
//!
//! This example implements an order fulfillment saga that coordinates three aggregates:
//! - **Order Aggregate**: Manages order lifecycle (placed, confirmed, cancelled)
//! - **Inventory Aggregate**: Handles inventory reservations
//! - **Payment Aggregate**: Processes payments
//!
//! ## Saga State Machine Flow
//!
//! ```text
//! Pending
//!   │
//!   │ OrderPlaced event
//!   ▼
//! (Saga dispatches ReserveItems command)
//!   │
//!   │ ItemsReserved event
//!   ▼
//! InventoryReserved
//!   │
//!   │ (Saga dispatches ProcessPayment command)
//!   ▼
//!   │ PaymentProcessed event
//!   ▼
//! PaymentProcessed
//!   │
//!   │ (Saga dispatches ConfirmOrder command)
//!   ▼
//!   │ OrderConfirmed event
//!   ▼
//! Completed
//! ```
//!
//! ## Key Patterns Demonstrated
//!
//! 1. **Saga State Machine**: Using Rust enums to model saga state transitions
//! 2. **Event-Driven Coordination**: Saga reacts to events and dispatches commands
//! 3. **Multi-Aggregate Orchestration**: Coordinating Order, Inventory, and Payment
//! 4. **Saga Subscription**: Using `SagaHandler` to subscribe to the event bus
//! 5. **SubscriberId Derive**: Using `#[derive(SubscriberId)]` for saga identification
//! 6. **Async Event Bus**: Commands can be awaited directly - the event bus handles async dispatch

use async_trait::async_trait;
use epoch::prelude::*;
use epoch_mem::{InMemoryEventBus, InMemoryEventStore, InMemoryStateStore};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

// ============================================================================
// Command Definitions
// ============================================================================

/// Application-wide command enum containing all possible commands in the system.
///
/// Uses `#[subset_enum]` to generate type-safe subsets for each aggregate.
/// This allows aggregates to only handle commands relevant to them while maintaining
/// a unified command type for the system.
#[subset_enum(OrderCommand, PlaceOrder, ConfirmOrder, CancelOrder)]
#[subset_enum(InventoryCommand, ReserveItems, ReleaseItems)]
#[subset_enum(PaymentCommand, ProcessPayment)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ApplicationCommand {
    /// Place a new order
    PlaceOrder { items: Vec<String>, total: f64 },
    /// Confirm an order after successful payment
    ConfirmOrder { order_id: Uuid },
    /// Cancel an order
    CancelOrder { order_id: Uuid },
    /// Reserve items in inventory for an order
    ReserveItems { order_id: Uuid, items: Vec<String> },
    /// Release reserved items back to inventory
    ReleaseItems { order_id: Uuid },
    /// Process payment for an order
    ProcessPayment { order_id: Uuid, amount: f64 },
}

// ============================================================================
// Event Definitions
// ============================================================================

/// Application-wide event enum containing all possible events in the system.
///
/// Uses `#[subset_enum]` to generate type-safe subsets for each aggregate and saga.
///
/// ## Subset Enums
///
/// - `OrderEvent`: Events specific to the Order aggregate
/// - `InventoryEvent`: Events specific to the Inventory aggregate  
/// - `PaymentEvent`: Events specific to the Payment aggregate
/// - `OrderFulfillmentEvent`: Events that the saga needs to observe
///
/// The `#[subset_enum]` macro generates From/TryFrom implementations for type-safe
/// conversions between the superset and subsets.
#[subset_enum(OrderEvent, OrderPlaced, OrderConfirmed, OrderCancelled)]
#[subset_enum(InventoryEvent, ItemsReserved, ItemsReleased)]
#[subset_enum(PaymentEvent, PaymentProcessed, PaymentFailed)]
#[subset_enum(
    OrderFulfillmentEvent,
    OrderPlaced,
    ItemsReserved,
    PaymentProcessed,
    OrderConfirmed
)]
#[derive(Debug, Clone, Serialize, Deserialize, EventData)]
pub enum ApplicationEvent {
    /// An order was placed by a customer
    OrderPlaced { items: Vec<String>, total: f64 },
    /// An order was confirmed after successful payment
    OrderConfirmed { order_id: Uuid },
    /// An order was cancelled
    OrderCancelled { order_id: Uuid },
    /// Items were reserved in inventory for an order
    ItemsReserved { order_id: Uuid },
    /// Reserved items were released back to inventory
    ItemsReleased { order_id: Uuid },
    /// Payment was successfully processed
    PaymentProcessed { order_id: Uuid, amount: f64 },
    /// Payment processing failed
    PaymentFailed { order_id: Uuid, reason: String },
}

// ============================================================================
// Order Aggregate
// ============================================================================

/// Status of an order in its lifecycle
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum OrderStatus {
    Pending,
    Confirmed,
    Cancelled,
}

/// Order aggregate state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    id: Uuid,
    items: Vec<String>,
    total: f64,
    status: OrderStatus,
    version: u64,
}

impl EventApplicatorState for Order {
    fn get_id(&self) -> &Uuid {
        &self.id
    }
}

impl AggregateState for Order {
    fn get_version(&self) -> u64 {
        self.version
    }

    fn set_version(&mut self, version: u64) {
        self.version = version;
    }
}

/// Errors that can occur in the Order aggregate
#[derive(Debug, thiserror::Error)]
pub enum OrderError {
    #[error("Error building event: {0}")]
    EventBuild(#[from] EventBuilderError),

    #[error("Order is already confirmed")]
    AlreadyConfirmed,

    #[error("Order is already cancelled")]
    AlreadyCancelled,
}

/// Order aggregate that manages order lifecycle
pub struct OrderAggregate {
    state_store: InMemoryStateStore<Order>,
    event_store: InMemoryEventStore<InMemoryEventBus<ApplicationEvent>>,
}

impl OrderAggregate {
    pub fn new(
        event_store: InMemoryEventStore<InMemoryEventBus<ApplicationEvent>>,
        state_store: InMemoryStateStore<Order>,
    ) -> Self {
        OrderAggregate {
            state_store,
            event_store,
        }
    }
}

impl EventApplicator<ApplicationEvent> for OrderAggregate {
    type State = Order;
    type EventType = OrderEvent;
    type StateStore = InMemoryStateStore<Order>;
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
            OrderEvent::OrderPlaced { items, total } => {
                log::debug!("Applying OrderPlaced event for order {}", event.stream_id);
                Ok(Some(Order {
                    id: event.stream_id,
                    items: items.clone(),
                    total: *total,
                    status: OrderStatus::Pending,
                    version: 0,
                }))
            }
            OrderEvent::OrderConfirmed { .. } => {
                if let Some(mut state) = state {
                    log::debug!("Applying OrderConfirmed event for order {}", state.id);
                    state.status = OrderStatus::Confirmed;
                    Ok(Some(state))
                } else {
                    Ok(None)
                }
            }
            OrderEvent::OrderCancelled { .. } => {
                if let Some(mut state) = state {
                    log::debug!("Applying OrderCancelled event for order {}", state.id);
                    state.status = OrderStatus::Cancelled;
                    Ok(Some(state))
                } else {
                    Ok(None)
                }
            }
        }
    }
}

#[async_trait]
impl Aggregate<ApplicationEvent> for OrderAggregate {
    type CommandData = ApplicationCommand;
    type CommandCredentials = ();
    type Command = OrderCommand;
    type AggregateError = OrderError;
    type EventStore = InMemoryEventStore<InMemoryEventBus<ApplicationEvent>>;

    fn get_event_store(&self) -> Self::EventStore {
        self.event_store.clone()
    }

    async fn handle_command(
        &self,
        state: &Option<Self::State>,
        command: Command<Self::Command, Self::CommandCredentials>,
    ) -> Result<Vec<Event<ApplicationEvent>>, Self::AggregateError> {
        match command.data {
            OrderCommand::PlaceOrder { items, total } => {
                log::info!(
                    "Placing order {} with {} items, total: ${}",
                    command.aggregate_id,
                    items.len(),
                    total
                );

                let event = ApplicationEvent::OrderPlaced { items, total }
                    .into_builder()
                    .stream_id(command.aggregate_id)
                    .build()?;

                Ok(vec![event])
            }
            OrderCommand::ConfirmOrder { .. } => {
                if let Some(state) = state {
                    if state.status == OrderStatus::Confirmed {
                        return Err(OrderError::AlreadyConfirmed);
                    }
                    if state.status == OrderStatus::Cancelled {
                        return Err(OrderError::AlreadyCancelled);
                    }

                    log::info!("Confirming order {}", state.id);

                    let event = ApplicationEvent::OrderConfirmed { order_id: state.id }
                        .into_builder()
                        .stream_id(state.id)
                        .build()?;

                    Ok(vec![event])
                } else {
                    Ok(vec![])
                }
            }
            OrderCommand::CancelOrder { .. } => {
                if let Some(state) = state {
                    if state.status == OrderStatus::Cancelled {
                        return Err(OrderError::AlreadyCancelled);
                    }

                    log::info!("Cancelling order {}", state.id);

                    let event = ApplicationEvent::OrderCancelled { order_id: state.id }
                        .into_builder()
                        .stream_id(state.id)
                        .build()?;

                    Ok(vec![event])
                } else {
                    Ok(vec![])
                }
            }
        }
    }
}

// ============================================================================
// Inventory Aggregate
// ============================================================================

/// Inventory aggregate state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Inventory {
    id: Uuid,
    reserved_for_order: Option<Uuid>,
    items: Vec<String>,
    version: u64,
}

impl EventApplicatorState for Inventory {
    fn get_id(&self) -> &Uuid {
        &self.id
    }
}

impl AggregateState for Inventory {
    fn get_version(&self) -> u64 {
        self.version
    }

    fn set_version(&mut self, version: u64) {
        self.version = version;
    }
}

/// Errors that can occur in the Inventory aggregate
#[derive(Debug, thiserror::Error)]
pub enum InventoryError {
    #[error("Error building event: {0}")]
    EventBuild(#[from] EventBuilderError),

    #[error("Inventory already reserved for order {0}")]
    AlreadyReserved(Uuid),

    #[error("Inventory not reserved")]
    NotReserved,
}

/// Inventory aggregate that manages inventory reservations
pub struct InventoryAggregate {
    state_store: InMemoryStateStore<Inventory>,
    event_store: InMemoryEventStore<InMemoryEventBus<ApplicationEvent>>,
}

impl InventoryAggregate {
    pub fn new(
        event_store: InMemoryEventStore<InMemoryEventBus<ApplicationEvent>>,
        state_store: InMemoryStateStore<Inventory>,
    ) -> Self {
        InventoryAggregate {
            state_store,
            event_store,
        }
    }
}

impl EventApplicator<ApplicationEvent> for InventoryAggregate {
    type State = Inventory;
    type EventType = InventoryEvent;
    type StateStore = InMemoryStateStore<Inventory>;
    type ApplyError = InventoryError;

    fn get_state_store(&self) -> Self::StateStore {
        self.state_store.clone()
    }

    fn apply(
        &self,
        state: Option<Self::State>,
        event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, Self::ApplyError> {
        match event.data.as_ref().unwrap() {
            InventoryEvent::ItemsReserved { order_id } => {
                if let Some(mut state) = state {
                    log::debug!("Applying ItemsReserved event for inventory {}", state.id);
                    state.reserved_for_order = Some(*order_id);
                    Ok(Some(state))
                } else {
                    // Create initial inventory state if it doesn't exist
                    Ok(Some(Inventory {
                        id: event.stream_id,
                        reserved_for_order: Some(*order_id),
                        items: vec![],
                        version: 0,
                    }))
                }
            }
            InventoryEvent::ItemsReleased { .. } => {
                if let Some(mut state) = state {
                    log::debug!("Applying ItemsReleased event for inventory {}", state.id);
                    state.reserved_for_order = None;
                    Ok(Some(state))
                } else {
                    Ok(None)
                }
            }
        }
    }
}

#[async_trait]
impl Aggregate<ApplicationEvent> for InventoryAggregate {
    type CommandData = ApplicationCommand;
    type CommandCredentials = ();
    type Command = InventoryCommand;
    type AggregateError = InventoryError;
    type EventStore = InMemoryEventStore<InMemoryEventBus<ApplicationEvent>>;

    fn get_event_store(&self) -> Self::EventStore {
        self.event_store.clone()
    }

    async fn handle_command(
        &self,
        state: &Option<Self::State>,
        command: Command<Self::Command, Self::CommandCredentials>,
    ) -> Result<Vec<Event<ApplicationEvent>>, Self::AggregateError> {
        match command.data {
            InventoryCommand::ReserveItems { order_id, items } => {
                if let Some(state) = state
                    && state.reserved_for_order.is_some()
                {
                    return Err(InventoryError::AlreadyReserved(
                        state.reserved_for_order.unwrap(),
                    ));
                }

                log::info!("Reserving {} items for order {}", items.len(), order_id);

                let event = ApplicationEvent::ItemsReserved { order_id }
                    .into_builder()
                    .stream_id(command.aggregate_id)
                    .build()?;

                Ok(vec![event])
            }
            InventoryCommand::ReleaseItems { order_id } => {
                if let Some(state) = state {
                    if state.reserved_for_order.is_none() {
                        return Err(InventoryError::NotReserved);
                    }

                    log::info!("Releasing items for order {}", order_id);

                    let event = ApplicationEvent::ItemsReleased { order_id }
                        .into_builder()
                        .stream_id(state.id)
                        .build()?;

                    Ok(vec![event])
                } else {
                    Ok(vec![])
                }
            }
        }
    }
}

// ============================================================================
// Payment Aggregate
// ============================================================================

/// Payment aggregate state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Payment {
    id: Uuid,
    order_id: Uuid,
    amount: f64,
    processed: bool,
    version: u64,
}

impl EventApplicatorState for Payment {
    fn get_id(&self) -> &Uuid {
        &self.id
    }
}

impl AggregateState for Payment {
    fn get_version(&self) -> u64 {
        self.version
    }

    fn set_version(&mut self, version: u64) {
        self.version = version;
    }
}

/// Errors that can occur in the Payment aggregate
#[derive(Debug, thiserror::Error)]
pub enum PaymentError {
    #[error("Error building event: {0}")]
    EventBuild(#[from] EventBuilderError),

    #[error("Payment already processed")]
    AlreadyProcessed,
}

/// Payment aggregate that processes payments
pub struct PaymentAggregate {
    state_store: InMemoryStateStore<Payment>,
    event_store: InMemoryEventStore<InMemoryEventBus<ApplicationEvent>>,
}

impl PaymentAggregate {
    pub fn new(
        event_store: InMemoryEventStore<InMemoryEventBus<ApplicationEvent>>,
        state_store: InMemoryStateStore<Payment>,
    ) -> Self {
        PaymentAggregate {
            state_store,
            event_store,
        }
    }
}

impl EventApplicator<ApplicationEvent> for PaymentAggregate {
    type State = Payment;
    type EventType = PaymentEvent;
    type StateStore = InMemoryStateStore<Payment>;
    type ApplyError = PaymentError;

    fn get_state_store(&self) -> Self::StateStore {
        self.state_store.clone()
    }

    fn apply(
        &self,
        state: Option<Self::State>,
        event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, Self::ApplyError> {
        match event.data.as_ref().unwrap() {
            PaymentEvent::PaymentProcessed { order_id, amount } => {
                log::debug!(
                    "Applying PaymentProcessed event for payment {}",
                    event.stream_id
                );
                if let Some(mut state) = state {
                    state.processed = true;
                    Ok(Some(state))
                } else {
                    Ok(Some(Payment {
                        id: event.stream_id,
                        order_id: *order_id,
                        amount: *amount,
                        processed: true,
                        version: 0,
                    }))
                }
            }
            PaymentEvent::PaymentFailed { .. } => {
                log::debug!(
                    "Applying PaymentFailed event for payment {}",
                    event.stream_id
                );
                Ok(state)
            }
        }
    }
}

#[async_trait]
impl Aggregate<ApplicationEvent> for PaymentAggregate {
    type CommandData = ApplicationCommand;
    type CommandCredentials = ();
    type Command = PaymentCommand;
    type AggregateError = PaymentError;
    type EventStore = InMemoryEventStore<InMemoryEventBus<ApplicationEvent>>;

    fn get_event_store(&self) -> Self::EventStore {
        self.event_store.clone()
    }

    async fn handle_command(
        &self,
        state: &Option<Self::State>,
        command: Command<Self::Command, Self::CommandCredentials>,
    ) -> Result<Vec<Event<ApplicationEvent>>, Self::AggregateError> {
        match command.data {
            PaymentCommand::ProcessPayment { order_id, amount } => {
                if let Some(state) = state
                    && state.processed
                {
                    return Err(PaymentError::AlreadyProcessed);
                }

                log::info!("Processing payment of ${} for order {}", amount, order_id);

                // Simulate successful payment processing
                let event = ApplicationEvent::PaymentProcessed { order_id, amount }
                    .into_builder()
                    .stream_id(command.aggregate_id)
                    .build()?;

                Ok(vec![event])
            }
        }
    }
}

// ============================================================================
// Saga State Machine
// ============================================================================

/// State of the order fulfillment saga.
///
/// This enum represents the saga's state machine, tracking the progress
/// of order fulfillment through multiple steps.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum OrderFulfillmentState {
    /// Initial state - waiting for order to be placed
    #[default]
    Pending,

    /// Inventory has been reserved for the order
    InventoryReserved { order_id: Uuid, items: Vec<String> },

    /// Payment has been processed successfully
    PaymentProcessed { order_id: Uuid },

    /// Order fulfillment is complete
    Completed,

    /// Order fulfillment failed
    Failed { reason: String },
}

/// Errors that can occur in the order fulfillment saga
#[derive(Debug, thiserror::Error)]
pub enum OrderFulfillmentError {
    #[error("Aggregate error: {0}")]
    AggregateError(String),

    #[error("Invalid state transition: {0}")]
    InvalidStateTransition(String),
}

// Implement From for aggregate errors to convert them to saga errors
impl From<OrderError> for OrderFulfillmentError {
    fn from(err: OrderError) -> Self {
        OrderFulfillmentError::AggregateError(err.to_string())
    }
}

impl From<InventoryError> for OrderFulfillmentError {
    fn from(err: InventoryError) -> Self {
        OrderFulfillmentError::AggregateError(err.to_string())
    }
}

impl From<PaymentError> for OrderFulfillmentError {
    fn from(err: PaymentError) -> Self {
        OrderFulfillmentError::AggregateError(err.to_string())
    }
}

/// Order fulfillment saga that coordinates the order processing workflow.
///
/// This saga orchestrates the interaction between Order, Inventory, and Payment
/// aggregates to fulfill an order.
#[derive(epoch_derive::SubscriberId)]
#[subscriber_id(prefix = "saga")]
pub struct OrderFulfillmentSaga {
    state_store: InMemoryStateStore<OrderFulfillmentState>,
    order_aggregate: Arc<OrderAggregate>,
    inventory_aggregate: Arc<InventoryAggregate>,
    payment_aggregate: Arc<PaymentAggregate>,
}

impl OrderFulfillmentSaga {
    pub fn new(
        state_store: InMemoryStateStore<OrderFulfillmentState>,
        order_aggregate: Arc<OrderAggregate>,
        inventory_aggregate: Arc<InventoryAggregate>,
        payment_aggregate: Arc<PaymentAggregate>,
    ) -> Self {
        OrderFulfillmentSaga {
            state_store,
            order_aggregate,
            inventory_aggregate,
            payment_aggregate,
        }
    }
}

#[async_trait]
impl Saga<ApplicationEvent> for OrderFulfillmentSaga {
    type State = OrderFulfillmentState;
    type StateStore = InMemoryStateStore<OrderFulfillmentState>;
    type SagaError = OrderFulfillmentError;
    type EventType = OrderFulfillmentEvent;

    fn get_state_store(&self) -> Self::StateStore {
        self.state_store.clone()
    }

    fn get_id_from_event(&self, event: &Event<Self::EventType>) -> Uuid {
        // All events in our saga relate to the order, so we extract the order ID
        // from the event data to use as the saga instance ID
        match event.data.as_ref().unwrap() {
            OrderFulfillmentEvent::OrderPlaced { .. } => event.stream_id,
            OrderFulfillmentEvent::ItemsReserved { order_id } => *order_id,
            OrderFulfillmentEvent::PaymentProcessed { order_id, .. } => *order_id,
            OrderFulfillmentEvent::OrderConfirmed { order_id } => *order_id,
        }
    }

    async fn handle_event(
        &self,
        state: Self::State,
        event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, Self::SagaError> {
        match (&state, event.data.as_ref().unwrap()) {
            // Step 1: Order placed -> Reserve inventory
            (OrderFulfillmentState::Pending, OrderFulfillmentEvent::OrderPlaced { items, .. }) => {
                log::info!(
                    "📦 Saga: Order {} placed, reserving inventory",
                    event.stream_id
                );

                // Dispatch command to inventory aggregate to reserve items
                // With the async event bus, we can now await directly without race conditions
                let inventory_id = Uuid::new_v4();
                self.inventory_aggregate
                    .handle(Command::new(
                        inventory_id,
                        ApplicationCommand::ReserveItems {
                            order_id: event.stream_id,
                            items: items.clone(),
                        },
                        None,
                        None,
                    ))
                    .await
                    .map_err(|e| OrderFulfillmentError::AggregateError(e.to_string()))?;

                Ok(Some(OrderFulfillmentState::InventoryReserved {
                    order_id: event.stream_id,
                    items: items.clone(),
                }))
            }

            // Step 2: Inventory reserved -> Process payment
            (
                OrderFulfillmentState::InventoryReserved { order_id, .. },
                OrderFulfillmentEvent::ItemsReserved { .. },
            ) => {
                log::info!(
                    "💰 Saga: Inventory reserved for order {}, processing payment",
                    order_id
                );

                // For this example, we'll use a fixed amount
                // In a real system, we'd fetch the order state to get the actual total
                let payment_id = Uuid::new_v4();
                let amount = 99.99;

                self.payment_aggregate
                    .handle(Command::new(
                        payment_id,
                        ApplicationCommand::ProcessPayment {
                            order_id: *order_id,
                            amount,
                        },
                        None,
                        None,
                    ))
                    .await
                    .map_err(|e| OrderFulfillmentError::AggregateError(e.to_string()))?;

                Ok(Some(OrderFulfillmentState::PaymentProcessed {
                    order_id: *order_id,
                }))
            }

            // Step 3: Payment processed -> Confirm order
            (
                OrderFulfillmentState::PaymentProcessed { order_id },
                OrderFulfillmentEvent::PaymentProcessed { .. },
            ) => {
                log::info!(
                    "✅ Saga: Payment processed for order {}, confirming order",
                    order_id
                );

                self.order_aggregate
                    .handle(Command::new(
                        *order_id,
                        ApplicationCommand::ConfirmOrder {
                            order_id: *order_id,
                        },
                        None,
                        None,
                    ))
                    .await
                    .map_err(|e| OrderFulfillmentError::AggregateError(e.to_string()))?;

                // Stay in PaymentProcessed state until we receive OrderConfirmed event
                Ok(Some(OrderFulfillmentState::PaymentProcessed {
                    order_id: *order_id,
                }))
            }

            // Step 4: Order confirmed -> Complete saga
            (_, OrderFulfillmentEvent::OrderConfirmed { order_id }) => {
                log::info!("🎉 Saga: Order {} confirmed, saga completed!", order_id);
                Ok(Some(OrderFulfillmentState::Completed))
            }

            // Catch-all for unexpected state transitions
            (state, event) => {
                log::warn!(
                    "⚠️  Saga: Unexpected state transition: {:?} with event {:?}",
                    state,
                    event
                );
                Ok(Some(state.clone()))
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    env_logger::init();

    log::info!("🚀 Starting Order Fulfillment Saga Example");
    log::info!("================================================");

    // Create the event bus and event store
    let bus: InMemoryEventBus<ApplicationEvent> = InMemoryEventBus::new();
    let event_store = InMemoryEventStore::new(bus.clone());

    // Create state stores for all aggregates
    let order_state_store = InMemoryStateStore::new();
    let inventory_state_store = InMemoryStateStore::new();
    let payment_state_store = InMemoryStateStore::new();
    let saga_state_store = InMemoryStateStore::new();

    // Create aggregate instances
    let order_aggregate = Arc::new(OrderAggregate::new(
        event_store.clone(),
        order_state_store.clone(),
    ));

    let inventory_aggregate = Arc::new(InventoryAggregate::new(
        event_store.clone(),
        inventory_state_store.clone(),
    ));

    let payment_aggregate = Arc::new(PaymentAggregate::new(
        event_store.clone(),
        payment_state_store.clone(),
    ));

    // Create the saga
    let saga = OrderFulfillmentSaga::new(
        saga_state_store.clone(),
        order_aggregate.clone(),
        inventory_aggregate.clone(),
        payment_aggregate.clone(),
    );

    // Subscribe the saga to the event bus using SagaHandler
    log::info!("📡 Subscribing saga to event bus");
    bus.subscribe(SagaHandler::new(saga)).await?;

    log::info!("================================================");
    log::info!("");

    // Execute the order fulfillment workflow
    let order_id = Uuid::new_v4();
    let items = vec![
        "Widget A".to_string(),
        "Widget B".to_string(),
        "Widget C".to_string(),
    ];
    let total = 99.99;

    log::info!("📝 Step 1: Placing order {}", order_id);
    order_aggregate
        .handle(Command::new(
            order_id,
            ApplicationCommand::PlaceOrder {
                items: items.clone(),
                total,
            },
            None,
            None,
        ))
        .await?;

    // Allow time for async event processing
    // The saga will process events asynchronously, so we need to wait for all steps to complete
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    log::info!("");
    log::info!("================================================");
    log::info!("✨ Workflow Complete!");
    log::info!("================================================");

    // Display final states
    log::info!("");
    log::info!("Final States:");
    log::info!("--------------");

    if let Some(order_state) = order_state_store.get_state(order_id).await? {
        log::info!("Order: {:?}", order_state.status);
    }

    if let Some(saga_state) = saga_state_store.get_state(order_id).await? {
        log::info!("Saga: {:?}", saga_state);
    }

    log::info!("");
    log::info!("================================================");
    log::info!("Example completed successfully! 🎉");
    log::info!("================================================");

    Ok(())
}
