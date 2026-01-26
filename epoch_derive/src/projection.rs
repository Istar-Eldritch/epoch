//! Derive macro for generating projection subscriber IDs.
//!
//! This module provides the `SubscriberId` derive macro that automatically
//! generates a `subscriber_id()` method for projection structs.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, LitStr};

/// Converts a PascalCase or camelCase identifier to kebab-case.
///
/// Examples:
/// - `UserProfile` -> `user-profile`
/// - `OrderSummary` -> `order-summary`
/// - `HTTPRequest` -> `http-request`
/// - `XMLParser` -> `xml-parser`
fn to_kebab_case(s: &str) -> String {
    let mut result = String::with_capacity(s.len() + 4);
    let mut prev_was_lowercase = false;
    let mut prev_was_uppercase = false;

    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() {
            // Insert hyphen before uppercase if:
            // - Not at the start
            // - Previous char was lowercase (camelCase boundary), OR
            // - Previous char was uppercase and next char is lowercase (end of acronym)
            if i > 0 {
                let next_is_lowercase =
                    s.chars().nth(i + 1).is_some_and(|next| next.is_lowercase());
                if prev_was_lowercase || (prev_was_uppercase && next_is_lowercase) {
                    result.push('-');
                }
            }
            result.push(c.to_ascii_lowercase());
            prev_was_uppercase = true;
            prev_was_lowercase = false;
        } else {
            result.push(c);
            prev_was_lowercase = c.is_lowercase();
            prev_was_uppercase = false;
        }
    }

    result
}

/// Implementation of the `SubscriberId` derive macro.
///
/// This generates a `subscriber_id()` method that returns a string in the format
/// `"projection:<kebab-case-struct-name>"`.
///
/// # Attributes
///
/// - `#[subscriber_id("custom-id")]` - Override the generated ID with a custom value.
///   The `projection:` prefix is still added automatically.
/// - `#[subscriber_id(prefix = "saga")]` - Use a different prefix instead of `projection`.
/// - `#[subscriber_id(name = "my-name", prefix = "saga")]` - Custom name and prefix.
///
/// # Examples
///
/// ```ignore
/// // Generates subscriber_id() returning "projection:user-profile"
/// #[derive(SubscriberId)]
/// struct UserProfileProjection;
///
/// // Generates subscriber_id() returning "projection:custom-name"
/// #[derive(SubscriberId)]
/// #[subscriber_id("custom-name")]
/// struct MyProjection;
///
/// // Generates subscriber_id() returning "saga:order-process"
/// #[derive(SubscriberId)]
/// #[subscriber_id(prefix = "saga")]
/// struct OrderProcessSaga;
/// ```
pub fn subscriber_id_impl(item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let item_tokens: TokenStream = item.into();
    let input = match syn::parse2::<DeriveInput>(item_tokens) {
        Ok(tree) => tree,
        Err(e) => return e.to_compile_error().into(),
    };

    // Ensure it's a struct
    match &input.data {
        Data::Struct(_) => {}
        Data::Enum(_) => {
            return syn::Error::new_spanned(&input, "SubscriberId can only be derived for structs")
                .to_compile_error()
                .into();
        }
        Data::Union(_) => {
            return syn::Error::new_spanned(&input, "SubscriberId can only be derived for structs")
                .to_compile_error()
                .into();
        }
    }

    let struct_name = &input.ident;
    let struct_name_str = struct_name.to_string();
    let (impl_generics, type_generics, where_clause) = input.generics.split_for_impl();

    // Parse attributes to find custom subscriber_id configuration
    let mut custom_name: Option<String> = None;
    let mut prefix = "projection".to_string();

    for attr in &input.attrs {
        if attr.path().is_ident("subscriber_id") {
            // Try to parse as `#[subscriber_id("value")]` (positional string literal)
            if let Ok(lit) = attr.parse_args::<LitStr>() {
                custom_name = Some(lit.value());
                continue;
            }

            // Try to parse as `#[subscriber_id(name = "...", prefix = "...")]`
            let result = attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("name") {
                    let value: LitStr = meta.value()?.parse()?;
                    custom_name = Some(value.value());
                    Ok(())
                } else if meta.path.is_ident("prefix") {
                    let value: LitStr = meta.value()?.parse()?;
                    prefix = value.value();
                    Ok(())
                } else {
                    Err(meta.error("expected `name` or `prefix`"))
                }
            });

            if let Err(e) = result {
                return e.to_compile_error().into();
            }
        }
    }

    // Generate the subscriber ID by stripping common suffixes and converting to kebab-case.
    //
    // Suffix stripping precedence (first match wins):
    // 1. "Projection"      - e.g., UserProjection -> user
    // 2. "ProcessManager"  - e.g., OrderProcessManager -> order
    // 3. "Saga"            - e.g., PaymentSaga -> payment
    // 4. "Handler"         - e.g., EventHandler -> event
    // 5. "Observer"        - e.g., MetricsObserver -> metrics
    //
    // Note: "ProjectionHandler" would match "Handler" (shorter suffix) since "Projection"
    // is checked first and won't match "ProjectionHandler". For explicit control,
    // use the `name = "..."` attribute to specify a custom subscriber ID.
    let name = custom_name.unwrap_or_else(|| {
        // Strip common suffixes before converting to kebab-case
        let stripped = struct_name_str
            .strip_suffix("Projection")
            .or_else(|| struct_name_str.strip_suffix("ProcessManager"))
            .or_else(|| struct_name_str.strip_suffix("Saga"))
            .or_else(|| struct_name_str.strip_suffix("Handler"))
            .or_else(|| struct_name_str.strip_suffix("Observer"))
            .unwrap_or(&struct_name_str);
        to_kebab_case(stripped)
    });

    let subscriber_id = format!("{}:{}", prefix, name);

    let expanded = quote! {
        impl #impl_generics ::epoch_core::SubscriberId for #struct_name #type_generics #where_clause {
            fn subscriber_id(&self) -> &str {
                #subscriber_id
            }
        }
    };

    expanded.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_kebab_case_simple() {
        assert_eq!(to_kebab_case("UserProfile"), "user-profile");
        assert_eq!(to_kebab_case("OrderSummary"), "order-summary");
        assert_eq!(to_kebab_case("Test"), "test");
    }

    #[test]
    fn to_kebab_case_acronyms() {
        assert_eq!(to_kebab_case("HTTPRequest"), "http-request");
        assert_eq!(to_kebab_case("XMLParser"), "xml-parser");
        assert_eq!(to_kebab_case("IOError"), "io-error");
    }

    #[test]
    fn to_kebab_case_single_word() {
        assert_eq!(to_kebab_case("User"), "user");
        assert_eq!(to_kebab_case("A"), "a");
    }

    #[test]
    fn to_kebab_case_already_lowercase() {
        assert_eq!(to_kebab_case("user"), "user");
        assert_eq!(to_kebab_case("userprofile"), "userprofile");
    }

    #[test]
    fn to_kebab_case_consecutive_uppercase() {
        assert_eq!(to_kebab_case("ABCDef"), "abc-def");
        assert_eq!(to_kebab_case("ABC"), "abc");
    }

    #[test]
    fn to_kebab_case_mixed() {
        assert_eq!(to_kebab_case("getHTTPResponse"), "get-http-response");
        assert_eq!(to_kebab_case("parseXMLData"), "parse-xml-data");
    }
}
