mod subset;

#[proc_macro_attribute]
pub fn subset_enum(
    attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    subset::subset_enum_impl(attr, item)
}
