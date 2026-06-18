//! Event schema evolution via forward-only **upcasting**.
//!
//! Once an event variant is persisted, its serialized shape is frozen in the store
//! forever, yet the in-memory [`EventData`](crate::event::EventData) type it must
//! deserialize into keeps changing as the domain evolves. Upcasting bridges that gap:
//! a stored payload is transformed forward, **one version at a time**, to the current
//! schema *before* it is deserialized into the domain type.
//!
//! # Conceptual model
//!
//! ```text
//! stored row:  { event_type: "OrderPlaced", schema_version: 1, data: <json v1> }
//!                                    │
//!                                    ▼
//!         ┌─────────────────────────────────────────────┐
//!         │  UpcasterRegistry.upcast(event_type, from=1) │   apply step 1→2, 2→3, …
//!         └─────────────────────────────────────────────┘
//!                                    │  json upcast to current version (3)
//!                                    ▼
//!                 serde_json::from_value::<D>(json_v3)  ──►  Event<D>
//! ```
//!
//! An [`Upcaster`] is a single, version-pinned transformation
//! `(event_type, from_version) → json at from_version + 1`. An [`UpcasterRegistry`]
//! holds the ordered chain per `event_type` and applies steps until the payload reaches
//! the type's *current* version, then hands off to serde. Chaining keeps each step
//! small and independently testable.
//!
//! This module is gated behind the opt-in `upcasting` cargo feature, which adds an
//! optional `serde_json` dependency. With the feature off, `epoch_core`'s dependency
//! set and public API are unchanged.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;
use uuid::Uuid;

use crate::event::EventData;

/// Re-export [`SchemaVersion`](crate::event::SchemaVersion) from the event module so
/// that code importing from this module gets a consistent, unambiguous type.
pub use crate::event::SchemaVersion;

/// Context handed to an [`Upcaster`] step.
///
/// Carries the immutable envelope metadata an upcaster may need to make a decision.
/// An upcaster must **not** mutate this context; it only transforms the payload value.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct UpcastContext<'a> {
    /// The `event_type` (PascalCase) the payload belongs to.
    pub event_type: &'a str,
    /// The version this step upgrades *from*. The step produces `from_version + 1`.
    pub from_version: SchemaVersion,
    /// The id of the stream the event belongs to.
    pub stream_id: Uuid,
    /// The id of the event being upcast.
    pub event_id: Uuid,
}

impl<'a> UpcastContext<'a> {
    /// Creates a new [`UpcastContext`].
    pub fn new(
        event_type: &'a str,
        from_version: SchemaVersion,
        stream_id: Uuid,
        event_id: Uuid,
    ) -> Self {
        Self {
            event_type,
            from_version,
            stream_id,
            event_id,
        }
    }
}

/// A single forward transformation of one `event_type`'s payload from
/// [`from_version()`](Upcaster::from_version) to `from_version() + 1`.
///
/// # Purity contract
///
/// Implementations **must be pure and deterministic**: given the same context and
/// payload they must always produce the same output and must not perform I/O or depend
/// on external mutable state. This guarantees that re-running a replay/rehydration
/// yields identical results, which the framework relies on for idempotent, repeatable
/// projections.
pub trait Upcaster: Send + Sync {
    /// The `event_type` (PascalCase, matching
    /// [`EventData::event_type`](crate::event::EventData::event_type)) this step applies
    /// to.
    fn event_type(&self) -> &str;

    /// The version this step upgrades *from*. It always produces `from_version() + 1`.
    #[allow(clippy::wrong_self_convention)]
    fn from_version(&self) -> SchemaVersion;

    /// Transform the JSON payload one version forward.
    ///
    /// Must be pure and deterministic (see the [trait-level contract](Upcaster)).
    fn upcast(&self, ctx: &UpcastContext<'_>, payload: Value) -> Result<Value, UpcastError>;
}

/// Outcome policy applied when the *final* deserialization (after all upcast steps)
/// fails, or when an upcaster step itself errors.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FailurePolicy {
    /// Abort the read/replay with a typed error. **Default** — never lose an event
    /// silently.
    #[default]
    Fail,
    /// Route the offending event to the registered dead-letter sink, increment the
    /// `upcast_dead_lettered` counter, log at `ERROR`, and *continue* the stream.
    ///
    /// This is the **only** way to skip an event, and it is explicit, counted, and
    /// captured.
    DeadLetter,
}

/// Errors produced while upcasting a stored payload forward to the current schema.
#[derive(Debug, thiserror::Error)]
pub enum UpcastError {
    /// No upcaster was registered for an intermediate version in the chain, so the
    /// payload could not be advanced to the current version.
    #[error(
        "no upcaster registered for {event_type} at version {from_version}, \
         but current version is {current_version}"
    )]
    MissingStep {
        /// The event type whose chain has a gap.
        event_type: String,
        /// The version at which the chain is missing a step.
        from_version: SchemaVersion,
        /// The current (target) version for the event type.
        current_version: SchemaVersion,
    },

    /// An upcaster step itself returned an error.
    #[error("upcaster for {event_type} v{from_version} failed: {source}")]
    Step {
        /// The event type being upcast.
        event_type: String,
        /// The version the failing step was upgrading from.
        from_version: SchemaVersion,
        /// The underlying error returned by the upcaster step.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// The stored version is newer than the current version: the running binary is
    /// older than the data it is reading. This is an operator/deploy error and is
    /// **never** skippable, even under [`FailurePolicy::DeadLetter`].
    #[error(
        "stored schema_version {stored} is newer than current {current} for {event_type} \
         (binary is older than the data it is reading)"
    )]
    FutureVersion {
        /// The event type being read.
        event_type: String,
        /// The version stored on disk.
        stored: SchemaVersion,
        /// The current version the binary understands.
        current: SchemaVersion,
    },

    /// The payload could not be deserialized into the domain type even after all
    /// upcast steps were applied.
    #[error("failed to deserialize {event_type} at version {version}: {source}")]
    Deserialize {
        /// The event type being deserialized.
        event_type: String,
        /// The (current) version the payload was upcast to before deserialization.
        version: SchemaVersion,
        /// The underlying serde error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// Captures an event that could not be upcast/deserialized, for the
/// [`FailurePolicy::DeadLetter`] policy.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct DeadLetteredEvent {
    /// The id of the event that could not be processed.
    pub event_id: Uuid,
    /// The id of the stream the event belongs to.
    pub stream_id: Uuid,
    /// The event type.
    pub event_type: String,
    /// The version the payload was stored at.
    pub stored_version: SchemaVersion,
    /// The raw, un-upcast payload as read from the store.
    pub raw_payload: Value,
    /// A human-readable description of why the event was dead-lettered.
    pub reason: String,
}

impl DeadLetteredEvent {
    /// Creates a new [`DeadLetteredEvent`].
    pub fn new(
        event_id: Uuid,
        stream_id: Uuid,
        event_type: impl Into<String>,
        stored_version: SchemaVersion,
        raw_payload: Value,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            event_id,
            stream_id,
            event_type: event_type.into(),
            stored_version,
            raw_payload,
            reason: reason.into(),
        }
    }
}

/// A sink that captures events which could not be upcast/deserialized under the
/// [`FailurePolicy::DeadLetter`] policy.
#[async_trait::async_trait]
pub trait DeadLetterSink: Send + Sync {
    /// Capture a dead-lettered event for later inspection / recovery.
    async fn capture(&self, event: DeadLetteredEvent);
}

/// Counters tracking upcasting outcomes.
///
/// Exposed via [`UpcasterRegistry::counters`] for metrics integration and assertions.
#[derive(Debug, Default)]
pub struct UpcastCounters {
    applied: AtomicU64,
    succeeded: AtomicU64,
    failed: AtomicU64,
    dead_lettered: AtomicU64,
}

impl UpcastCounters {
    /// Number of individual upcaster steps applied across all reads.
    pub fn applied(&self) -> u64 {
        self.applied.load(Ordering::Relaxed)
    }

    /// Number of events that were successfully upcast and deserialized.
    pub fn succeeded(&self) -> u64 {
        self.succeeded.load(Ordering::Relaxed)
    }

    /// Number of events whose upcasting/deserialization failed.
    pub fn failed(&self) -> u64 {
        self.failed.load(Ordering::Relaxed)
    }

    /// Number of events routed to the dead-letter sink.
    pub fn dead_lettered(&self) -> u64 {
        self.dead_lettered.load(Ordering::Relaxed)
    }
}

/// Holds the per-`event_type` upcaster chains, the per-type current version, the
/// failure policy, and an optional dead-letter sink.
///
/// An empty registry (the [`Default`]) applies no transformations: any event that
/// still deserializes cleanly behaves exactly as it would without the registry.
#[derive(Default)]
pub struct UpcasterRegistry {
    /// `event_type → (from_version → step)`, ordered ascending by `from_version`.
    chains: HashMap<String, BTreeMap<SchemaVersion, Box<dyn Upcaster>>>,
    /// `event_type → current version`, derived as `max(from_version) + 1` over the
    /// registered steps. Types with no registered upcaster are implicitly version `1`.
    current_versions: HashMap<String, SchemaVersion>,
    /// Policy applied when upcasting/deserialization fails.
    policy: FailurePolicy,
    /// Optional sink invoked under [`FailurePolicy::DeadLetter`].
    sink: Option<std::sync::Arc<dyn DeadLetterSink>>,
    /// Observability counters.
    counters: UpcastCounters,
}

impl UpcasterRegistry {
    /// Creates a new, empty registry with the default [`FailurePolicy::Fail`] policy.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers an upcaster step.
    ///
    /// Steps may be registered in any order; the registry keeps each chain sorted by
    /// `from_version`. The type's current version is updated to `from_version + 1` if
    /// that exceeds the previously-known current version.
    pub fn register(&mut self, upcaster: impl Upcaster + 'static) -> &mut Self {
        let event_type = upcaster.event_type().to_string();
        let from_version = upcaster.from_version();
        let entry = self.current_versions.entry(event_type.clone()).or_insert(1);
        if from_version + 1 > *entry {
            *entry = from_version + 1;
        }
        self.chains
            .entry(event_type)
            .or_default()
            .insert(from_version, Box::new(upcaster));
        self
    }

    /// Sets the [`FailurePolicy`].
    pub fn with_policy(&mut self, policy: FailurePolicy) -> &mut Self {
        self.policy = policy;
        self
    }

    /// Sets the dead-letter sink used under [`FailurePolicy::DeadLetter`].
    pub fn with_dead_letter_sink(&mut self, sink: impl DeadLetterSink + 'static) -> &mut Self {
        self.sink = Some(std::sync::Arc::new(sink));
        self
    }

    /// The configured failure policy.
    pub fn policy(&self) -> FailurePolicy {
        self.policy
    }

    /// The observability counters for this registry.
    pub fn counters(&self) -> &UpcastCounters {
        &self.counters
    }

    /// The current (target) version for `event_type`.
    ///
    /// Derived from the registered upcaster chain as `max(from_version) + 1`; a type
    /// with no registered upcaster is version `1`.
    pub fn current_version(&self, event_type: &str) -> SchemaVersion {
        self.current_versions.get(event_type).copied().unwrap_or(1)
    }

    /// Apply the chain for `event_type` starting at `from_version` until the current
    /// version, returning the upcast JSON ready for `serde_json::from_value::<D>`.
    ///
    /// Returns [`UpcastError::FutureVersion`] if `from_version` is newer than the
    /// current version, and [`UpcastError::MissingStep`] if a step is missing from the
    /// chain before the current version is reached.
    pub fn upcast(&self, ctx: &UpcastContext<'_>, payload: Value) -> Result<Value, UpcastError> {
        let current = self.current_version(ctx.event_type);

        if ctx.from_version > current {
            return Err(UpcastError::FutureVersion {
                event_type: ctx.event_type.to_string(),
                stored: ctx.from_version,
                current,
            });
        }

        let mut version = ctx.from_version;
        let mut payload = payload;

        while version < current {
            let step = self
                .chains
                .get(ctx.event_type)
                .and_then(|chain| chain.get(&version))
                .ok_or_else(|| UpcastError::MissingStep {
                    event_type: ctx.event_type.to_string(),
                    from_version: version,
                    current_version: current,
                })?;

            let step_ctx = UpcastContext {
                event_type: ctx.event_type,
                from_version: version,
                stream_id: ctx.stream_id,
                event_id: ctx.event_id,
            };

            payload = step.upcast(&step_ctx, payload)?;
            self.counters.applied.fetch_add(1, Ordering::Relaxed);
            log::debug!(
                "upcast applied: event_type={} {}→{}",
                ctx.event_type,
                version,
                version + 1
            );
            version += 1;
        }

        Ok(payload)
    }

    /// One-call convenience used by backends: upcast then deserialize into `D`,
    /// applying the configured [`FailurePolicy`].
    ///
    /// Returns `Ok(None)` ONLY when an event was explicitly dead-lettered (so the
    /// backend skips exactly that row) or when `payload` is `None` (a purged event).
    /// A [`UpcastError::FutureVersion`] always propagates as `Err`, even under
    /// [`FailurePolicy::DeadLetter`].
    pub fn upcast_and_deserialize<D: EventData>(
        &self,
        event_type: &str,
        stored_version: SchemaVersion,
        stream_id: Uuid,
        event_id: Uuid,
        payload: Option<Value>,
    ) -> Result<Option<D>, UpcastError> {
        let Some(payload) = payload else {
            // Purged event (no data) — nothing to upcast.
            return Ok(None);
        };

        let ctx = UpcastContext::new(event_type, stored_version, stream_id, event_id);
        let current = self.current_version(event_type);

        let upcasted = match self.upcast(&ctx, payload.clone()) {
            Ok(value) => value,
            // A future version is an operator/deploy error and is never skippable.
            Err(err @ UpcastError::FutureVersion { .. }) => {
                self.counters.failed.fetch_add(1, Ordering::Relaxed);
                return Err(err);
            }
            Err(err) => {
                return self.handle_failure(
                    event_type,
                    stored_version,
                    stream_id,
                    event_id,
                    payload,
                    err,
                );
            }
        };

        match serde_json::from_value::<D>(upcasted) {
            Ok(data) => {
                self.counters.succeeded.fetch_add(1, Ordering::Relaxed);
                Ok(Some(data))
            }
            Err(source) => {
                let err = UpcastError::Deserialize {
                    event_type: event_type.to_string(),
                    version: current,
                    source: Box::new(source),
                };
                self.handle_failure(
                    event_type,
                    stored_version,
                    stream_id,
                    event_id,
                    payload,
                    err,
                )
            }
        }
    }

    /// Applies the configured [`FailurePolicy`] to a non-future-version failure.
    fn handle_failure<D>(
        &self,
        event_type: &str,
        stored_version: SchemaVersion,
        stream_id: Uuid,
        event_id: Uuid,
        raw_payload: Value,
        err: UpcastError,
    ) -> Result<Option<D>, UpcastError> {
        match self.policy {
            FailurePolicy::Fail => {
                self.counters.failed.fetch_add(1, Ordering::Relaxed);
                log::error!(
                    "upcast failed (policy=Fail): event_id={event_id} event_type={event_type} \
                     stored_version={stored_version} current_version={} reason={err}",
                    self.current_version(event_type),
                );
                Err(err)
            }
            FailurePolicy::DeadLetter => {
                self.counters.dead_lettered.fetch_add(1, Ordering::Relaxed);
                let reason = err.to_string();
                log::error!(
                    "upcast dead-lettered (policy=DeadLetter): event_id={event_id} \
                     event_type={event_type} stored_version={stored_version} \
                     current_version={} reason={reason}",
                    self.current_version(event_type),
                );
                if let Some(sink) = self.sink.clone() {
                    let dle = DeadLetteredEvent::new(
                        event_id,
                        stream_id,
                        event_type,
                        stored_version,
                        raw_payload,
                        reason,
                    );
                    // Capture is fire-and-forget: it must not block or fail the read.
                    tokio::spawn(async move {
                        sink.capture(dle).await;
                    });
                }
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    /// A simple test upcaster that merges a fixed JSON object into the payload and
    /// records that it was invoked.
    struct AddFieldUpcaster {
        event_type: &'static str,
        from_version: SchemaVersion,
        field: &'static str,
        value: Value,
    }

    impl Upcaster for AddFieldUpcaster {
        fn event_type(&self) -> &str {
            self.event_type
        }
        fn from_version(&self) -> SchemaVersion {
            self.from_version
        }
        fn upcast(
            &self,
            _ctx: &UpcastContext<'_>,
            mut payload: Value,
        ) -> Result<Value, UpcastError> {
            if let Value::Object(map) = &mut payload {
                map.insert(self.field.to_string(), self.value.clone());
            }
            Ok(payload)
        }
    }

    fn ctx(event_type: &str, from: SchemaVersion) -> UpcastContext<'_> {
        UpcastContext::new(event_type, from, Uuid::new_v4(), Uuid::new_v4())
    }

    #[test]
    fn empty_registry_is_a_no_op() {
        let registry = UpcasterRegistry::new();
        let payload = json!({ "a": 1 });
        let out = registry
            .upcast(&ctx("OrderPlaced", 1), payload.clone())
            .unwrap();
        assert_eq!(out, payload);
        assert_eq!(registry.current_version("OrderPlaced"), 1);
    }

    #[test]
    fn single_step_chain_applies_once() {
        let mut registry = UpcasterRegistry::new();
        registry.register(AddFieldUpcaster {
            event_type: "OrderPlaced",
            from_version: 1,
            field: "currency",
            value: json!("USD"),
        });
        assert_eq!(registry.current_version("OrderPlaced"), 2);

        let out = registry
            .upcast(&ctx("OrderPlaced", 1), json!({ "amount": 10 }))
            .unwrap();
        assert_eq!(out, json!({ "amount": 10, "currency": "USD" }));
        assert_eq!(registry.counters().applied(), 1);
    }

    #[test]
    fn multi_step_chain_applies_in_order() {
        let mut registry = UpcasterRegistry::new();
        // Register out of order to prove ordering is by from_version.
        registry.register(AddFieldUpcaster {
            event_type: "OrderPlaced",
            from_version: 2,
            field: "region",
            value: json!("EU"),
        });
        registry.register(AddFieldUpcaster {
            event_type: "OrderPlaced",
            from_version: 1,
            field: "currency",
            value: json!("USD"),
        });
        assert_eq!(registry.current_version("OrderPlaced"), 3);

        let out = registry
            .upcast(&ctx("OrderPlaced", 1), json!({ "amount": 10 }))
            .unwrap();
        assert_eq!(
            out,
            json!({ "amount": 10, "currency": "USD", "region": "EU" })
        );
        assert_eq!(registry.counters().applied(), 2);
    }

    #[test]
    fn partial_chain_from_intermediate_version() {
        let mut registry = UpcasterRegistry::new();
        registry.register(AddFieldUpcaster {
            event_type: "OrderPlaced",
            from_version: 1,
            field: "currency",
            value: json!("USD"),
        });
        registry.register(AddFieldUpcaster {
            event_type: "OrderPlaced",
            from_version: 2,
            field: "region",
            value: json!("EU"),
        });

        // Starting at version 2 only applies the 2→3 step.
        let out = registry
            .upcast(&ctx("OrderPlaced", 2), json!({ "amount": 10 }))
            .unwrap();
        assert_eq!(out, json!({ "amount": 10, "region": "EU" }));
        assert_eq!(registry.counters().applied(), 1);
    }

    #[test]
    fn missing_step_is_detected() {
        let mut registry = UpcasterRegistry::new();
        // Register 1→2 and 3→4 but NOT 2→3, leaving a gap.
        registry.register(AddFieldUpcaster {
            event_type: "OrderPlaced",
            from_version: 1,
            field: "a",
            value: json!(1),
        });
        registry.register(AddFieldUpcaster {
            event_type: "OrderPlaced",
            from_version: 3,
            field: "c",
            value: json!(3),
        });
        assert_eq!(registry.current_version("OrderPlaced"), 4);

        let err = registry
            .upcast(&ctx("OrderPlaced", 1), json!({}))
            .unwrap_err();
        match err {
            UpcastError::MissingStep {
                event_type,
                from_version,
                current_version,
            } => {
                assert_eq!(event_type, "OrderPlaced");
                assert_eq!(from_version, 2);
                assert_eq!(current_version, 4);
            }
            other => panic!("expected MissingStep, got {other:?}"),
        }
    }

    #[test]
    fn future_version_is_detected() {
        let mut registry = UpcasterRegistry::new();
        registry.register(AddFieldUpcaster {
            event_type: "OrderPlaced",
            from_version: 1,
            field: "a",
            value: json!(1),
        });
        // current is 2; stored version 3 is from the future.
        let err = registry
            .upcast(&ctx("OrderPlaced", 3), json!({}))
            .unwrap_err();
        match err {
            UpcastError::FutureVersion {
                event_type,
                stored,
                current,
            } => {
                assert_eq!(event_type, "OrderPlaced");
                assert_eq!(stored, 3);
                assert_eq!(current, 2);
            }
            other => panic!("expected FutureVersion, got {other:?}"),
        }
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct OrderPlaced {
        amount: u64,
        currency: String,
    }

    impl EventData for OrderPlaced {
        fn event_type(&self) -> &'static str {
            "OrderPlaced"
        }
    }

    #[tokio::test]
    async fn upcast_and_deserialize_applies_chain() {
        let mut registry = UpcasterRegistry::new();
        registry.register(AddFieldUpcaster {
            event_type: "OrderPlaced",
            from_version: 1,
            field: "currency",
            value: json!("USD"),
        });

        let out: Option<OrderPlaced> = registry
            .upcast_and_deserialize(
                "OrderPlaced",
                1,
                Uuid::new_v4(),
                Uuid::new_v4(),
                Some(json!({ "amount": 42 })),
            )
            .unwrap();
        assert_eq!(
            out,
            Some(OrderPlaced {
                amount: 42,
                currency: "USD".to_string(),
            })
        );
        assert_eq!(registry.counters().succeeded(), 1);
    }

    #[tokio::test]
    async fn fail_policy_aborts_on_deserialize_error() {
        let registry = UpcasterRegistry::new();
        // No upcaster, payload missing `currency` → deserialize fails under Fail.
        let result: Result<Option<OrderPlaced>, _> = registry.upcast_and_deserialize(
            "OrderPlaced",
            1,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Some(json!({ "amount": 42 })),
        );
        assert!(matches!(result, Err(UpcastError::Deserialize { .. })));
        assert_eq!(registry.counters().failed(), 1);
    }
}
