//! Subscriber identification for event bus consumers.
//!
//! This module provides the [`SubscriberId`] trait which identifies subscribers
//! in the event bus system. The ID is used for checkpoint tracking, multi-instance
//! coordination, and dead letter queue association.

/// Provides a unique identifier for event bus subscribers.
///
/// This trait is required by [`EventObserver`](crate::event_store::EventObserver),
/// [`Projection`](crate::projection::Projection), and [`Saga`](crate::saga::Saga).
/// Instead of implementing it manually, use the `#[derive(SubscriberId)]` macro
/// from `epoch_derive`.
///
/// # Naming Convention
///
/// - Projections: `"projection:<name>"` (e.g., `"projection:user-profile"`)
/// - Sagas/Process Managers: `"saga:<name>"` (e.g., `"saga:order-fulfillment"`)
/// - Other observers: `"observer:<name>"` (e.g., `"observer:audit-logger"`)
///
/// # Using the Derive Macro
///
/// ```ignore
/// use epoch_derive::SubscriberId;
///
/// #[derive(SubscriberId)]
/// struct UserProfileProjection { /* ... */ }
///
/// // Automatically implements SubscriberId with:
/// // subscriber_id() -> "projection:user-profile"
/// ```
///
/// The macro automatically:
/// - Converts struct names to kebab-case
/// - Strips common suffixes (`Projection`, `Saga`, `Handler`, `Observer`)
/// - Adds the appropriate prefix
///
/// ## Customization
///
/// ```ignore
/// // Custom name
/// #[derive(SubscriberId)]
/// #[subscriber_id("custom-name")]  // -> "projection:custom-name"
/// struct MyProjection;
///
/// // Custom prefix
/// #[derive(SubscriberId)]
/// #[subscriber_id(prefix = "saga")]  // -> "saga:order-process"
/// struct OrderProcessSaga;
///
/// // Both
/// #[derive(SubscriberId)]
/// #[subscriber_id(name = "my-saga", prefix = "saga")]
/// struct MySaga;
/// ```
///
/// # Manual Implementation
///
/// ```ignore
/// use epoch_core::SubscriberId;
///
/// struct MyObserver;
///
/// impl SubscriberId for MyObserver {
///     fn subscriber_id(&self) -> &str {
///         "observer:my-observer"
///     }
/// }
/// ```
// NOTE: A `const fn subscriber_id_static() -> &'static str` associated function was considered
// for compile-time known IDs, but this requires associated const functions on traits which
// are not yet stable in Rust (tracking issue #92827). The current implementation already
// returns `&'static str` literals from the derive macro, so there's no allocation overhead.
// Revisit if/when the feature stabilizes and const-context usage becomes needed.
pub trait SubscriberId {
    /// Returns a unique identifier for this subscriber.
    ///
    /// This ID is used for:
    /// - **Checkpoint tracking**: The event bus tracks the last processed event per subscriber
    /// - **Multi-instance coordination**: Multiple instances with the same subscriber_id form a
    ///   consumer pool where only one instance processes each event
    /// - **Dead letter queue**: Failed events are associated with their subscriber_id
    ///
    /// The ID must be stable across deployments and restarts.
    fn subscriber_id(&self) -> &str;
}
