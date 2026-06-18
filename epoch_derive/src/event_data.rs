use proc_macro2::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Expr, ExprLit, Lit, Meta};

pub fn event_data_enum_impl(item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let item_tokens: TokenStream = item.into();
    let input = match syn::parse2::<DeriveInput>(item_tokens) {
        Ok(tree) => tree,
        Err(e) => return e.to_compile_error().into(),
    };

    let enum_name = &input.ident;
    let variants = match &input.data {
        Data::Enum(data) => &data.variants,
        _ => panic!("EventData can only be derived for enums"),
    };

    // Parse optional `#[event_data(schema_version = N)]` attribute on the enum.
    let schema_version_override = parse_schema_version(&input);

    let event_type_matches = variants.iter().map(|variant| {
        let variant_name = &variant.ident;
        let variant_name_str = variant_name.to_string();
        match &variant.fields {
            syn::Fields::Unit => {
                quote! {
                    #enum_name::#variant_name => #variant_name_str,
                }
            }
            syn::Fields::Unnamed(_) => {
                quote! {
                    #enum_name::#variant_name(..) => #variant_name_str,
                }
            }
            syn::Fields::Named(_) => {
                quote! {
                    #enum_name::#variant_name { .. } => #variant_name_str,
                }
            }
        }
    });

    // Only emit the schema_version override when the attribute was present.
    let schema_version_method = schema_version_override.map(|v| {
        quote! {
            fn schema_version(&self) -> ::epoch_core::event::SchemaVersion {
                #v
            }
        }
    });

    let expanded = quote! {
        impl EventData for #enum_name {
            fn event_type(&self) -> &'static str {
                match self {
                    #(#event_type_matches)*
                }
            }
            #schema_version_method
        }
    };

    expanded.into()
}

/// Parses the `#[event_data(schema_version = N)]` attribute from an enum's
/// attributes and returns the version literal as a `u32` if present.
///
/// Returns `None` if the attribute is absent or does not have the expected form.
fn parse_schema_version(input: &DeriveInput) -> Option<u32> {
    for attr in &input.attrs {
        if !attr.path().is_ident("event_data") {
            continue;
        }
        // The attribute must be in "list" form: `#[event_data(key = value)]`.
        let meta_list = match &attr.meta {
            Meta::List(list) => list,
            _ => continue,
        };
        // Parse the token stream inside the parens as `schema_version = <integer>`.
        let name_value: syn::MetaNameValue = match syn::parse2(meta_list.tokens.clone()) {
            Ok(nv) => nv,
            Err(_) => continue,
        };
        if !name_value.path.is_ident("schema_version") {
            continue;
        }
        if let Expr::Lit(ExprLit {
            lit: Lit::Int(int_lit),
            ..
        }) = &name_value.value
            && let Ok(v) = int_lit.base10_parse::<u32>()
        {
            return Some(v);
        }
    }
    None
}
