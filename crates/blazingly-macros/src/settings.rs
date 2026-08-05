//! `#[settings]` — a struct whose fields come from configuration.
//!
//! The generated loader reads every field before it fails, because a container
//! missing three variables should learn all three from one failed boot.

use proc_macro2::TokenStream;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::spanned::Spanned as _;
use syn::{Attribute, Fields, Ident, ItemStruct, LitInt, LitStr, Token, Type};

pub struct SettingsArgs {
    prefix: Option<LitStr>,
}

impl Parse for SettingsArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut prefix = None;
        while !input.is_empty() {
            let key = input.parse::<Ident>()?;
            input.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "prefix" => {
                    if prefix.is_some() {
                        return Err(syn::Error::new(key.span(), "`prefix` was specified twice"));
                    }
                    prefix = Some(input.parse::<LitStr>()?);
                }
                _ => return Err(syn::Error::new(key.span(), "the only key is `prefix`")),
            }
            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }
        Ok(Self { prefix })
    }
}

/// What one field declares about where its value comes from.
#[derive(Default)]
struct FieldArgs {
    variable: Option<LitStr>,
    default: Option<LitStr>,
    min_length: Option<LitInt>,
    max_length: Option<LitInt>,
}

fn take_field_arguments(attributes: &mut Vec<Attribute>) -> syn::Result<FieldArgs> {
    let mut arguments = FieldArgs::default();
    let mut error = None;
    attributes.retain(|attribute| {
        let Some(name) = attribute.path().get_ident().map(Ident::to_string) else {
            return true;
        };
        let mut take = |slot: &mut Option<LitStr>| match attribute.parse_args::<LitStr>() {
            Ok(value) => *slot = Some(value),
            Err(parse_error) => {
                error.get_or_insert(parse_error);
            }
        };
        match name.as_str() {
            "env" => take(&mut arguments.variable),
            "default" => take(&mut arguments.default),
            "min_length" | "max_length" => {
                let slot = if name == "min_length" {
                    &mut arguments.min_length
                } else {
                    &mut arguments.max_length
                };
                match attribute.parse_args::<LitInt>() {
                    Ok(value) => *slot = Some(value),
                    Err(parse_error) => {
                        error.get_or_insert(parse_error);
                    }
                }
            }
            _ => return true,
        }
        false
    });
    error.map_or(Ok(arguments), Err)
}

/// `Option<T>` is how a field says an unset variable is not an error.
fn option_inner(ty: &Type) -> Option<&Type> {
    let Type::Path(path) = ty else { return None };
    let segment = path.path.segments.last()?;
    if segment.ident != "Option" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };
    arguments.args.iter().find_map(|argument| match argument {
        syn::GenericArgument::Type(inner) => Some(inner),
        _ => None,
    })
}

/// `field_name` becomes `PREFIX_FIELD_NAME`, which is what a deployment writes.
fn derived_variable(field: &Ident, prefix: Option<&LitStr>) -> String {
    let name = field.to_string().to_uppercase();
    prefix.map_or(name.clone(), |prefix| format!("{}{name}", prefix.value()))
}

/// What one field contributes to the generated loader.
struct FieldTokens {
    read: TokenStream,
    assignment: TokenStream,
    descriptor: TokenStream,
}

/// Reads one field, records what went wrong, and keeps going.
fn read_tokens(
    variable: &str,
    ty: &Type,
    optional: Option<&Type>,
    binding: &Ident,
    default: Option<&LitStr>,
) -> TokenStream {
    if let Some(inner) = optional {
        return quote! {
            let #binding = ::blazingly_config::__private::read_optional::<#inner>(
                source,
                #variable,
                &mut errors,
            );
        };
    }
    let default = default.map_or_else(
        || quote!(::core::option::Option::None),
        |value| quote!(::core::option::Option::Some(#value)),
    );
    quote! {
        let #binding = ::blazingly_config::__private::read::<#ty>(
            source,
            #variable,
            #default,
            &mut errors,
        );
    }
}

/// Declared bounds, checked on the parsed value's string form so a length bound
/// reads the same here as it does on an API model.
fn check_tokens(arguments: &FieldArgs, variable: &str, binding: &Ident) -> TokenStream {
    let mut checks = Vec::new();
    if let Some(minimum) = &arguments.min_length {
        checks.push(quote! {
            ::blazingly_config::__private::check_min_length(
                &::std::string::ToString::to_string(&checked),
                #minimum,
                #variable,
                &mut errors,
            );
        });
    }
    if let Some(maximum) = &arguments.max_length {
        checks.push(quote! {
            ::blazingly_config::__private::check_max_length(
                &::std::string::ToString::to_string(&checked),
                #maximum,
                #variable,
                &mut errors,
            );
        });
    }
    if checks.is_empty() {
        return quote!();
    }
    quote! {
        if let ::core::option::Option::Some(checked) = &#binding {
            #(#checks)*
        }
    }
}

fn field_tokens(field: &mut syn::Field, prefix: Option<&LitStr>) -> syn::Result<FieldTokens> {
    let arguments = take_field_arguments(&mut field.attrs)?;
    let name = field
        .ident
        .as_ref()
        .ok_or_else(|| syn::Error::new(field.span(), "a settings field needs a name"))?;
    let variable = arguments
        .variable
        .as_ref()
        .map_or_else(|| derived_variable(name, prefix), LitStr::value);
    let ty = &field.ty;
    let optional = option_inner(ty);

    if let (Some(default), Some(_)) = (&arguments.default, optional) {
        return Err(syn::Error::new(
            default.span(),
            "an `Option<T>` field already reads an unset variable as `None`; give it a \
             `#[default]` or make it a `T`, not both",
        ));
    }

    let binding = quote::format_ident!("value_of_{name}");
    let read = read_tokens(
        &variable,
        ty,
        optional,
        &binding,
        arguments.default.as_ref(),
    );
    let check = check_tokens(&arguments, &variable, &binding);

    // A field whose read failed has already recorded why, and the accumulated
    // errors are what the caller receives — but a required field has no value
    // to put in the struct, so the loader returns once every field has spoken.
    let assignment = if optional.is_some() {
        quote! { #name: #binding }
    } else {
        quote! {
            #name: match #binding {
                ::core::option::Option::Some(value) => value,
                ::core::option::Option::None => {
                    return ::core::result::Result::Err(errors);
                }
            }
        }
    };

    let required = optional.is_none() && arguments.default.is_none();
    let has_default = arguments.default.is_some();
    let field_name = name.to_string();
    let mut rules = Vec::new();
    if let Some(minimum) = &arguments.min_length {
        rules.push(quote!(::blazingly_config::ValidationRule::MinLength(#minimum)));
    }
    if let Some(maximum) = &arguments.max_length {
        rules.push(quote!(::blazingly_config::ValidationRule::MaxLength(#maximum)));
    }

    Ok(FieldTokens {
        read: quote! { #read #check },
        assignment,
        descriptor: quote! {
            ::blazingly_config::SettingDescriptor {
                variable: ::std::string::String::from(#variable),
                field: #field_name,
                required: #required,
                has_default: #has_default,
                rules: ::std::vec![#(#rules),*],
            }
        },
    })
}

pub fn settings_tokens(
    arguments: &SettingsArgs,
    item: &mut ItemStruct,
) -> syn::Result<TokenStream> {
    let Fields::Named(fields) = &mut item.fields else {
        return Err(syn::Error::new(
            item.span(),
            "`#[settings]` needs a struct with named fields: a configuration variable is named \
             after the field it fills, and a tuple struct has no names to use",
        ));
    };

    let mut reads = Vec::new();
    let mut assignments = Vec::new();
    let mut descriptors = Vec::new();
    for field in &mut fields.named {
        let tokens = field_tokens(field, arguments.prefix.as_ref())?;
        reads.push(tokens.read);
        assignments.push(tokens.assignment);
        descriptors.push(tokens.descriptor);
    }

    let name = &item.ident;
    let (impl_generics, type_generics, where_clause) = item.generics.split_for_impl();

    Ok(quote! {
        #item

        impl #impl_generics ::blazingly_config::Settings for #name #type_generics #where_clause {
            fn load(
                source: &dyn ::blazingly_config::ConfigSource,
            ) -> ::core::result::Result<Self, ::blazingly_config::ConfigError> {
                let mut errors = ::blazingly_config::ConfigError::new();
                #(#reads)*
                let value = Self { #(#assignments),* };
                errors.into_result(value)
            }

            fn variables() -> ::std::vec::Vec<::blazingly_config::SettingDescriptor> {
                ::std::vec![#(#descriptors),*]
            }
        }
    })
}
