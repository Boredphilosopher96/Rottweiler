//! Derive source-exhaustive accounting of nested owned allocations.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{Data, DeriveInput, Fields, parse_macro_input};

#[proc_macro_derive(PrepareAllocation)]
pub fn prepare_allocation(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn additions(bindings: &[proc_macro2::TokenStream], normalize: bool) -> proc_macro2::TokenStream {
    if normalize {
        return quote! { #(crate::allocation::PrepareAllocation::prepare_allocations(#bindings);)* };
    }
    if bindings.is_empty() {
        return quote!(Some(0));
    }
    quote! {
        let mut total = 0usize;
        #(total = total.checked_add(crate::allocation::PrepareAllocation::prepared_heap_bytes(#bindings)?)?;)*
        Some(total)
    }
}

fn expand(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    for attribute in &input.attrs {
        if attribute.path().is_ident("serde")
            && attribute
                .meta
                .require_list()?
                .tokens
                .to_string()
                .split(',')
                .any(|part| part.trim() == "untagged")
        {
            return Err(syn::Error::new_spanned(
                attribute,
                "allocation profiles require a tagged or structurally unambiguous decoder",
            ));
        }
    }
    let name = &input.ident;
    let mut generics = input.generics.clone();
    for parameter in generics.type_params_mut() {
        parameter
            .bounds
            .push(syn::parse_quote!(crate::allocation::PrepareAllocation));
    }
    let (implementation, types, clause) = generics.split_for_impl();
    let bytes = operation(input, false)?;
    let normalization = operation(input, true)?;
    let fields: Vec<_> = match &input.data {
        Data::Struct(data) => data.fields.iter().map(|field| &field.ty).collect(),
        Data::Enum(data) => data
            .variants
            .iter()
            .flat_map(|variant| variant.fields.iter().map(|field| &field.ty))
            .collect(),
        Data::Union(_) => Vec::new(),
    };
    Ok(quote! {
        impl #implementation crate::allocation::DecodeAllocation for #name #types #clause {
            fn decode_node_bytes() -> Option<usize> {
                let mut largest = std::mem::size_of::<Self>();
                #(largest = largest.max(<#fields as crate::allocation::DecodeAllocation>::decode_node_bytes()?);)*
                Some(largest)
            }
        }
        impl #implementation crate::allocation::PrepareAllocation for #name #types #clause {
            fn prepared_heap_bytes(&self) -> Option<usize> { #bytes }
            fn prepare_allocations(&mut self) { #normalization }
        }
    })
}

fn operation(input: &DeriveInput, normalize: bool) -> syn::Result<proc_macro2::TokenStream> {
    let body = match &input.data {
        Data::Struct(data) => {
            let fields: Vec<_> = data
                .fields
                .iter()
                .enumerate()
                .map(|(index, field)| {
                    let member = field.ident.clone().map_or_else(
                        || syn::Member::Unnamed(syn::Index::from(index)),
                        syn::Member::Named,
                    );
                    if normalize {
                        quote!(&mut self.#member)
                    } else {
                        quote!(&self.#member)
                    }
                })
                .collect();
            additions(&fields, normalize)
        }
        Data::Enum(data) => {
            let arms = data.variants.iter().map(|variant| {
                let variant_name = &variant.ident;
                let names: Vec<_> = variant
                    .fields
                    .iter()
                    .enumerate()
                    .map(|(index, field)| {
                        field
                            .ident
                            .clone()
                            .unwrap_or_else(|| format_ident!("field_{index}"))
                    })
                    .collect();
                let fields: Vec<_> = names.iter().map(|name| quote!(#name)).collect();
                let sum = additions(&fields, normalize);
                let pattern = match &variant.fields {
                    Fields::Named(_) => quote!(Self::#variant_name { #(#names),* }),
                    Fields::Unnamed(_) => quote!(Self::#variant_name(#(#names),*)),
                    Fields::Unit => quote!(Self::#variant_name),
                };
                quote!(#pattern => { #sum })
            });
            quote!(match self { #(#arms),* })
        }
        Data::Union(_) => {
            return Err(syn::Error::new_spanned(
                input,
                "owned allocation accounting cannot inspect a union",
            ));
        }
    };
    Ok(body)
}
