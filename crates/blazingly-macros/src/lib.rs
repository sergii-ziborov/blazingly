#![forbid(unsafe_code)]

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::{
    Attribute, Fields, FnArg, Ident, ItemEnum, ItemFn, ItemStruct, LitBool, LitInt, LitStr, Pat,
    PatType, ReturnType, Token, Type, TypePath, bracketed, parse_macro_input,
};

struct OperationArgs {
    path: LitStr,
    id: LitStr,
    summary: LitStr,
}

struct UniversalOperationArgs {
    method: HttpMethodArgument,
    operation: OperationArgs,
}

struct HttpMethodArgument {
    value: String,
    span: proc_macro2::Span,
}

#[derive(Clone, Copy)]
enum ProviderLifetimeArgument {
    Singleton,
    Request,
    Transient,
}

struct ProviderArgs {
    lifetime: ProviderLifetimeArgument,
}

struct SecurityArgs {
    scheme: LitStr,
    scopes: Vec<LitStr>,
}

#[derive(Default)]
struct McpArgs {
    name: Option<LitStr>,
    description: Option<LitStr>,
    risk: Option<LitStr>,
    confirmation: Option<LitStr>,
    idempotent: Option<LitBool>,
    expose_output: Option<LitStr>,
}

#[derive(Default)]
struct ModelArgs {
    rename_all: Option<LitStr>,
}

impl Parse for ModelArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        if input.is_empty() {
            return Ok(Self::default());
        }

        let key = input.parse::<Ident>()?;
        if key != "rename_all" {
            return Err(syn::Error::new(
                key.span(),
                "the supported model option is `rename_all`",
            ));
        }
        input.parse::<Token![=]>()?;
        let rename_all = input.parse::<LitStr>()?;
        if !input.is_empty() {
            return Err(input.error("unexpected model option"));
        }
        Ok(Self {
            rename_all: Some(rename_all),
        })
    }
}

#[derive(Default)]
struct FieldRules {
    min_length: Option<(usize, proc_macro2::Span)>,
    max_length: Option<(usize, proc_macro2::Span)>,
    email: bool,
}

struct OperationOutput {
    status: u16,
    success: Option<Type>,
    error: Option<Type>,
}

#[derive(Clone, Copy)]
enum OperationInputKind {
    Path,
    Query,
    Header,
    Cookie,
    Json,
    Form,
    Multipart,
    File,
    Dependency,
    DirectDependency,
}

struct OperationInput {
    name: LitStr,
    kind: OperationInputKind,
    argument_type: Type,
    inner: Type,
    required: bool,
}

struct ErrorVariant {
    status: u16,
    code: LitStr,
    message: LitStr,
    identifier: Ident,
    payload: Option<Type>,
    headers: Vec<(LitStr, LitStr)>,
}

impl Parse for McpArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut arguments = Self::default();

        while !input.is_empty() {
            let key = input.parse::<Ident>()?;
            input.parse::<Token![=]>()?;

            match key.to_string().as_str() {
                "name" => arguments.name = Some(input.parse()?),
                "description" => arguments.description = Some(input.parse()?),
                "risk" => arguments.risk = Some(input.parse()?),
                "confirmation" => arguments.confirmation = Some(input.parse()?),
                "idempotent" => arguments.idempotent = Some(input.parse()?),
                "expose_output" => arguments.expose_output = Some(input.parse()?),
                _ => {
                    return Err(syn::Error::new(
                        key.span(),
                        "supported MCP keys are `name`, `description`, `risk`, \
                         `confirmation`, `idempotent`, and `expose_output`",
                    ));
                }
            }

            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }

        Ok(arguments)
    }
}

impl Parse for OperationArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let path = input.parse::<LitStr>()?;
        let mut id = None;
        let mut summary = None;

        while !input.is_empty() {
            input.parse::<Token![,]>()?;
            if input.is_empty() {
                break;
            }

            let key = input.parse::<Ident>()?;
            input.parse::<Token![=]>()?;
            let value = input.parse::<LitStr>()?;

            match key.to_string().as_str() {
                "id" => id = Some(value),
                "summary" => summary = Some(value),
                _ => {
                    return Err(syn::Error::new(
                        key.span(),
                        "supported keys are `id` and `summary`",
                    ));
                }
            }
        }

        let id = id.ok_or_else(|| {
            syn::Error::new(path.span(), "an explicit stable `id = \"...\"` is required")
        })?;
        let summary = summary.unwrap_or_else(|| LitStr::new("", path.span()));

        Ok(Self { path, id, summary })
    }
}

impl Parse for UniversalOperationArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut method = None;
        let mut path = None;
        let mut id = None;
        let mut summary = None;

        while !input.is_empty() {
            let key = input.parse::<Ident>()?;
            input.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "method" => {
                    if method.is_some() {
                        return Err(syn::Error::new(key.span(), "`method` was specified twice"));
                    }
                    method = Some(if input.peek(LitStr) {
                        let value = input.parse::<LitStr>()?;
                        HttpMethodArgument {
                            value: value.value(),
                            span: value.span(),
                        }
                    } else {
                        let value = input.parse::<Ident>()?;
                        HttpMethodArgument {
                            value: value.to_string(),
                            span: value.span(),
                        }
                    });
                }
                "path" => {
                    if path.is_some() {
                        return Err(syn::Error::new(key.span(), "`path` was specified twice"));
                    }
                    path = Some(input.parse::<LitStr>()?);
                }
                "id" => {
                    if id.is_some() {
                        return Err(syn::Error::new(key.span(), "`id` was specified twice"));
                    }
                    id = Some(input.parse::<LitStr>()?);
                }
                "summary" => {
                    if summary.is_some() {
                        return Err(syn::Error::new(key.span(), "`summary` was specified twice"));
                    }
                    summary = Some(input.parse::<LitStr>()?);
                }
                _ => {
                    return Err(syn::Error::new(
                        key.span(),
                        "supported keys are `method`, `path`, `id`, and `summary`",
                    ));
                }
            }

            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }

        let method =
            method.ok_or_else(|| syn::Error::new(input.span(), "`method = ...` is required"))?;
        let path =
            path.ok_or_else(|| syn::Error::new(input.span(), "`path = \"...\"` is required"))?;
        let id = id.ok_or_else(|| {
            syn::Error::new(
                input.span(),
                "an explicit stable `id = \"...\"` is required",
            )
        })?;
        let summary = summary.unwrap_or_else(|| LitStr::new("", path.span()));

        Ok(Self {
            method,
            operation: OperationArgs { path, id, summary },
        })
    }
}

impl Parse for ProviderArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        if input.is_empty() {
            return Ok(Self {
                lifetime: ProviderLifetimeArgument::Request,
            });
        }

        let lifetime = input.parse::<Ident>()?;
        if !input.is_empty() {
            return Err(input.error(
                "use `#[provider]`, `#[provider(singleton)]`, \
                 `#[provider(request)]`, or `#[provider(transient)]`",
            ));
        }
        let lifetime = match lifetime.to_string().as_str() {
            "singleton" => ProviderLifetimeArgument::Singleton,
            "request" => ProviderLifetimeArgument::Request,
            "transient" => ProviderLifetimeArgument::Transient,
            _ => {
                return Err(syn::Error::new(
                    lifetime.span(),
                    "provider lifetime must be `singleton`, `request`, or `transient`",
                ));
            }
        };
        Ok(Self { lifetime })
    }
}

impl Parse for SecurityArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let scheme = input.parse::<LitStr>()?;
        let mut scopes = Vec::new();
        if !input.is_empty() {
            input.parse::<Token![,]>()?;
            let key = input.parse::<Ident>()?;
            if key != "scopes" {
                return Err(syn::Error::new(
                    key.span(),
                    "the supported security option is `scopes = [\"...\"]`",
                ));
            }
            input.parse::<Token![=]>()?;
            let content;
            bracketed!(content in input);
            while !content.is_empty() {
                scopes.push(content.parse()?);
                if !content.is_empty() {
                    content.parse::<Token![,]>()?;
                }
            }
        }
        if !input.is_empty() {
            return Err(input.error("unexpected security option"));
        }
        Ok(Self { scheme, scopes })
    }
}

/// Defines an operation using an explicit HTTP method.
///
/// This is the universal form behind the method-specific operation macros:
/// `#[operation(method = PUT, path = "/users/{id}", id = "users.replace")]`.
#[proc_macro_attribute]
pub fn operation(arguments: TokenStream, item: TokenStream) -> TokenStream {
    let arguments = parse_macro_input!(arguments as UniversalOperationArgs);
    let method = match http_method_tokens(&arguments.method) {
        Ok(method) => method,
        Err(error) => return error.into_compile_error().into(),
    };
    let mut function = parse_macro_input!(item as ItemFn);

    match operation_tokens(arguments.operation, &mut function, &method) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.into_compile_error().into(),
    }
}

#[proc_macro_attribute]
pub fn get(arguments: TokenStream, item: TokenStream) -> TokenStream {
    expand_operation(arguments, item, &quote!(::blazingly::HttpMethod::Get))
}

#[proc_macro_attribute]
pub fn head(arguments: TokenStream, item: TokenStream) -> TokenStream {
    expand_operation(arguments, item, &quote!(::blazingly::HttpMethod::Head))
}

#[proc_macro_attribute]
pub fn post(arguments: TokenStream, item: TokenStream) -> TokenStream {
    expand_operation(arguments, item, &quote!(::blazingly::HttpMethod::Post))
}

#[proc_macro_attribute]
pub fn put(arguments: TokenStream, item: TokenStream) -> TokenStream {
    expand_operation(arguments, item, &quote!(::blazingly::HttpMethod::Put))
}

#[proc_macro_attribute]
pub fn patch(arguments: TokenStream, item: TokenStream) -> TokenStream {
    expand_operation(arguments, item, &quote!(::blazingly::HttpMethod::Patch))
}

#[proc_macro_attribute]
pub fn delete(arguments: TokenStream, item: TokenStream) -> TokenStream {
    expand_operation(arguments, item, &quote!(::blazingly::HttpMethod::Delete))
}

#[proc_macro_attribute]
pub fn options(arguments: TokenStream, item: TokenStream) -> TokenStream {
    expand_operation(arguments, item, &quote!(::blazingly::HttpMethod::Options))
}

#[proc_macro_attribute]
pub fn trace(arguments: TokenStream, item: TokenStream) -> TokenStream {
    expand_operation(arguments, item, &quote!(::blazingly::HttpMethod::Trace))
}

#[proc_macro_attribute]
pub fn connect(arguments: TokenStream, item: TokenStream) -> TokenStream {
    expand_operation(arguments, item, &quote!(::blazingly::HttpMethod::Connect))
}

#[proc_macro_attribute]
pub fn api_model(arguments: TokenStream, item: TokenStream) -> TokenStream {
    let arguments = parse_macro_input!(arguments as ModelArgs);
    let mut model = parse_macro_input!(item as ItemStruct);

    match model_tokens(arguments, &mut model) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.into_compile_error().into(),
    }
}

#[proc_macro_attribute]
pub fn api_error(_arguments: TokenStream, item: TokenStream) -> TokenStream {
    let mut error = parse_macro_input!(item as ItemEnum);

    match error_tokens(&mut error) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.into_compile_error().into(),
    }
}

/// Turns a typed factory function into a compiled DI provider declaration.
///
/// `#[provider]` defaults to request scope. `singleton`, `request`, and
/// `transient` can be selected explicitly. Asyncness and a
/// `Result<T, DependencyError>` return are inferred from the function.
#[proc_macro_attribute]
pub fn provider(arguments: TokenStream, item: TokenStream) -> TokenStream {
    let arguments = parse_macro_input!(arguments as ProviderArgs);
    let function = parse_macro_input!(item as ItemFn);

    match provider_tokens(&arguments, &function) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.into_compile_error().into(),
    }
}

#[proc_macro_attribute]
pub fn tool(_arguments: TokenStream, item: TokenStream) -> TokenStream {
    let function = parse_macro_input!(item as ItemFn);
    syn::Error::new_spanned(
        function.sig.ident,
        "place `#[post(...)]` or `#[operation(...)]` above `#[mcp::tool(...)]`",
    )
    .into_compile_error()
    .into()
}

#[proc_macro_attribute]
pub fn security(_arguments: TokenStream, item: TokenStream) -> TokenStream {
    let function = parse_macro_input!(item as ItemFn);
    syn::Error::new_spanned(
        function.sig.ident,
        "place an HTTP method macro or `#[operation(...)]` above `#[security(...)]`",
    )
    .into_compile_error()
    .into()
}

fn expand_operation(
    arguments: TokenStream,
    item: TokenStream,
    method: &proc_macro2::TokenStream,
) -> TokenStream {
    let arguments = parse_macro_input!(arguments as OperationArgs);
    let mut function = parse_macro_input!(item as ItemFn);

    match operation_tokens(arguments, &mut function, method) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.into_compile_error().into(),
    }
}

fn http_method_tokens(method: &HttpMethodArgument) -> syn::Result<proc_macro2::TokenStream> {
    let method = match method.value.to_ascii_uppercase().as_str() {
        "GET" => quote!(::blazingly::HttpMethod::Get),
        "HEAD" => quote!(::blazingly::HttpMethod::Head),
        "POST" => quote!(::blazingly::HttpMethod::Post),
        "PUT" => quote!(::blazingly::HttpMethod::Put),
        "PATCH" => quote!(::blazingly::HttpMethod::Patch),
        "DELETE" => quote!(::blazingly::HttpMethod::Delete),
        "OPTIONS" => quote!(::blazingly::HttpMethod::Options),
        "TRACE" => quote!(::blazingly::HttpMethod::Trace),
        "CONNECT" => quote!(::blazingly::HttpMethod::Connect),
        _ => {
            return Err(syn::Error::new(
                method.span,
                "unsupported HTTP method; expected GET, HEAD, POST, PUT, PATCH, \
                 DELETE, OPTIONS, TRACE, or CONNECT",
            ));
        }
    };
    Ok(method)
}

fn provider_tokens(
    arguments: &ProviderArgs,
    function: &ItemFn,
) -> syn::Result<proc_macro2::TokenStream> {
    if function.sig.constness.is_some()
        || matches!(&function.sig.safety, syn::Safety::Unsafe(_))
        || function.sig.abi.is_some()
        || function.sig.variadic.is_some()
        || !function.sig.generics.params.is_empty()
    {
        return Err(syn::Error::new_spanned(
            &function.sig,
            "Blazingly providers must be plain, non-generic Rust functions",
        ));
    }
    if function.sig.inputs.len() > 8 {
        return Err(syn::Error::new_spanned(
            &function.sig.inputs,
            "Blazingly providers accept at most eight `Depends<T>` arguments",
        ));
    }
    for input in &function.sig.inputs {
        let FnArg::Typed(argument) = input else {
            return Err(syn::Error::new_spanned(
                input,
                "provider inputs must use `Depends<T>`",
            ));
        };
        if wrapper_inner(&argument.ty, "Depends").is_none() {
            return Err(syn::Error::new_spanned(
                &argument.ty,
                "provider inputs must use `Depends<T>`",
            ));
        }
    }

    let ReturnType::Type(_, output) = &function.sig.output else {
        return Err(syn::Error::new_spanned(
            &function.sig,
            "providers require an explicit output type",
        ));
    };
    let fallible = if let Some((_, error)) = result_types(output) {
        if !type_is(&error, "DependencyError") {
            return Err(syn::Error::new_spanned(
                error,
                "fallible providers must return `Result<T, DependencyError>`",
            ));
        }
        true
    } else {
        false
    };
    let asynchronous = function.sig.asyncness.is_some();
    if asynchronous && matches!(arguments.lifetime, ProviderLifetimeArgument::Singleton) {
        return Err(syn::Error::new_spanned(
            &function.sig,
            "async singleton providers are unsupported because singleton \
             initialization is deterministic and synchronous at build time",
        ));
    }

    let constructor = match (arguments.lifetime, asynchronous, fallible) {
        (ProviderLifetimeArgument::Singleton, false, false) => format_ident!("singleton"),
        (ProviderLifetimeArgument::Singleton, false, true) => format_ident!("try_singleton"),
        (ProviderLifetimeArgument::Request, false, false) => format_ident!("request"),
        (ProviderLifetimeArgument::Request, false, true) => format_ident!("try_request"),
        (ProviderLifetimeArgument::Transient, false, false) => format_ident!("transient"),
        (ProviderLifetimeArgument::Transient, false, true) => format_ident!("try_transient"),
        (ProviderLifetimeArgument::Request, true, false) => format_ident!("request_async"),
        (ProviderLifetimeArgument::Request, true, true) => format_ident!("try_request_async"),
        (ProviderLifetimeArgument::Transient, true, false) => format_ident!("transient_async"),
        (ProviderLifetimeArgument::Transient, true, true) => {
            format_ident!("try_transient_async")
        }
        (ProviderLifetimeArgument::Singleton, true, _) => unreachable!(),
    };
    let function_name = &function.sig.ident;
    let provider_module = format_ident!("{function_name}");
    let visibility = &function.vis;

    Ok(quote! {
        #function

        #[doc(hidden)]
        #visibility mod #provider_module {
            #[allow(unused_imports)]
            use super::*;

            #[must_use]
            pub fn provider() -> ::blazingly::Provider {
                ::blazingly::Provider::#constructor(super::#function_name)
            }
        }
    })
}

#[allow(clippy::too_many_lines)]
fn operation_tokens(
    arguments: OperationArgs,
    function: &mut ItemFn,
    method: &proc_macro2::TokenStream,
) -> syn::Result<proc_macro2::TokenStream> {
    if function.sig.asyncness.is_none() {
        return Err(syn::Error::new_spanned(
            function.sig.fn_token,
            "Blazingly operations must be async functions",
        ));
    }

    let mcp = take_mcp_arguments(&mut function.attrs)?;
    let security = take_security_arguments(&mut function.attrs)?;
    let inputs = operation_inputs(&function.sig.inputs)?;
    let output = operation_output(&function.sig.output)?;
    let function_name = &function.sig.ident;
    let descriptor_module = format_ident!("{function_name}");
    let visibility = &function.vis;
    let path = arguments.path;
    let id = arguments.id;
    let summary = arguments.summary;

    let input_descriptors = inputs.iter().filter_map(|input| {
        let source = input.kind.source_tokens()?;
        let name = &input.name;
        let required = input.required;
        let inner = &input.inner;
        Some(quote! {
            ::blazingly::InputDescriptor::new(
                #name,
                #source,
                #required,
                <#inner as ::blazingly::ApiSchema>::type_descriptor(),
            )
        })
    });
    let dependency_descriptors = inputs
        .iter()
        .filter(|input| input.kind.is_dependency())
        .map(|input| {
            let inner = &input.inner;
            quote! {
                ::blazingly::DependencyDescriptor::new(
                    ::core::any::type_name::<#inner>()
                )
            }
        });
    let mcp_projection = mcp_projection(mcp, function_name, &summary)?;
    let security_requirements = security.iter().map(|security| {
        let scheme = &security.scheme;
        let scopes = &security.scopes;
        quote! {
            ::blazingly::SecurityRequirement::new(#scheme)
                .with_scopes(::std::vec![#(#scopes.to_owned()),*])
        }
    });
    let status = output.status;
    let success_descriptor = output.success.as_ref().map_or_else(
        || quote!(::core::option::Option::None),
        |success| {
            quote!(
                ::core::option::Option::Some(
                    <#success as ::blazingly::ApiSchema>::type_descriptor()
                )
            )
        },
    );
    let error_responses = output.error.map_or_else(
        || quote!(),
        |error| {
            quote! {
                responses.extend(
                    <#error as ::blazingly::ApiError>::response_descriptors()
                );
            }
        },
    );
    let executable = operation_executable(&inputs, function_name);

    Ok(quote! {
        #function

        #[doc(hidden)]
        #visibility mod #descriptor_module {
            #[allow(unused_imports)]
            use super::*;

            #[must_use]
            pub fn descriptor() -> ::blazingly::OperationDescriptor {
                let mut responses = ::std::vec![
                    ::blazingly::ResponseDescriptor::success(
                        #status,
                        #success_descriptor,
                    )
                ];
                #error_responses
                let descriptor = ::blazingly::OperationDescriptor::new(
                    #method,
                    #path,
                    #id,
                    #summary,
                    ::core::option::Option::None,
                    responses,
                )
                .expect("the operation id was validated by the Blazingly macro")
                .with_inputs(::std::vec![#(#input_descriptors),*])
                .with_dependencies(::std::vec![#(#dependency_descriptors),*])
                .with_security(::std::vec![#(#security_requirements),*]);
                #mcp_projection
            }

            #[must_use]
            pub fn executable() -> ::blazingly::ExecutableOperation {
                #executable
            }
        }
    })
}

fn operation_executable(
    inputs: &[OperationInput],
    function_name: &Ident,
) -> proc_macro2::TokenStream {
    let mut dependency_index = 0_usize;
    let extracted_arguments = inputs.iter().enumerate().map(|(index, input)| {
        let binding = format_ident!("__blazingly_argument_{index}");
        if input.kind.is_dependency() {
            let inner = &input.inner;
            let index = dependency_index;
            dependency_index += 1;
            if matches!(input.kind, OperationInputKind::Dependency) {
                quote! {
                    let #binding = dependencies
                        .get::<#inner>(#index)
                        .map_err(::blazingly::dependency_error_outcome)?;
                }
            } else {
                quote! {
                    let #binding = dependencies
                        .get_cloned::<#inner>(#index)
                        .map_err(::blazingly::dependency_error_outcome)?;
                }
            }
        } else {
            let argument_type = &input.argument_type;
            let name = &input.name;
            let required = input.required;
            quote! {
                let #binding = <#argument_type as ::blazingly::FromInvocation>::from_invocation(
                    &input,
                    #name,
                    #required,
                )
                .map_err(::blazingly::InputRejection::into_execution_outcome)?;
            }
        }
    });
    let dependency_requests = inputs
        .iter()
        .filter(|input| input.kind.is_dependency())
        .map(|input| {
            let inner = &input.inner;
            quote!(::blazingly::DependencyRequest::of::<#inner>())
        });
    let handler_arguments = (0..inputs.len()).map(|index| {
        let binding = format_ident!("__blazingly_argument_{index}");
        quote!(#binding)
    });
    quote! {
        ::blazingly::ExecutableOperation::typed_with_dependencies(
            descriptor(),
            ::std::vec![#(#dependency_requests),*],
            |input, dependencies| {
                #(#extracted_arguments)*
                let output = super::#function_name(#(#handler_arguments),*);
                ::core::result::Result::Ok(
                    ::std::boxed::Box::pin(async move {
                        let output = output.await;
                        ::blazingly::OperationOutput::into_execution_outcome(output)
                    }) as ::blazingly::OperationFuture
                )
            },
        )
    }
}

fn error_tokens(error: &mut ItemEnum) -> syn::Result<proc_macro2::TokenStream> {
    let variants = error
        .variants
        .iter_mut()
        .map(parse_error_variant)
        .collect::<syn::Result<Vec<_>>>()?;

    let name = &error.ident;
    let descriptors = variants.iter().map(error_descriptor_tokens);
    let failures = variants.iter().map(error_failure_tokens);

    Ok(quote! {
        #error

        impl ::blazingly::ApiError for #name {
            fn response_descriptors() -> ::std::vec::Vec<::blazingly::ResponseDescriptor> {
                ::std::vec![#(#descriptors),*]
            }

            fn into_failure(
                self,
            ) -> ::core::result::Result<
                ::blazingly::OperationFailure,
                ::blazingly::ResponseBuildError,
            > {
                match self {
                    #(#failures),*
                }
            }
        }
    })
}

fn parse_error_variant(variant: &mut syn::Variant) -> syn::Result<ErrorVariant> {
    let payload = match &variant.fields {
        Fields::Unit => None,
        Fields::Unnamed(fields) if fields.unnamed.len() == 1 => {
            fields.unnamed.first().map(|field| field.ty.clone())
        }
        Fields::Unnamed(_) | Fields::Named(_) => {
            return Err(syn::Error::new_spanned(
                &variant.fields,
                "typed errors support unit variants or one unnamed payload",
            ));
        }
    };
    let mut status = None;
    let mut code = None;
    let mut message = None;
    let mut headers = Vec::new();
    let mut retained = Vec::new();

    for attribute in variant.attrs.drain(..) {
        if attribute.path().is_ident("status") {
            status = Some(attribute.parse_args::<LitInt>()?);
        } else if attribute.path().is_ident("code") {
            code = Some(attribute.parse_args::<LitStr>()?);
        } else if attribute.path().is_ident("message") {
            message = Some(attribute.parse_args::<LitStr>()?);
        } else if attribute.path().is_ident("header") {
            headers.push(parse_error_header(attribute)?);
        } else {
            retained.push(attribute);
        }
    }
    variant.attrs = retained;
    let status = status.ok_or_else(|| {
        syn::Error::new_spanned(&variant.ident, "typed errors require `#[status(...)]`")
    })?;
    let status = status.base10_parse::<u16>()?;
    if !(400..=599).contains(&status) {
        return Err(syn::Error::new_spanned(
            &variant.ident,
            "typed error status must be between 400 and 599",
        ));
    }
    let code = code.ok_or_else(|| {
        syn::Error::new_spanned(&variant.ident, "typed errors require `#[code(\"...\")]`")
    })?;
    let message = message.unwrap_or_else(|| LitStr::new(&code.value(), code.span()));
    Ok(ErrorVariant {
        status,
        code,
        message,
        identifier: variant.ident.clone(),
        payload,
        headers,
    })
}

fn parse_error_header(attribute: Attribute) -> syn::Result<(LitStr, LitStr)> {
    let values = attribute
        .parse_args_with(syn::punctuated::Punctuated::<LitStr, Token![,]>::parse_terminated)?;
    if values.len() != 2 {
        return Err(syn::Error::new_spanned(
            attribute,
            "response headers require `#[header(\"name\", \"value\")]`",
        ));
    }
    let mut values = values.into_iter();
    let name = values
        .next()
        .ok_or_else(|| syn::Error::new(proc_macro2::Span::call_site(), "missing header name"))?;
    let value = values
        .next()
        .ok_or_else(|| syn::Error::new(proc_macro2::Span::call_site(), "missing header value"))?;
    validate_response_header(&name, &value)?;
    Ok((name, value))
}

fn error_descriptor_tokens(variant: &ErrorVariant) -> proc_macro2::TokenStream {
    let status = variant.status;
    let code = &variant.code;
    let message = &variant.message;
    let body = variant.payload.as_ref().map_or_else(
        || quote!(::core::option::Option::None),
        |payload| {
            quote!(
                ::core::option::Option::Some(
                    <#payload as ::blazingly::ApiSchema>::type_descriptor()
                )
            )
        },
    );
    let headers = variant
        .headers
        .iter()
        .map(|(name, value)| quote!(::blazingly::ResponseHeader::new(#name, #value)));
    quote!(
        ::blazingly::ResponseDescriptor::error(#status, #code, #message, #body)
            .with_headers(::std::vec![#(#headers),*])
    )
}

fn error_failure_tokens(variant: &ErrorVariant) -> proc_macro2::TokenStream {
    let identifier = &variant.identifier;
    let status = variant.status;
    let code = &variant.code;
    let message = &variant.message;
    let pattern = variant.payload.as_ref().map_or_else(
        || quote!(Self::#identifier),
        |_| quote!(Self::#identifier(payload)),
    );
    let serialize_payload = variant.payload.as_ref().map_or_else(
        || quote!(),
        |_| {
            quote! {
                let details = ::blazingly::__private::serde_json::to_vec(&payload)
                    .map_err(|_| ::blazingly::ResponseBuildError::serialization_failed())?;
                failure = failure.with_details(details);
            }
        },
    );
    let apply_headers = variant.headers.iter().map(|(name, value)| {
        quote! {
            failure = failure.with_header(#name, #value);
        }
    });
    quote! {
        #pattern => {
            let mut failure = ::blazingly::OperationFailure::new(#status, #code, #message);
            #serialize_payload
            #(#apply_headers)*
            ::core::result::Result::Ok(failure)
        }
    }
}

fn validate_response_header(name: &LitStr, value: &LitStr) -> syn::Result<()> {
    let valid_name = !name.value().is_empty()
        && name.value().bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        });
    if !valid_name {
        return Err(syn::Error::new(
            name.span(),
            "response header name contains invalid bytes",
        ));
    }
    if !value
        .value()
        .bytes()
        .all(|byte| byte == b'\t' || (byte >= b' ' && byte != 127))
    {
        return Err(syn::Error::new(
            value.span(),
            "response header value contains control bytes",
        ));
    }
    Ok(())
}

fn model_tokens(
    arguments: ModelArgs,
    model: &mut ItemStruct,
) -> syn::Result<proc_macro2::TokenStream> {
    let Fields::Named(fields) = &mut model.fields else {
        return Err(syn::Error::new_spanned(
            &model.fields,
            "`#[api_model]` requires a struct with named fields",
        ));
    };

    let rename_rule = model_rename_rule(&arguments)?;

    let mut descriptors = Vec::new();
    let mut validations = Vec::new();

    for field in &mut fields.named {
        let identifier = field
            .ident
            .as_ref()
            .expect("named fields always have identifiers");
        let rules = take_field_rules(&mut field.attrs)?;
        let field_type = &field.ty;
        let optional = wrapper_inner(field_type, "Option");
        let validation_type = optional.as_ref().unwrap_or(field_type);
        let public_name = if rename_rule == "camelCase" {
            snake_to_camel(&identifier.to_string())
        } else {
            identifier.to_string()
        };
        let public_name = LitStr::new(&public_name, identifier.span());
        let mut rule_descriptors = Vec::new();

        if let Some((minimum, _)) = rules.min_length {
            rule_descriptors.push(quote!(::blazingly::ValidationRule::MinLength(#minimum)));
        }
        if let Some((maximum, _)) = rules.max_length {
            rule_descriptors.push(quote!(::blazingly::ValidationRule::MaxLength(#maximum)));
        }
        if rules.email {
            rule_descriptors.push(quote!(::blazingly::ValidationRule::Email));
        }

        if !rule_descriptors.is_empty() && !is_string_type(validation_type) {
            return Err(syn::Error::new_spanned(
                validation_type,
                "length and email validation currently require `String` or `Option<String>`",
            ));
        }

        let required = optional.is_none();
        descriptors.push(quote! {
            ::blazingly::FieldDescriptor::new(
                #public_name,
                #required,
                <#field_type as ::blazingly::ApiSchema>::type_descriptor(),
                ::std::vec![#(#rule_descriptors),*],
            )
        });

        let checks = validation_checks(identifier, &public_name, &rules);
        if optional.is_some() {
            validations.push(quote! {
                if let ::core::option::Option::Some(value) = &self.#identifier {
                    #checks
                }
            });
        } else {
            validations.push(quote! {
                {
                    let value = &self.#identifier;
                    #checks
                }
            });
        }
    }

    let model_name = &model.ident;
    let serde_rename = arguments
        .rename_all
        .map_or_else(|| quote!(), |rename| quote!(#[serde(rename_all = #rename)]));

    Ok(quote! {
        #[derive(
            ::blazingly::__private::serde::Serialize,
            ::blazingly::__private::serde::Deserialize
        )]
        #[serde(crate = "::blazingly::__private::serde")]
        #serde_rename
        #model

        impl ::blazingly::ApiModel for #model_name {
            fn model_descriptor() -> ::blazingly::ModelDescriptor {
                ::blazingly::ModelDescriptor::new(
                    stringify!(#model_name),
                    ::std::vec![#(#descriptors),*],
                )
            }

            fn validate(
                &self,
            ) -> ::core::result::Result<(), ::blazingly::ValidationErrors> {
                let mut errors = ::blazingly::ValidationErrors::new();
                #(#validations)*

                if errors.is_empty() {
                    ::core::result::Result::Ok(())
                } else {
                    ::core::result::Result::Err(errors)
                }
            }
        }
    })
}

fn model_rename_rule(arguments: &ModelArgs) -> syn::Result<String> {
    let rename_rule = arguments
        .rename_all
        .as_ref()
        .map_or_else(|| "none".to_owned(), LitStr::value);
    if matches!(rename_rule.as_str(), "none" | "camelCase") {
        return Ok(rename_rule);
    }

    Err(syn::Error::new(
        arguments
            .rename_all
            .as_ref()
            .map_or_else(proc_macro2::Span::call_site, LitStr::span),
        "the first milestone supports only `rename_all = \"camelCase\"`",
    ))
}

fn take_field_rules(attributes: &mut Vec<Attribute>) -> syn::Result<FieldRules> {
    let mut retained = Vec::new();
    let mut rules = FieldRules::default();

    for attribute in attributes.drain(..) {
        if attribute.path().is_ident("min_length") {
            let value = attribute.parse_args::<LitInt>()?;
            rules.min_length = Some((value.base10_parse()?, value.span()));
        } else if attribute.path().is_ident("max_length") {
            let value = attribute.parse_args::<LitInt>()?;
            rules.max_length = Some((value.base10_parse()?, value.span()));
        } else if attribute.path().is_ident("email") {
            rules.email = true;
        } else {
            retained.push(attribute);
        }
    }

    if let (Some((minimum, span)), Some((maximum, _))) = (rules.min_length, rules.max_length)
        && minimum > maximum
    {
        return Err(syn::Error::new(
            span,
            "`min_length` cannot be greater than `max_length`",
        ));
    }

    *attributes = retained;
    Ok(rules)
}

fn validation_checks(
    _identifier: &Ident,
    public_name: &LitStr,
    rules: &FieldRules,
) -> proc_macro2::TokenStream {
    let minimum = rules.min_length.map(|(minimum, _)| {
        quote! {
            if value.chars().count() < #minimum {
                errors.push(
                    #public_name,
                    "min_length",
                    ::std::format!("must contain at least {} characters", #minimum),
                );
            }
        }
    });
    let maximum = rules.max_length.map(|(maximum, _)| {
        quote! {
            if value.chars().count() > #maximum {
                errors.push(
                    #public_name,
                    "max_length",
                    ::std::format!("must contain at most {} characters", #maximum),
                );
            }
        }
    });
    let email = rules.email.then(|| {
        quote! {
            if !::blazingly::is_email(value) {
                errors.push(
                    #public_name,
                    "email",
                    "must be a valid email address",
                );
            }
        }
    });

    quote! {
        #minimum
        #maximum
        #email
    }
}

fn is_string_type(ty: &Type) -> bool {
    let Type::Path(path) = ty else {
        return false;
    };
    path.path
        .segments
        .last()
        .is_some_and(|segment| segment.ident == "String")
}

fn snake_to_camel(value: &str) -> String {
    let mut output = String::new();
    let mut uppercase = false;

    for character in value.chars() {
        if character == '_' {
            uppercase = true;
        } else if uppercase {
            output.extend(character.to_uppercase());
            uppercase = false;
        } else {
            output.push(character);
        }
    }

    output
}

fn take_mcp_arguments(attributes: &mut Vec<Attribute>) -> syn::Result<Option<McpArgs>> {
    let Some(index) = attributes.iter().position(|attribute| {
        let segments = &attribute.path().segments;
        segments.len() == 2 && segments[0].ident == "mcp" && segments[1].ident == "tool"
    }) else {
        return Ok(None);
    };

    let attribute = attributes.remove(index);
    attribute.parse_args().map(Some)
}

fn take_security_arguments(attributes: &mut Vec<Attribute>) -> syn::Result<Vec<SecurityArgs>> {
    let mut parsed = Vec::new();
    let mut retained = Vec::with_capacity(attributes.len());
    for attribute in attributes.drain(..) {
        if attribute.path().is_ident("security") {
            parsed.push(attribute.parse_args()?);
        } else {
            retained.push(attribute);
        }
    }
    *attributes = retained;
    Ok(parsed)
}

fn mcp_projection(
    arguments: Option<McpArgs>,
    function_name: &Ident,
    summary: &LitStr,
) -> syn::Result<proc_macro2::TokenStream> {
    let Some(arguments) = arguments else {
        return Ok(quote!(descriptor));
    };

    let name = arguments
        .name
        .unwrap_or_else(|| LitStr::new(&function_name.to_string(), function_name.span()));
    let description = arguments.description.unwrap_or_else(|| summary.clone());
    let risk = enum_variant(
        arguments.risk.as_ref(),
        "read",
        &[
            ("read", quote!(::blazingly::OperationRisk::Read)),
            ("write", quote!(::blazingly::OperationRisk::Write)),
            (
                "destructive",
                quote!(::blazingly::OperationRisk::Destructive),
            ),
        ],
        "risk",
    )?;
    let confirmation = enum_variant(
        arguments.confirmation.as_ref(),
        "never",
        &[
            ("never", quote!(::blazingly::Confirmation::Never)),
            ("required", quote!(::blazingly::Confirmation::Required)),
        ],
        "confirmation",
    )?;
    let exposure = enum_variant(
        arguments.expose_output.as_ref(),
        "full",
        &[
            ("full", quote!(::blazingly::OutputExposure::Full)),
            (
                "summary_only",
                quote!(::blazingly::OutputExposure::SummaryOnly),
            ),
            ("none", quote!(::blazingly::OutputExposure::None)),
        ],
        "expose_output",
    )?;
    let idempotent = arguments
        .idempotent
        .map_or_else(|| quote!(false), |value| quote!(#value));

    Ok(quote! {
        descriptor.with_mcp_tool(
            ::blazingly::McpToolDescriptor::new(#name, #description)
                .with_output_exposure(#exposure),
            ::blazingly::AgentPolicy {
                risk: #risk,
                confirmation: #confirmation,
                idempotent: #idempotent,
            },
        )
    })
}

fn enum_variant(
    value: Option<&LitStr>,
    default: &str,
    variants: &[(&str, proc_macro2::TokenStream)],
    key: &str,
) -> syn::Result<proc_macro2::TokenStream> {
    let selected = value.map_or_else(|| default.to_owned(), LitStr::value);
    variants
        .iter()
        .find(|(name, _)| *name == selected)
        .map(|(_, tokens)| tokens.clone())
        .ok_or_else(|| {
            let message = format!("unsupported `{key}` value `{selected}`");
            let span = value.map_or_else(proc_macro2::Span::call_site, LitStr::span);
            syn::Error::new(span, message)
        })
}

fn operation_inputs(
    inputs: &syn::punctuated::Punctuated<FnArg, Token![,]>,
) -> syn::Result<Vec<OperationInput>> {
    let mut operation_inputs = Vec::new();
    let mut body_inputs = 0;

    for input in inputs {
        let FnArg::Typed(PatType { pat, ty, .. }) = input else {
            return Err(syn::Error::new_spanned(
                input,
                "methods with a `self` receiver are not supported",
            ));
        };
        let name = operation_argument_name(pat)?;
        let (kind, inner) = OperationInputKind::from_type(ty)
            .unwrap_or_else(|| (OperationInputKind::DirectDependency, (**ty).clone()));
        if matches!(
            kind,
            OperationInputKind::Json
                | OperationInputKind::Form
                | OperationInputKind::Multipart
                | OperationInputKind::File
        ) {
            body_inputs += 1;
            if body_inputs > 1 {
                return Err(syn::Error::new_spanned(
                    ty,
                    "an operation may declare only one body extractor",
                ));
            }
        }
        let required = wrapper_inner(&inner, "Option").is_none();
        if matches!(kind, OperationInputKind::Path) && !required {
            return Err(syn::Error::new_spanned(
                ty,
                "Path<T> arguments are always required and cannot wrap Option<T>",
            ));
        }
        operation_inputs.push(OperationInput {
            name: LitStr::new(&name.to_string(), name.span()),
            kind,
            argument_type: (**ty).clone(),
            inner,
            required,
        });
    }

    Ok(operation_inputs)
}

fn operation_argument_name(pattern: &Pat) -> syn::Result<&Ident> {
    match pattern {
        Pat::Ident(pattern) => Ok(&pattern.ident),
        Pat::TupleStruct(pattern) if pattern.elems.len() == 1 => {
            let Some(Pat::Ident(pattern)) = pattern.elems.first() else {
                return Err(syn::Error::new_spanned(
                    pattern,
                    "extractor tuple patterns must contain one identifier",
                ));
            };
            Ok(&pattern.ident)
        }
        _ => Err(syn::Error::new_spanned(
            pattern,
            "operation arguments require an identifier or `Extractor(identifier)` pattern",
        )),
    }
}

impl OperationInputKind {
    fn from_type(ty: &Type) -> Option<(Self, Type)> {
        [
            (Self::Path, "Path"),
            (Self::Query, "Query"),
            (Self::Header, "Header"),
            (Self::Cookie, "Cookie"),
            (Self::Json, "Json"),
            (Self::Form, "Form"),
            (Self::Multipart, "Multipart"),
            (Self::File, "File"),
            (Self::Dependency, "Depends"),
        ]
        .into_iter()
        .find_map(|(kind, wrapper)| wrapper_inner(ty, wrapper).map(|inner| (kind, inner)))
    }

    fn source_tokens(self) -> Option<proc_macro2::TokenStream> {
        match self {
            Self::Path => Some(quote!(::blazingly::InputSource::Path)),
            Self::Query => Some(quote!(::blazingly::InputSource::Query)),
            Self::Header => Some(quote!(::blazingly::InputSource::Header)),
            Self::Cookie => Some(quote!(::blazingly::InputSource::Cookie)),
            Self::Json => Some(quote!(::blazingly::InputSource::Json)),
            Self::Form => Some(quote!(::blazingly::InputSource::Form)),
            Self::Multipart => Some(quote!(::blazingly::InputSource::Multipart)),
            Self::File => Some(quote!(::blazingly::InputSource::File)),
            Self::Dependency | Self::DirectDependency => None,
        }
    }

    const fn is_dependency(self) -> bool {
        matches!(self, Self::Dependency | Self::DirectDependency)
    }
}

fn operation_output(output: &ReturnType) -> syn::Result<OperationOutput> {
    let ReturnType::Type(_, ty) = output else {
        return Err(syn::Error::new_spanned(
            output,
            "an explicit typed response is required",
        ));
    };

    if let Some((success, error)) = result_types(ty) {
        let (status, success) = success_output(&success)?;
        return Ok(OperationOutput {
            status,
            success,
            error: Some(error),
        });
    }
    let (status, success) = success_output(ty)?;
    Ok(OperationOutput {
        status,
        success,
        error: None,
    })
}

fn success_output(ty: &Type) -> syn::Result<(u16, Option<Type>)> {
    if type_is(ty, "NoContent") {
        return Ok((204, None));
    }
    if let Some(inner) = wrapper_inner(ty, "WithHeaders") {
        return success_output(&inner);
    }
    if let Some((status, inner)) = status_wrapper(ty)? {
        let (_, body) = success_output(&inner)?;
        if matches!(status, 204 | 304) && body.is_some() {
            return Err(syn::Error::new_spanned(
                ty,
                "HTTP status 204 and 304 responses cannot contain a body",
            ));
        }
        return Ok((status, body));
    }
    if let Some(inner) = wrapper_inner(ty, "Accepted") {
        return Ok((202, Some(inner)));
    }
    if let Some(inner) = wrapper_inner(ty, "Created") {
        return Ok((201, Some(inner)));
    }
    if let Some(inner) = wrapper_inner(ty, "Json") {
        return Ok((200, Some(inner)));
    }
    Ok((200, Some(ty.clone())))
}

fn status_wrapper(ty: &Type) -> syn::Result<Option<(u16, Type)>> {
    let Type::Path(TypePath { path, .. }) = ty else {
        return Ok(None);
    };
    let Some(segment) = path.segments.last() else {
        return Ok(None);
    };
    if segment.ident != "Status" {
        return Ok(None);
    }
    let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return Err(syn::Error::new_spanned(
            ty,
            "Status requires `Status<CODE, Response>`",
        ));
    };
    let mut arguments = arguments.args.iter();
    let Some(syn::GenericArgument::Const(syn::Expr::Lit(status))) = arguments.next() else {
        return Err(syn::Error::new_spanned(
            ty,
            "Status code must be an integer literal",
        ));
    };
    let syn::Lit::Int(status) = &status.lit else {
        return Err(syn::Error::new_spanned(
            status,
            "Status code must be an integer literal",
        ));
    };
    let status = status.base10_parse::<u16>()?;
    if !(200..=399).contains(&status) {
        return Err(syn::Error::new_spanned(
            ty,
            "typed success status must be between 200 and 399",
        ));
    }
    let Some(syn::GenericArgument::Type(inner)) = arguments.next() else {
        return Err(syn::Error::new_spanned(
            ty,
            "Status requires an inner typed response",
        ));
    };
    Ok(Some((status, inner.clone())))
}

fn type_is(ty: &Type, expected: &str) -> bool {
    let Type::Path(TypePath { path, .. }) = ty else {
        return false;
    };
    path.segments
        .last()
        .is_some_and(|segment| segment.ident == expected)
}

fn result_types(ty: &Type) -> Option<(Type, Type)> {
    let Type::Path(TypePath { path, .. }) = ty else {
        return None;
    };
    let segment = path.segments.last()?;
    if segment.ident != "Result" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };
    let mut types = arguments.args.iter().filter_map(|argument| {
        if let syn::GenericArgument::Type(ty) = argument {
            Some(ty.clone())
        } else {
            None
        }
    });
    Some((types.next()?, types.next()?))
}

fn wrapper_inner(ty: &Type, wrapper: &str) -> Option<Type> {
    let Type::Path(TypePath { path, .. }) = ty else {
        return None;
    };
    let segment = path.segments.last()?;
    if segment.ident != wrapper {
        return None;
    }
    let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };
    let syn::GenericArgument::Type(inner) = arguments.args.first()? else {
        return None;
    };
    Some(inner.clone())
}
