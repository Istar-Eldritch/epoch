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
    //
    // - `None`     → attribute absent → default `schema_version() == 1` from the trait
    // - `Some(Ok(v))` → attribute present and valid → emit override
    // - `Some(Err(e))` → attribute present but malformed → compile error
    let schema_version_override = parse_schema_version(&input);

    let schema_version_method = match schema_version_override {
        Some(Ok(v)) => Some(quote! {
            fn schema_version(&self) -> ::epoch_core::event::SchemaVersion {
                #v
            }
        }),
        Some(Err(e)) => return e.into_compile_error().into(),
        None => None, // absent → rely on the defaulted `fn schema_version() -> 1`
    };

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

/// Parses the optional `#[event_data(schema_version = N)]` attribute.
///
/// Returns:
/// - `None`           – attribute is absent; the trait default (`1`) applies.
/// - `Some(Ok(v))`    – attribute present and valid; `v` overrides `schema_version()`.
/// - `Some(Err(e))`   – attribute present but malformed; `e` becomes a compile error.
///
/// This design means a typo'd key (`scheema_version`) or a wrong value type
/// (`= "two"`) is always a hard compile error, while omitting the attribute
/// entirely is perfectly legal.
fn parse_schema_version(input: &DeriveInput) -> Option<Result<u32, syn::Error>> {
    for attr in &input.attrs {
        if !attr.path().is_ident("event_data") {
            continue;
        }
        // Attribute is present — from this point every invalid form is a hard error.
        let meta_list = match &attr.meta {
            Meta::List(list) => list,
            _ => {
                return Some(Err(syn::Error::new_spanned(
                    attr,
                    "expected `#[event_data(schema_version = N)]` (list form)",
                )));
            }
        };
        let name_value: syn::MetaNameValue = match syn::parse2(meta_list.tokens.clone()) {
            Ok(nv) => nv,
            Err(_) => {
                return Some(Err(syn::Error::new_spanned(
                    attr,
                    "couldn't parse `event_data` options; expected `schema_version = N`",
                )));
            }
        };
        if !name_value.path.is_ident("schema_version") {
            return Some(Err(syn::Error::new_spanned(
                &name_value.path,
                "unknown key in `#[event_data(...)]`; expected `schema_version`",
            )));
        }
        match &name_value.value {
            Expr::Lit(ExprLit {
                lit: Lit::Int(int_lit),
                ..
            }) => {
                let parsed: Result<u32, _> = int_lit.base10_parse();
                return Some(parsed.map_err(|_| {
                    syn::Error::new_spanned(
                        int_lit,
                        "schema_version must be a valid u32 integer literal",
                    )
                }));
            }
            other => {
                return Some(Err(syn::Error::new_spanned(
                    other,
                    "schema_version must be an integer literal (e.g. `schema_version = 2`)",
                )));
            }
        }
    }
    // No `event_data` attribute found — absent is valid, default applies.
    None
}
