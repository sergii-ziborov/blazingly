#![forbid(unsafe_code)]

use core::fmt::Write as _;
use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::{
    Attribute, Fields, FnArg, Ident, ItemEnum, ItemFn, ItemStruct, LitBool, LitInt, LitStr, Pat,
    PatType, Path as SynPath, ReturnType, Token, Type, TypePath, bracketed, parse_macro_input,
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
    validator: Option<SynPath>,
    /// Present when `#[api_model(borrowed)]` selected the output-only form.
    borrowed: Option<Ident>,
}

impl Parse for ModelArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut arguments = Self::default();

        while !input.is_empty() {
            let key = input.parse::<Ident>()?;

            match key.to_string().as_str() {
                // A bare flag, not a `key = value` pair: a view either borrows
                // or it does not.
                "borrowed" => {
                    if arguments.borrowed.is_some() {
                        return Err(syn::Error::new(
                            key.span(),
                            "`borrowed` was specified twice",
                        ));
                    }
                    arguments.borrowed = Some(key);
                }
                "rename_all" => {
                    input.parse::<Token![=]>()?;
                    if arguments.rename_all.is_some() {
                        return Err(syn::Error::new(
                            key.span(),
                            "`rename_all` was specified twice",
                        ));
                    }
                    arguments.rename_all = Some(input.parse::<LitStr>()?);
                }
                "validate_with" => {
                    input.parse::<Token![=]>()?;
                    if arguments.validator.is_some() {
                        return Err(syn::Error::new(
                            key.span(),
                            "only one model `validate_with` function may be declared",
                        ));
                    }
                    arguments.validator = Some(input.parse::<SynPath>()?);
                }
                _ => {
                    return Err(syn::Error::new(
                        key.span(),
                        "the supported model options are `borrowed`, `rename_all`, \
                         and `validate_with`",
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

/// A declarative numeric bound written as an attribute literal.
#[derive(Clone, Copy, Debug, PartialEq)]
enum NumericLiteral {
    Integer(i128),
    Float(f64),
}

impl NumericLiteral {
    fn tokens(self) -> proc_macro2::TokenStream {
        match self {
            Self::Integer(value) => {
                let literal = proc_macro2::Literal::i128_suffixed(value);
                quote!(::blazingly::validation::NumericValue::Integer(#literal))
            }
            Self::Float(value) => {
                let literal = proc_macro2::Literal::f64_suffixed(value);
                quote!(::blazingly::validation::NumericValue::Float(#literal))
            }
        }
    }

    /// The bare literal, so the field type it is written for infers itself.
    fn literal_tokens(self) -> proc_macro2::TokenStream {
        match self {
            Self::Integer(value) => {
                let literal = proc_macro2::Literal::i128_unsuffixed(value);
                quote!(#literal)
            }
            Self::Float(value) => {
                let literal = proc_macro2::Literal::f64_unsuffixed(value);
                quote!(#literal)
            }
        }
    }

    fn encoded(self) -> String {
        match self {
            Self::Integer(value) => value.to_string(),
            Self::Float(value) if value.fract() == 0.0 => format!("{value:.1}"),
            Self::Float(value) => value.to_string(),
        }
    }

    // Comparing a mixed integer and float pair follows JSON Schema semantics.
    #[allow(clippy::cast_precision_loss)]
    fn widened(self) -> f64 {
        match self {
            Self::Integer(value) => value as f64,
            Self::Float(value) => value,
        }
    }

    fn exceeds(self, other: Self) -> bool {
        if let (Self::Integer(left), Self::Integer(right)) = (self, other) {
            return left > right;
        }
        self.widened() > other.widened()
    }

    fn is_zero(self) -> bool {
        match self {
            Self::Integer(value) => value == 0,
            Self::Float(value) => value == 0.0,
        }
    }
}

/// A field default written as an attribute literal.
///
/// The literal is emitted twice: once as the body of the serde default so the
/// handler never sees an absent field, and once as JSON in the descriptor so a
/// schema projection can state the value a client may omit.
#[derive(Clone)]
enum DefaultLiteral {
    Text(LitStr),
    Number(NumericLiteral),
    Boolean(LitBool),
}

impl DefaultLiteral {
    fn expression(&self) -> proc_macro2::TokenStream {
        match self {
            Self::Text(value) => quote!(::std::string::String::from(#value)),
            Self::Number(value) => value.literal_tokens(),
            Self::Boolean(value) => quote!(#value),
        }
    }

    /// The JSON form recorded in the descriptor.
    fn encoded(&self) -> String {
        match self {
            Self::Text(value) => json_string(&value.value()),
            Self::Number(value) => value.encoded(),
            Self::Boolean(value) => value.value().to_string(),
        }
    }

    /// What the literal is, and the field it can be written on.
    const fn expectation(&self) -> (&'static str, &'static str) {
        match self {
            Self::Text(_) => ("a string literal", "a `String` field"),
            Self::Number(NumericLiteral::Integer(_)) => {
                ("an integer literal", "an integer or floating-point field")
            }
            Self::Number(NumericLiteral::Float(_)) => {
                ("a floating-point literal", "a floating-point field")
            }
            Self::Boolean(_) => ("a boolean literal", "a `bool` field"),
        }
    }
}

/// Encodes a Rust string as a JSON string literal.
fn json_string(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len() + 2);
    encoded.push('"');
    for character in value.chars() {
        match character {
            '"' => encoded.push_str("\\\""),
            '\\' => encoded.push_str("\\\\"),
            '\n' => encoded.push_str("\\n"),
            '\r' => encoded.push_str("\\r"),
            '\t' => encoded.push_str("\\t"),
            control if control < ' ' => {
                let _ = write!(encoded, "\\u{:04x}", control as u32);
            }
            other => encoded.push(other),
        }
    }
    encoded.push('"');
    encoded
}

/// The syntactic value shape a field's declarative rules are checked against.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FieldShape {
    Text,
    Integer,
    Float,
    Collection,
    Other,
}

impl FieldShape {
    const fn is_numeric(self) -> bool {
        matches!(self, Self::Integer | Self::Float)
    }

    const fn may_be_model(self) -> bool {
        matches!(self, Self::Collection | Self::Other)
    }
}

#[derive(Default)]
struct FieldRules {
    min_length: Option<(usize, proc_macro2::Span)>,
    max_length: Option<(usize, proc_macro2::Span)>,
    email: Option<proc_macro2::Span>,
    aliases: Vec<LitStr>,
    validator: Option<SynPath>,
    nested: bool,
    minimum: Option<(NumericLiteral, proc_macro2::Span)>,
    maximum: Option<(NumericLiteral, proc_macro2::Span)>,
    exclusive_minimum: Option<(NumericLiteral, proc_macro2::Span)>,
    exclusive_maximum: Option<(NumericLiteral, proc_macro2::Span)>,
    multiple_of: Option<(NumericLiteral, proc_macro2::Span)>,
    pattern: Option<LitStr>,
    min_items: Option<(usize, proc_macro2::Span)>,
    max_items: Option<(usize, proc_macro2::Span)>,
    unique_items: Option<proc_macro2::Span>,
    default: Option<(DefaultLiteral, proc_macro2::Span)>,
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
    Stream,
    WebSocket,
    Extension,
    Extract,
    Dependency,
    DirectDependency,
}

struct OperationInput {
    name: LitStr,
    kind: OperationInputKind,
    argument_type: Type,
    inner: Type,
    required: bool,
    /// Set when the handler wrote `&Depends<T>` or `&T`.
    ///
    /// A dependency taken by reference is what lets a handler return a borrowed
    /// view over it: the view's lifetime is the argument's, and the response is
    /// encoded before the argument goes out of scope.
    by_reference: bool,
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

/// Declares an API model.
///
/// The default form owns its data: it derives `Serialize` and `Deserialize`,
/// implements `ApiModel`, and runs every declared field rule before the handler
/// sees the value.
///
/// `#[api_model(borrowed)]` declares the output-only form instead. A borrowed
/// view derives `Serialize` and implements `ApiSchema` directly; it gains no
/// `Deserialize` impl and no validation, because a response body is produced by
/// the operation rather than parsed from a client. Only the borrowed form may
/// carry lifetime and type parameters, so one `Page<'store, T>` describes every
/// paginated response instead of one envelope per item type.
///
/// ```ignore
/// #[api_model(borrowed)]
/// struct SummaryView<'store> {
///     title: &'store str,
///     tags: Vec<&'store TagRef>,
/// }
/// ```
///
/// A one-field tuple struct declares a *value type*: a bundle of field rules
/// named once and applied by every field declared with it.
///
/// ```ignore
/// #[api_model]
/// #[min_length(8)]
/// #[max_length(200)]
/// struct Title(String);
/// ```
///
/// A unit-variant enum declares a closed set of strings, schema included.
///
/// ```ignore
/// #[api_model(rename_all = "lowercase")]
/// enum Language {
///     Uk,
///     Ru,
///     En,
/// }
/// ```
#[proc_macro_attribute]
pub fn api_model(arguments: TokenStream, item: TokenStream) -> TokenStream {
    let arguments = parse_macro_input!(arguments as ModelArgs);
    let mut model = parse_macro_input!(item as syn::Item);

    match api_model_tokens(&arguments, &mut model) {
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
    let asynchronous = function.sig.asyncness.is_some();
    let mcp = take_mcp_arguments(&mut function.attrs)?;
    let security = take_security_arguments(&mut function.attrs)?;
    let inputs = operation_inputs(&function.sig.inputs)?;
    if mcp.is_some()
        && let Some(stream) = inputs
            .iter()
            .find(|input| matches!(input.kind, OperationInputKind::Stream))
    {
        return Err(syn::Error::new_spanned(
            &stream.argument_type,
            "streaming request bodies are HTTP-only and cannot be exposed as an MCP tool",
        ));
    }
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
            // A borrowed response is written `Json<PageView<'_>>`, and an
            // elided lifetime has nothing to be inferred from in the
            // descriptor's body. The schema is the same at every lifetime, so
            // it is asked for at `'static`.
            let success = documented_type(success);
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
    let executable = operation_executable(&inputs, function_name, asynchronous);

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

/// Everything both handler shapes need: the argument prologue, the compiled
/// dependency requests, and the call itself.
struct ExecutableParts {
    extracted_arguments: Vec<proc_macro2::TokenStream>,
    dependency_requests: Vec<proc_macro2::TokenStream>,
    call: proc_macro2::TokenStream,
    input_binding: proc_macro2::TokenStream,
    dependency_binding: proc_macro2::TokenStream,
}

fn operation_executable(
    inputs: &[OperationInput],
    function_name: &Ident,
    asynchronous: bool,
) -> proc_macro2::TokenStream {
    let parts = executable_parts(inputs, function_name);
    if asynchronous {
        asynchronous_executable(&parts)
    } else {
        synchronous_executable(&parts)
    }
}

fn executable_parts(inputs: &[OperationInput], function_name: &Ident) -> ExecutableParts {
    let mut dependency_index = 0_usize;
    let extracted_arguments = inputs
        .iter()
        .enumerate()
        .map(|(index, input)| {
            let binding = format_ident!("__blazingly_argument_{index}");
            if input.kind.is_dependency() {
                let inner = &input.inner;
                let index = dependency_index;
                dependency_index += 1;
                // A dependency taken by reference is read through the handle
                // rather than out of it, so `&Depends<T>` and `&T` share one
                // extraction and neither clones `T`.
                if input.by_reference || matches!(input.kind, OperationInputKind::Dependency) {
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
        })
        .collect::<Vec<_>>();
    let dependency_requests = inputs
        .iter()
        .filter(|input| input.kind.is_dependency())
        .map(|input| {
            let inner = &input.inner;
            quote!(::blazingly::DependencyRequest::of::<#inner>())
        })
        .collect::<Vec<_>>();
    let handler_arguments = inputs
        .iter()
        .enumerate()
        .map(|(index, input)| {
            let binding = format_ident!("__blazingly_argument_{index}");
            if input.by_reference {
                // `&Depends<T>` is passed straight through; `&T` reaches the
                // same place through the handle's `Deref`.
                quote!(&#binding)
            } else {
                quote!(#binding)
            }
        })
        .collect::<Vec<_>>();
    let call = quote!(super::#function_name(#(#handler_arguments),*));
    // Naming a closure parameter that nothing reads is a warning, and CI
    // rejects warnings.
    let input_binding = if inputs.iter().any(|input| !input.kind.is_dependency()) {
        quote!(input)
    } else {
        quote!(_)
    };
    let dependency_binding = if dependency_requests.is_empty() {
        quote!(_)
    } else {
        quote!(dependencies)
    };

    ExecutableParts {
        extracted_arguments,
        dependency_requests,
        call,
        input_binding,
        dependency_binding,
    }
}

fn asynchronous_executable(parts: &ExecutableParts) -> proc_macro2::TokenStream {
    let ExecutableParts {
        extracted_arguments,
        dependency_requests,
        call,
        input_binding,
        dependency_binding,
    } = parts;
    quote! {
        ::blazingly::ExecutableOperation::typed_with_dependencies(
            descriptor(),
            ::std::vec![#(#dependency_requests),*],
            |#input_binding, #dependency_binding| {
                #(#extracted_arguments)*
                ::core::result::Result::Ok(
                    ::std::boxed::Box::pin(async move {
                        let output = #call.await;
                        ::blazingly::OperationOutput::into_execution_outcome(output)
                    }) as ::blazingly::OperationFuture
                )
            },
        )
    }
}

/// A handler that is not `async` completes when it is called, so its outcome is
/// produced without a future.
///
/// The fallback exists because `ExecutableOperation` still needs one when
/// plugin hooks, cancellation, or request-scoped finalizers wrap the operation.
/// It runs the same body at the same point in the pipeline an async handler
/// would, so the two paths cannot observe different hook ordering.
fn synchronous_executable(parts: &ExecutableParts) -> proc_macro2::TokenStream {
    let ExecutableParts {
        extracted_arguments,
        dependency_requests,
        call,
        input_binding,
        dependency_binding,
    } = parts;
    quote! {
        ::blazingly::ExecutableOperation::typed_sync_with_dependencies(
            descriptor(),
            ::std::vec![#(#dependency_requests),*],
            |#input_binding, #dependency_binding| {
                #(#extracted_arguments)*
                ::core::result::Result::Ok(
                    ::blazingly::OperationOutput::into_execution_outcome(#call)
                )
            },
            |#input_binding, #dependency_binding| {
                #(#extracted_arguments)*
                ::core::result::Result::Ok(
                    ::std::boxed::Box::pin(async move {
                        ::blazingly::OperationOutput::into_execution_outcome(#call)
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
                let details = ::blazingly::__private::blazingly_json::to_vec(&payload)
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

#[allow(clippy::too_many_lines)]
fn api_model_tokens(
    arguments: &ModelArgs,
    item: &mut syn::Item,
) -> syn::Result<proc_macro2::TokenStream> {
    match item {
        syn::Item::Struct(model) => model_tokens(arguments, model),
        syn::Item::Enum(model) => enum_model_tokens(arguments, model),
        other => Err(syn::Error::new_spanned(
            other,
            "`#[api_model]` describes a struct, a one-field value type, or a \
             unit-variant enum",
        )),
    }
}

fn model_tokens(
    arguments: &ModelArgs,
    model: &mut ItemStruct,
) -> syn::Result<proc_macro2::TokenStream> {
    if matches!(model.fields, Fields::Unnamed(_)) {
        return value_model_tokens(arguments, model);
    }
    if arguments.borrowed.is_some() {
        return borrowed_model_tokens(arguments, model);
    }
    owned_model_tokens(arguments, model)
}

/// Expands the value form: one named bundle of field rules, applied by type.
///
/// `Title` is declared once with the rules written exactly as they are on a
/// field, and every model that declares a `Title` field inherits them — into the
/// descriptor through [`ApiConstrained::constraint_rules`], and into validation
/// through [`ApiConstrained::validate_constraints`]. A newtype was chosen over a
/// rule alias because a proc macro cannot see a declaration from another
/// expansion: only the type system carries a name across item boundaries.
fn value_model_tokens(
    arguments: &ModelArgs,
    model: &mut ItemStruct,
) -> syn::Result<proc_macro2::TokenStream> {
    let inner = value_type_inner(arguments, model)?;
    let mut rules = take_field_rules(&mut model.attrs)?;
    reject_field_only_rules(&rules, &model.ident)?;

    let shape = field_shape(&inner);
    reject_incompatible_rules(&rules, &inner, shape)?;
    normalize_collection_rules(&mut rules, shape);

    let model_name = &model.ident;
    // A value type has no field name of its own: the model that declares the
    // field supplies one when it merges these violations.
    let anonymous = LitStr::new("", model_name.span());
    let declared_rules = rule_descriptors(&rules, false);
    let checks = field_checks(&rules, shape, &anonymous);
    let custom = rules.validator.as_ref().map(|validator| {
        quote! {
            if let ::core::result::Result::Err(custom_errors) = #validator(value) {
                ::blazingly::merge_field_validation_errors(
                    &mut errors,
                    #anonymous,
                    &custom_errors,
                );
            }
        }
    });

    Ok(quote! {
        #[derive(
            ::blazingly::__private::serde::Serialize,
            ::blazingly::__private::serde::Deserialize
        )]
        #[serde(crate = "::blazingly::__private::serde")]
        #[serde(transparent)]
        #model

        impl #model_name {
            /// Wraps a value without checking it; the declared rules run when a
            /// request is validated.
            #[must_use]
            pub fn new(value: #inner) -> Self {
                Self(value)
            }

            /// The wrapped value.
            #[must_use]
            pub const fn as_inner(&self) -> &#inner {
                &self.0
            }

            /// Unwraps the value.
            #[must_use]
            pub fn into_inner(self) -> #inner {
                self.0
            }
        }

        const _: () = {
            impl ::blazingly::ApiSchema for #model_name {
                fn type_descriptor() -> ::blazingly::TypeDescriptor {
                    let mut descriptor =
                        <#inner as ::blazingly::ApiSchema>::type_descriptor();
                    descriptor.rust_name = ::std::string::String::from(
                        stringify!(#model_name)
                    );
                    // Carried on the type, not only inherited by the field that
                    // uses it: a `Vec<#model_name>` item has no field name.
                    descriptor.constraints.extend(
                        <Self as ::blazingly::ApiConstrained>::constraint_rules(),
                    );
                    descriptor
                }

                fn validate_input(
                    &self,
                ) -> ::core::result::Result<(), ::blazingly::ValidationErrors> {
                    <Self as ::blazingly::ApiConstrained>::validate_constraints(self)
                }
            }

            impl ::blazingly::ApiConstrained for #model_name {
                fn constraint_rules() -> ::std::vec::Vec<::blazingly::ValidationRule> {
                    ::std::vec![#(#declared_rules),*]
                }

                fn validate_constraints(
                    &self,
                ) -> ::core::result::Result<(), ::blazingly::ValidationErrors> {
                    let mut errors = ::blazingly::ValidationErrors::new();
                    {
                        let value = &self.0;
                        #checks
                        #custom
                    }

                    if errors.is_empty() {
                        ::core::result::Result::Ok(())
                    } else {
                        ::core::result::Result::Err(errors)
                    }
                }
            }
        };
    })
}

/// Resolves the one type a value type wraps, rejecting every other shape.
fn value_type_inner(arguments: &ModelArgs, model: &ItemStruct) -> syn::Result<Type> {
    let Fields::Unnamed(fields) = &model.fields else {
        unreachable!("the value form is selected by an unnamed field list");
    };
    if fields.unnamed.len() != 1 {
        return Err(syn::Error::new_spanned(
            &model.fields,
            "a value type wraps exactly one field; declare a struct with named \
             fields for anything else",
        ));
    }
    if let Some(borrowed) = &arguments.borrowed {
        return Err(syn::Error::new_spanned(
            borrowed,
            "a value type is validated wherever it appears, which a borrowed \
             output view never is",
        ));
    }
    if let Some(rename) = &arguments.rename_all {
        return Err(syn::Error::new_spanned(
            rename,
            "`rename_all` renames fields, and a value type has none",
        ));
    }
    if let Some(validator) = &arguments.validator {
        return Err(syn::Error::new_spanned(
            validator,
            "declare `#[validate_with(...)]` beside the other rules; a value type \
             has no cross-field check to run",
        ));
    }
    reject_owned_generics(&model.generics)?;

    let inner = fields.unnamed[0].ty.clone();
    if wrapper_inner(&inner, "Option").is_some() {
        return Err(syn::Error::new_spanned(
            &inner,
            "a value type wraps the value its rules describe; declare the field \
             that uses it as `Option<_>` instead",
        ));
    }
    Ok(inner)
}

/// Rejects the rules that only mean something on a field of a model.
fn reject_field_only_rules(rules: &FieldRules, model_name: &Ident) -> syn::Result<()> {
    if let Some(alias) = rules.aliases.first() {
        return Err(syn::Error::new_spanned(
            alias,
            "`alias` names an extra wire key for one field, which a value type \
             does not have",
        ));
    }
    if let Some((_, span)) = &rules.default {
        return Err(syn::Error::new(
            *span,
            "a `default` belongs to the field that may be absent, not to the type \
             the field is declared with",
        ));
    }
    if rules.nested {
        return Err(syn::Error::new_spanned(
            model_name,
            "`nested` recurses into a model; a value type validates itself",
        ));
    }
    Ok(())
}

/// Expands the enumeration form: a string schema with a closed variant set.
fn enum_model_tokens(
    arguments: &ModelArgs,
    model: &mut ItemEnum,
) -> syn::Result<proc_macro2::TokenStream> {
    if let Some(borrowed) = &arguments.borrowed {
        return Err(syn::Error::new_spanned(
            borrowed,
            "an enumeration owns its variants and never borrows",
        ));
    }
    if let Some(validator) = &arguments.validator {
        return Err(syn::Error::new_spanned(
            validator,
            "an enumeration accepts its declared variants and nothing else, so \
             there is nothing left to validate",
        ));
    }
    if let Some(parameter) = model.generics.params.first() {
        return Err(syn::Error::new_spanned(
            parameter,
            "an API enumeration is a closed set of strings and cannot be generic",
        ));
    }
    if model.variants.is_empty() {
        return Err(syn::Error::new_spanned(
            &model.ident,
            "an API enumeration needs at least one variant",
        ));
    }

    let wire_names = enum_wire_names(arguments, model)?;
    let identifiers = model
        .variants
        .iter()
        .map(|variant| variant.ident.clone())
        .collect::<Vec<_>>();
    let model_name = &model.ident;
    let encoded = format!(
        "enum={}",
        wire_names
            .iter()
            .map(LitStr::value)
            .collect::<Vec<_>>()
            .join("|")
    );

    Ok(quote! {
        #[derive(
            ::blazingly::__private::serde::Serialize,
            ::blazingly::__private::serde::Deserialize
        )]
        #[serde(crate = "::blazingly::__private::serde")]
        #model

        impl #model_name {
            /// Every accepted wire value, in declaration order.
            pub const VARIANTS: &'static [&'static str] = &[#(#wire_names),*];

            /// The wire value this variant serializes to.
            #[must_use]
            pub const fn as_str(&self) -> &'static str {
                match self {
                    #(Self::#identifiers => #wire_names,)*
                }
            }
        }

        const _: () = {
            impl ::blazingly::ApiSchema for #model_name {
                fn type_descriptor() -> ::blazingly::TypeDescriptor {
                    ::blazingly::TypeDescriptor::scalar(
                        stringify!(#model_name),
                        ::blazingly::SchemaKind::String,
                    )
                    .with_constraints(
                        <Self as ::blazingly::ApiConstrained>::constraint_rules(),
                    )
                }
            }

            impl ::blazingly::ApiConstrained for #model_name {
                fn constraint_rules() -> ::std::vec::Vec<::blazingly::ValidationRule> {
                    ::std::vec![::blazingly::ValidationRule::Custom(#encoded.to_owned())]
                }

                fn validate_constraints(
                    &self,
                ) -> ::core::result::Result<(), ::blazingly::ValidationErrors> {
                    ::core::result::Result::Ok(())
                }
            }
        };
    })
}

/// Resolves every variant's wire value and pins it with `#[serde(rename)]`.
fn enum_wire_names(arguments: &ModelArgs, model: &mut ItemEnum) -> syn::Result<Vec<LitStr>> {
    let rename_rule = enum_rename_rule(arguments)?;
    let mut wire_names: Vec<LitStr> = Vec::new();

    for variant in &mut model.variants {
        if !matches!(variant.fields, Fields::Unit) {
            return Err(syn::Error::new_spanned(
                &variant.fields,
                "an API enumeration projects to a string, so a variant cannot \
                 carry data",
            ));
        }
        let wire = variant_wire_name(variant, rename_rule)?;
        if wire.value().contains('|') {
            return Err(syn::Error::new(
                wire.span(),
                "`|` separates the variants in the recorded schema and cannot \
                 appear in one",
            ));
        }
        if wire_names
            .iter()
            .any(|declared| declared.value() == wire.value())
        {
            return Err(syn::Error::new(
                wire.span(),
                format!("the wire value {:?} is declared twice", wire.value()),
            ));
        }
        variant
            .attrs
            .push(syn::parse_quote!(#[serde(rename = #wire)]));
        wire_names.push(wire);
    }

    Ok(wire_names)
}

/// Resolves one variant's wire value, consuming an explicit `#[rename("...")]`.
fn variant_wire_name(variant: &mut syn::Variant, rule: RenameRule) -> syn::Result<LitStr> {
    let mut explicit = None;
    let mut retained = Vec::new();
    for attribute in variant.attrs.drain(..) {
        if attribute.path().is_ident("rename") {
            if explicit.is_some() {
                return Err(syn::Error::new_spanned(
                    attribute,
                    "only one `rename` may be declared per variant",
                ));
            }
            explicit = Some(attribute.parse_args::<LitStr>()?);
        } else {
            retained.push(attribute);
        }
    }
    variant.attrs = retained;

    Ok(explicit.unwrap_or_else(|| {
        LitStr::new(
            &rule.apply(&variant.ident.to_string()),
            variant.ident.span(),
        )
    }))
}

/// The variant renaming rules an API enumeration understands.
///
/// Every variant is emitted with an explicit `#[serde(rename = "...")]`, so the
/// recorded schema and the wire form cannot drift apart.
#[derive(Clone, Copy)]
enum RenameRule {
    Pascal,
    Lower,
    Upper,
    Camel,
    Snake,
    ScreamingSnake,
    Kebab,
    ScreamingKebab,
}

impl RenameRule {
    const SUPPORTED: &'static str = "`PascalCase`, `lowercase`, `UPPERCASE`, `camelCase`, \
                                     `snake_case`, `SCREAMING_SNAKE_CASE`, `kebab-case`, \
                                     or `SCREAMING-KEBAB-CASE`";

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "PascalCase" => Self::Pascal,
            "lowercase" => Self::Lower,
            "UPPERCASE" => Self::Upper,
            "camelCase" => Self::Camel,
            "snake_case" => Self::Snake,
            "SCREAMING_SNAKE_CASE" => Self::ScreamingSnake,
            "kebab-case" => Self::Kebab,
            "SCREAMING-KEBAB-CASE" => Self::ScreamingKebab,
            _ => return None,
        })
    }

    fn apply(self, variant: &str) -> String {
        match self {
            Self::Pascal => variant.to_owned(),
            Self::Lower => variant.to_ascii_lowercase(),
            Self::Upper => variant.to_ascii_uppercase(),
            Self::Camel => {
                let mut characters = variant.chars();
                characters.next().map_or_else(String::new, |first| {
                    first.to_ascii_lowercase().to_string() + characters.as_str()
                })
            }
            Self::Snake => snake_case(variant),
            Self::ScreamingSnake => snake_case(variant).to_ascii_uppercase(),
            Self::Kebab => snake_case(variant).replace('_', "-"),
            Self::ScreamingKebab => snake_case(variant).to_ascii_uppercase().replace('_', "-"),
        }
    }
}

fn snake_case(variant: &str) -> String {
    let mut output = String::with_capacity(variant.len());
    for (index, character) in variant.char_indices() {
        if index > 0 && character.is_uppercase() {
            output.push('_');
        }
        output.push(character.to_ascii_lowercase());
    }
    output
}

fn enum_rename_rule(arguments: &ModelArgs) -> syn::Result<RenameRule> {
    let Some(rename) = &arguments.rename_all else {
        return Ok(RenameRule::Pascal);
    };
    RenameRule::parse(&rename.value()).ok_or_else(|| {
        syn::Error::new(
            rename.span(),
            format!(
                "an enumeration renames its variants with {}",
                RenameRule::SUPPORTED
            ),
        )
    })
}

/// Expands the owning form: `Serialize`, `Deserialize`, and a validating
/// [`ApiModel`] implementation.
fn owned_model_tokens(
    arguments: &ModelArgs,
    model: &mut ItemStruct,
) -> syn::Result<proc_macro2::TokenStream> {
    let Fields::Named(fields) = &mut model.fields else {
        return Err(syn::Error::new_spanned(
            &model.fields,
            "`#[api_model]` requires a struct with named fields",
        ));
    };
    reject_owned_generics(&model.generics)?;

    let rename_rule = model_rename_rule(arguments)?;
    let model_name = model.ident.clone();
    let OwnedFields {
        descriptors,
        validations,
        defaults,
        needs_probes,
    } = owned_field_tokens(fields, &rename_rule, &model_name)?;

    let serde_rename = arguments
        .rename_all
        .as_ref()
        .map_or_else(|| quote!(), |rename| quote!(#[serde(rename_all = #rename)]));
    let model_validation = arguments.validator.as_ref().map(|validator| {
        quote! {
            if let ::core::result::Result::Err(failure) = #validator(self) {
                ::blazingly::validation::merge_model_violations(&mut errors, failure);
            }
        }
    });
    let probes = if needs_probes {
        nested_probe_definitions()
    } else {
        quote!()
    };

    Ok(quote! {
        #[derive(
            ::blazingly::__private::serde::Serialize,
            ::blazingly::__private::serde::Deserialize
        )]
        #[serde(crate = "::blazingly::__private::serde")]
        #serde_rename
        #model

        #(#defaults)*

        const _: () = {
            #probes

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
                    #model_validation

                    if errors.is_empty() {
                        ::core::result::Result::Ok(())
                    } else {
                        ::core::result::Result::Err(errors)
                    }
                }
            }
        };
    })
}

#[derive(Default)]
struct OwnedFields {
    descriptors: Vec<proc_macro2::TokenStream>,
    validations: Vec<proc_macro2::TokenStream>,
    /// Serde default functions, emitted beside the model rather than inside it
    /// because `#[serde(default = "...")]` names a path in the enclosing scope.
    defaults: Vec<proc_macro2::TokenStream>,
    needs_probes: bool,
}

fn owned_field_tokens(
    fields: &mut syn::FieldsNamed,
    rename_rule: &str,
    model_name: &Ident,
) -> syn::Result<OwnedFields> {
    let mut owned = OwnedFields::default();

    for field in &mut fields.named {
        let identifier = field
            .ident
            .clone()
            .expect("named fields always have identifiers");
        let mut rules = take_field_rules(&mut field.attrs)?;
        for alias in &rules.aliases {
            field
                .attrs
                .push(syn::parse_quote!(#[serde(alias = #alias)]));
        }
        let field_type = field.ty.clone();
        let optional = wrapper_inner(&field_type, "Option");
        let validation_type = optional.as_ref().unwrap_or(&field_type);
        let shape = field_shape(validation_type);
        reject_incompatible_rules(&rules, validation_type, shape)?;
        normalize_collection_rules(&mut rules, shape);
        owned.needs_probes |= shape.may_be_model();

        if let Some((literal, span)) = &rules.default {
            if optional.is_some() {
                return Err(syn::Error::new(
                    *span,
                    "a field with a `default` is never absent from the handler; \
                     declare it without `Option`",
                ));
            }
            let function = default_function_name(model_name, &identifier);
            let expression = literal.expression();
            let path = LitStr::new(&function.to_string(), *span);
            field
                .attrs
                .push(syn::parse_quote!(#[serde(default = #path)]));
            owned.defaults.push(quote! {
                #[doc(hidden)]
                fn #function() -> #field_type {
                    #expression
                }
            });
        }

        let public_name = public_field_name(&identifier, rename_rule);
        let required = optional.is_none() && rules.default.is_none();
        let declared_rules = rule_descriptors(&rules, optional.is_some());
        let inherited_rules = inherited_rule_descriptor(validation_type, shape);
        let nested_descriptor = nested_rule_descriptor(validation_type, shape, rules.nested);
        owned.descriptors.push(quote! {
            ::blazingly::FieldDescriptor::new(
                #public_name,
                #required,
                <#field_type as ::blazingly::ApiSchema>::type_descriptor(),
                {
                    let mut rules = ::std::vec::Vec::new();
                    #inherited_rules
                    #(rules.push(#declared_rules);)*
                    #nested_descriptor
                    rules
                },
            )
        });

        let checks = field_checks(&rules, shape, &public_name);
        let nested_checks = nested_validation_checks(shape, &public_name, &rules);
        if checks.is_empty() && nested_checks.is_empty() {
            continue;
        }
        if optional.is_some() {
            owned.validations.push(quote! {
                if let ::core::option::Option::Some(value) = &self.#identifier {
                    #checks
                    #nested_checks
                }
            });
        } else {
            owned.validations.push(quote! {
                {
                    let value = &self.#identifier;
                    #checks
                    #nested_checks
                }
            });
        }
    }

    Ok(owned)
}

/// Names the serde default function for one field.
///
/// The model name is folded in because two models in one module may both
/// declare a defaulted field of the same name.
fn default_function_name(model_name: &Ident, field: &Ident) -> Ident {
    let model = model_name.to_string().to_lowercase();
    let field = field.to_string();
    let field = field.trim_start_matches("r#");
    format_ident!("__blazingly_default_{model}_{field}")
}

fn public_field_name(identifier: &Ident, rename_rule: &str) -> LitStr {
    let name = if rename_rule == "camelCase" {
        snake_to_camel(&identifier.to_string())
    } else {
        identifier.to_string()
    };
    LitStr::new(&name, identifier.span())
}

/// Expands the borrowed form: `Serialize` plus a direct [`ApiSchema`] impl.
///
/// A borrowed view is an output type. It is produced by the operation rather
/// than parsed from a client, so it gets neither `Deserialize` nor validation,
/// and it may carry lifetime and type parameters that an owning model cannot.
fn borrowed_model_tokens(
    arguments: &ModelArgs,
    model: &mut ItemStruct,
) -> syn::Result<proc_macro2::TokenStream> {
    let Fields::Named(fields) = &mut model.fields else {
        return Err(syn::Error::new_spanned(
            &model.fields,
            "`#[api_model]` requires a struct with named fields",
        ));
    };
    if let Some(validator) = &arguments.validator {
        return Err(syn::Error::new_spanned(
            validator,
            "a borrowed view is an output type and is never validated; declare \
             `validate_with` on the owning model the client sends",
        ));
    }

    let rename_rule = model_rename_rule(arguments)?;
    let mut descriptors = Vec::new();

    for field in &mut fields.named {
        let identifier = field
            .ident
            .clone()
            .expect("named fields always have identifiers");
        reject_borrowed_field_rules(&mut field.attrs)?;

        let public_name = public_field_name(&identifier, &rename_rule);
        let required = wrapper_inner(&field.ty, "Option").is_none();
        // The wire shape a borrowed view prints is the shape its owning
        // counterpart prints, so the documented schema is the field type with
        // its borrows resolved: `&'store str` is a string, `Vec<&'store Tag>`
        // is an array of `Tag`.
        let schema_type = schema_type(&field.ty);
        // A view is never validated, but an optional field still prints `null`,
        // and a reader of the document has no other way to learn that.
        let metadata = if required {
            quote!(::std::vec::Vec::new())
        } else {
            quote!(::std::vec![::blazingly::ValidationRule::Custom(
                "nullable=true".to_owned()
            )])
        };
        descriptors.push(quote! {
            ::blazingly::FieldDescriptor::new(
                #public_name,
                #required,
                <#schema_type as ::blazingly::ApiSchema>::type_descriptor(),
                #metadata,
            )
        });
    }

    let model_name = &model.ident;
    let serde_rename = arguments
        .rename_all
        .as_ref()
        .map_or_else(|| quote!(), |rename| quote!(#[serde(rename_all = #rename)]));
    let (impl_generics, type_generics, where_clause) = model.generics.split_for_impl();
    let schema_bounds = schema_parameter_bounds(&model.generics);
    let where_clause = merge_where_predicates(where_clause, &schema_bounds);
    let descriptor_name = borrowed_descriptor_name(&model.generics, model_name);

    Ok(quote! {
        #[derive(::blazingly::__private::serde::Serialize)]
        #[serde(crate = "::blazingly::__private::serde")]
        #serde_rename
        #model

        const _: () = {
            #[allow(dead_code)]
            fn __blazingly_schema_name(
                base: &str,
                parameters: &[::blazingly::TypeDescriptor],
            ) -> ::std::string::String {
                let mut name = ::std::string::String::from(base);
                for parameter in parameters {
                    name.push('_');
                    // `OpenAPI` component keys accept only `[A-Za-z0-9._-]`,
                    // and `Vec<Tag>` is a perfectly ordinary Rust name.
                    for character in parameter.rust_name.chars() {
                        if character.is_ascii_alphanumeric()
                            || character == '_'
                            || character == '.'
                            || character == '-'
                        {
                            name.push(character);
                        } else {
                            name.push('_');
                        }
                    }
                }
                name
            }

            impl #impl_generics ::blazingly::ApiSchema for #model_name #type_generics
            #where_clause
            {
                fn type_descriptor() -> ::blazingly::TypeDescriptor {
                    ::blazingly::TypeDescriptor::model(
                        ::blazingly::ModelDescriptor::new(
                            #descriptor_name,
                            ::std::vec![#(#descriptors),*],
                        )
                    )
                }
            }
        };
    })
}

/// An owning model is deserialized and validated, and neither survives an
/// unbounded parameter, so generics are the borrowed form's alone.
fn reject_owned_generics(generics: &syn::Generics) -> syn::Result<()> {
    let Some(parameter) = generics.params.first() else {
        return Ok(());
    };
    let reason = match parameter {
        syn::GenericParam::Lifetime(_) => {
            "an owning model deserializes into itself and cannot borrow from the \
             request buffer"
        }
        syn::GenericParam::Type(_) | syn::GenericParam::Const(_) => {
            "field rules cannot recurse into an unbounded parameter, so an owning \
             model would silently skip validating it"
        }
    };
    Err(syn::Error::new_spanned(
        parameter,
        format!("{reason}; declare `#[api_model(borrowed)]` for a generic output view"),
    ))
}

/// Every declarative rule is a request-side check, so none of them mean
/// anything on a view the framework only ever writes.
fn reject_borrowed_field_rules(attributes: &mut Vec<Attribute>) -> syn::Result<()> {
    let mut retained = Vec::new();
    for attribute in attributes.drain(..) {
        if BORROWED_REJECTED_ATTRIBUTES
            .iter()
            .any(|name| attribute.path().is_ident(name))
        {
            let name = attribute
                .path()
                .get_ident()
                .map_or_else(String::new, ToString::to_string);
            let purpose = if name == "default" {
                "fills in an absent request field"
            } else {
                "validates a request"
            };
            return Err(syn::Error::new(
                attribute_span(&attribute),
                format!(
                    "`#[{name}]` {purpose}; a borrowed view is an output type and is \
                     never validated. Declare the rule on the owning model the client \
                     sends"
                ),
            ));
        }
        retained.push(attribute);
    }
    *attributes = retained;
    Ok(())
}

const BORROWED_REJECTED_ATTRIBUTES: &[&str] = &[
    "min_length",
    "max_length",
    "email",
    "alias",
    "validate_with",
    "nested",
    "default",
    "minimum",
    "maximum",
    "exclusive_minimum",
    "exclusive_maximum",
    "multiple_of",
    "pattern",
    "min_items",
    "max_items",
    "unique_items",
];

/// Adds `T: ApiSchema` for every type parameter so the field descriptors can
/// ask each one for its own schema.
fn schema_parameter_bounds(generics: &syn::Generics) -> Vec<syn::WherePredicate> {
    generics
        .params
        .iter()
        .filter_map(|parameter| match parameter {
            syn::GenericParam::Type(parameter) => {
                let identifier = &parameter.ident;
                Some(syn::parse_quote!(#identifier: ::blazingly::ApiSchema))
            }
            syn::GenericParam::Lifetime(_) | syn::GenericParam::Const(_) => None,
        })
        .collect()
}

fn merge_where_predicates(
    existing: Option<&syn::WhereClause>,
    added: &[syn::WherePredicate],
) -> Option<syn::WhereClause> {
    if added.is_empty() {
        return existing.cloned();
    }
    let mut clause = existing.cloned().unwrap_or_else(|| syn::WhereClause {
        where_token: <Token![where]>::default(),
        predicates: syn::punctuated::Punctuated::new(),
    });
    clause.predicates.extend(added.iter().cloned());
    Some(clause)
}

/// One `Page<'store, T>` describes every paginated response, so the documented
/// name has to distinguish `Page<Article>` from `Page<Company>`; an `OpenAPI`
/// projection keys its component schemas by this name.
fn borrowed_descriptor_name(
    generics: &syn::Generics,
    model_name: &Ident,
) -> proc_macro2::TokenStream {
    let parameters = generics
        .params
        .iter()
        .filter_map(|parameter| match parameter {
            syn::GenericParam::Type(parameter) => {
                let identifier = &parameter.ident;
                Some(quote!(<#identifier as ::blazingly::ApiSchema>::type_descriptor()))
            }
            syn::GenericParam::Lifetime(_) | syn::GenericParam::Const(_) => None,
        })
        .collect::<Vec<_>>();
    if parameters.is_empty() {
        return quote!(stringify!(#model_name));
    }
    quote! {
        __blazingly_schema_name(
            stringify!(#model_name),
            &[#(#parameters),*],
        )
    }
}

/// Resolves a written field type to the type whose schema it prints.
///
/// A borrowed view exists to avoid owning its data, so its fields are written
/// as `&'store str`, `Vec<&'store Tag>`, or `Cow<'store, str>`. All three print
/// exactly what their owning counterparts print, and this is where that is
/// stated once instead of by every application that writes a view.
fn schema_type(ty: &Type) -> Type {
    match ty {
        Type::Reference(reference) => borrowed_schema_type(&reference.elem),
        Type::Slice(slice) => vector_of(&schema_type(&slice.elem)),
        Type::Array(array) => vector_of(&schema_type(&array.elem)),
        Type::Paren(inner) => schema_type(&inner.elem),
        Type::Group(inner) => schema_type(&inner.elem),
        Type::Path(path) => path_schema_type(path),
        other => other.clone(),
    }
}

fn borrowed_schema_type(referent: &Type) -> Type {
    // `&str` is the schema the contract implements; `str` alone is not.
    if bare_type_matches(referent, &["str"]) {
        return syn::parse_quote!(&str);
    }
    match referent {
        Type::Slice(slice) => vector_of(&schema_type(&slice.elem)),
        Type::Array(array) => vector_of(&schema_type(&array.elem)),
        other => schema_type(other),
    }
}

fn path_schema_type(path: &TypePath) -> Type {
    let mut path = path.clone();
    if let Some(segment) = path.path.segments.last()
        && segment.ident == "Cow"
        && let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments
    {
        let owned = arguments.args.iter().find_map(|argument| match argument {
            syn::GenericArgument::Type(ty) => Some(ty),
            _ => None,
        });
        if let Some(owned) = owned {
            return borrowed_schema_type(owned);
        }
    }
    for segment in &mut path.path.segments {
        let syn::PathArguments::AngleBracketed(arguments) = &mut segment.arguments else {
            continue;
        };
        let resolved = arguments
            .args
            .iter()
            .filter_map(|argument| match argument {
                syn::GenericArgument::Lifetime(_) => None,
                syn::GenericArgument::Type(ty) => Some(syn::GenericArgument::Type(schema_type(ty))),
                other => Some(other.clone()),
            })
            .collect::<syn::punctuated::Punctuated<_, Token![,]>>();
        if resolved.is_empty() {
            segment.arguments = syn::PathArguments::None;
        } else {
            arguments.args = resolved;
        }
    }
    Type::Path(path)
}

fn vector_of(item: &Type) -> Type {
    syn::parse_quote!(::std::vec::Vec<#item>)
}

/// Names a type in a descriptor body, where no lifetime can be inferred.
///
/// `Json<PageView<'_>>` and `Json<PageView<'store>>` document one schema, so
/// every lifetime is written `'static` and the impl that answers is the same
/// one either way.
fn documented_type(ty: &Type) -> Type {
    match ty {
        Type::Reference(reference) => {
            let mut reference = reference.clone();
            reference.lifetime = Some(syn::Lifetime::new(
                "'static",
                proc_macro2::Span::call_site(),
            ));
            reference.elem = Box::new(documented_type(&reference.elem));
            Type::Reference(reference)
        }
        Type::Path(path) => {
            let mut path = path.clone();
            for segment in &mut path.path.segments {
                let syn::PathArguments::AngleBracketed(arguments) = &mut segment.arguments else {
                    continue;
                };
                for argument in &mut arguments.args {
                    match argument {
                        syn::GenericArgument::Lifetime(lifetime) => {
                            *lifetime =
                                syn::Lifetime::new("'static", proc_macro2::Span::call_site());
                        }
                        syn::GenericArgument::Type(inner) => *inner = documented_type(inner),
                        _ => {}
                    }
                }
            }
            Type::Path(path)
        }
        Type::Paren(inner) => documented_type(&inner.elem),
        Type::Group(inner) => documented_type(&inner.elem),
        Type::Slice(slice) => {
            let mut slice = slice.clone();
            slice.elem = Box::new(documented_type(&slice.elem));
            Type::Slice(slice)
        }
        other => other.clone(),
    }
}

/// Emits the autoref-specialization probes that drive recursion into models.
///
/// The specialized trait is implemented for the probe itself and therefore wins
/// method resolution whenever the field type implements `ApiModel`. Otherwise
/// resolution falls through to the reference impl, which does nothing.
fn nested_probe_definitions() -> proc_macro2::TokenStream {
    let value = value_probe_definitions();
    let items = items_probe_definitions();
    let kind = kind_probe_definitions();
    let constrained = constrained_probe_definitions();
    quote! {
        #[allow(dead_code)]
        struct __BlazinglyValue<'probe, T>(&'probe T);
        #[allow(dead_code)]
        struct __BlazinglyItems<'probe, T>(&'probe [T]);
        #[allow(dead_code)]
        struct __BlazinglyKind<T>(::core::marker::PhantomData<T>);

        #value
        #items
        #kind
        #constrained
    }
}

/// Emits the probes that carry a declared value type's rules into the model.
///
/// A field written `title: Title` has to pick up the rules `Title` declared,
/// both in the descriptor and in the validation pass, without the model
/// knowing at expansion time whether `Title` declares any.
fn constrained_probe_definitions() -> proc_macro2::TokenStream {
    let rules = declared_rules_probe_definitions();
    let checks = constrained_check_probe_definitions();
    quote! {
        #rules
        #checks
    }
}

fn declared_rules_probe_definitions() -> proc_macro2::TokenStream {
    quote! {
        #[allow(dead_code)]
        trait __BlazinglyDeclaredRules {
            fn __blazingly_declared_rules(
                &self,
            ) -> ::std::vec::Vec<::blazingly::ValidationRule>;
        }

        impl<T: ::blazingly::ApiConstrained> __BlazinglyDeclaredRules for __BlazinglyKind<T> {
            fn __blazingly_declared_rules(
                &self,
            ) -> ::std::vec::Vec<::blazingly::ValidationRule> {
                <T as ::blazingly::ApiConstrained>::constraint_rules()
            }
        }

        #[allow(dead_code)]
        trait __BlazinglyPlainRules {
            fn __blazingly_declared_rules(
                &self,
            ) -> ::std::vec::Vec<::blazingly::ValidationRule>;
        }

        impl<T> __BlazinglyPlainRules for &__BlazinglyKind<T> {
            fn __blazingly_declared_rules(
                &self,
            ) -> ::std::vec::Vec<::blazingly::ValidationRule> {
                ::std::vec::Vec::new()
            }
        }
    }
}

fn constrained_check_probe_definitions() -> proc_macro2::TokenStream {
    quote! {
        #[allow(dead_code)]
        trait __BlazinglyConstrainedValue {
            fn __blazingly_constrained(
                &self,
                errors: &mut ::blazingly::ValidationErrors,
                field: &str,
            );
        }

        impl<T: ::blazingly::ApiConstrained> __BlazinglyConstrainedValue
            for __BlazinglyValue<'_, T>
        {
            fn __blazingly_constrained(
                &self,
                errors: &mut ::blazingly::ValidationErrors,
                field: &str,
            ) {
                if let ::core::result::Result::Err(declared) =
                    ::blazingly::ApiConstrained::validate_constraints(self.0)
                {
                    ::blazingly::merge_field_validation_errors(errors, field, &declared);
                }
            }
        }

        #[allow(dead_code)]
        trait __BlazinglyPlainConstrained {
            fn __blazingly_constrained(
                &self,
                errors: &mut ::blazingly::ValidationErrors,
                field: &str,
            );
        }

        impl<T> __BlazinglyPlainConstrained for &__BlazinglyValue<'_, T> {
            fn __blazingly_constrained(
                &self,
                _errors: &mut ::blazingly::ValidationErrors,
                _field: &str,
            ) {
            }
        }

        #[allow(dead_code)]
        trait __BlazinglyConstrainedItems {
            fn __blazingly_constrained_items(
                &self,
                errors: &mut ::blazingly::ValidationErrors,
                field: &str,
            );
        }

        impl<T: ::blazingly::ApiConstrained> __BlazinglyConstrainedItems
            for __BlazinglyItems<'_, T>
        {
            fn __blazingly_constrained_items(
                &self,
                errors: &mut ::blazingly::ValidationErrors,
                field: &str,
            ) {
                for (index, item) in self.0.iter().enumerate() {
                    if let ::core::result::Result::Err(declared) =
                        ::blazingly::ApiConstrained::validate_constraints(item)
                    {
                        let prefix = ::std::format!("{}[{}]", field, index);
                        ::blazingly::merge_field_validation_errors(errors, &prefix, &declared);
                    }
                }
            }
        }

        #[allow(dead_code)]
        trait __BlazinglyPlainConstrainedItems {
            fn __blazingly_constrained_items(
                &self,
                errors: &mut ::blazingly::ValidationErrors,
                field: &str,
            );
        }

        impl<T> __BlazinglyPlainConstrainedItems for &__BlazinglyItems<'_, T> {
            fn __blazingly_constrained_items(
                &self,
                _errors: &mut ::blazingly::ValidationErrors,
                _field: &str,
            ) {
            }
        }
    }
}

fn value_probe_definitions() -> proc_macro2::TokenStream {
    quote! {
        #[allow(dead_code)]
        trait __BlazinglyNestedValue {
            fn __blazingly_nested(
                &self,
                errors: &mut ::blazingly::ValidationErrors,
                field: &str,
            );
        }

        impl<T: ::blazingly::ApiModel> __BlazinglyNestedValue for __BlazinglyValue<'_, T> {
            fn __blazingly_nested(
                &self,
                errors: &mut ::blazingly::ValidationErrors,
                field: &str,
            ) {
                if let ::core::result::Result::Err(nested) =
                    ::blazingly::ApiModel::validate(self.0)
                {
                    ::blazingly::merge_validation_errors(errors, field, &nested);
                }
            }
        }

        #[allow(dead_code)]
        trait __BlazinglyPlainValue {
            fn __blazingly_nested(
                &self,
                errors: &mut ::blazingly::ValidationErrors,
                field: &str,
            );
        }

        impl<T> __BlazinglyPlainValue for &__BlazinglyValue<'_, T> {
            fn __blazingly_nested(
                &self,
                _errors: &mut ::blazingly::ValidationErrors,
                _field: &str,
            ) {
            }
        }
    }
}

fn items_probe_definitions() -> proc_macro2::TokenStream {
    quote! {
        #[allow(dead_code)]
        trait __BlazinglyNestedItems {
            fn __blazingly_nested_items(
                &self,
                errors: &mut ::blazingly::ValidationErrors,
                field: &str,
            );
        }

        impl<T: ::blazingly::ApiModel> __BlazinglyNestedItems for __BlazinglyItems<'_, T> {
            fn __blazingly_nested_items(
                &self,
                errors: &mut ::blazingly::ValidationErrors,
                field: &str,
            ) {
                for (index, item) in self.0.iter().enumerate() {
                    if let ::core::result::Result::Err(nested) =
                        ::blazingly::ApiModel::validate(item)
                    {
                        let prefix = ::std::format!("{}[{}]", field, index);
                        ::blazingly::merge_validation_errors(errors, &prefix, &nested);
                    }
                }
            }
        }

        #[allow(dead_code)]
        trait __BlazinglyPlainItems {
            fn __blazingly_nested_items(
                &self,
                errors: &mut ::blazingly::ValidationErrors,
                field: &str,
            );
        }

        impl<T> __BlazinglyPlainItems for &__BlazinglyItems<'_, T> {
            fn __blazingly_nested_items(
                &self,
                _errors: &mut ::blazingly::ValidationErrors,
                _field: &str,
            ) {
            }
        }
    }
}

fn kind_probe_definitions() -> proc_macro2::TokenStream {
    quote! {
        #[allow(dead_code)]
        trait __BlazinglyModelKind {
            fn __blazingly_is_model(&self) -> bool;
        }

        impl<T: ::blazingly::ApiModel> __BlazinglyModelKind for __BlazinglyKind<T> {
            fn __blazingly_is_model(&self) -> bool {
                true
            }
        }

        #[allow(dead_code)]
        trait __BlazinglyPlainKind {
            fn __blazingly_is_model(&self) -> bool;
        }

        impl<T> __BlazinglyPlainKind for &__BlazinglyKind<T> {
            fn __blazingly_is_model(&self) -> bool {
                false
            }
        }
    }
}

fn nested_rule_descriptor(
    validation_type: &Type,
    shape: FieldShape,
    explicit: bool,
) -> proc_macro2::TokenStream {
    if explicit {
        return quote!(rules.push(::blazingly::ValidationRule::Nested););
    }
    if !shape.may_be_model() {
        return quote!();
    }
    let probe_type = model_probe_type(validation_type, shape);
    quote! {
        if (&__BlazinglyKind::<#probe_type>(::core::marker::PhantomData))
            .__blazingly_is_model()
        {
            rules.push(::blazingly::ValidationRule::Nested);
        }
    }
}

/// Copies a declared value type's rules into the field that uses it.
///
/// Only a scalar-shaped field inherits: a `Vec<Title>` field would otherwise
/// claim the item's bounds as its own.
fn inherited_rule_descriptor(
    validation_type: &Type,
    shape: FieldShape,
) -> proc_macro2::TokenStream {
    if shape != FieldShape::Other {
        return quote!();
    }
    quote! {
        rules.extend(
            (&__BlazinglyKind::<#validation_type>(::core::marker::PhantomData))
                .__blazingly_declared_rules(),
        );
    }
}

fn model_probe_type(validation_type: &Type, shape: FieldShape) -> Type {
    if shape == FieldShape::Collection {
        wrapper_inner(validation_type, "Vec").unwrap_or_else(|| validation_type.clone())
    } else {
        validation_type.clone()
    }
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

#[allow(clippy::too_many_lines)]
fn take_field_rules(attributes: &mut Vec<Attribute>) -> syn::Result<FieldRules> {
    let mut retained = Vec::new();
    let mut rules = FieldRules::default();

    for attribute in attributes.drain(..) {
        let path = attribute.path().clone();
        if path.is_ident("min_length") {
            let value = attribute.parse_args::<LitInt>()?;
            rules.min_length = Some((value.base10_parse()?, value.span()));
        } else if path.is_ident("max_length") {
            let value = attribute.parse_args::<LitInt>()?;
            rules.max_length = Some((value.base10_parse()?, value.span()));
        } else if path.is_ident("min_items") {
            let value = attribute.parse_args::<LitInt>()?;
            rules.min_items = Some((value.base10_parse()?, value.span()));
        } else if path.is_ident("max_items") {
            let value = attribute.parse_args::<LitInt>()?;
            rules.max_items = Some((value.base10_parse()?, value.span()));
        } else if path.is_ident("unique_items") {
            rules.unique_items = Some(attribute_span(&attribute));
        } else if path.is_ident("minimum") {
            rules.minimum = Some(numeric_attribute(&attribute)?);
        } else if path.is_ident("maximum") {
            rules.maximum = Some(numeric_attribute(&attribute)?);
        } else if path.is_ident("exclusive_minimum") {
            rules.exclusive_minimum = Some(numeric_attribute(&attribute)?);
        } else if path.is_ident("exclusive_maximum") {
            rules.exclusive_maximum = Some(numeric_attribute(&attribute)?);
        } else if path.is_ident("multiple_of") {
            let (factor, span) = numeric_attribute(&attribute)?;
            if factor.is_zero() {
                return Err(syn::Error::new(span, "`multiple_of` cannot be zero"));
            }
            rules.multiple_of = Some((factor, span));
        } else if path.is_ident("pattern") {
            let pattern = attribute.parse_args::<LitStr>()?;
            if let Err(reason) = lint_pattern_syntax(&pattern.value()) {
                return Err(syn::Error::new(pattern.span(), reason));
            }
            rules.pattern = Some(pattern);
        } else if path.is_ident("default") {
            if rules.default.is_some() {
                return Err(syn::Error::new_spanned(
                    attribute,
                    "only one `default` may be declared per field",
                ));
            }
            rules.default = Some(default_attribute(&attribute)?);
        } else if path.is_ident("email") {
            rules.email = Some(attribute_span(&attribute));
        } else if path.is_ident("alias") {
            rules.aliases.push(attribute.parse_args::<LitStr>()?);
        } else if path.is_ident("validate_with") {
            if rules.validator.is_some() {
                return Err(syn::Error::new_spanned(
                    attribute,
                    "only one `validate_with` function may be declared per field",
                ));
            }
            rules.validator = Some(attribute.parse_args::<SynPath>()?);
        } else if path.is_ident("nested") {
            rules.nested = true;
        } else {
            retained.push(attribute);
        }
    }

    reject_inverted_bounds(&rules)?;

    *attributes = retained;
    Ok(rules)
}

fn attribute_span(attribute: &Attribute) -> proc_macro2::Span {
    attribute
        .path()
        .segments
        .last()
        .map_or_else(proc_macro2::Span::call_site, |segment| segment.ident.span())
}

fn default_attribute(attribute: &Attribute) -> syn::Result<(DefaultLiteral, proc_macro2::Span)> {
    let expression = attribute.parse_args::<syn::Expr>()?;
    let unsupported = || {
        syn::Error::new_spanned(
            &expression,
            "`default` requires a string, integer, floating-point, or boolean literal",
        )
    };
    let (negative, literal) = match &expression {
        syn::Expr::Lit(literal) => (false, &literal.lit),
        syn::Expr::Unary(unary) if matches!(unary.op, syn::UnOp::Neg(_)) => {
            let syn::Expr::Lit(literal) = unary.expr.as_ref() else {
                return Err(unsupported());
            };
            (true, &literal.lit)
        }
        _ => return Err(unsupported()),
    };
    let span = literal.span();
    let value = match literal {
        syn::Lit::Str(value) if !negative => DefaultLiteral::Text(value.clone()),
        syn::Lit::Bool(value) if !negative => DefaultLiteral::Boolean(value.clone()),
        syn::Lit::Int(value) => {
            let magnitude = value.base10_parse::<i128>()?;
            DefaultLiteral::Number(NumericLiteral::Integer(if negative {
                -magnitude
            } else {
                magnitude
            }))
        }
        syn::Lit::Float(value) => {
            let magnitude = value.base10_parse::<f64>()?;
            let magnitude = if negative { -magnitude } else { magnitude };
            if !magnitude.is_finite() {
                return Err(syn::Error::new(span, "a `default` must be finite"));
            }
            DefaultLiteral::Number(NumericLiteral::Float(magnitude))
        }
        _ => return Err(unsupported()),
    };
    Ok((value, span))
}

fn numeric_attribute(attribute: &Attribute) -> syn::Result<(NumericLiteral, proc_macro2::Span)> {
    let expression = attribute.parse_args::<syn::Expr>()?;
    let (negative, literal) = match &expression {
        syn::Expr::Lit(literal) => (false, &literal.lit),
        syn::Expr::Unary(unary) if matches!(unary.op, syn::UnOp::Neg(_)) => {
            let syn::Expr::Lit(literal) = unary.expr.as_ref() else {
                return Err(syn::Error::new_spanned(
                    &expression,
                    "numeric bounds require an integer or floating-point literal",
                ));
            };
            (true, &literal.lit)
        }
        _ => {
            return Err(syn::Error::new_spanned(
                &expression,
                "numeric bounds require an integer or floating-point literal",
            ));
        }
    };
    let span = literal.span();
    let value = match literal {
        syn::Lit::Int(value) => {
            let magnitude = value.base10_parse::<i128>()?;
            NumericLiteral::Integer(if negative { -magnitude } else { magnitude })
        }
        syn::Lit::Float(value) => {
            let magnitude = value.base10_parse::<f64>()?;
            let magnitude = if negative { -magnitude } else { magnitude };
            if !magnitude.is_finite() {
                return Err(syn::Error::new(span, "numeric bounds must be finite"));
            }
            NumericLiteral::Float(magnitude)
        }
        _ => {
            return Err(syn::Error::new(
                span,
                "numeric bounds require an integer or floating-point literal",
            ));
        }
    };
    Ok((value, span))
}

fn reject_inverted_bounds(rules: &FieldRules) -> syn::Result<()> {
    if let (Some((minimum, span)), Some((maximum, _))) = (rules.min_length, rules.max_length)
        && minimum > maximum
    {
        return Err(syn::Error::new(
            span,
            "`min_length` cannot be greater than `max_length`",
        ));
    }
    if let (Some((minimum, span)), Some((maximum, _))) = (rules.min_items, rules.max_items)
        && minimum > maximum
    {
        return Err(syn::Error::new(
            span,
            "`min_items` cannot be greater than `max_items`",
        ));
    }
    if let (Some((minimum, span)), Some((maximum, _))) = (rules.minimum, rules.maximum)
        && minimum.exceeds(maximum)
    {
        return Err(syn::Error::new(
            span,
            "`minimum` cannot be greater than `maximum`",
        ));
    }
    if let (Some((minimum, span)), Some((maximum, _))) =
        (rules.exclusive_minimum, rules.exclusive_maximum)
        && minimum.exceeds(maximum)
    {
        return Err(syn::Error::new(
            span,
            "`exclusive_minimum` cannot be greater than `exclusive_maximum`",
        ));
    }
    Ok(())
}

fn reject_incompatible_rules(
    rules: &FieldRules,
    validation_type: &Type,
    shape: FieldShape,
) -> syn::Result<()> {
    let numeric = [
        (rules.minimum.map(|(_, span)| span), "minimum"),
        (rules.maximum.map(|(_, span)| span), "maximum"),
        (
            rules.exclusive_minimum.map(|(_, span)| span),
            "exclusive_minimum",
        ),
        (
            rules.exclusive_maximum.map(|(_, span)| span),
            "exclusive_maximum",
        ),
        (rules.multiple_of.map(|(_, span)| span), "multiple_of"),
    ];
    for (span, name) in numeric {
        if let Some(span) = span
            && !shape.is_numeric()
        {
            return Err(syn::Error::new(
                span,
                format!(
                    "`{name}` requires an integer or floating-point field, \
                     but `{}` is not numeric",
                    type_label(validation_type)
                ),
            ));
        }
    }

    let collection = [
        (rules.min_items.map(|(_, span)| span), "min_items"),
        (rules.max_items.map(|(_, span)| span), "max_items"),
        (rules.unique_items, "unique_items"),
    ];
    for (span, name) in collection {
        if let Some(span) = span
            && shape != FieldShape::Collection
        {
            return Err(syn::Error::new(
                span,
                format!(
                    "`{name}` requires a `Vec<T>` or `Option<Vec<T>>` field, \
                     but `{}` is not a collection",
                    type_label(validation_type)
                ),
            ));
        }
    }

    if let Some(pattern) = &rules.pattern
        && shape != FieldShape::Text
    {
        return Err(syn::Error::new(
            pattern.span(),
            format!(
                "`pattern` requires a `String` or `Option<String>` field, \
                 but `{}` is not a string",
                type_label(validation_type)
            ),
        ));
    }

    if let Some(span) = rules.email
        && shape != FieldShape::Text
    {
        return Err(syn::Error::new(
            span,
            format!(
                "`email` requires a `String` or `Option<String>` field, \
                 but `{}` is not a string",
                type_label(validation_type)
            ),
        ));
    }

    reject_incompatible_default(rules, validation_type, shape)?;

    let lengths = [
        (rules.min_length.map(|(_, span)| span), "min_length"),
        (rules.max_length.map(|(_, span)| span), "max_length"),
    ];
    for (span, name) in lengths {
        if let Some(span) = span
            && !matches!(shape, FieldShape::Text | FieldShape::Collection)
        {
            return Err(syn::Error::new(
                span,
                format!(
                    "`{name}` requires a `String`, `Option<String>`, or `Vec<T>` field, \
                     but `{}` is neither",
                    type_label(validation_type)
                ),
            ));
        }
    }

    Ok(())
}

/// A default is substituted for the field itself, so it has to be the field's
/// own type; nothing here converts.
fn reject_incompatible_default(
    rules: &FieldRules,
    validation_type: &Type,
    shape: FieldShape,
) -> syn::Result<()> {
    let Some((literal, span)) = &rules.default else {
        return Ok(());
    };
    let accepted = match literal {
        DefaultLiteral::Text(_) => shape == FieldShape::Text,
        DefaultLiteral::Number(NumericLiteral::Integer(_)) => shape.is_numeric(),
        DefaultLiteral::Number(NumericLiteral::Float(_)) => shape == FieldShape::Float,
        DefaultLiteral::Boolean(_) => bare_type_matches(validation_type, &["bool"]),
    };
    if accepted {
        return Ok(());
    }
    let (written, expected) = literal.expectation();
    Err(syn::Error::new(
        *span,
        format!(
            "`default` with {written} requires {expected}, but `{}` is not one",
            type_label(validation_type)
        ),
    ))
}

/// Folds `min_length`/`max_length` into the item bounds for collection fields.
fn normalize_collection_rules(rules: &mut FieldRules, shape: FieldShape) {
    if shape != FieldShape::Collection {
        return;
    }
    if rules.min_items.is_none() {
        rules.min_items = rules.min_length;
    }
    if rules.max_items.is_none() {
        rules.max_items = rules.max_length;
    }
    rules.min_length = None;
    rules.max_length = None;
}

fn rule_descriptors(rules: &FieldRules, nullable: bool) -> Vec<proc_macro2::TokenStream> {
    let mut descriptors = Vec::new();

    if let Some((minimum, _)) = rules.min_length {
        descriptors.push(quote!(::blazingly::ValidationRule::MinLength(#minimum)));
    }
    if let Some((maximum, _)) = rules.max_length {
        descriptors.push(quote!(::blazingly::ValidationRule::MaxLength(#maximum)));
    }
    if rules.email.is_some() {
        descriptors.push(quote!(::blazingly::ValidationRule::Email));
    }
    for encoded in constraint_encodings(rules) {
        descriptors.push(quote!(::blazingly::ValidationRule::Custom(#encoded.to_owned())));
    }
    for encoded in metadata_encodings(rules, nullable) {
        descriptors.push(quote!(::blazingly::ValidationRule::Custom(#encoded.to_owned())));
    }
    for alias in &rules.aliases {
        descriptors.push(quote!(::blazingly::ValidationRule::Alias(#alias.to_owned())));
    }
    if let Some(validator) = &rules.validator {
        descriptors.push(quote!(::blazingly::ValidationRule::Custom(
            stringify!(#validator).to_owned()
        )));
    }

    descriptors
}

/// Encodes constraints without a dedicated contract variant as `key=value`.
fn constraint_encodings(rules: &FieldRules) -> Vec<String> {
    let mut encodings = Vec::new();
    let numeric = [
        (rules.minimum, "minimum"),
        (rules.maximum, "maximum"),
        (rules.exclusive_minimum, "exclusive_minimum"),
        (rules.exclusive_maximum, "exclusive_maximum"),
        (rules.multiple_of, "multiple_of"),
    ];
    for (rule, keyword) in numeric {
        if let Some((value, _)) = rule {
            encodings.push(format!("{keyword}={}", value.encoded()));
        }
    }
    if let Some(pattern) = &rules.pattern {
        encodings.push(format!("pattern={}", pattern.value()));
    }
    if let Some((minimum, _)) = rules.min_items {
        encodings.push(format!("min_items={minimum}"));
    }
    if let Some((maximum, _)) = rules.max_items {
        encodings.push(format!("max_items={maximum}"));
    }
    if rules.unique_items.is_some() {
        encodings.push("unique_items=true".to_owned());
    }
    encodings
}

/// Encodes field metadata the frozen contract format cannot name.
///
/// `blazingly::FieldMetadata` is the reader; the `keyword=value` channel is the
/// one already used for the constraints above.
fn metadata_encodings(rules: &FieldRules, nullable: bool) -> Vec<String> {
    let mut encodings = Vec::new();
    if let Some((literal, _)) = &rules.default {
        encodings.push(format!("default={}", literal.encoded()));
    }
    if nullable {
        encodings.push("nullable=true".to_owned());
    }
    encodings
}

fn field_checks(
    rules: &FieldRules,
    shape: FieldShape,
    public_name: &LitStr,
) -> proc_macro2::TokenStream {
    match shape {
        FieldShape::Text => text_checks(rules, public_name),
        FieldShape::Integer | FieldShape::Float => numeric_checks(rules, public_name),
        FieldShape::Collection => collection_checks(rules, public_name),
        FieldShape::Other => quote!(),
    }
}

fn text_checks(rules: &FieldRules, public_name: &LitStr) -> proc_macro2::TokenStream {
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
    let email = rules.email.map(|_| {
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
    let pattern = rules.pattern.as_ref().map(|pattern| {
        quote! {
            ::blazingly::validation::check_pattern(
                &mut errors,
                #public_name,
                value.as_str(),
                #pattern,
            );
        }
    });

    quote! {
        #minimum
        #maximum
        #email
        #pattern
    }
}

fn numeric_checks(rules: &FieldRules, public_name: &LitStr) -> proc_macro2::TokenStream {
    let checks = [
        (rules.minimum, quote!(check_minimum)),
        (rules.maximum, quote!(check_maximum)),
        (rules.exclusive_minimum, quote!(check_exclusive_minimum)),
        (rules.exclusive_maximum, quote!(check_exclusive_maximum)),
        (rules.multiple_of, quote!(check_multiple_of)),
    ]
    .into_iter()
    .filter_map(|(rule, function)| {
        let (bound, _) = rule?;
        let bound = bound.tokens();
        Some(quote! {
            ::blazingly::validation::#function(&mut errors, #public_name, *value, #bound);
        })
    });

    quote!(#(#checks)*)
}

fn collection_checks(rules: &FieldRules, public_name: &LitStr) -> proc_macro2::TokenStream {
    let minimum = rules.min_items.map(|(minimum, _)| {
        quote! {
            ::blazingly::validation::check_min_items(
                &mut errors,
                #public_name,
                value.as_slice(),
                #minimum,
            );
        }
    });
    let maximum = rules.max_items.map(|(maximum, _)| {
        quote! {
            ::blazingly::validation::check_max_items(
                &mut errors,
                #public_name,
                value.as_slice(),
                #maximum,
            );
        }
    });
    let unique = rules.unique_items.map(|_| {
        quote! {
            ::blazingly::validation::check_unique_items(
                &mut errors,
                #public_name,
                value.as_slice(),
            );
        }
    });

    quote! {
        #minimum
        #maximum
        #unique
    }
}

fn nested_validation_checks(
    shape: FieldShape,
    public_name: &LitStr,
    rules: &FieldRules,
) -> proc_macro2::TokenStream {
    let nested = shape.may_be_model().then(|| {
        if shape == FieldShape::Collection {
            quote! {
                (&__BlazinglyItems(value.as_slice()))
                    .__blazingly_nested_items(&mut errors, #public_name);
                (&__BlazinglyItems(value.as_slice()))
                    .__blazingly_constrained_items(&mut errors, #public_name);
            }
        } else {
            quote! {
                (&__BlazinglyValue(value)).__blazingly_nested(&mut errors, #public_name);
                (&__BlazinglyValue(value)).__blazingly_constrained(&mut errors, #public_name);
            }
        }
    });
    let custom = rules.validator.as_ref().map(|validator| {
        quote! {
            if let ::core::result::Result::Err(custom_errors) = #validator(value) {
                ::blazingly::merge_field_validation_errors(
                    &mut errors,
                    #public_name,
                    &custom_errors,
                );
            }
        }
    });
    quote! {
        #nested
        #custom
    }
}

const INTEGER_TYPES: &[&str] = &[
    "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128", "usize",
];

const FLOAT_TYPES: &[&str] = &["f32", "f64"];

fn field_shape(ty: &Type) -> FieldShape {
    if is_string_type(ty) {
        return FieldShape::Text;
    }
    if bare_type_matches(ty, INTEGER_TYPES) {
        return FieldShape::Integer;
    }
    if bare_type_matches(ty, FLOAT_TYPES) {
        return FieldShape::Float;
    }
    if wrapper_inner(ty, "Vec").is_some() {
        return FieldShape::Collection;
    }
    FieldShape::Other
}

fn bare_type_matches(ty: &Type, names: &[&str]) -> bool {
    let Type::Path(path) = ty else {
        return false;
    };
    path.path.segments.last().is_some_and(|segment| {
        segment.arguments.is_none() && names.contains(&segment.ident.to_string().as_str())
    })
}

fn type_label(ty: &Type) -> String {
    let Type::Path(path) = ty else {
        return "this field type".to_owned();
    };
    path.path.segments.last().map_or_else(
        || "this field type".to_owned(),
        |segment| segment.ident.to_string(),
    )
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

/// Maximum pattern length accepted by `blazingly_validation::Pattern`.
const MAX_PATTERN_CHARS: usize = 512;

/// Maximum group nesting accepted by `blazingly_validation::Pattern`.
const MAX_PATTERN_DEPTH: i32 = 16;

const fn is_supported_escape(value: char) -> bool {
    matches!(
        value,
        'd' | 'D'
            | 'w'
            | 'W'
            | 's'
            | 'S'
            | 't'
            | 'n'
            | 'r'
            | '\\'
            | '.'
            | '*'
            | '+'
            | '?'
            | '('
            | ')'
            | '['
            | ']'
            | '{'
            | '}'
            | '|'
            | '^'
            | '$'
            | '-'
            | '/'
    )
}

/// Rejects patterns outside the runtime matcher's supported subset.
///
/// The runtime matcher stays authoritative; this check exists so the common
/// mistakes surface at compile time instead of as a violation on every request.
#[allow(clippy::too_many_lines)]
fn lint_pattern_syntax(pattern: &str) -> Result<(), String> {
    if pattern.is_empty() {
        return Err("the pattern is empty".to_owned());
    }
    let characters = pattern.chars().collect::<Vec<_>>();
    if characters.len() > MAX_PATTERN_CHARS {
        return Err(format!(
            "the pattern exceeds {MAX_PATTERN_CHARS} characters"
        ));
    }
    let last = characters.len() - 1;
    let mut depth = 0_i32;
    let mut deepest = 0_i32;
    let mut in_class = false;
    let mut quantifiable = false;
    let mut index = 0;

    while let Some(&value) = characters.get(index) {
        match value {
            '\\' => {
                let Some(&escaped) = characters.get(index + 1) else {
                    return Err("the pattern ends with a lone backslash".to_owned());
                };
                if !is_supported_escape(escaped) {
                    return Err(format!("the escape `\\{escaped}` is not supported"));
                }
                quantifiable = true;
                index += 2;
                continue;
            }
            '[' if !in_class => {
                in_class = true;
                quantifiable = false;
            }
            ']' if in_class => {
                in_class = false;
                quantifiable = true;
            }
            _ if in_class => {}
            '(' => {
                if characters.get(index + 1) == Some(&'?') {
                    return Err(
                        "group flags, lookaround, and non-capturing groups are not supported"
                            .to_owned(),
                    );
                }
                depth += 1;
                deepest = deepest.max(depth);
                quantifiable = false;
            }
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return Err("the pattern has an unbalanced group".to_owned());
                }
                quantifiable = true;
            }
            '*' | '+' | '?' => {
                if !quantifiable {
                    return Err("a quantifier has no preceding expression".to_owned());
                }
                quantifiable = false;
            }
            '{' | '}' => {
                return Err("counted repetition `{m,n}` is not supported".to_owned());
            }
            '^' if index != 0 => {
                return Err("`^` is supported only at the start of a pattern".to_owned());
            }
            '$' if index != last => {
                return Err("`$` is supported only at the end of a pattern".to_owned());
            }
            '^' | '$' | '|' => quantifiable = false,
            _ => quantifiable = true,
        }
        index += 1;
    }

    if in_class {
        return Err("the pattern has an unterminated character class".to_owned());
    }
    if depth != 0 {
        return Err("the pattern has an unbalanced group".to_owned());
    }
    if deepest > MAX_PATTERN_DEPTH {
        return Err(format!("groups nest deeper than {MAX_PATTERN_DEPTH}"));
    }
    Ok(())
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
        let (declared, by_reference) = match &**ty {
            Type::Reference(reference) if reference.mutability.is_some() => {
                return Err(syn::Error::new_spanned(
                    ty,
                    "operation arguments cannot be taken by unique reference; a \
                     dependency is shared across the request",
                ));
            }
            Type::Reference(reference) => (&*reference.elem, true),
            other => (other, false),
        };
        let (kind, inner) = OperationInputKind::from_type(declared)
            .unwrap_or_else(|| (OperationInputKind::DirectDependency, declared.clone()));
        if by_reference && !kind.is_dependency() {
            return Err(syn::Error::new_spanned(
                ty,
                "only dependencies may be taken by reference; an extracted \
                 argument is decoded from the request and owns its data",
            ));
        }
        if matches!(
            kind,
            OperationInputKind::Json
                | OperationInputKind::Form
                | OperationInputKind::Multipart
                | OperationInputKind::File
                | OperationInputKind::Stream
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
            argument_type: declared.clone(),
            inner,
            required,
            by_reference,
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
        if type_is(ty, "WebSocketRequest") {
            return Some((Self::WebSocket, ty.clone()));
        }
        if type_is(ty, "UploadBody") {
            return Some((Self::Stream, ty.clone()));
        }
        [
            (Self::Path, "Path"),
            (Self::Query, "Query"),
            (Self::Header, "Header"),
            (Self::Cookie, "Cookie"),
            (Self::Json, "Json"),
            (Self::Form, "Form"),
            (Self::Multipart, "Multipart"),
            (Self::File, "File"),
            (Self::Extension, "Extension"),
            (Self::Extract, "Extract"),
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
            Self::Stream => Some(quote!(::blazingly::InputSource::Stream)),
            Self::WebSocket
            | Self::Extension
            | Self::Extract
            | Self::Dependency
            | Self::DirectDependency => None,
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
    if let Some(inner) = wrapper_inner(ty, "Background") {
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

#[cfg(test)]
mod tests {
    use super::{
        Attribute, FieldRules, FieldShape, Fields, ItemEnum, ItemStruct, ModelArgs, NumericLiteral,
        RenameRule, Type, constraint_encodings, enum_model_tokens, field_shape,
        lint_pattern_syntax, metadata_encodings, model_tokens, normalize_collection_rules, quote,
        reject_incompatible_rules, schema_type, snake_to_camel, take_field_rules,
    };

    fn field_attributes(declaration: &str) -> Vec<Attribute> {
        let model = syn::parse_str::<ItemStruct>(&format!("struct Probe {{ {declaration} }}"))
            .expect("fixture struct parses");
        let Fields::Named(fields) = model.fields else {
            panic!("fixture struct must use named fields");
        };
        fields
            .named
            .into_iter()
            .next()
            .expect("fixture struct declares one field")
            .attrs
    }

    fn rules_of(declaration: &str) -> syn::Result<FieldRules> {
        let mut attributes = field_attributes(declaration);
        take_field_rules(&mut attributes)
    }

    fn parse_type(source: &str) -> Type {
        syn::parse_str::<Type>(source).expect("fixture type parses")
    }

    fn rejection_message<T>(result: syn::Result<T>) -> String {
        match result {
            Ok(_) => panic!("the declaration must be rejected"),
            Err(error) => error.to_string(),
        }
    }

    fn reject(declaration: &str, field_type: &str) -> String {
        let rules = rules_of(declaration).expect("attributes parse");
        let field_type = parse_type(field_type);
        let shape = field_shape(&field_type);
        reject_incompatible_rules(&rules, &field_type, shape)
            .expect_err("the rule must be rejected")
            .to_string()
    }

    fn expand(source: &str, arguments: &ModelArgs) -> syn::Result<String> {
        let mut model = syn::parse_str::<ItemStruct>(source).expect("fixture model parses");
        model_tokens(arguments, &mut model).map(|tokens| tokens.to_string())
    }

    fn expand_default(source: &str) -> syn::Result<String> {
        expand(source, &ModelArgs::default())
    }

    #[test]
    fn field_shapes_classify_strings_numbers_and_collections() {
        assert_eq!(field_shape(&parse_type("String")), FieldShape::Text);
        assert_eq!(field_shape(&parse_type("u8")), FieldShape::Integer);
        assert_eq!(field_shape(&parse_type("i128")), FieldShape::Integer);
        assert_eq!(field_shape(&parse_type("usize")), FieldShape::Integer);
        assert_eq!(field_shape(&parse_type("f32")), FieldShape::Float);
        assert_eq!(
            field_shape(&parse_type("Vec<Address>")),
            FieldShape::Collection
        );
        assert_eq!(field_shape(&parse_type("Uuid")), FieldShape::Other);
        assert_eq!(field_shape(&parse_type("bool")), FieldShape::Other);
    }

    #[test]
    fn numeric_rules_are_rejected_on_non_numeric_fields() {
        for rule in [
            "#[minimum(1)]",
            "#[maximum(1)]",
            "#[exclusive_minimum(1)]",
            "#[exclusive_maximum(1)]",
            "#[multiple_of(2)]",
        ] {
            let message = reject(&format!("{rule} name: String"), "String");
            assert!(
                message.contains("requires an integer or floating-point field"),
                "{rule} produced {message}"
            );
            assert!(message.contains("String"), "{rule} produced {message}");
        }
    }

    #[test]
    fn collection_rules_are_rejected_outside_collections() {
        for rule in ["#[min_items(1)]", "#[max_items(1)]", "#[unique_items]"] {
            let message = reject(&format!("{rule} name: String"), "String");
            assert!(
                message.contains("requires a `Vec<T>` or `Option<Vec<T>>` field"),
                "{rule} produced {message}"
            );
        }
    }

    #[test]
    fn string_rules_are_rejected_outside_strings() {
        let message = reject("#[pattern(\"^a$\")] count: u32", "u32");
        assert!(
            message.contains("`pattern` requires a `String`"),
            "{message}"
        );
        let message = reject("#[email] count: u32", "u32");
        assert!(message.contains("`email` requires a `String`"), "{message}");
    }

    #[test]
    fn length_rules_accept_strings_and_collections_but_not_numbers() {
        let rules = rules_of("#[min_length(2)] tags: Vec<String>").expect("attributes parse");
        let collection = parse_type("Vec<String>");
        reject_incompatible_rules(&rules, &collection, FieldShape::Collection)
            .expect("collections accept length rules");

        let message = reject("#[min_length(2)] count: u32", "u32");
        assert!(
            message.contains("`min_length` requires a `String`, `Option<String>`, or `Vec<T>`"),
            "{message}"
        );
    }

    #[test]
    fn inverted_and_degenerate_bounds_are_rejected() {
        let message = rejection_message(rules_of("#[min_length(5)] #[max_length(2)] name: String"));
        assert!(message.contains("`min_length` cannot be greater than `max_length`"));

        let message = rejection_message(rules_of(
            "#[min_items(5)] #[max_items(2)] tags: Vec<String>",
        ));
        assert!(message.contains("`min_items` cannot be greater than `max_items`"));

        let message = rejection_message(rules_of("#[minimum(5)] #[maximum(2)] count: u32"));
        assert!(message.contains("`minimum` cannot be greater than `maximum`"));

        let message = rejection_message(rules_of("#[multiple_of(0)] count: u32"));
        assert!(message.contains("`multiple_of` cannot be zero"));
    }

    #[test]
    fn numeric_attributes_accept_negative_and_floating_point_literals() {
        let rules = rules_of("#[minimum(-3)] #[maximum(2.5)] ratio: f64").expect("bounds parse");
        assert_eq!(
            rules.minimum.map(|(value, _)| value),
            Some(NumericLiteral::Integer(-3))
        );
        assert_eq!(
            rules.maximum.map(|(value, _)| value),
            Some(NumericLiteral::Float(2.5))
        );

        let message = rejection_message(rules_of("#[minimum(\"one\")] count: u32"));
        assert!(
            message.contains("integer or floating-point literal"),
            "{message}"
        );
    }

    #[test]
    fn constraints_use_a_canonical_key_value_encoding() {
        let rules = rules_of(
            "#[minimum(1)] #[maximum(10.0)] #[exclusive_minimum(0)] \
             #[exclusive_maximum(11)] #[multiple_of(2)] count: u32",
        )
        .expect("bounds parse");
        assert_eq!(
            constraint_encodings(&rules),
            [
                "minimum=1",
                "maximum=10.0",
                "exclusive_minimum=0",
                "exclusive_maximum=11",
                "multiple_of=2"
            ]
        );

        let rules = rules_of("#[pattern(\"^[a-z]+$\")] slug: String").expect("pattern parses");
        assert_eq!(constraint_encodings(&rules), ["pattern=^[a-z]+$"]);

        let rules = rules_of("#[min_items(1)] #[max_items(4)] #[unique_items] tags: Vec<String>")
            .expect("collection rules parse");
        assert_eq!(
            constraint_encodings(&rules),
            ["min_items=1", "max_items=4", "unique_items=true"]
        );
    }

    #[test]
    fn collection_length_rules_are_folded_into_item_bounds() {
        let mut rules = rules_of("#[min_length(2)] #[max_length(4)] tags: Vec<String>")
            .expect("length rules parse");
        normalize_collection_rules(&mut rules, FieldShape::Collection);
        assert!(rules.min_length.is_none());
        assert!(rules.max_length.is_none());
        assert_eq!(constraint_encodings(&rules), ["min_items=2", "max_items=4"]);
    }

    #[test]
    fn unsupported_patterns_are_rejected_when_the_macro_expands() {
        for (pattern, fragment) in [
            ("", "the pattern is empty"),
            ("^a{2,3}$", "counted repetition"),
            ("^(?:a)$", "not supported"),
            ("^(a$", "unbalanced group"),
            ("^a)$", "unbalanced group"),
            ("^[a-z$", "unterminated character class"),
            ("^*a$", "no preceding expression"),
            (r"^\q$", "the escape `\\q` is not supported"),
            (r"^a\", "lone backslash"),
            ("^a$b$", "`$` is supported only at the end"),
            ("a^b", "`^` is supported only at the start"),
        ] {
            let error = lint_pattern_syntax(pattern)
                .expect_err(&format!("{pattern} must be rejected"))
                .to_string();
            assert!(error.contains(fragment), "{pattern} produced {error}");
        }
    }

    #[test]
    fn supported_patterns_pass_the_compile_time_lint() {
        for pattern in [
            "^[a-z][a-z0-9_]*$",
            r"^(cat|dog)-\d+$",
            r"\w+@\w+\.\w+",
            "^a[^0-9]?$",
            r"^cost\$",
            "^[a-z-]+$",
        ] {
            assert!(
                lint_pattern_syntax(pattern).is_ok(),
                "{pattern} must be accepted"
            );
        }
    }

    #[test]
    fn nested_models_recurse_without_an_explicit_attribute() {
        let expansion = expand_default("struct Order { address: Address, items: Vec<Line> }")
            .expect("model expands");
        assert!(expansion.contains("__blazingly_nested"));
        assert!(expansion.contains("__blazingly_nested_items"));
        assert!(expansion.contains("__blazingly_is_model"));
    }

    #[test]
    fn explicit_nested_stays_accepted_and_still_marks_the_descriptor() {
        let expansion =
            expand_default("struct Order { #[nested] address: Address }").expect("model expands");
        assert!(expansion.contains("ValidationRule :: Nested"));
    }

    #[test]
    fn scalar_fields_without_rules_emit_no_validation_body() {
        let expansion =
            expand_default("struct Order { name: String, count: u32 }").expect("model expands");
        assert!(!expansion.contains("__BlazinglyValue"));
        assert!(!expansion.contains("self . name"));
    }

    #[test]
    fn model_level_validate_with_runs_after_the_field_rules() {
        let arguments = ModelArgs {
            validator: Some(syn::parse_str("checks::validate_window").expect("path parses")),
            ..ModelArgs::default()
        };
        let expansion = expand(
            "struct Window { #[minimum(0)] start: i64, #[minimum(0)] end: i64 }",
            &arguments,
        )
        .expect("model expands");
        let validator = expansion
            .find("checks :: validate_window")
            .expect("the model validator is called");
        let last_field_rule = expansion
            .rfind("check_minimum")
            .expect("field rules are emitted");
        assert!(validator > last_field_rule);
        assert!(expansion.contains("merge_model_violations"));
    }

    #[test]
    fn model_arguments_reject_unknown_keys() {
        let error = rejection_message(syn::parse_str::<ModelArgs>(
            "rename_all = \"camelCase\", frobnicate = \"x\"",
        ));
        assert!(
            error.contains("`borrowed`, `rename_all`, and `validate_with`"),
            "{error}"
        );
    }

    #[test]
    fn snake_case_names_become_camel_case() {
        assert_eq!(snake_to_camel("public_name"), "publicName");
        assert_eq!(snake_to_camel("id"), "id");
        assert_eq!(snake_to_camel("a_b_c"), "aBC");
    }

    fn borrowed_arguments() -> ModelArgs {
        syn::parse_str::<ModelArgs>("borrowed").expect("`borrowed` is a bare flag")
    }

    fn expand_borrowed(source: &str) -> syn::Result<String> {
        expand(source, &borrowed_arguments())
    }

    #[test]
    fn a_borrowed_view_serializes_and_describes_itself_but_never_parses() {
        let expansion = expand_borrowed("struct View<'store> { title: &'store str }")
            .expect("a borrowed view expands");
        assert!(expansion.contains("Serialize"));
        assert!(
            !expansion.contains("Deserialize"),
            "a borrowed view is an output type"
        );
        assert!(expansion.contains("ApiSchema for View"));
        assert!(
            !expansion.contains("ApiModel for"),
            "a borrowed view is never validated"
        );
        assert!(!expansion.contains("ValidationErrors"));
    }

    #[test]
    fn borrowed_field_types_document_the_schema_their_owned_form_documents() {
        let resolved = |source: &str| {
            let ty = schema_type(&parse_type(source));
            quote!(#ty).to_string().replace(' ', "")
        };
        assert_eq!(resolved("&'store str"), "&str");
        assert_eq!(resolved("Vec<&'store Tag>"), "Vec<Tag>");
        assert_eq!(resolved("Option<&'store str>"), "Option<&str>");
        assert_eq!(resolved("&'store [Tag]"), "::std::vec::Vec<Tag>");
        assert_eq!(resolved("Cow<'store, str>"), "&str");
        assert_eq!(resolved("Page<'store, Tag>"), "Page<Tag>");
        // A type that borrows nothing is left exactly as written.
        assert_eq!(resolved("Vec<Tag>"), "Vec<Tag>");
    }

    #[test]
    fn a_generic_borrowed_envelope_names_one_schema_per_item_type() {
        let expansion = expand_borrowed("struct Page<'store, T> { items: Vec<&'store T> }")
            .expect("a generic borrowed view expands");
        assert!(expansion.contains("__blazingly_schema_name"));
        assert!(expansion.contains("T : :: blazingly :: ApiSchema"));
    }

    #[test]
    fn validation_rules_are_rejected_on_a_borrowed_view() {
        let error = rejection_message(expand_borrowed(
            "struct View<'store> { #[min_length(2)] title: &'store str }",
        ));
        assert!(error.contains("`#[min_length]`"), "{error}");
        assert!(error.contains("never validated"), "{error}");

        let arguments = syn::parse_str::<ModelArgs>("borrowed, validate_with = checks::window")
            .expect("parses");
        let error = rejection_message(expand("struct View<'a> { title: &'a str }", &arguments));
        assert!(error.contains("never validated"), "{error}");
    }

    fn expand_enum(source: &str, arguments: &ModelArgs) -> syn::Result<String> {
        let mut model = syn::parse_str::<ItemEnum>(source).expect("fixture enum parses");
        enum_model_tokens(arguments, &mut model).map(|tokens| tokens.to_string())
    }

    fn model_arguments(source: &str) -> ModelArgs {
        syn::parse_str::<ModelArgs>(source).expect("fixture arguments parse")
    }

    #[test]
    fn a_default_is_recorded_as_json_beside_the_field_rules() {
        let rules = rules_of("#[default(20)] limit: u32").expect("the default parses");
        assert_eq!(metadata_encodings(&rules, false), ["default=20"]);

        let rules = rules_of("#[default(-2.5)] ratio: f64").expect("the default parses");
        assert_eq!(metadata_encodings(&rules, false), ["default=-2.5"]);

        let rules = rules_of("#[default(\"dr\\\"aft\")] status: String").expect("parses");
        assert_eq!(metadata_encodings(&rules, false), [r#"default="dr\"aft""#]);

        let rules = rules_of("#[default(false)] verbose: bool").expect("the default parses");
        assert_eq!(
            metadata_encodings(&rules, true),
            ["default=false", "nullable=true"]
        );
    }

    #[test]
    fn a_default_must_match_the_field_it_fills_in() {
        let message = reject("#[default(\"draft\")] limit: u32", "u32");
        assert!(
            message.contains("a string literal requires a `String` field"),
            "{message}"
        );

        let message = reject("#[default(20)] status: String", "String");
        assert!(
            message.contains("an integer literal requires an integer or floating-point field"),
            "{message}"
        );

        let message = reject("#[default(true)] limit: u32", "u32");
        assert!(
            message.contains("a boolean literal requires a `bool` field"),
            "{message}"
        );

        let message = rejection_message(rules_of("#[default(limit())] limit: u32"));
        assert!(
            message.contains("string, integer, floating-point, or boolean literal"),
            "{message}"
        );
    }

    #[test]
    fn a_defaulted_field_is_not_optional_and_is_no_longer_required() {
        let message = rejection_message(expand_default(
            "struct List { #[default(20)] limit: Option<u32> }",
        ));
        assert!(message.contains("declare it without `Option`"), "{message}");

        let expansion =
            expand_default("struct List { #[default(20)] limit: u32 }").expect("model expands");
        assert!(expansion.contains("__blazingly_default_list_limit"));
        assert!(expansion.contains("serde (default = \"__blazingly_default_list_limit\")"));
        assert!(
            expansion.contains("FieldDescriptor :: new (\"limit\" , false ,"),
            "a field with a default is not required of the client: {expansion}"
        );
    }

    #[test]
    fn an_optional_field_records_its_nullability() {
        let expansion =
            expand_default("struct Article { summary: Option<String> }").expect("model expands");
        assert!(expansion.contains("\"nullable=true\""));

        let expansion = expand_default("struct Article { summary: String }").expect("expands");
        assert!(!expansion.contains("nullable"));
    }

    #[test]
    fn a_value_type_declares_one_reusable_bundle_of_rules() {
        let expansion = expand_default("struct Title (String) ;").expect("a value type expands");
        assert!(expansion.contains("serde (transparent)"));
        assert!(expansion.contains("impl :: blazingly :: ApiConstrained for Title"));
        assert!(expansion.contains("fn into_inner"));

        let mut model = syn::parse_str::<ItemStruct>("#[min_length(8)] struct Title (String) ;")
            .expect("parses");
        let expansion = model_tokens(&ModelArgs::default(), &mut model)
            .expect("a value type with rules expands")
            .to_string();
        assert!(expansion.contains("ValidationRule :: MinLength (8usize)"));
        assert!(expansion.contains("min_length"));
    }

    #[test]
    fn a_value_type_rejects_what_only_a_field_can_carry() {
        let message = rejection_message(expand_default("struct Pair (String , u32) ;"));
        assert!(message.contains("wraps exactly one field"), "{message}");

        let message = rejection_message(expand_default("#[alias(\"t\")] struct Title (String) ;"));
        assert!(
            message.contains("`alias` names an extra wire key"),
            "{message}"
        );

        let message =
            rejection_message(expand_default("#[default(\"x\")] struct Title (String) ;"));
        assert!(message.contains("belongs to the field"), "{message}");

        let message = rejection_message(expand_default("struct Title (Option<String>) ;"));
        assert!(
            message.contains("declare the field that uses it"),
            "{message}"
        );

        let message = rejection_message(expand(
            "struct Title (String) ;",
            &model_arguments("rename_all = \"camelCase\""),
        ));
        assert!(message.contains("`rename_all` renames fields"), "{message}");
    }

    #[test]
    fn an_enumeration_pins_every_variant_to_an_explicit_wire_value() {
        let expansion = expand_enum(
            "enum Language { Uk, Ru, En }",
            &model_arguments("rename_all = \"lowercase\""),
        )
        .expect("an enumeration expands");
        assert!(expansion.contains("serde (rename = \"uk\")"));
        assert!(expansion.contains("\"enum=uk|ru|en\""));
        assert!(expansion.contains("SchemaKind :: String"));
        assert!(expansion.contains("const VARIANTS"));

        let expansion = expand_enum(
            "enum Status { NotFound, #[rename(\"ok\")] Fine }",
            &ModelArgs::default(),
        )
        .expect("an enumeration expands");
        assert!(expansion.contains("serde (rename = \"NotFound\")"));
        assert!(expansion.contains("serde (rename = \"ok\")"));
        assert!(expansion.contains("\"enum=NotFound|ok\""));
    }

    #[test]
    fn enum_rename_rules_match_the_serde_spelling() {
        let cases = [
            ("PascalCase", "NotFound"),
            ("lowercase", "notfound"),
            ("UPPERCASE", "NOTFOUND"),
            ("camelCase", "notFound"),
            ("snake_case", "not_found"),
            ("SCREAMING_SNAKE_CASE", "NOT_FOUND"),
            ("kebab-case", "not-found"),
            ("SCREAMING-KEBAB-CASE", "NOT-FOUND"),
        ];
        for (rule, expected) in cases {
            let rule = RenameRule::parse(rule).expect("the rule is supported");
            assert_eq!(rule.apply("NotFound"), expected);
        }
        assert!(RenameRule::parse("Train-Case").is_none());
    }

    #[test]
    fn an_enumeration_rejects_data_carrying_and_ambiguous_variants() {
        let message = rejection_message(expand_enum(
            "enum Payload { Text(String) }",
            &ModelArgs::default(),
        ));
        assert!(message.contains("a variant cannot carry data"), "{message}");

        let message = rejection_message(expand_enum(
            "enum Language { Uk, #[rename(\"Uk\")] Ukrainian }",
            &ModelArgs::default(),
        ));
        assert!(message.contains("declared twice"), "{message}");

        let message = rejection_message(expand_enum(
            "enum Language { #[rename(\"a|b\")] Both }",
            &ModelArgs::default(),
        ));
        assert!(message.contains("`|` separates the variants"), "{message}");

        let message = rejection_message(expand_enum("enum Empty { }", &ModelArgs::default()));
        assert!(message.contains("at least one variant"), "{message}");

        let message = rejection_message(expand_enum(
            "enum Language { Uk }",
            &model_arguments("rename_all = \"Train-Case\""),
        ));
        assert!(message.contains("SCREAMING-KEBAB-CASE"), "{message}");
    }

    #[test]
    fn a_field_validator_reports_through_the_undoubled_merge() {
        let expansion = expand_default("struct Window { #[validate_with(checks::at)] at: u32 }")
            .expect("model expands");
        assert!(expansion.contains("merge_field_validation_errors"));
        assert!(!expansion.contains("merge_validation_errors (& mut errors"));
    }

    #[test]
    fn an_owning_model_rejects_generics_and_points_at_the_borrowed_form() {
        let error = rejection_message(expand_default("struct Page<T> { items: Vec<T> }"));
        assert!(error.contains("silently skip validating it"), "{error}");
        assert!(error.contains("#[api_model(borrowed)]"), "{error}");

        let error = rejection_message(expand_default("struct View<'a> { title: &'a str }"));
        assert!(error.contains("cannot borrow from the request"), "{error}");
        assert!(error.contains("#[api_model(borrowed)]"), "{error}");
    }
}
