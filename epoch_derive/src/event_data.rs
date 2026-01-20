use quote::quote;
use syn::{Data, DeriveInput};

pub fn event_data_enum_impl(item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let item_tokens: proc_macro2::TokenStream = item.into();
    let input = match syn::parse2::<DeriveInput>(item_tokens) {
        Ok(tree) => tree,
        Err(e) => return e.to_compile_error().into(),
    };

    let enum_name = &input.ident;
    let variants = match &input.data {
        Data::Enum(data) => &data.variants,
        _ => panic!("EventData can only be derived for enums"),
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
        }
    };

    expanded.into()
}
