# Event Schema Evolution Guide

Event sourcing stores facts forever. The in-memory type those facts deserialize into
keeps changing as the domain evolves. This guide explains how to bridge that gap using
Epoch's built-in **upcasting** mechanism: forward-only, version-pinned JSON
transformations applied at the deserialization boundary, before serde ever sees the
payload.

> **TL;DR** — Every breaking change to a persisted event type needs an upcaster. This
> guide tells you which changes are breaking, how to write and register the upcaster,
> and how to choose a failure policy.

---

## Table of Contents

1. [Conceptual Model](#1-conceptual-model)
2. [Change-Kind Matrix](#2-change-kind-matrix)
3. [Getting Started: Adding the Feature](#3-getting-started-adding-the-feature)
4. [Worked Examples](#4-worked-examples)
   - [a. Add an optional field (additive-safe)](#a-add-an-optional-field-additive-safe)
   - [b. Rename a field](#b-rename-a-field)
   - [c. Remove a field](#c-remove-a-field)
   - [d. Change a field type](#d-change-a-field-type)
   - [e. Add a new required field with a default](#e-add-a-new-required-field-with-an-upcaster-supplied-default)
   - [f. Multi-step chain (chain multiple upcasters)](#f-multi-step-chain)
5. [Failure Policy: Fail vs. DeadLetter](#5-failure-policy-fail-vs-deadletter)
6. [Splitting or Merging Event Types](#6-splitting-or-merging-event-types)
7. [Testing Your Upcasters](#7-testing-your-upcasters)
8. [Operational Checklist](#8-operational-checklist)
9. [FAQ](#9-faq)

---

## 1. Conceptual Model

```text
stored row:  { event_type: "OrderPlaced", schema_version: 1, data: <json v1> }
                                   │
                                   ▼
       ┌─────────────────────────────────────────────────┐
       │  UpcasterRegistry.upcast("OrderPlaced", from=1)  │  1→2, 2→3, …
       └─────────────────────────────────────────────────┘
                                   │  json at current version (3)
                                   ▼
               serde_json::from_value::<OrderPlaced>(json_v3) ──► Event<D>
```

Key rules:

- **One upcaster per version step.** A step takes a payload from version N to N+1. If
  the current type is at version 3, you need two steps: `1→2` and `2→3`.
- **Steps are pure and deterministic.** No I/O, no external state. Replaying the same
  stored event must always produce the same result.
- **The `upcasting` feature is opt-in.** Projects that register no upcasters behave
  exactly as before.
- **Fail by default.** If upcasting fails and no `DeadLetter` policy is set, the error
  propagates loudly. Silent event loss is impossible unless you explicitly opt in to
  `FailurePolicy::DeadLetter`.

---

## 2. Change-Kind Matrix

| Change kind | `#[serde(default)]` enough? | Requires upcaster? | Notes |
|---|---|---|---|
| Add optional field (`Option<T>`) | ✅ yes | no | `#[serde(default)]` → `None` for old rows |
| Add optional field with non-`None` default | ✅ yes | no | `#[serde(default = "fn")]` supplies a non-`None` value |
| Add required field with a derived default | ❌ no | **yes** | Upcaster supplies the value (example [e](#e-add-a-new-required-field-with-an-upcaster-supplied-default)) |
| Rename a field | ❌ no | **yes** | Upcaster copies old key to new key (example [b](#b-rename-a-field)) |
| Remove a field | ❌ no | **yes** (or `#[serde(rename)]` workaround) | Upcaster drops the key; or use `#[serde(rename)]` to keep the stored name but rename Rust field |
| Change a field type (widening) | ❌ no | **yes** | Upcaster converts the raw value (example [d](#d-change-a-field-type)) |
| Change a field type (narrowing) | ❌ no | **yes** | Same; error-handling inside the step |
| Rename a variant | ❌ no | **yes** | The `event_type` key in the DB changes; treat as a new type + rebuild |
| Split a variant into two | ❌ no | **new type + rebuild** | See §6 |
| Merge two variants into one | ❌ no | **new type + rebuild** | See §6 |

> **Rule of thumb:** if the stored JSON for version N cannot round-trip through the
> current Rust type without data loss or an error, you need an upcaster.

---

## 3. Getting Started: Adding the Feature

### 3.1 Enable the feature

In `epoch_core` (or via the `epoch` umbrella crate):

```toml
# Cargo.toml
[dependencies]
epoch_core = { version = "0.1", features = ["upcasting"] }

# — or via the umbrella crate:
epoch = { version = "0.1", features = ["upcasting"] }
```

`epoch_pg` already enables `upcasting` internally. No extra flag is needed there.

### 3.2 Apply the database migration

The upcasting feature requires the `schema_version` column added by migration
`m012_add_schema_version_to_events`. Run pending migrations at startup as usual:

```rust
epoch_pg::Migrator::run(&pool).await?;
```

This adds a nullable `INT DEFAULT 1` column with no backfill and no table rewrite.
Pre-migration rows keep `NULL`, which the read path interprets as version `1`.

### 3.3 Create a registry and wire it to the store

```rust
use epoch_core::upcasting::{UpcasterRegistry, FailurePolicy};
use epoch_pg::PgEventStore;
use std::sync::Arc;

let mut registry = UpcasterRegistry::new();
// register upcasters here …

let store = PgEventStore::with_upcasters(
    pool.clone(),
    bus.clone(),
    Arc::new(registry),
);
```

---

## 4. Worked Examples

All examples evolve an `OrderPlaced` event defined in `AppEvent`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, EventData)]
pub enum AppEvent {
    OrderPlaced { /* fields change per example */ },
}
```

### a. Add an optional field (additive-safe)

**Before:**

```rust
OrderPlaced { amount: u64 }
```

**After:**

```rust
OrderPlaced { amount: u64, currency: Option<String> }
```

No upcaster needed. Add `#[serde(default)]` to the new field:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, EventData)]
pub enum AppEvent {
    OrderPlaced {
        amount: u64,
        #[serde(default)]
        currency: Option<String>,
    },
}
```

Old rows deserialize with `currency: None`. New rows carry the value. ✅

---

### b. Rename a field

**Before (v1):**

```rust
OrderPlaced { amount: u64 }
```

**After (v2):**

```rust
OrderPlaced { total_amount: u64 }   // renamed `amount` → `total_amount`
```

**Step 1 — bump the schema version on the type:**

```rust
#[derive(Debug, Clone, Serialize, Deserialize, EventData)]
#[event_data(schema_version = 2)]
pub enum AppEvent {
    OrderPlaced { total_amount: u64 },
}
```

**Step 2 — write the 1→2 upcaster:**

```rust
use epoch_core::upcasting::{Upcaster, UpcastContext, UpcastError, SchemaVersion};
use serde_json::Value;

struct OrderPlacedV1ToV2;

impl Upcaster for OrderPlacedV1ToV2 {
    fn event_type(&self) -> &str { "OrderPlaced" }
    fn from_version(&self) -> SchemaVersion { 1 }

    fn upcast(&self, _ctx: &UpcastContext<'_>, mut payload: Value) -> Result<Value, UpcastError> {
        if let Value::Object(map) = &mut payload {
            if let Some(v) = map.remove("amount") {
                map.insert("total_amount".to_string(), v);
            }
        }
        Ok(payload)
    }
}
```

**Step 3 — register the upcaster:**

```rust
let mut registry = UpcasterRegistry::new();
registry.register(OrderPlacedV1ToV2);
```

Now every stored row with `schema_version = NULL` or `1` has `amount` renamed to
`total_amount` before serde is invoked. New rows are written with `schema_version = 2`
and skip the step entirely.

---

### c. Remove a field

**Before (v1):**

```rust
OrderPlaced { amount: u64, internal_debug: String }
```

**After (v2):**

```rust
OrderPlaced { amount: u64 }   // `internal_debug` removed
```

Option A — **upcaster that drops the key:**

```rust
struct OrderPlacedV1ToV2;

impl Upcaster for OrderPlacedV1ToV2 {
    fn event_type(&self) -> &str { "OrderPlaced" }
    fn from_version(&self) -> SchemaVersion { 1 }

    fn upcast(&self, _ctx: &UpcastContext<'_>, mut payload: Value) -> Result<Value, UpcastError> {
        if let Value::Object(map) = &mut payload {
            map.remove("internal_debug");
        }
        Ok(payload)
    }
}
```

Option B — **`#[serde(skip_deserializing)]` / `#[serde(deny_unknown_fields)]`** is
*not* recommended here because it affects *all* deserialization, not just old rows.
Prefer the explicit upcaster.

---

### d. Change a field type

**Before (v1):** `amount` was stored as a floating-point cents representation.
**After (v2):** `amount` is now an integer (whole cents).

```rust
struct OrderPlacedV1ToV2;

impl Upcaster for OrderPlacedV1ToV2 {
    fn event_type(&self) -> &str { "OrderPlaced" }
    fn from_version(&self) -> SchemaVersion { 1 }

    fn upcast(&self, _ctx: &UpcastContext<'_>, mut payload: Value) -> Result<Value, UpcastError> {
        if let Value::Object(map) = &mut payload {
            if let Some(Value::Number(n)) = map.get("amount") {
                if let Some(f) = n.as_f64() {
                    map.insert("amount".to_string(), Value::Number((f.round() as u64).into()));
                }
            }
        }
        Ok(payload)
    }
}
```

If the conversion can fail (e.g., a value is out of range), return an
`Err(UpcastError::Step { … })`:

```rust
use std::fmt;

#[derive(Debug)]
struct ConversionError(String);
impl fmt::Display for ConversionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{}", self.0) }
}
impl std::error::Error for ConversionError {}

fn upcast(&self, _ctx: &UpcastContext<'_>, mut payload: Value) -> Result<Value, UpcastError> {
    // … validate …
    if value_is_invalid {
        return Err(UpcastError::Step {
            event_type: "OrderPlaced".to_string(),
            from_version: 1,
            source: Box::new(ConversionError("amount out of range".to_string())),
        });
    }
    Ok(payload)
}
```

The `FailurePolicy` then determines whether this error aborts the replay (`Fail`) or
routes the event to the dead-letter sink (`DeadLetter`).

---

### e. Add a new required field with an upcaster-supplied default

**Before (v1):**

```rust
OrderPlaced { amount: u64 }
```

**After (v2):**

```rust
OrderPlaced { amount: u64, currency: String }  // required, non-optional
```

Historic rows have no `currency`. The upcaster supplies a sentinel default:

```rust
struct OrderPlacedV1ToV2;

impl Upcaster for OrderPlacedV1ToV2 {
    fn event_type(&self) -> &str { "OrderPlaced" }
    fn from_version(&self) -> SchemaVersion { 1 }

    fn upcast(&self, _ctx: &UpcastContext<'_>, mut payload: Value) -> Result<Value, UpcastError> {
        if let Value::Object(map) = &mut payload {
            map.entry("currency").or_insert(Value::String("USD".to_string()));
        }
        Ok(payload)
    }
}
```

> **Choose the default carefully.** Using `"USD"` assumes all historic orders were
> placed in USD. If that assumption is wrong, you need a different default or a
> migration job to back-fill a real value via a side-channel (e.g., join with an order
> currency table). The upcaster supplies a *best-effort structural default* so the type
> compiles; business correctness is your responsibility.

---

### f. Multi-step chain

When you accumulate multiple breaking changes over time, each gets its own step:

```
v1 → v2  (rename `amount` → `total_amount`)
v2 → v3  (add required `currency` field)
```

```rust
#[event_data(schema_version = 3)]
pub enum AppEvent {
    OrderPlaced { total_amount: u64, currency: String },
}

let mut registry = UpcasterRegistry::new();
// Register in any order; the registry sorts by from_version automatically.
registry.register(OrderPlacedV2ToV3);  // adds `currency`
registry.register(OrderPlacedV1ToV2);  // renames `amount` → `total_amount`
```

A row stored at version 1 flows through `1→2` then `2→3`. A row stored at version 2
flows through only `2→3`. A row already at version 3 skips both steps.

---

## 5. Failure Policy: Fail vs. DeadLetter

### 5.1 `FailurePolicy::Fail` (default)

Any upcasting or deserialization error propagates as a hard error and aborts the
read/replay. Use this in production unless you have a compelling reason not to. It
prevents silent event loss and ensures your domain state is never silently wrong.

```rust
let mut registry = UpcasterRegistry::new();
// FailurePolicy::Fail is the default — no extra call needed.
```

**When to expect hard errors:**

| Situation | Error produced |
|---|---|
| Missing upcaster step in chain | `UpcastError::MissingStep` |
| Upcaster step returns `Err` | `UpcastError::Step` |
| Final `serde_json::from_value` fails | `UpcastError::Deserialize` |
| Stored `schema_version` > current version | `UpcastError::FutureVersion` |

> **`FutureVersion` always propagates** even under `DeadLetter`. A stored version
> newer than what the binary understands means the binary is older than its data — an
> operator/deployment error that must page someone, not silently skip events.

### 5.2 `FailurePolicy::DeadLetter` (opt-in)

Failed events are captured in a `DeadLetterSink`, an `upcast_dead_lettered` counter is
incremented, an `ERROR` log is emitted, and the stream continues. Use this only when:

- You have a real sink with persistence and alerting.
- You accept that some events will be skipped and have a recovery plan.
- You are in a migration window where legacy data may have unfixable schema issues.

```rust
use epoch_core::upcasting::{
    DeadLetterSink, DeadLetteredEvent, FailurePolicy, UpcasterRegistry,
};
use async_trait::async_trait;

struct MyDeadLetterSink; // persist to DB, publish metric, etc.

#[async_trait]
impl DeadLetterSink for MyDeadLetterSink {
    async fn capture(&self, event: DeadLetteredEvent) {
        // Persist event.raw_payload, event.reason, event.event_type, etc.
        log::error!(
            "dead-lettered event: id={} type={} reason={}",
            event.event_id, event.event_type, event.reason,
        );
        // … write to DLQ table, increment Prometheus counter, etc.
    }
}

let mut registry = UpcasterRegistry::new();
registry
    .with_policy(FailurePolicy::DeadLetter)
    .with_dead_letter_sink(MyDeadLetterSink);
```

### 5.3 Observability counters

```rust
let counters = registry.counters();
println!("applied:       {}", counters.applied());       // individual steps applied
println!("succeeded:     {}", counters.succeeded());     // events fully deserialized
println!("failed:        {}", counters.failed());        // hard failures (Fail policy)
println!("dead_lettered: {}", counters.dead_lettered()); // events routed to DLQ
```

Expose these to your metrics system (Prometheus, etc.) for operational monitoring.

---

## 6. Splitting or Merging Event Types

**This is a structural change that cannot be expressed as an upcaster** because upcasting
is scoped to a single `event_type`. Epoch upcasters do not support cross-type
transformations.

The recommended pattern is:

1. **Introduce the new event type(s)** — add `OrderItemAdded` / `OrderShippingSet` etc.
   alongside the old `OrderPlaced`. Both types coexist.
2. **Rebuild the affected projections** — old events replay into the old shape; new
   events use the new shape. Projections must handle both variants.
3. **Deprecate the old type** — stop writing new `OrderPlaced` events.
4. **Eventually archive** — once the historical window no longer matters (or after a
   controlled migration job), the old variant can be excluded from projections with
   `#[serde(other)]` / skip patterns.

This approach keeps the event log immutable and audit-complete while allowing the domain
model to evolve. The key principle: **never rewrite stored events**; always adapt the
read side.

---

## 7. Testing Your Upcasters

### 7.1 Unit-test each step in isolation

Each upcaster step is a pure JSON→JSON function. Test it directly:

```rust
#[test]
fn v1_to_v2_renames_amount() {
    use serde_json::json;
    use uuid::Uuid;
    use epoch_core::upcasting::{Upcaster, UpcastContext};

    let upcaster = OrderPlacedV1ToV2;
    let ctx = UpcastContext::new("OrderPlaced", 1, Uuid::new_v4(), Uuid::new_v4());
    let input = json!({ "amount": 100 });
    let output = upcaster.upcast(&ctx, input).unwrap();
    assert_eq!(output, json!({ "total_amount": 100 }));
}
```

### 7.2 Test the full chain with `verify_upcasting_chain`

`epoch_core::testing` ships a reusable contract helper (enabled by the `testing`
feature) that verifies a multi-step chain upcasts a v1 payload all the way to the
current version and deserializes it correctly:

```toml
[dev-dependencies]
epoch_core = { version = "0.1", features = ["upcasting", "testing"] }
```

```rust
#[tokio::test]
async fn order_placed_upcasting_chain_is_correct() {
    let mut registry = UpcasterRegistry::new();
    registry.register(OrderPlacedV1ToV2);
    registry.register(OrderPlacedV2ToV3);

    let expected = AppEvent::OrderPlaced { total_amount: 42, currency: "USD".into() };
    epoch_core::testing::verify_upcasting_chain::<AppEvent>(
        &registry,
        "OrderPlaced",
        serde_json::json!({ "amount": 42 }),     // v1 raw payload
        expected,                                // expected deserialized value after full chain
    )
    .await;
}
```

### 7.3 End-to-end test (with Postgres)

Store a raw v1 JSON payload directly into the DB (bypassing the write path), then
reload and assert the aggregate/projection state matches the v2 expectation. See
`epoch_pg/tests/upcasting_integration_tests.rs` for a full example.

### 7.4 Failure-path tests

Always test both `Fail` and `DeadLetter` paths for your registry:

```rust
#[tokio::test]
async fn missing_step_under_fail_aborts() {
    let registry = UpcasterRegistry::new(); // no steps registered, type at v2
    // … register only 1→2 but not 2→3 …
    let result: Result<Option<MyEvent>, _> = registry.upcast_and_deserialize(
        "MyEvent", 1, stream_id, event_id, Some(raw_v1_payload),
    );
    assert!(matches!(result, Err(UpcastError::MissingStep { .. })));
}
```

---

## 8. Operational Checklist

Before deploying a breaking event-type change:

- [ ] Written a unit test for each new upcaster step.
- [ ] Bumped `#[event_data(schema_version = N)]` on the affected enum (or its variant's
  wrapper type).
- [ ] Registered all required steps in `UpcasterRegistry` and wired it to
  `PgEventStore::with_upcasters`.
- [ ] Run `epoch_pg::Migrator::run(&pool).await?` (adds `schema_version` column if not
  present — idempotent).
- [ ] Verified the full upcasting chain with `verify_upcasting_chain` or an equivalent
  end-to-end test.
- [ ] Decided on a `FailurePolicy`. Default (`Fail`) is recommended unless you have a
  live `DeadLetterSink` with alerting.
- [ ] Deployed the new binary before writing any events at the new `schema_version`.
  (Write the readers before the writers.)
- [ ] Confirmed that `counters.failed()` and `counters.dead_lettered()` stay at zero
  post-deploy.

---

## 9. FAQ

**Q: Do I need to backfill the `schema_version` column for existing rows?**

No. Pre-migration rows have `schema_version = NULL`, and the read path treats `NULL`
as `1`. Existing rows are indistinguishable from rows explicitly written with
`schema_version = 1`, which is exactly correct.

**Q: What happens to projects that register no upcasters?**

Behavior is identical to before. The registry is empty; `upcast` is a no-op; events
that still deserialize cleanly succeed. The only visible change is the new nullable
`schema_version` column in the database.

**Q: Can I skip version numbers?**

No. The chain must be contiguous. If you have a type at version 3, you need steps
`1→2` and `2→3`. The registry detects any gap and returns `UpcastError::MissingStep`.

**Q: Can I remove a no-longer-needed upcaster step?**

Only when you are certain no rows at the old version exist in any environment. Removing
a step while rows at that version still exist will cause `MissingStep` errors. The safe
approach: keep old steps indefinitely (they are cheap), and track the minimum version
across all environments before pruning.

**Q: What if the upcaster chain itself has a bug?**

Fix the bug and redeploy. Because upcasting happens on read, not write, the stored
events are unchanged. A corrected upcaster produces the right output on the next replay
or read — no data was permanently lost.

**Q: Can I upcast across event types (e.g., split `OrderPlaced` into two types)?**

Not with the upcaster mechanism. See §6 (Splitting or Merging Event Types) for the
recommended approach.

**Q: What does `FutureVersion` mean and what should I do?**

The stored `schema_version` is higher than what the running binary knows about. This
means the binary is **older than its data** — a deployment order error (e.g., a
rollback that left new data behind, or a new writer deployed before the reader). Fix
the deployment, not the data. `FutureVersion` always propagates as a hard error and
cannot be dead-lettered.
