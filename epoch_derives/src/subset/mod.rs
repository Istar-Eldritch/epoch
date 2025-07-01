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
    let item_tokens: proc_macro2::TokenStream = item.into();
    let attr_tokens: proc_macro2::TokenStream = attr.into();

    let input_enum = match syn::parse2::<ItemEnum>(item_tokens) {
        Ok(tree) => tree,
        Err(e) => return e.to_compile_error().into(),
    };
    let parsed_attr = match syn::parse2::<SubsetEnumAttr>(attr_tokens) {
        Ok(tree) => tree,
        Err(e) => return e.to_compile_error().into(),
    };

    let subset_enum_name = parsed_attr.subset_enum_name;
    let included_variants: Vec<Ident> = parsed_attr.included_variants.into_iter().collect();

    let original_enum_name = &input_enum.ident;
    let original_variants = &input_enum.variants;

    let mut new_variants = quote! {};
    let mut from_impls = quote! {};

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
                let field_names: Vec<&Ident> = fields.named.iter().map(|f| f.ident.as_ref().unwrap()).collect();
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

    let expanded = quote! {
        #[derive(Debug, PartialEq, Clone)]
        pub enum #subset_enum_name {
            #new_variants
        }

        #from_impls

        #input_enum
    };

    expanded.into()
}

