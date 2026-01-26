# Implementation Plan: Saga Example - Order Fulfillment

## Status: Approved for Implementation

## Overview

This document provides a step-by-step Test-Driven Development (TDD) implementation plan for the saga example as specified in `0009-saga-example.md`.

## Implementation Steps

### Step 1: Set up the example file skeleton

**File**: `epoch/examples/saga-order-fulfillment.rs`

**Actions**:
1. Create the file with basic imports
2. Add module-level documentation explaining the example
3. Set up the main function structure

**No test needed** - This is just scaffolding.

**Verification**: File compiles with `cargo build --example saga-order-fulfillment`

---

### Step 2: Define event enums with subset_enum macro

**File**: `epoch/examples/saga-order-fulfillment.rs`

**Actions**:
1. Define `ApplicationEvent` enum with all event variants:
   - `OrderPlaced { items: Vec<String>, total: f64 }`
   - `OrderConfirmed { order_id: Uuid }`
   - `OrderCancelled { order_id: Uuid }`
   - `ItemsReserved { order_id: Uuid }`
   - `ItemsReleased { order_id: Uuid }`
   - `PaymentProcessed { order_id: Uuid, amount: f64 }`
   - `PaymentFailed { order_id: Uuid, reason: String }`

2. Add `#[subset_enum]` attributes for:
   - `OrderEvent` (OrderPlaced, OrderConfirmed, OrderCancelled)
   - `InventoryEvent` (ItemsReserved, ItemsReleased)
   - `PaymentEvent` (PaymentProcessed, PaymentFailed)
   - `OrderFulfillmentEvent` (OrderPlaced, ItemsReserved, PaymentProcessed, OrderConfirmed)

3. Add derives: `Debug, Clone, Serialize, Deserialize, EventData`

**Test**: Verify the code compiles and subset enums are generated correctly

**Verification**: `cargo build --example saga-order-fulfillment`

---

### Step 3: Define command enums with subset_enum macro

**File**: `epoch/examples/saga-order-fulfillment.rs`

**Actions**:
1. Define `ApplicationCommand` enum with all command variants:
   - `PlaceOrder { items: Vec<String>, total: f64 }`
   - `ConfirmOrder { order_id: Uuid }`
   - `CancelOrder { order_id: Uuid }`
   - `ReserveItems { order_id: Uuid, items: Vec<String> }`
   - `ReleaseItems { order_id: Uuid }`
   - `ProcessPayment { order_id: Uuid, amount: f64 }`

2. Add `#[subset_enum]` attributes for:
   - `OrderCommand` (PlaceOrder, ConfirmOrder, CancelOrder)
   - `InventoryCommand` (ReserveItems, ReleaseItems)
   - `PaymentCommand` (ProcessPayment)

3. Add derives: `Debug, Clone, Serialize, Deserialize`

**Test**: Verify the code compiles and subset enums are generated correctly

**Verification**: `cargo build --example saga-order-fulfillment`

---

### Step 4: Implement Order aggregate state and error types

**File**: `epoch/examples/saga-order-fulfillment.rs`

**Actions**:
1. Define `Order` struct with fields:
   - `id: Uuid`
   - `items: Vec<String>`
   - `total: f64`
   - `status: OrderStatus` (enum: Pending, Confirmed, Cancelled)
   - `version: u64`

2. Define `OrderStatus` enum

3. Implement `EventApplicatorState` trait for `Order`

4. Implement `AggregateState` trait for `Order`

5. Define `OrderError` enum using `thiserror`:
   - `EventBuild(EventBuilderError)`
   - `AlreadyConfirmed`
   - `AlreadyCancelled`

**Test**: Verify traits are correctly implemented

**Verification**: `cargo build --example saga-order-fulfillment`

---

### Step 5: Implement OrderAggregate with EventApplicator

**File**: `epoch/examples/saga-order-fulfillment.rs`

**Actions**:
1. Define `OrderAggregate` struct with:
   - `state_store: InMemoryStateStore<Order>`
   - `event_store: InMemoryEventStore<InMemoryEventBus<ApplicationEvent>>`

2. Implement `EventApplicator<ApplicationEvent>` for `OrderAggregate`:
   - `type State = Order`
   - `type EventType = OrderEvent`
   - `type StateStore = InMemoryStateStore<Order>`
   - `type ApplyError = OrderError`
   - Implement `get_state_store()` method
   - Implement `apply()` method to handle:
     - `OrderPlaced`: Create new Order with Pending status
     - `OrderConfirmed`: Update status to Confirmed
     - `OrderCancelled`: Update status to Cancelled

**Test**: Verify `apply()` logic handles all event types correctly

**Verification**: `cargo build --example saga-order-fulfillment`

---

### Step 6: Implement OrderAggregate command handling

**File**: `epoch/examples/saga-order-fulfillment.rs`

**Actions**:
1. Implement `Aggregate<ApplicationEvent>` for `OrderAggregate`:
   - `type CommandData = ApplicationCommand`
   - `type CommandCredentials = ()`
   - `type Command = OrderCommand`
   - `type AggregateError = OrderError`
   - `type EventStore = InMemoryEventStore<InMemoryEventBus<ApplicationEvent>>`
   - Implement `get_event_store()` method
   - Implement `handle_command()` method to handle:
     - `PlaceOrder`: Emit `OrderPlaced` event
     - `ConfirmOrder`: Check state, emit `OrderConfirmed` event
     - `CancelOrder`: Check state, emit `OrderCancelled` event

**Test**: Commands should produce appropriate events

**Verification**: `cargo build --example saga-order-fulfillment`

---

### Step 7: Implement Inventory aggregate (state, errors, EventApplicator, Aggregate)

**File**: `epoch/examples/saga-order-fulfillment.rs`

**Actions**:
1. Define `Inventory` struct with:
   - `id: Uuid`
   - `reserved_for_order: Option<Uuid>`
   - `items: Vec<String>`
   - `version: u64`

2. Define `InventoryError` enum

3. Implement `EventApplicatorState` and `AggregateState` traits

4. Implement `EventApplicator<ApplicationEvent>` for `InventoryAggregate`

5. Implement `Aggregate<ApplicationEvent>` for `InventoryAggregate`:
   - Handle `ReserveItems`: Emit `ItemsReserved`
   - Handle `ReleaseItems`: Emit `ItemsReleased`

**Test**: Verify inventory reservation logic

**Verification**: `cargo build --example saga-order-fulfillment`

---

### Step 8: Implement Payment aggregate (state, errors, EventApplicator, Aggregate)

**File**: `epoch/examples/saga-order-fulfillment.rs`

**Actions**:
1. Define `Payment` struct with:
   - `id: Uuid`
   - `order_id: Uuid`
   - `amount: f64`
   - `processed: bool`
   - `version: u64`

2. Define `PaymentError` enum

3. Implement `EventApplicatorState` and `AggregateState` traits

4. Implement `EventApplicator<ApplicationEvent>` for `PaymentAggregate`

5. Implement `Aggregate<ApplicationEvent>` for `PaymentAggregate`:
   - Handle `ProcessPayment`: Emit `PaymentProcessed` (simulated success)

**Test**: Verify payment processing logic

**Verification**: `cargo build --example saga-order-fulfillment`

---

### Step 9: Define saga state machine

**File**: `epoch/examples/saga-order-fulfillment.rs`

**Actions**:
1. Define `OrderFulfillmentState` enum with derives `Debug, Clone, Serialize, Deserialize`:
   - `Pending` (with `#[default]`)
   - `InventoryReserved { order_id: Uuid, items: Vec<String> }`
   - `PaymentProcessed { order_id: Uuid }`
   - `Completed`
   - `Failed { reason: String }`

2. Implement `Default` trait (should default to `Pending`)

**Test**: Verify state enum compiles and has correct default

**Verification**: `cargo build --example saga-order-fulfillment`

---

### Step 10: Implement OrderFulfillmentSaga struct and error types

**File**: `epoch/examples/saga-order-fulfillment.rs`

**Actions**:
1. Define `OrderFulfillmentSaga` struct with:
   - `state_store: InMemoryStateStore<OrderFulfillmentState>`
   - `order_aggregate: Arc<OrderAggregate>`
   - `inventory_aggregate: Arc<InventoryAggregate>`
   - `payment_aggregate: Arc<PaymentAggregate>`

2. Add `#[derive(SubscriberId)]` with `#[subscriber_id(prefix = "saga")]`

3. Define `OrderFulfillmentError` enum using `thiserror`:
   - `AggregateError(String)`
   - `InvalidStateTransition(String)`

4. Implement `new()` constructor method

**Test**: Verify struct compiles with SubscriberId derive

**Verification**: `cargo build --example saga-order-fulfillment`

---

### Step 11: Implement Saga trait for OrderFulfillmentSaga

**File**: `epoch/examples/saga-order-fulfillment.rs`

**Actions**:
1. Implement `Saga<ApplicationEvent>` for `OrderFulfillmentSaga`:
   - `type State = OrderFulfillmentState`
   - `type StateStore = InMemoryStateStore<OrderFulfillmentState>`
   - `type SagaError = OrderFulfillmentError`
   - `type EventType = OrderFulfillmentEvent`
   - Implement `get_state_store()` method

2. Implement `handle_event()` method with state machine logic:
   - `(Pending, OrderPlaced)` → Dispatch `ReserveItems` → `InventoryReserved`
   - `(InventoryReserved, ItemsReserved)` → Dispatch `ProcessPayment` → `PaymentProcessed`
   - `(PaymentProcessed, PaymentProcessed)` → Dispatch `ConfirmOrder` → `Completed`
   - `(*, OrderConfirmed)` → `Completed`

3. Add logging statements using `log::info!` for each state transition

**Test**: Verify saga state transitions work correctly

**Verification**: `cargo build --example saga-order-fulfillment`

---

### Step 12: Implement main function

**File**: `epoch/examples/saga-order-fulfillment.rs`

**Actions**:
1. Initialize `env_logger`

2. Create event bus: `InMemoryEventBus<ApplicationEvent>`

3. Create event store: `InMemoryEventStore::new(bus)`

4. Create state stores for all aggregates and saga

5. Create aggregate instances (OrderAggregate, InventoryAggregate, PaymentAggregate)

6. Create saga instance with Arc-wrapped aggregates

7. Subscribe saga to event bus using `SagaHandler`:
   ```rust
   bus.subscribe(SagaHandler::new(saga)).await?;
   ```

8. Execute the workflow:
   - Create order ID
   - Place order using `order_aggregate.handle(PlaceOrder command)`
   - Add delays or waits to allow event processing
   - Print final states

9. Add logging statements to trace the workflow

**Test**: Run the example and verify complete workflow execution

**Verification**: `cargo run --example saga-order-fulfillment`

**Expected output**: Should show order placement, saga transitions, and completion

---

### Step 13: Add comprehensive documentation

**File**: `epoch/examples/saga-order-fulfillment.rs`

**Actions**:
1. Add module-level doc comment explaining:
   - What sagas are
   - The order fulfillment scenario
   - The state machine flow
   - Key patterns demonstrated

2. Add doc comments to:
   - `OrderFulfillmentState` enum
   - `OrderFulfillmentSaga` struct
   - Key methods

3. Add inline comments explaining complex logic

**Test**: Run `cargo doc` and verify documentation renders correctly

**Verification**: `cargo doc --no-deps --open`

---

### Step 14: Update Cargo.toml

**File**: `epoch/Cargo.toml`

**Actions**:
1. Add example entry:
```toml
[[example]]
name = "saga-order-fulfillment"
path = "examples/saga-order-fulfillment.rs"
```

**Test**: Verify example can be run with `cargo run --example saga-order-fulfillment`

**Verification**: `cargo run --example saga-order-fulfillment`

---

### Step 15: Update README.md

**File**: `README.md`

**Actions**:
1. Add reference to the new example in the Quick Start or Examples section:
```markdown
See [`epoch/examples/hello-world.rs`](epoch/examples/hello-world.rs) for a complete example.
See [`epoch/examples/saga-order-fulfillment.rs`](epoch/examples/saga-order-fulfillment.rs) for saga pattern demonstration.
```

**Test**: Verify README renders correctly

**Verification**: Read the updated README

---

### Step 16: Final verification

**Actions**:
1. Run the example: `cargo run --example saga-order-fulfillment`
2. Verify output shows complete workflow
3. Run tests: `cargo test`
4. Run clippy: `cargo clippy --example saga-order-fulfillment -- -D warnings`
5. Run fmt: `cargo fmt --check`

**Expected results**:
- Example runs without errors
- Shows clear state transitions
- All tests pass
- No clippy warnings
- Code is properly formatted

---

## Summary

This implementation plan follows TDD principles by:
1. Building incrementally from simple to complex
2. Verifying each step compiles before proceeding
3. Creating all necessary types before using them
4. Testing the final integration

The implementation is structured to minimize errors and maximize clarity, with each step building on the previous one.

## Estimated Time

- Steps 1-3: Event/Command setup (~15 min)
- Steps 4-8: Aggregate implementation (~45 min)
- Steps 9-11: Saga implementation (~30 min)
- Steps 12-16: Integration and documentation (~30 min)
- **Total**: ~2 hours

## Notes

- Use `Arc` for sharing aggregates with the saga
- Remember to use `SagaHandler::new()` when subscribing saga to event bus
- Add appropriate `tokio::time::sleep()` calls if needed to allow async event processing
- Keep business logic simple - focus on demonstrating the pattern
