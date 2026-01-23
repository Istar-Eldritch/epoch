mod event_data;
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
    event_data::event_data_enum_impl(item.into()).into()
}
