# Specification: Saga Example - Order Fulfillment

## Status: Proposed

## Overview

Create a comprehensive example demonstrating the Saga pattern in Epoch. The example will showcase a realistic order fulfillment saga that coordinates multiple aggregates (Order, Inventory, Payment) through a state machine-driven process.

## Motivation

Currently, Epoch has no example demonstrating the Saga pattern, despite it being a core feature of the framework. Users need to see:
- How to implement the `Saga` trait
- How to use saga state machines to coordinate long-running processes
- How to dispatch commands to multiple aggregates from saga event handlers
- How to use `SagaHandler` to subscribe sagas to the event bus
- How to handle saga state transitions and error scenarios
- How to use the `#[derive(SubscriberId)]` macro with sagas

## Goals

1. Demonstrate a realistic multi-aggregate coordination scenario
2. Show saga state machine implementation using enums
3. Illustrate command dispatching from saga event handlers
4. Show proper error handling in sagas
5. Demonstrate saga subscription to the event bus via `SagaHandler`
6. Provide clear documentation and comments explaining the pattern

## Non-Goals

- Compensation logic (saga rollback) - this is advanced and can be a future example
- Database persistence - will use in-memory stores for simplicity
- Complex business rules - keep the business logic straightforward

## Proposed Changes

### File to Create

**File**: `epoch/examples/saga-order-fulfillment.rs`

### Example Structure

The example will include:

1. **Event Definitions** (using `#[subset_enum]`):
   - `ApplicationEvent` - Superset of all events
   - `OrderEvent` - Order aggregate events (OrderPlaced, OrderConfirmed, OrderCancelled)
   - `InventoryEvent` - Inventory events (ItemsReserved, ItemsReleased)
   - `PaymentEvent` - Payment events (PaymentProcessed, PaymentFailed)

2. **Command Definitions** (using `#[subset_enum]`):
   - `ApplicationCommand` - Superset of all commands
   - `OrderCommand` - Order commands
   - `InventoryCommand` - Inventory commands (ReserveItems, ReleaseItems)
   - `PaymentCommand` - Payment commands (ProcessPayment)

3. **Aggregates**:
   - `OrderAggregate` - Handles order placement and confirmation
   - `InventoryAggregate` - Manages inventory reservations
   - `PaymentAggregate` - Processes payments

4. **Saga**:
   - `OrderFulfillmentSaga` - Coordinates the order fulfillment process
   - `OrderFulfillmentState` - State machine enum (Pending, InventoryReserved, PaymentProcessed, Completed, Failed)

5. **Main Flow**:
   - Place an order (emits `OrderPlaced` event)
   - Saga receives `OrderPlaced` and dispatches `ReserveItems` command
   - Inventory aggregate reserves items (emits `ItemsReserved` event)
   - Saga receives `ItemsReserved` and dispatches `ProcessPayment` command
   - Payment aggregate processes payment (emits `PaymentProcessed` event)
   - Saga receives `PaymentProcessed` and dispatches `ConfirmOrder` command
   - Order aggregate confirms order (emits `OrderConfirmed` event)

### Saga State Machine

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum OrderFulfillmentState {
    #[default]
    Pending,
    InventoryReserved { order_id: Uuid, items: Vec<String> },
    PaymentProcessed { order_id: Uuid },
    Completed,
    Failed { reason: String },
}
```

### Key Code Patterns to Demonstrate

1. **Saga Implementation**:
```rust
#[derive(SubscriberId)]
#[subscriber_id(prefix = "saga")]
pub struct OrderFulfillmentSaga {
    state_store: InMemoryStateStore<OrderFulfillmentState>,
    order_aggregate: Arc<OrderAggregate>,
    inventory_aggregate: Arc<InventoryAggregate>,
    payment_aggregate: Arc<PaymentAggregate>,
}

#[async_trait]
impl Saga<ApplicationEvent> for OrderFulfillmentSaga {
    type State = OrderFulfillmentState;
    type StateStore = InMemoryStateStore<OrderFulfillmentState>;
    type SagaError = OrderFulfillmentError;
    type EventType = OrderFulfillmentEvent; // subset of ApplicationEvent
    
    async fn handle_event(
        &self,
        state: Self::State,
        event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, Self::SagaError> {
        match (&state, event.data.as_ref().unwrap()) {
            (OrderFulfillmentState::Pending, OrderFulfillmentEvent::OrderPlaced { items, .. }) => {
                // Dispatch command to inventory aggregate
                self.inventory_aggregate.handle(
                    Command::new(event.stream_id, InventoryCommand::ReserveItems { items: items.clone() }, None, None)
                ).await?;
                Ok(Some(OrderFulfillmentState::InventoryReserved { 
                    order_id: event.stream_id,
                    items: items.clone()
                }))
            },
            // ... more state transitions
        }
    }
}
```

2. **Saga Subscription**:
```rust
bus.subscribe(SagaHandler::new(order_fulfillment_saga)).await?;
```

3. **Event Subset Definition**:
```rust
#[subset_enum(OrderFulfillmentEvent, OrderPlaced, ItemsReserved, PaymentProcessed, OrderConfirmed)]
#[derive(Debug, Clone, Serialize, Deserialize, EventData)]
pub enum ApplicationEvent {
    OrderPlaced { items: Vec<String>, total: f64 },
    ItemsReserved { order_id: Uuid },
    ItemsReleased { order_id: Uuid },
    PaymentProcessed { order_id: Uuid, amount: f64 },
    PaymentFailed { order_id: Uuid, reason: String },
    OrderConfirmed { order_id: Uuid },
    OrderCancelled { order_id: Uuid },
}
```

### Dependencies

The example will use existing dependencies from the `hello-world` example:
- `async-trait`
- `epoch` with features `["derive", "in-memory"]`
- `serde` with derive feature
- `tokio` with full features
- `uuid` with v4 feature
- `thiserror`
- `log` and `env_logger`

### Output

The example should print clear logging showing:
1. Order placement
2. Saga state transitions
3. Command dispatching to aggregates
4. Event emissions from aggregates
5. Final saga completion

Example output:
```
Order placed: <order-id>
Saga: Transitioning to InventoryReserved
Dispatching ReserveItems command to inventory
Inventory: Items reserved for order <order-id>
Saga: Transitioning to PaymentProcessed
Dispatching ProcessPayment command
Payment: Processed $100.00 for order <order-id>
Saga: Order fulfillment completed for <order-id>
```

## Implementation Notes

1. **State Machine Pattern**: Use Rust enums for saga state to enforce valid transitions at compile time
2. **Error Handling**: Define clear error types using `thiserror`
3. **Documentation**: Add extensive rustdoc comments explaining the saga pattern
4. **Simplicity**: Keep business logic simple to focus on the saga pattern itself
5. **Logging**: Use `log::info!` and `log::debug!` to trace the saga flow

## Testing Strategy

The example itself serves as a test. When run, it should:
1. Execute without panics or errors
2. Complete the full saga workflow
3. Demonstrate clear state transitions
4. Show successful coordination between aggregates

## Documentation Updates

Add the following to `epoch/Cargo.toml`:
```toml
[[example]]
name = "saga-order-fulfillment"
path = "examples/saga-order-fulfillment.rs"
```

Update `README.md` to reference the new example in the "Examples" section:
```markdown
- [`epoch/examples/saga-order-fulfillment.rs`](epoch/examples/saga-order-fulfillment.rs) - Saga pattern for coordinating multi-aggregate workflows
```

## Open Questions

None at this time.

## Alternatives Considered

1. **Simpler single-aggregate saga**: Rejected because it wouldn't show the coordination aspect
2. **Including compensation logic**: Deferred to a future example to keep this one focused
3. **Using PostgreSQL**: Rejected to keep the example self-contained and easy to run

## References

- `epoch_core/src/saga.rs` - Saga trait definition
- `epoch_derive/tests/subscriber_id_tests.rs` - SubscriberId macro usage
- `epoch/examples/hello-world.rs` - Existing example structure
- `AGENTS.md` - Project collaboration guidelines
