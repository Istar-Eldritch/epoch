use quote::quote;
use syn::{Data, DeriveInput};

pub fn event_data_enum_impl(item: proc_macro2::TokenStream) -> proc_macro2::TokenStream {
    let input = match syn::parse2::<DeriveInput>(item) {
        Ok(tree) => tree,
        Err(e) => return e.to_compile_error(),
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

    expanded
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::quote;

    fn test_event_data_enum(item: proc_macro2::TokenStream, expected: proc_macro2::TokenStream) {
        let actual = event_data_enum_impl(item);
        let actual_str = actual.to_string();
        let expected_str = expected.to_string();

        let actual_file = syn::parse_file(&actual_str).expect("Failed to parse actual tokens");
        let expected_file =
            syn::parse_file(&expected_str).expect("Failed to parse expected tokens");

        assert_eq!(
            prettyplease::unparse(&actual_file),
            prettyplease::unparse(&expected_file)
        );
    }

    #[test]
    fn test_variants() {
        let item = quote! {
            enum OriginalEnum {
                VariantA,
                VariantB(i32),
                VariantC { field: String },
            }
        };
        let expected = quote! {
            impl EventData for OriginalEnum {
                fn event_type(&self) -> &'static str {
                    match self {
                        OriginalEnum::VariantA => "VariantA",
                        OriginalEnum::VariantB(..) => "VariantB",
                        OriginalEnum::VariantC{..} => "VariantC",
                    }
                }
            }
        };
        test_event_data_enum(item, expected);
    }
}
