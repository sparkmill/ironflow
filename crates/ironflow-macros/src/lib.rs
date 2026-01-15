//! Procedural macros for the ironflow workflow engine.
//!
//! # HasWorkflowId Derive Macro
//!
//! Automatically implements `HasWorkflowId` for enum input types.
//!
//! ## Usage
//!
//! ```ignore
//! #[derive(HasWorkflowId)]
//! #[workflow_id(order_id)]  // default field name for all variants
//! enum OrderInput {
//!     Create { order_id: String, items: Vec<Item> },
//!     Confirm { order_id: String },
//!     #[workflow_id(id)]  // override for this variant
//!     Cancel { id: String },
//! }
//! ```

use proc_macro::TokenStream;
use quote::quote;
use syn::{
    Attribute, Data, DeriveInput, Fields, Ident, Variant, parse_macro_input, spanned::Spanned,
};

/// Derives `HasWorkflowId` for an enum.
///
/// Use `#[workflow_id(field_name)]` on the enum to set the default field,
/// and optionally on individual variants to override.
#[proc_macro_derive(HasWorkflowId, attributes(workflow_id))]
pub fn derive_has_workflow_id(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);

    match derive_has_workflow_id_impl(input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn derive_has_workflow_id_impl(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let name = &input.ident;

    // Get the default field from enum-level #[workflow_id(field)]
    let default_field = get_workflow_id_attr(&input.attrs)?;

    let data = match &input.data {
        Data::Enum(data) => data,
        _ => {
            return Err(syn::Error::new(
                input.span(),
                "HasWorkflowId can only be derived for enums",
            ));
        }
    };

    let mut match_arms = Vec::new();

    for variant in &data.variants {
        let field_name = get_variant_workflow_id(variant, &default_field)?;
        let arm = generate_match_arm(name, variant, &field_name)?;
        match_arms.push(arm);
    }

    Ok(quote! {
        impl ::ironflow::HasWorkflowId for #name {
            fn workflow_id(&self) -> ::ironflow::WorkflowId {
                match self {
                    #(#match_arms)*
                }
            }
        }
    })
}

/// Extract the field name from `#[workflow_id(field_name)]` attribute.
fn get_workflow_id_attr(attrs: &[Attribute]) -> syn::Result<Option<Ident>> {
    for attr in attrs {
        if attr.path().is_ident("workflow_id") {
            let ident: Ident = attr.parse_args()?;
            return Ok(Some(ident));
        }
    }
    Ok(None)
}

/// Get the workflow_id field for a variant (variant-level override or default).
fn get_variant_workflow_id(variant: &Variant, default_field: &Option<Ident>) -> syn::Result<Ident> {
    // Check for variant-level override
    if let Some(field) = get_workflow_id_attr(&variant.attrs)? {
        return Ok(field);
    }

    // Use default if available
    if let Some(field) = default_field {
        return Ok(field.clone());
    }

    // No default and no override - error
    Err(syn::Error::new(
        variant.span(),
        format!(
            "Variant `{}` has no #[workflow_id(field)] attribute and no default is set. \
             Either add #[workflow_id(field_name)] to this variant or add a default \
             #[workflow_id(field_name)] attribute to the enum.",
            variant.ident
        ),
    ))
}

/// Generate a match arm for a variant.
fn generate_match_arm(
    enum_name: &Ident,
    variant: &Variant,
    field_name: &Ident,
) -> syn::Result<proc_macro2::TokenStream> {
    let variant_name = &variant.ident;

    match &variant.fields {
        Fields::Named(fields) => {
            // Verify the field exists
            let field_exists = fields
                .named
                .iter()
                .any(|f| f.ident.as_ref() == Some(field_name));

            if !field_exists {
                return Err(syn::Error::new(
                    variant.span(),
                    format!(
                        "Field `{}` not found in variant `{}`. Available fields: {}",
                        field_name,
                        variant_name,
                        fields
                            .named
                            .iter()
                            .filter_map(|f| f.ident.as_ref())
                            .map(|i| i.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ));
            }

            Ok(quote! {
                #enum_name::#variant_name { #field_name, .. } => ::ironflow::WorkflowId::new(#field_name),
            })
        }
        Fields::Unnamed(_) => Err(syn::Error::new(
            variant.span(),
            "HasWorkflowId derive does not support tuple variants. Use named fields instead.",
        )),
        Fields::Unit => Err(syn::Error::new(
            variant.span(),
            "HasWorkflowId derive does not support unit variants. All variants must have fields.",
        )),
    }
}
