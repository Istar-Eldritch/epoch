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
    let schema_version_override = match parse_schema_version(&input) {
        Ok(v) => v,
        Err(e) => return e.into_compile_error().into(),
    };

    let schema_version_method = Some(quote! {
        fn schema_version(&self) -> ::epoch_core::event::SchemaVersion {
            #schema_version_override
        }
    });

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
/// attributes and returns the version literal as a `u32`.
/// Returns Err for malformed attributes, causing a compile-time error.
fn parse_schema_version(input: &DeriveInput) -> Result<u32, syn::Error> {
    for attr in &input.attrs {
        if !attr.path().is_ident("event_data") {
            continue;
        }
        let meta_list = match &attr.meta {
            Meta::List(list) => list,
            _ => {
                return Err(syn::Error::new_spanned(
                    attr,
                    "expected #[event_data(schema_version = N)] (list form)",
                ));
            }
        };
        let name_value: syn::MetaNameValue = match syn::parse2(meta_list.tokens.clone()) {
            Ok(nv) => nv,
            Err(_) => {
                return Err(syn::Error::new_spanned(attr, "couldn't parse event_data options"));
            }
        };
        if !name_value.path.is_ident("schema_version") {
            return Err(syn::Error::new_spanned(
                &name_value.path,
                "unknown key; expected schema_version",
            ));
        }
        match &name_value.value {
            Expr::Lit(ExprLit {
                lit: Lit::Int(int_lit),
                ..
            }) => {
                let parsed: Result<u32, _> = int_lit.base10_parse();
                match parsed {
                    Ok(v) => return Ok(v),
                    Err(_) => {
                        return Err(syn::Error::new_spanned(
                            int_lit,
                            "schema_version must be an integer literal",
                        ));
                    }
                }
            }
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    "schema_version must be an integer literal",
                ));
            }
        }
    }
    Err(syn::Error::new_spanned(
        input,
        "no #[event_data(schema_version = N)] attribute found",
    ))
}
