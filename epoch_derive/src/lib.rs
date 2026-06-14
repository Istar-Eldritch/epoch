mod event_data;
mod projection;
mod subset;
mod subset_of;

#[proc_macro_attribute]
pub fn subset_enum(
    attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    subset::subset_enum_impl(attr, item)
}

/// Derive macro for generating consumer-side subset-enum conversion impls.
///
/// Place `#[derive(SubsetOf)]` on a *consumer-defined* enum together with the
/// `#[subset_of(PathToSupersetEnum)]` helper attribute. The macro generates the
/// three conversion impls required by the `Saga::EventType` bound:
///
/// * `From<Sub> for Super` — total, moves ownership.
/// * `TryFrom<Super> for Sub` — owned narrowing.
/// * `TryFrom<&Super> for Sub` — reference narrowing with per-field `.clone()`;
///   this is the impl that satisfies `for<'a> TryFrom<&'a Super, Error = EnumConversionError>`.
///
/// Unlike the producer-side `#[subset_enum]` attribute macro, `SubsetOf` works when
/// the superset enum lives in a crate you do not own — the consumer writes their own
/// subset enum body and the macro wires up the conversions. Structural compatibility
/// (variant name, field names, field types) is enforced by the Rust compiler when it
/// type-checks the generated match bodies against the real superset definition.
///
/// # Basic Usage
///
/// ```ignore
/// use epoch_derive::{EventData, SubsetOf};
///
/// // The superset enum lives somewhere else (here inline for illustration).
/// #[derive(Debug, Clone, EventData)]
/// pub enum AppEvent {
///     UserCreated { id: String, name: String },
///     UserDeleted,
///     NewSession { user_id: String },
/// }
///
/// // Consumer declares only the variants they care about.
/// #[derive(Debug, Clone, EventData, SubsetOf)]
/// #[subset_of(AppEvent)]
/// pub enum UserSubset {
///     UserCreated { id: String, name: String },
///     UserDeleted,
/// }
/// ```
///
/// # Cross-Crate Usage
///
/// The superset path may be a fully-qualified path to a type in another crate:
///
/// ```ignore
/// #[derive(Debug, Clone, EventData, SubsetOf)]
/// #[subset_of(some_upstream_crate::events::AppEvent)]
/// pub enum MachineSagaEvent {
///     MachineProvisioned { machine_id: uuid::Uuid, pool_id: uuid::Uuid },
///     MachineReleased { machine_id: uuid::Uuid },
/// }
/// ```
///
/// # Generic Subset Enums
///
/// Generic type parameters on the *subset* enum are threaded through the generated
/// `impl` blocks automatically:
///
/// ```ignore
/// #[derive(Debug, Clone, EventData, SubsetOf)]
/// #[subset_of(AppEvent)]
/// pub enum TypedSubset<T: Clone> {
///     UserCreated { id: T },
/// }
/// ```
///
/// # Errors
///
/// Applying `#[derive(SubsetOf)]` to a non-enum item, omitting the `#[subset_of(...)]`
/// attribute, or providing a malformed argument all produce clear compile-time errors.
#[proc_macro_derive(SubsetOf, attributes(subset_of))]
pub fn subset_of(item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    subset_of::subset_of_impl(item)
}

#[proc_macro_derive(EventData)]
pub fn event_data(item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    event_data::event_data_enum_impl(item)
}

/// Derive macro for generating projection subscriber IDs.
///
/// This macro generates a `subscriber_id()` method that returns a stable identifier
/// for the projection. The ID follows the format `"projection:<kebab-case-name>"`.
///
/// # Basic Usage
///
/// ```ignore
/// use epoch_derive::SubscriberId;
///
/// #[derive(SubscriberId)]
/// struct UserProfileProjection {
///     // fields...
/// }
///
/// let projection = UserProfileProjection { /* ... */ };
/// assert_eq!(projection.subscriber_id(), "projection:user-profile");
/// ```
///
/// Common suffixes (`Projection`, `Saga`, `Handler`, `Observer`) are automatically
/// stripped before converting to kebab-case.
///
/// # Custom ID
///
/// Override the generated name with a custom value:
///
/// ```ignore
/// #[derive(SubscriberId)]
/// #[subscriber_id("my-custom-projection")]
/// struct MyProjection;
///
/// // Returns "projection:my-custom-projection"
/// ```
///
/// # Custom Prefix
///
/// Use a different prefix (e.g., for sagas):
///
/// ```ignore
/// #[derive(SubscriberId)]
/// #[subscriber_id(prefix = "saga")]
/// struct OrderProcessSaga;
///
/// // Returns "saga:order-process"
/// ```
///
/// # Full Customization
///
/// Specify both name and prefix:
///
/// ```ignore
/// #[derive(SubscriberId)]
/// #[subscriber_id(name = "custom", prefix = "handler")]
/// struct MyHandler;
///
/// // Returns "handler:custom"
/// ```
#[proc_macro_derive(SubscriberId, attributes(subscriber_id))]
pub fn subscriber_id(item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    projection::subscriber_id_impl(item)
}
