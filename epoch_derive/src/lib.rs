mod event_data;
mod projection;
mod subset;

#[proc_macro_attribute]
pub fn subset_enum(
    attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    subset::subset_enum_impl(attr, item)
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
