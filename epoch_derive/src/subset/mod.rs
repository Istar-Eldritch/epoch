use proc_macro2;
use quote::quote;
use syn::{Ident, ItemEnum, Token, punctuated::Punctuated};

struct SubsetEnumAttr {
    subset_enum_name: Ident,
    included_variants: Punctuated<Ident, Token![,]>,
}

impl syn::parse::Parse for SubsetEnumAttr {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let subset_enum_name: Ident = input.parse()?;
        let included_variants = if input.is_empty() {
            Punctuated::new()
        } else {
            input.parse::<Token![,]>()?;
            Punctuated::parse_terminated(input)?
        };

        Ok(SubsetEnumAttr {
            subset_enum_name,
            included_variants,
        })
    }
}

pub fn subset_enum_impl(
    attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    subset_enum_impl_internal(attr.into(), item.into()).into()
}

fn subset_enum_impl_internal(
    attr: proc_macro2::TokenStream,
    item: proc_macro2::TokenStream,
) -> proc_macro2::TokenStream {
    let input_enum = match syn::parse2::<ItemEnum>(item) {
        Ok(tree) => tree,
        Err(e) => return e.to_compile_error(),
    };
    let parsed_attr = match syn::parse2::<SubsetEnumAttr>(attr) {
        Ok(tree) => tree,
        Err(e) => return e.to_compile_error(),
    };

    let subset_enum_name = parsed_attr.subset_enum_name;
    let included_variants: Vec<Ident> = parsed_attr.included_variants.into_iter().collect();

    let original_enum_name = &input_enum.ident;
    let original_variants = &input_enum.variants;
    let original_enum_attrs = &input_enum.attrs;

    let forwarded_attrs = original_enum_attrs
        .iter()
        .filter(|attr| !attr.path().is_ident("subset_enum"));

    let mut new_variants = quote! {};
    let mut from_impls = quote! {};
    let mut try_from_impls = quote! {};

    let filtered_variants = original_variants
        .iter()
        .filter(|v| included_variants.is_empty() || included_variants.contains(&v.ident))
        .collect::<Vec<_>>();

    for variant in &filtered_variants {
        let variant_name = &variant.ident;
        let fields = &variant.fields;
        new_variants.extend(quote! {
            #variant_name #fields,
        });
    }

    let from_matches = filtered_variants.iter().map(|variant| {
        let variant_name = &variant.ident;
        let fields = &variant.fields;
        match fields {
            syn::Fields::Unit => {
                quote! {
                    #subset_enum_name::#variant_name => #original_enum_name::#variant_name,
                }
            }
            syn::Fields::Unnamed(fields) => {
                let num_fields = fields.unnamed.len();
                let var_names: Vec<Ident> = (0..num_fields).map(|i| Ident::new(&format!("__field{}", i), proc_macro2::Span::call_site())).collect();
                quote! {
                    #subset_enum_name::#variant_name(#(#var_names),*) => #original_enum_name::#variant_name(#(#var_names),*),
                }
            }
            syn::Fields::Named(fields) => {
                let field_names: Vec<_> = fields.named.iter().map(|f| f.ident.as_ref().unwrap()).collect();
                quote! {
                    #subset_enum_name::#variant_name { #(#field_names),* } => #original_enum_name::#variant_name { #(#field_names),* },
                }
            }
        }
    });

    from_impls.extend(quote! {
        impl From<#subset_enum_name> for #original_enum_name {
            fn from(value: #subset_enum_name) -> Self {
                match value {
                    #(#from_matches)*
                }
            }
        }
    });

    let try_from_matches = original_variants.iter().map(|variant| {
        let variant_name = &variant.ident;
        let fields = &variant.fields;
        if included_variants.is_empty() || included_variants.contains(&variant_name) {
            match fields {
                syn::Fields::Unit => {
                    quote! {
                        #original_enum_name::#variant_name => Ok(#subset_enum_name::#variant_name),
                    }
                }
                syn::Fields::Unnamed(fields) => {
                    let num_fields = fields.unnamed.len();
                    let var_names: Vec<Ident> = (0..num_fields).map(|i| Ident::new(&format!("__field{}", i), proc_macro2::Span::call_site())).collect();
                    quote! {
                        #original_enum_name::#variant_name(#(#var_names),*) => Ok(#subset_enum_name::#variant_name(#(#var_names),*)),
                    }
                }
                syn::Fields::Named(fields) => {
                    let field_names: Vec<_> = fields.named.iter().map(|f| f.ident.as_ref().unwrap()).collect();
                    quote! {
                        #original_enum_name::#variant_name { #(#field_names),* } => Ok(#subset_enum_name::#variant_name { #(#field_names),* }),
                    }
                }
            }
        } else {
            let original_enum_name_str = original_enum_name.to_string();
            let original_enum_variant_str = format!("{}::{}", original_enum_name_str, variant_name.to_string());
            let subset_enum_name_str = subset_enum_name.to_string();
            let error = quote!(epoch_core::EnumConversionError::new(#original_enum_variant_str.to_string(), #subset_enum_name_str.to_string()));

            match fields {
                syn::Fields::Unit => {
                    quote! {
                        #original_enum_name::#variant_name => Err(#error),
                    }
                }
                syn::Fields::Unnamed(_) => {
                    quote! {
                        #original_enum_name::#variant_name(..) => Err(#error),
                    }
                }
                syn::Fields::Named(_) => {
                    quote! {
                        #original_enum_name::#variant_name { .. } => Err(#error),
                    }
                }
            }
        }
    });

    try_from_impls.extend(quote! {
        impl std::convert::TryFrom<#original_enum_name> for #subset_enum_name {
            type Error = epoch_core::EnumConversionError;

            fn try_from(value: #original_enum_name) -> Result<Self, Self::Error> {
                match value {
                    #(#try_from_matches)*
                }
            }
        }
    });

    let expanded = quote! {
        #( #forwarded_attrs )*
        pub enum #subset_enum_name {
            #new_variants
        }

        #from_impls

        #try_from_impls

        #input_enum
    };

    expanded
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::quote;

    fn test_subset_enum(
        attr: proc_macro2::TokenStream,
        item: proc_macro2::TokenStream,
        expected: proc_macro2::TokenStream,
    ) {
        let actual = subset_enum_impl_internal(attr, item);
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
    fn test_empty_included_variants() {
        let attr = quote! { MySubsetEnum };
        let item = quote! {
            enum OriginalEnum {
                VariantA,
                VariantB(i32),
                VariantC { field: String },
            }
        };
        let expected = quote! {
            pub enum MySubsetEnum {
                VariantA,
                VariantB(i32),
                VariantC { field: String },
            }

            impl From<MySubsetEnum> for OriginalEnum {
                fn from(value: MySubsetEnum) -> Self {
                    match value {
                        MySubsetEnum::VariantA => OriginalEnum::VariantA,
                        MySubsetEnum::VariantB(__field0) => OriginalEnum::VariantB(__field0),
                        MySubsetEnum::VariantC { field } => OriginalEnum::VariantC { field },
                    }
                }
            }

            impl std::convert::TryFrom<OriginalEnum> for MySubsetEnum {
                type Error = epoch_core::EnumConversionError;

                fn try_from(value: OriginalEnum) -> Result<Self, Self::Error> {
                    match value {
                        OriginalEnum::VariantA => Ok(MySubsetEnum::VariantA),
                        OriginalEnum::VariantB(__field0) => Ok(MySubsetEnum::VariantB(__field0)),
                        OriginalEnum::VariantC { field } => Ok(MySubsetEnum::VariantC { field }),
                    }
                }
            }

            enum OriginalEnum {
                VariantA,
                VariantB(i32),
                VariantC { field: String },
            }
        };
        test_subset_enum(attr, item, expected);
    }

    #[test]
    fn test_specific_included_variants() {
        let attr = quote! { MySubsetEnum, VariantA, VariantC };
        let item = quote! {
            enum OriginalEnum {
                VariantA,
                VariantB(i32),
                VariantC { field: String },
                VariantD,
            }
        };
        let expected = quote! {
            pub enum MySubsetEnum {
                VariantA,
                VariantC { field: String },
            }

            impl From<MySubsetEnum> for OriginalEnum {
                fn from(value: MySubsetEnum) -> Self {
                    match value {
                        MySubsetEnum::VariantA => OriginalEnum::VariantA,
                        MySubsetEnum::VariantC { field } => OriginalEnum::VariantC { field },
                    }
                }
            }

            impl std::convert::TryFrom<OriginalEnum> for MySubsetEnum {
                type Error = epoch_core::EnumConversionError;

                fn try_from(value: OriginalEnum) -> Result<Self, Self::Error> {
                    match value {
                        OriginalEnum::VariantA => Ok(MySubsetEnum::VariantA),
                        OriginalEnum::VariantB(..) => Err(epoch_core::EnumConversionError::new("OriginalEnum::VariantB".to_string(), "MySubsetEnum".to_string())),
                        OriginalEnum::VariantC { field } => Ok(MySubsetEnum::VariantC { field }),
                        OriginalEnum::VariantD => Err(epoch_core::EnumConversionError::new("OriginalEnum::VariantD".to_string(), "MySubsetEnum".to_string())),
                    }
                }
            }

            enum OriginalEnum {
                VariantA,
                VariantB(i32),
                VariantC { field: String },
                VariantD,
            }
        };
        test_subset_enum(attr, item, expected);
    }

    #[test]
    fn test_mixed_variant_types() {
        let attr = quote! { MySubsetEnum, UnitVariant, TupleVariant, StructVariant };
        let item = quote! {
            enum OriginalEnum {
                UnitVariant,
                TupleVariant(u32, bool),
                StructVariant { name: String, id: u64 },
                AnotherUnit,
            }
        };
        let expected = quote! {
            pub enum MySubsetEnum {
                UnitVariant,
                TupleVariant(u32, bool),
                StructVariant { name: String, id: u64 },
            }

            impl From<MySubsetEnum> for OriginalEnum {
                fn from(value: MySubsetEnum) -> Self {
                    match value {
                        MySubsetEnum::UnitVariant => OriginalEnum::UnitVariant,
                        MySubsetEnum::TupleVariant(__field0, __field1) => OriginalEnum::TupleVariant(__field0, __field1),
                        MySubsetEnum::StructVariant { name, id } => OriginalEnum::StructVariant { name, id },
                    }
                }
            }

            impl std::convert::TryFrom<OriginalEnum> for MySubsetEnum {
                type Error = epoch_core::EnumConversionError;

                fn try_from(value: OriginalEnum) -> Result<Self, Self::Error> {
                    match value {
                        OriginalEnum::UnitVariant => Ok(MySubsetEnum::UnitVariant),
                        OriginalEnum::TupleVariant(__field0, __field1) => Ok(MySubsetEnum::TupleVariant(__field0, __field1)),
                        OriginalEnum::StructVariant { name, id } => Ok(MySubsetEnum::StructVariant { name, id }),
                        OriginalEnum::AnotherUnit => Err(epoch_core::EnumConversionError::new("OriginalEnum::AnotherUnit".to_string(), "MySubsetEnum".to_string())),
                    }
                }
            }

            enum OriginalEnum {
                UnitVariant,
                TupleVariant(u32, bool),
                StructVariant { name: String, id: u64 },
                AnotherUnit,
            }
        };
        test_subset_enum(attr, item, expected);
    }

    #[test]
    fn test_attribute_forwarding() {
        let attr = quote! { MySubsetEnum };
        let item = quote! {
            #[derive(Debug, Clone)]
            #[allow(dead_code)]
            enum OriginalEnum {
                VariantA,
                VariantB(i32),
            }
        };
        let expected = quote! {
            #[derive(Debug, Clone)]
            #[allow(dead_code)]
            pub enum MySubsetEnum {
                VariantA,
                VariantB(i32),
            }

            impl From<MySubsetEnum> for OriginalEnum {
                fn from(value: MySubsetEnum) -> Self {
                    match value {
                        MySubsetEnum::VariantA => OriginalEnum::VariantA,
                        MySubsetEnum::VariantB(__field0) => OriginalEnum::VariantB(__field0),
                    }
                }
            }

            impl std::convert::TryFrom<OriginalEnum> for MySubsetEnum {
                type Error = epoch_core::EnumConversionError;

                fn try_from(value: OriginalEnum) -> Result<Self, Self::Error> {
                    match value {
                        OriginalEnum::VariantA => Ok(MySubsetEnum::VariantA),
                        OriginalEnum::VariantB(__field0) => Ok(MySubsetEnum::VariantB(__field0)),
                    }
                }
            }

            #[derive(Debug, Clone)]
            #[allow(dead_code)]
            enum OriginalEnum {
                VariantA,
                VariantB(i32),
            }
        };
        test_subset_enum(attr, item, expected);
    }
}
