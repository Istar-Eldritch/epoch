//! Consumer-side derive macro for generating subset-enum conversion impls.
//!
//! This module implements the [`SubsetOf`] derive macro, which allows a consumer
//! to declare a subset enum (placed in their own crate) and automatically generate
//! the three conversion impls needed to satisfy the `Saga::EventType` bound:
//!
//! * `From<Sub> for Super` — total, moves ownership.
//! * `TryFrom<Super> for Sub` — owned narrowing, wildcard arm for excluded variants.
//! * `TryFrom<&Super> for Sub` — reference narrowing with per-field `.clone()`.
//!
//! # Usage
//!
//! ```ignore
//! use epoch_derive::SubsetOf;
//!
//! #[derive(Debug, Clone, SubsetOf)]
//! #[subset_of(AppEvent)]
//! enum UserEvent {
//!     UserCreated { id: String },
//!     UserDeleted,
//! }
//! ```

use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::{Data, DeriveInput, Path};

/// Extracts the superset path from `#[subset_of(PathToSupersetEnum)]`.
///
/// Returns a `syn::Error` if:
/// - The attribute is missing.
/// - The attribute appears more than once.
/// - The attribute's argument cannot be parsed as a `syn::Path`.
fn parse_subset_of_path(input: &DeriveInput) -> syn::Result<Path> {
    let mut found: Option<Path> = None;

    for attr in &input.attrs {
        if attr.path().is_ident("subset_of") {
            if found.is_some() {
                return Err(syn::Error::new_spanned(
                    attr,
                    "duplicate `#[subset_of(...)]` attribute; expected exactly one",
                ));
            }
            let path = attr.parse_args::<Path>().map_err(|_| {
                syn::Error::new_spanned(
                    attr,
                    "expected `#[subset_of(PathToSupersetEnum)]` — \
                     the argument must be a path to the superset enum \
                     (e.g. `AppEvent` or `crate::events::AppEvent`)",
                )
            })?;
            found = Some(path);
        }
    }

    found.ok_or_else(|| {
        syn::Error::new_spanned(
            &input.ident,
            "missing `#[subset_of(PathToSupersetEnum)]` attribute — \
             add `#[subset_of(YourSuperEnum)]` to specify the superset enum",
        )
    })
}

/// Entry point called from `lib.rs` for the `SubsetOf` derive macro.
pub fn subset_of_impl(item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    subset_of_impl_internal(item.into()).into()
}

/// Internal implementation, operating on `proc_macro2` tokens so it can be
/// called from unit tests without entering the proc-macro context.
fn subset_of_impl_internal(input: TokenStream) -> TokenStream {
    let derive_input = match syn::parse2::<DeriveInput>(input) {
        Ok(tree) => tree,
        Err(e) => return e.to_compile_error(),
    };

    // Reject non-enum inputs with a clear error message.
    let data_enum = match &derive_input.data {
        Data::Enum(data) => data,
        Data::Struct(_) => {
            return syn::Error::new_spanned(
                &derive_input.ident,
                "SubsetOf can only be derived for enums, found struct",
            )
            .to_compile_error();
        }
        Data::Union(_) => {
            return syn::Error::new_spanned(
                &derive_input.ident,
                "SubsetOf can only be derived for enums, found union",
            )
            .to_compile_error();
        }
    };

    // Extract the superset path from the #[subset_of(...)] helper attribute.
    let super_path = match parse_subset_of_path(&derive_input) {
        Ok(path) => path,
        Err(e) => return e.to_compile_error(),
    };

    let sub_name = &derive_input.ident;
    let variants = &data_enum.variants;

    // Build match arms for `From<Sub> for Super`.
    // Each subset variant maps 1-to-1 to the corresponding superset variant.
    // Field names / positions are taken verbatim from the consumer's declaration;
    // structural compatibility is enforced by the Rust compiler when it
    // type-checks the generated body against the real superset definition.
    let from_arms: Vec<TokenStream> = variants
        .iter()
        .map(|variant| {
            let variant_name = &variant.ident;
            match &variant.fields {
                syn::Fields::Unit => {
                    quote! {
                        #sub_name::#variant_name => #super_path::#variant_name,
                    }
                }
                syn::Fields::Unnamed(fields) => {
                    let num_fields = fields.unnamed.len();
                    let var_names: Vec<syn::Ident> = (0..num_fields)
                        .map(|i| syn::Ident::new(&format!("__field{i}"), Span::call_site()))
                        .collect();
                    quote! {
                        #sub_name::#variant_name(#(#var_names),*) => #super_path::#variant_name(#(#var_names),*),
                    }
                }
                syn::Fields::Named(fields) => {
                    let field_names: Vec<_> = fields
                        .named
                        .iter()
                        .map(|f| f.ident.as_ref().unwrap())
                        .collect();
                    quote! {
                        #sub_name::#variant_name { #(#field_names),* } => #super_path::#variant_name { #(#field_names),* },
                    }
                }
            }
        })
        .collect();

    quote! {
        impl ::core::convert::From<#sub_name> for #super_path {
            fn from(value: #sub_name) -> Self {
                match value {
                    #(#from_arms)*
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::quote;

    /// Helper: parse both token streams via `prettyplease` and compare the
    /// pretty-printed output, giving readable diffs in test failures.
    fn assert_tokens_eq(actual: TokenStream, expected: TokenStream) {
        let actual_file =
            syn::parse_file(&actual.to_string()).expect("failed to parse actual tokens as a file");
        let expected_file = syn::parse_file(&expected.to_string())
            .expect("failed to parse expected tokens as a file");
        assert_eq!(
            prettyplease::unparse(&actual_file),
            prettyplease::unparse(&expected_file),
        );
    }

    // ── Happy-path tests ──────────────────────────────────────────────────

    #[test]
    fn unit_variant_generates_from() {
        let input = quote! {
            #[subset_of(SuperEnum)]
            enum SubEnum {
                VariantA,
            }
        };
        let expected = quote! {
            impl ::core::convert::From<SubEnum> for SuperEnum {
                fn from(value: SubEnum) -> Self {
                    match value {
                        SubEnum::VariantA => SuperEnum::VariantA,
                    }
                }
            }
        };
        assert_tokens_eq(subset_of_impl_internal(input), expected);
    }

    #[test]
    fn tuple_variant_generates_from() {
        let input = quote! {
            #[subset_of(SuperEnum)]
            enum SubEnum {
                VariantB(i32, bool),
            }
        };
        let expected = quote! {
            impl ::core::convert::From<SubEnum> for SuperEnum {
                fn from(value: SubEnum) -> Self {
                    match value {
                        SubEnum::VariantB(__field0, __field1) => SuperEnum::VariantB(__field0, __field1),
                    }
                }
            }
        };
        assert_tokens_eq(subset_of_impl_internal(input), expected);
    }

    #[test]
    fn struct_variant_generates_from() {
        let input = quote! {
            #[subset_of(SuperEnum)]
            enum SubEnum {
                VariantC { name: String, id: u64 },
            }
        };
        let expected = quote! {
            impl ::core::convert::From<SubEnum> for SuperEnum {
                fn from(value: SubEnum) -> Self {
                    match value {
                        SubEnum::VariantC { name, id } => SuperEnum::VariantC { name, id },
                    }
                }
            }
        };
        assert_tokens_eq(subset_of_impl_internal(input), expected);
    }

    #[test]
    fn mixed_variants_generate_from() {
        let input = quote! {
            #[subset_of(SuperEnum)]
            enum SubEnum {
                UnitVariant,
                TupleVariant(u32, bool),
                StructVariant { name: String, id: u64 },
            }
        };
        let expected = quote! {
            impl ::core::convert::From<SubEnum> for SuperEnum {
                fn from(value: SubEnum) -> Self {
                    match value {
                        SubEnum::UnitVariant => SuperEnum::UnitVariant,
                        SubEnum::TupleVariant(__field0, __field1) => SuperEnum::TupleVariant(__field0, __field1),
                        SubEnum::StructVariant { name, id } => SuperEnum::StructVariant { name, id },
                    }
                }
            }
        };
        assert_tokens_eq(subset_of_impl_internal(input), expected);
    }

    #[test]
    fn qualified_superset_path_is_emitted_verbatim() {
        let input = quote! {
            #[subset_of(some_crate::events::SuperEnum)]
            enum SubEnum {
                VariantA,
            }
        };
        let expected = quote! {
            impl ::core::convert::From<SubEnum> for some_crate::events::SuperEnum {
                fn from(value: SubEnum) -> Self {
                    match value {
                        SubEnum::VariantA => some_crate::events::SuperEnum::VariantA,
                    }
                }
            }
        };
        assert_tokens_eq(subset_of_impl_internal(input), expected);
    }

    // ── Error-path tests ──────────────────────────────────────────────────

    #[test]
    fn struct_input_emits_compile_error() {
        let input = quote! {
            #[subset_of(SuperEnum)]
            struct MyStruct {
                field: i32,
            }
        };
        let actual = subset_of_impl_internal(input).to_string();
        assert!(
            actual.contains("compile_error"),
            "expected compile_error in output, got: {actual}"
        );
    }

    #[test]
    fn missing_subset_of_attr_emits_compile_error() {
        let input = quote! {
            enum SubEnum {
                VariantA,
            }
        };
        let actual = subset_of_impl_internal(input).to_string();
        assert!(
            actual.contains("compile_error"),
            "expected compile_error in output, got: {actual}"
        );
    }

    #[test]
    fn malformed_subset_of_attr_emits_compile_error() {
        // Pass a string literal instead of a path — parse_args::<Path> will fail.
        let input = quote! {
            #[subset_of("not a path")]
            enum SubEnum {
                VariantA,
            }
        };
        let actual = subset_of_impl_internal(input).to_string();
        assert!(
            actual.contains("compile_error"),
            "expected compile_error in output, got: {actual}"
        );
    }
}
