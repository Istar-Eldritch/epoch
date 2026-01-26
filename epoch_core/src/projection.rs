//! Projection trait for read models that subscribe to the event bus.
//!
//! A [`Projection`] is a read-model built from a stream of events. Unlike
//! [`Aggregate`](crate::aggregate::Aggregate), a projection is designed to be
//! subscribed to an event bus via [`ProjectionHandler`].
//!
//! # Important
//!
//! Do NOT implement `Projection` for types that also implement `Aggregate`.
//! Aggregates manage their own state persistence in `handle()` and should never
//! be subscribed to the event bus for their own events. This would cause
//! duplicate writes and race conditions.
//!
//! The type system enforces this: `Aggregate` extends [`EventApplicator`](crate::event_applicator::EventApplicator),
//! NOT `Projection`, so aggregates cannot be wrapped in `ProjectionHandler`.

use std::sync::Arc;

use crate::event::{Event, EventData};
use crate::event_applicator::EventApplicator;
use crate::state_store::StateStoreBackend;
use async_trait::async_trait;

// Re-export for backward compatibility
#[deprecated(
    since = "0.2.0",
    note = "Use EventApplicatorState from epoch_core::event_applicator instead"
)]
pub use crate::event_applicator::EventApplicatorState as ProjectionState;

#[deprecated(
    since = "0.2.0",
    note = "Use ReHydrateError from epoch_core::event_applicator instead"
)]
pub use crate::event_applicator::ReHydrateError;

/// Errors that can occur during [`Projection::apply_and_store`].
///
/// Note: Conversion failures (when converting from a superset event to the projection's
/// subset event type) are intentionally ignored in `apply_and_store()`. This is expected
/// behavior since projections only handle events from their subset.
#[derive(Debug, thiserror::Error)]
pub enum ApplyAndStoreError<E, S> {
    /// An error occurred while applying an event to the projection.
    #[error("Event application error: {0}")]
    Event(E),

    /// An error occurred while persisting the projection's state to storage.
    #[error("Error persisting state: {0}")]
    State(S),
}

/// A read model that subscribes to the event bus to maintain derived state.
///
/// Unlike [`Aggregate`](crate::aggregate::Aggregate), a `Projection` is designed
/// to be subscribed to an event bus via [`ProjectionHandler`]. It receives events
/// asynchronously and updates its state.
///
/// # Type Safety
///
/// `Projection` extends [`EventApplicator`], adding the [`apply_and_store`](Projection::apply_and_store)
/// method for event bus integration. `Aggregate` also extends `EventApplicator` but does NOT
/// implement `Projection`. This means:
///
/// - ✅ `ProjectionHandler::new(my_projection)` - Works
/// - ❌ `ProjectionHandler::new(my_aggregate)` - Compile error
///
/// # Subscriber ID
///
/// Implementors must also implement [`SubscriberId`](crate::SubscriberId) to provide
/// a unique identifier for checkpoint tracking, multi-instance coordination, and
/// dead letter queue association. Use the `#[derive(SubscriberId)]` macro from
/// `epoch_derive` for automatic implementation:
///
/// ```ignore
/// use epoch_derive::SubscriberId;
///
/// #[derive(SubscriberId)]
/// struct UserProfileProjection { /* ... */ }
/// // Automatically implements SubscriberId with subscriber_id() -> "projection:user-profile"
/// ```
///
/// # Example
///
/// ```ignore
/// use epoch_core::prelude::*;
/// use epoch_derive::SubscriberId;
///
/// #[derive(SubscriberId)]
/// struct ProductProjection {
///     state_store: InMemoryStateStore<Product>,
/// }
///
/// impl EventApplicator<ApplicationEvent> for ProductProjection {
///     type State = Product;
///     type StateStore = InMemoryStateStore<Product>;
///     type EventType = ProductEvent;
///     type ApplyError = ProductError;
///
///     fn apply(&self, state: Option<Self::State>, event: &Event<Self::EventType>)
///         -> Result<Option<Self::State>, Self::ApplyError>
///     {
///         // Apply event to state...
///     }
///
///     fn get_state_store(&self) -> Self::StateStore {
///         self.state_store.clone()
///     }
/// }
///
/// // Enable subscription to event bus
/// impl Projection<ApplicationEvent> for ProductProjection {}
///
/// // Usage:
/// event_bus.subscribe(ProjectionHandler::new(product_projection)).await?;
/// ```
#[async_trait]
pub trait Projection<ED>: EventApplicator<ED> + crate::SubscriberId
where
    ED: EventData + Send + Sync + 'static,
{
    /// Applies an event received from the event bus and persists the resulting state.
    ///
    /// This method is called by [`ProjectionHandler`] when events are received from
    /// the event bus. It:
    ///
    /// 1. Attempts to convert the event to the projection's subset type
    /// 2. If successful, retrieves the current state from the state store
    /// 3. Applies the event to produce new state
    /// 4. Persists (or deletes) the resulting state
    ///
    /// Events that don't match the projection's subset type are silently ignored.
    ///
    /// # Default Implementation
    ///
    /// The default implementation handles the full apply-and-store cycle. Override
    /// only if you need custom behavior (e.g., batching, custom error handling).
    async fn apply_and_store(
        &self,
        event: &Event<ED>,
    ) -> Result<
        (),
        ApplyAndStoreError<
            Self::ApplyError,
            <Self::StateStore as StateStoreBackend<Self::State>>::Error,
        >,
    > {
        if let Ok(event) = event.to_subset_event_ref::<Self::EventType>() {
            let id = self.get_id_from_event(&event);
            let mut storage = self.get_state_store();
            let state = storage
                .get_state(id)
                .await
                .map_err(ApplyAndStoreError::State)?;
            if let Some(new_state) = self
                .apply(state, &event)
                .map_err(ApplyAndStoreError::Event)?
            {
                log::debug!("Persisting state for projection: {:?}", id);
                storage
                    .persist_state(id, new_state)
                    .await
                    .map_err(ApplyAndStoreError::State)?;
            } else {
                log::debug!("Deleting state for projection: {:?}", id);
                storage
                    .delete_state(id)
                    .await
                    .map_err(ApplyAndStoreError::State)?;
            }
        }
        Ok(())
    }
}

/// Wraps a [`Projection`] to implement [`EventObserver`](crate::event_store::EventObserver)
/// for event bus subscription.
///
/// # Type Safety
///
/// This handler only accepts types implementing `Projection`, NOT `Aggregate`.
/// This prevents the anti-pattern of subscribing aggregates to their own events,
/// which would cause duplicate writes and race conditions.
///
/// # Example
///
/// ```ignore
/// use epoch_core::projection::ProjectionHandler;
///
/// let projection = MyProjection::new();
/// let handler = ProjectionHandler::new(projection);
/// event_bus.subscribe(handler).await?;
/// ```
/// Wraps a [`Projection`] to implement [`EventObserver`](crate::event_store::EventObserver)
/// for event bus subscription.
///
/// # Type Safety
///
/// While `ProjectionHandler::new()` accepts any type, the handler can only be
/// subscribed to an event bus if the inner type implements `Projection<ED>`.
/// This is enforced by the `EventObserver` implementation's trait bounds.
///
/// Attempting to subscribe a `ProjectionHandler<MyAggregate>` to an event bus
/// will fail to compile because `Aggregate` extends `EventApplicator`, not `Projection`.
pub struct ProjectionHandler<P>(pub P);

impl<P> ProjectionHandler<P> {
    /// Creates a new `ProjectionHandler` wrapping the given type.
    ///
    /// Note: The handler can only be used with an event bus if `P` implements
    /// `Projection<ED>`. Aggregates cannot be used because they don't implement
    /// `Projection`.
    pub fn new(projection: P) -> Self {
        Self(projection)
    }

    /// Returns a reference to the inner projection.
    pub fn inner(&self) -> &P {
        &self.0
    }

    /// Consumes the handler and returns the inner projection.
    pub fn into_inner(self) -> P {
        self.0
    }
}

impl<P> crate::SubscriberId for ProjectionHandler<P>
where
    P: crate::SubscriberId,
{
    fn subscriber_id(&self) -> &str {
        self.0.subscriber_id()
    }
}

#[async_trait]
impl<ED, P> crate::event_store::EventObserver<ED> for ProjectionHandler<P>
where
    ED: EventData + Send + Sync + 'static,
    P: Projection<ED> + Send + Sync,
{
    async fn on_event(
        &self,
        event: Arc<Event<ED>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.0.apply_and_store(&event).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EnumConversionError;
    use crate::event_applicator::EventApplicatorState;
    use crate::event_store::EventObserver;
    use crate::state_store::StateStoreBackend;
    use uuid::Uuid;

    #[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
    enum TestEventData {
        TestEvent { value: String },
    }

    impl EventData for TestEventData {
        fn event_type(&self) -> &'static str {
            match self {
                TestEventData::TestEvent { .. } => "TestEvent",
            }
        }
    }

    impl TryFrom<&TestEventData> for TestEventData {
        type Error = EnumConversionError;

        fn try_from(value: &TestEventData) -> Result<Self, Self::Error> {
            Ok(value.clone())
        }
    }

    #[derive(Debug, Clone, serde::Serialize)]
    struct TestState {
        id: Uuid,
    }

    impl EventApplicatorState for TestState {
        fn get_id(&self) -> &Uuid {
            &self.id
        }
    }

    #[derive(Debug, thiserror::Error)]
    enum TestStateStoreError {}

    #[derive(Debug, Clone)]
    struct TestStateStore;

    #[async_trait]
    impl StateStoreBackend<TestState> for TestStateStore {
        type Error = TestStateStoreError;

        async fn get_state(&self, _id: Uuid) -> Result<Option<TestState>, Self::Error> {
            Ok(None)
        }

        async fn persist_state(&mut self, _id: Uuid, _state: TestState) -> Result<(), Self::Error> {
            Ok(())
        }

        async fn delete_state(&mut self, _id: Uuid) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[derive(Debug, thiserror::Error)]
    enum TestProjectionError {}

    struct TestProjection;

    impl crate::SubscriberId for TestProjection {
        fn subscriber_id(&self) -> &str {
            "projection:test-projection"
        }
    }

    impl EventApplicator<TestEventData> for TestProjection {
        type State = TestState;
        type StateStore = TestStateStore;
        type EventType = TestEventData;
        type ApplyError = TestProjectionError;

        fn get_state_store(&self) -> Self::StateStore {
            TestStateStore
        }

        fn apply(
            &self,
            _state: Option<Self::State>,
            event: &Event<Self::EventType>,
        ) -> Result<Option<Self::State>, Self::ApplyError> {
            Ok(Some(TestState {
                id: event.stream_id,
            }))
        }
    }

    impl Projection<TestEventData> for TestProjection {}

    #[test]
    fn projection_subscriber_id_is_available_via_event_observer() {
        let projection = TestProjection;

        // Access subscriber_id via SubscriberId trait
        assert_eq!(
            crate::SubscriberId::subscriber_id(&projection),
            "projection:test-projection"
        );

        // Access subscriber_id via EventObserver trait using ProjectionHandler
        let handler = ProjectionHandler::new(projection);
        let observer: &dyn EventObserver<TestEventData> = &handler;
        assert_eq!(observer.subscriber_id(), "projection:test-projection");
    }
}
