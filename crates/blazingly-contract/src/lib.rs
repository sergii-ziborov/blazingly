#![forbid(unsafe_code)]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The current canonical contract encoding version.
pub const CURRENT_CONTRACT_FORMAT_VERSION: ContractFormatVersion = ContractFormatVersion::new(1, 0);

/// Version of the canonical Blazingly contract format.
///
/// Major versions may change canonical encoding or compatibility semantics.
/// Minor versions may add backward-compatible metadata.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct ContractFormatVersion {
    pub major: u16,
    pub minor: u16,
}

impl ContractFormatVersion {
    #[must_use]
    pub const fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }
}

impl Default for ContractFormatVersion {
    fn default() -> Self {
        CURRENT_CONTRACT_FORMAT_VERSION
    }
}

impl fmt::Display for ContractFormatVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}", self.major, self.minor)
    }
}

/// Stable SHA-256 identity of one canonical operation contract.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct ContractFingerprint {
    pub format_version: ContractFormatVersion,
    digest: [u8; 32],
}

impl ContractFingerprint {
    #[must_use]
    pub const fn new(format_version: ContractFormatVersion, digest: [u8; 32]) -> Self {
        Self {
            format_version,
            digest,
        }
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.digest
    }
}

impl fmt::Display for ContractFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "blazingly-contract-v{}-sha256:",
            self.format_version
        )?;
        for byte in self.digest {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Compatibility impact of one semantic contract change.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompatibilityImpact {
    Breaking,
    NonBreaking,
    Metadata,
}

/// One deterministic compatibility finding.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CompatibilityChange {
    pub impact: CompatibilityImpact,
    pub path: String,
    pub code: String,
    pub message: String,
}

impl CompatibilityChange {
    #[must_use]
    pub fn new(
        impact: CompatibilityImpact,
        path: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            impact,
            path: path.into(),
            code: code.into(),
            message: message.into(),
        }
    }
}

/// Semantic compatibility report between two versions of one operation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CompatibilityReport {
    pub previous: ContractFingerprint,
    pub current: ContractFingerprint,
    pub changes: Vec<CompatibilityChange>,
}

impl CompatibilityReport {
    #[must_use]
    pub fn is_backward_compatible(&self) -> bool {
        !self
            .changes
            .iter()
            .any(|change| change.impact == CompatibilityImpact::Breaking)
    }

    pub fn breaking_changes(&self) -> impl Iterator<Item = &CompatibilityChange> {
        self.changes
            .iter()
            .filter(|change| change.impact == CompatibilityImpact::Breaking)
    }
}

/// Entry point for semantic contract comparison.
#[derive(Clone, Copy, Debug, Default)]
pub struct Compatibility;

/// A stable, human-readable operation identity such as `users.create`.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OperationId(String);

impl OperationId {
    /// Creates an operation identity.
    ///
    /// Identities are deliberately conservative because they will later be
    /// used in generated clients, contract snapshots, and internal service
    /// calls.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidOperationId`] when the identity is empty or contains
    /// characters outside the stable operation-id alphabet.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidOperationId> {
        let value = value.into();
        let valid = !value.is_empty()
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'));

        if valid {
            Ok(Self(value))
        } else {
            Err(InvalidOperationId { value })
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OperationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// An invalid operation identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvalidOperationId {
    value: String,
}

impl InvalidOperationId {
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }
}

impl fmt::Display for InvalidOperationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "operation id {:?} must contain only ASCII letters, digits, '.', '-' or '_'",
            self.value
        )
    }
}

#[cfg(feature = "std")]
impl std::error::Error for InvalidOperationId {}

/// Transport-independent JSON shape.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaKind {
    String,
    Binary,
    Integer,
    Number,
    Boolean,
    Array(Box<SchemaKind>),
    Object,
    Any,
}

/// Validation generated as native Rust code by `#[api_model]`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationRule {
    MinLength(usize),
    MaxLength(usize),
    Email,
}

/// One field in an API model.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FieldDescriptor {
    pub name: String,
    pub required: bool,
    pub ty: TypeDescriptor,
    pub validation: Vec<ValidationRule>,
}

impl FieldDescriptor {
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        required: bool,
        ty: TypeDescriptor,
        validation: Vec<ValidationRule>,
    ) -> Self {
        Self {
            name: name.into(),
            required,
            ty,
            validation,
        }
    }
}

/// A complete model used by validation, `OpenAPI`, MCP, and Markdown.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelDescriptor {
    pub name: String,
    pub fields: Vec<FieldDescriptor>,
}

impl ModelDescriptor {
    #[must_use]
    pub fn new(name: impl Into<String>, fields: Vec<FieldDescriptor>) -> Self {
        Self {
            name: name.into(),
            fields,
        }
    }
}

/// The type identity and schema captured by the Rust frontend.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TypeDescriptor {
    pub rust_name: String,
    pub schema: SchemaKind,
    pub model: Option<Box<ModelDescriptor>>,
    /// Complete item contract for collections.
    ///
    /// `SchemaKind::Array` retains the lightweight JSON shape while this field
    /// preserves nested model metadata for validation, `OpenAPI`, MCP, and
    /// compatibility analysis.
    #[serde(default)]
    pub items: Option<Box<TypeDescriptor>>,
}

impl TypeDescriptor {
    #[must_use]
    pub fn new(rust_name: impl Into<String>) -> Self {
        Self {
            rust_name: rust_name.into(),
            schema: SchemaKind::Any,
            model: None,
            items: None,
        }
    }

    #[must_use]
    pub fn scalar(rust_name: impl Into<String>, schema: SchemaKind) -> Self {
        Self {
            rust_name: rust_name.into(),
            schema,
            model: None,
            items: None,
        }
    }

    #[must_use]
    pub fn model(model: ModelDescriptor) -> Self {
        Self {
            rust_name: model.name.clone(),
            schema: SchemaKind::Object,
            model: Some(Box::new(model)),
            items: None,
        }
    }
}

/// A model that can describe and validate itself without runtime reflection.
pub trait ApiModel {
    fn model_descriptor() -> ModelDescriptor;

    /// Validates the model using generated native Rust checks.
    ///
    /// # Errors
    ///
    /// Returns all field violations found in the model.
    fn validate(&self) -> Result<(), ValidationErrors>;
}

/// A Rust type that can participate in an operation schema.
pub trait ApiSchema {
    fn type_descriptor() -> TypeDescriptor;

    /// Validates a decoded operation argument.
    ///
    /// Primitive schemas are valid by default. Models generated with
    /// `#[api_model]` override this through their [`ApiModel`] implementation.
    ///
    /// # Errors
    ///
    /// Returns all model validation failures.
    fn validate_input(&self) -> Result<(), ValidationErrors> {
        Ok(())
    }
}

impl<T: ApiModel> ApiSchema for T {
    fn type_descriptor() -> TypeDescriptor {
        TypeDescriptor::model(T::model_descriptor())
    }

    fn validate_input(&self) -> Result<(), ValidationErrors> {
        self.validate()
    }
}

impl ApiSchema for String {
    fn type_descriptor() -> TypeDescriptor {
        TypeDescriptor::scalar("String", SchemaKind::String)
    }
}

impl ApiSchema for &str {
    fn type_descriptor() -> TypeDescriptor {
        TypeDescriptor::scalar("&str", SchemaKind::String)
    }
}

impl ApiSchema for bool {
    fn type_descriptor() -> TypeDescriptor {
        TypeDescriptor::scalar("bool", SchemaKind::Boolean)
    }
}

macro_rules! integer_schemas {
    ($($type:ty),+ $(,)?) => {
        $(
            impl ApiSchema for $type {
                fn type_descriptor() -> TypeDescriptor {
                    TypeDescriptor::scalar(stringify!($type), SchemaKind::Integer)
                }
            }
        )+
    };
}

integer_schemas!(
    u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128, isize
);

macro_rules! number_schemas {
    ($($type:ty),+ $(,)?) => {
        $(
            impl ApiSchema for $type {
                fn type_descriptor() -> TypeDescriptor {
                    TypeDescriptor::scalar(stringify!($type), SchemaKind::Number)
                }
            }
        )+
    };
}

number_schemas!(f32, f64);

impl<T: ApiSchema> ApiSchema for Vec<T> {
    fn type_descriptor() -> TypeDescriptor {
        let item = T::type_descriptor();
        TypeDescriptor {
            rust_name: alloc::format!("Vec<{}>", item.rust_name),
            schema: SchemaKind::Array(Box::new(item.schema.clone())),
            model: None,
            items: Some(Box::new(item)),
        }
    }
}

impl<T: ApiSchema> ApiSchema for Option<T> {
    fn type_descriptor() -> TypeDescriptor {
        T::type_descriptor()
    }
}

/// One typed model-validation failure.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FieldViolation {
    pub field: String,
    pub code: String,
    pub message: String,
}

/// All model-validation failures collected in one pass.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ValidationErrors {
    violations: Vec<FieldViolation>,
}

/// A typed domain failure shared by HTTP and MCP projections.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OperationFailure {
    pub status: u16,
    pub code: String,
    pub message: String,
    pub details: Option<Vec<u8>>,
    pub headers: Vec<ResponseHeader>,
}

impl OperationFailure {
    #[must_use]
    pub fn new(status: u16, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
            details: None,
            headers: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_details(mut self, details: Vec<u8>) -> Self {
        self.details = Some(details);
        self
    }

    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push(ResponseHeader::new(name, value));
        self
    }
}

/// A response construction failure that must be redacted by transports.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponseBuildError {
    pub code: String,
    pub message: String,
}

impl ResponseBuildError {
    #[must_use]
    pub fn serialization_failed() -> Self {
        Self {
            code: "serialization_failed".to_string(),
            message: "operation response could not be serialized".to_string(),
        }
    }
}

/// A user-declared operation error with stable transport semantics.
pub trait ApiError {
    fn response_descriptors() -> Vec<ResponseDescriptor>;

    /// Converts the declared error into its transport-neutral failure.
    ///
    /// # Errors
    ///
    /// Returns a response build error when a typed error payload cannot be
    /// serialized.
    fn into_failure(self) -> Result<OperationFailure, ResponseBuildError>;
}

impl ValidationErrors {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            violations: Vec::new(),
        }
    }

    pub fn push(
        &mut self,
        field: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) {
        self.violations.push(FieldViolation {
            field: field.into(),
            code: code.into(),
            message: message.into(),
        });
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.violations.is_empty()
    }

    #[must_use]
    pub fn violations(&self) -> &[FieldViolation] {
        &self.violations
    }
}

/// Conservative, allocation-free email syntax check used by generated code.
#[doc(hidden)]
#[must_use]
pub fn is_email(value: &str) -> bool {
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && domain.contains('.')
        && !value.chars().any(char::is_whitespace)
}

/// A single successful or error response declared by an operation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResponseDescriptor {
    pub status: u16,
    pub body: Option<TypeDescriptor>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub headers: Vec<ResponseHeader>,
}

/// A response header emitted without transport-specific dependencies.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResponseHeader {
    pub name: String,
    pub value: String,
}

impl ResponseHeader {
    #[must_use]
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

/// The transport-neutral source of one operation argument.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputSource {
    Path,
    Query,
    Header,
    Cookie,
    Json,
    Form,
    Multipart,
    File,
}

/// One typed operation argument and its HTTP extraction source.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InputDescriptor {
    pub name: String,
    pub source: InputSource,
    pub required: bool,
    pub ty: TypeDescriptor,
}

impl InputDescriptor {
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        source: InputSource,
        required: bool,
        ty: TypeDescriptor,
    ) -> Self {
        Self {
            name: name.into(),
            source,
            required,
            ty,
        }
    }
}

impl ResponseDescriptor {
    #[must_use]
    pub fn success(status: u16, body: Option<TypeDescriptor>) -> Self {
        Self {
            status,
            body,
            error_code: None,
            error_message: None,
            headers: Vec::new(),
        }
    }

    #[must_use]
    pub fn error(
        status: u16,
        code: impl Into<String>,
        message: impl Into<String>,
        body: Option<TypeDescriptor>,
    ) -> Self {
        Self {
            status,
            body,
            error_code: Some(code.into()),
            error_message: Some(message.into()),
            headers: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_headers(mut self, headers: Vec<ResponseHeader>) -> Self {
        self.headers = headers;
        self
    }
}

/// Agent-visible risk associated with invoking an operation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationRisk {
    #[default]
    Read,
    Write,
    Destructive,
}

/// Whether an agent must ask for confirmation before invoking an operation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confirmation {
    #[default]
    Never,
    Required,
}

/// Protocol-neutral metadata used by MCP and other agent transports.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentPolicy {
    pub risk: OperationRisk,
    pub confirmation: Confirmation,
    pub idempotent: bool,
}

/// How much operation output may be exposed to an agent.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputExposure {
    #[default]
    Full,
    SummaryOnly,
    None,
}

/// MCP tool semantics declared alongside an operation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct McpToolDescriptor {
    pub name: String,
    pub description: String,
    pub expose_output: OutputExposure,
}

impl McpToolDescriptor {
    #[must_use]
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            expose_output: OutputExposure::Full,
        }
    }

    #[must_use]
    pub const fn with_output_exposure(mut self, exposure: OutputExposure) -> Self {
        self.expose_output = exposure;
        self
    }
}

/// A typed dependency declared by an operation handler.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DependencyDescriptor {
    pub rust_name: String,
}

impl DependencyDescriptor {
    #[must_use]
    pub fn new(rust_name: impl Into<String>) -> Self {
        Self {
            rust_name: rust_name.into(),
        }
    }
}

/// HTTP location used by an API-key security scheme.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecurityLocation {
    Header,
    Query,
    Cookie,
}

/// Transport-independent description of an application security scheme.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SecuritySchemeKind {
    ApiKey {
        location: SecurityLocation,
        name: String,
    },
    Http {
        scheme: String,
        bearer_format: Option<String>,
    },
    OAuth2 {
        authorization_url: Option<String>,
        token_url: Option<String>,
        scopes: Vec<String>,
    },
    OpenIdConnect {
        discovery_url: String,
    },
    MutualTls,
}

/// Named security scheme registered by an application.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SecuritySchemeDescriptor {
    pub name: String,
    pub description: Option<String>,
    pub kind: SecuritySchemeKind,
}

impl SecuritySchemeDescriptor {
    #[must_use]
    pub fn new(name: impl Into<String>, kind: SecuritySchemeKind) -> Self {
        Self {
            name: name.into(),
            description: None,
            kind,
        }
    }

    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// Security scheme and scopes required by one operation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SecurityRequirement {
    pub scheme: String,
    pub scopes: Vec<String>,
}

impl SecurityRequirement {
    #[must_use]
    pub fn new(scheme: impl Into<String>) -> Self {
        Self {
            scheme: scheme.into(),
            scopes: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_scopes(mut self, scopes: Vec<String>) -> Self {
        self.scopes = scopes;
        self
    }
}

/// The protocol-neutral semantic contract for one operation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OperationContract {
    #[serde(default)]
    pub format_version: ContractFormatVersion,
    pub id: OperationId,
    pub summary: String,
    /// Deprecated single-body view retained for serialized snapshot migration.
    ///
    /// Canonical encoding uses `inputs`, which is the source of truth.
    pub input: Option<TypeDescriptor>,
    pub inputs: Vec<InputDescriptor>,
    #[serde(default)]
    pub dependencies: Vec<DependencyDescriptor>,
    #[serde(default)]
    pub security: Vec<SecurityRequirement>,
    pub responses: Vec<ResponseDescriptor>,
    #[serde(default)]
    pub agent: AgentPolicy,
    #[serde(default)]
    pub mcp: Option<McpToolDescriptor>,
}

impl OperationContract {
    /// Creates a protocol-neutral operation contract.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidOperationId`] when `id` is not a valid stable
    /// operation identity.
    pub fn new(
        id: impl Into<String>,
        summary: impl Into<String>,
        input: Option<TypeDescriptor>,
        responses: Vec<ResponseDescriptor>,
    ) -> Result<Self, InvalidOperationId> {
        let inputs = input
            .as_ref()
            .map(|input| {
                vec![InputDescriptor::new(
                    "body",
                    InputSource::Json,
                    true,
                    input.clone(),
                )]
            })
            .unwrap_or_default();
        Ok(Self {
            format_version: CURRENT_CONTRACT_FORMAT_VERSION,
            id: OperationId::new(id)?,
            summary: summary.into(),
            input,
            inputs,
            dependencies: Vec::new(),
            security: Vec::new(),
            responses,
            agent: AgentPolicy::default(),
            mcp: None,
        })
    }

    #[must_use]
    pub fn with_agent_policy(mut self, policy: AgentPolicy) -> Self {
        self.agent = policy;
        self
    }

    #[must_use]
    pub fn with_inputs(mut self, inputs: Vec<InputDescriptor>) -> Self {
        self.input = inputs
            .iter()
            .find(|input| input.source == InputSource::Json)
            .map(|input| input.ty.clone());
        self.inputs = inputs;
        self
    }

    #[must_use]
    pub fn with_dependencies(mut self, dependencies: Vec<DependencyDescriptor>) -> Self {
        self.dependencies = dependencies;
        self
    }

    #[must_use]
    pub fn with_security(mut self, security: Vec<SecurityRequirement>) -> Self {
        self.security = security;
        self
    }

    #[must_use]
    pub fn with_mcp_tool(mut self, tool: McpToolDescriptor) -> Self {
        self.mcp = Some(tool);
        self
    }

    /// Returns the deterministic, versioned canonical binary representation.
    ///
    /// The encoding deliberately excludes the deprecated `input` mirror and
    /// uses `inputs` as its only source of truth.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut encoder = CanonicalEncoder::new();
        encoder.bytes(b"blazingly.operation-contract");
        encoder.u16(self.format_version.major);
        encoder.u16(self.format_version.minor);
        encoder.operation(self);
        encoder.finish()
    }

    /// Returns the SHA-256 identity of the canonical representation.
    #[must_use]
    pub fn fingerprint(&self) -> ContractFingerprint {
        let digest = Sha256::digest(self.canonical_bytes());
        let mut bytes = [0_u8; 32];
        bytes.copy_from_slice(&digest);
        ContractFingerprint::new(self.format_version, bytes)
    }
}

struct CanonicalEncoder {
    output: Vec<u8>,
}

impl CanonicalEncoder {
    fn new() -> Self {
        Self { output: Vec::new() }
    }

    fn finish(self) -> Vec<u8> {
        self.output
    }

    fn u8(&mut self, value: u8) {
        self.output.push(value);
    }

    fn u16(&mut self, value: u16) {
        self.output.extend_from_slice(&value.to_be_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.output.extend_from_slice(&value.to_be_bytes());
    }

    fn bool(&mut self, value: bool) {
        self.u8(u8::from(value));
    }

    fn bytes(&mut self, value: &[u8]) {
        self.u64(value.len() as u64);
        self.output.extend_from_slice(value);
    }

    fn string(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }

    fn optional_string(&mut self, value: Option<&str>) {
        match value {
            Some(value) => {
                self.u8(1);
                self.string(value);
            }
            None => self.u8(0),
        }
    }

    /// Encodes a semantically unordered collection as sorted item encodings.
    fn sorted<T>(&mut self, values: &[T], mut encode: impl FnMut(&mut Self, &T)) {
        let mut encoded = Vec::with_capacity(values.len());
        for value in values {
            let mut item = Self::new();
            encode(&mut item, value);
            encoded.push(item.finish());
        }
        encoded.sort();

        self.u64(encoded.len() as u64);
        for item in encoded {
            self.bytes(&item);
        }
    }

    fn operation(&mut self, contract: &OperationContract) {
        self.string(contract.id.as_str());
        self.string(&contract.summary);
        self.sorted(&contract.inputs, Self::input);
        self.sorted(&contract.dependencies, |encoder, dependency| {
            encoder.string(&dependency.rust_name);
        });
        self.sorted(&contract.security, Self::security_requirement);
        self.sorted(&contract.responses, Self::response);
        self.agent(&contract.agent);
        match &contract.mcp {
            Some(tool) => {
                self.u8(1);
                self.mcp(tool);
            }
            None => self.u8(0),
        }
    }

    fn input(&mut self, input: &InputDescriptor) {
        self.u8(input_source_tag(input.source));
        if input.source == InputSource::Header {
            self.string(&input.name.to_ascii_lowercase());
        } else {
            self.string(&input.name);
        }
        self.bool(input.required);
        self.ty(&input.ty);
    }

    fn ty(&mut self, ty: &TypeDescriptor) {
        self.string(&ty.rust_name);
        self.schema(&ty.schema);
        match &ty.model {
            Some(model) => {
                self.u8(1);
                self.model(model);
            }
            None => self.u8(0),
        }
        match &ty.items {
            Some(items) => {
                self.u8(1);
                self.ty(items);
            }
            None => self.u8(0),
        }
    }

    fn schema(&mut self, schema: &SchemaKind) {
        match schema {
            SchemaKind::String => self.u8(0),
            SchemaKind::Binary => self.u8(1),
            SchemaKind::Integer => self.u8(2),
            SchemaKind::Number => self.u8(3),
            SchemaKind::Boolean => self.u8(4),
            SchemaKind::Array(item) => {
                self.u8(5);
                self.schema(item);
            }
            SchemaKind::Object => self.u8(6),
            SchemaKind::Any => self.u8(7),
        }
    }

    fn model(&mut self, model: &ModelDescriptor) {
        self.string(&model.name);
        self.sorted(&model.fields, Self::field);
    }

    fn field(&mut self, field: &FieldDescriptor) {
        self.string(&field.name);
        self.bool(field.required);
        self.ty(&field.ty);
        self.sorted(&field.validation, Self::validation);
    }

    fn validation(&mut self, rule: &ValidationRule) {
        match rule {
            ValidationRule::MinLength(value) => {
                self.u8(0);
                self.u64(*value as u64);
            }
            ValidationRule::MaxLength(value) => {
                self.u8(1);
                self.u64(*value as u64);
            }
            ValidationRule::Email => self.u8(2),
        }
    }

    fn response(&mut self, response: &ResponseDescriptor) {
        self.u16(response.status);
        match &response.body {
            Some(body) => {
                self.u8(1);
                self.ty(body);
            }
            None => self.u8(0),
        }
        self.optional_string(response.error_code.as_deref());
        self.optional_string(response.error_message.as_deref());
        self.sorted(&response.headers, |encoder, header| {
            encoder.string(&header.name.to_ascii_lowercase());
            encoder.string(&header.value);
        });
    }

    fn security_requirement(&mut self, requirement: &SecurityRequirement) {
        self.string(&requirement.scheme);
        self.sorted(&requirement.scopes, |encoder, scope| encoder.string(scope));
    }

    fn agent(&mut self, policy: &AgentPolicy) {
        self.u8(risk_tag(policy.risk));
        self.u8(confirmation_tag(policy.confirmation));
        self.bool(policy.idempotent);
    }

    fn mcp(&mut self, tool: &McpToolDescriptor) {
        self.string(&tool.name);
        self.string(&tool.description);
        self.u8(exposure_tag(tool.expose_output));
    }
}

impl Compatibility {
    /// Compares two operation contracts using transport-neutral compatibility
    /// rules. Findings are returned in deterministic path/code order.
    #[must_use]
    pub fn compare(
        previous: &OperationContract,
        current: &OperationContract,
    ) -> CompatibilityReport {
        let previous_fingerprint = previous.fingerprint();
        let current_fingerprint = current.fingerprint();
        let mut report = CompatibilityReport {
            previous: previous_fingerprint,
            current: current_fingerprint,
            changes: Vec::new(),
        };

        if previous_fingerprint == current_fingerprint {
            return report;
        }

        compare_versions(previous, current, &mut report.changes);
        compare_identity(previous, current, &mut report.changes);
        compare_inputs(previous, current, &mut report.changes);
        compare_dependencies(previous, current, &mut report.changes);
        compare_security(previous, current, &mut report.changes);
        compare_responses(previous, current, &mut report.changes);
        compare_agent(previous, current, &mut report.changes);
        compare_mcp(previous, current, &mut report.changes);

        report.changes.sort_by(|left, right| {
            (&left.path, &left.code, impact_tag(left.impact)).cmp(&(
                &right.path,
                &right.code,
                impact_tag(right.impact),
            ))
        });
        report
    }
}

fn compare_versions(
    previous: &OperationContract,
    current: &OperationContract,
    changes: &mut Vec<CompatibilityChange>,
) {
    if previous.format_version.major != current.format_version.major {
        changes.push(change(
            CompatibilityImpact::Breaking,
            "format_version",
            "format_major_changed",
            format!(
                "canonical contract format changed from {} to {}",
                previous.format_version, current.format_version
            ),
        ));
    } else if previous.format_version.minor != current.format_version.minor {
        changes.push(change(
            CompatibilityImpact::Metadata,
            "format_version",
            "format_minor_changed",
            format!(
                "canonical contract format changed from {} to {}",
                previous.format_version, current.format_version
            ),
        ));
    }
}

fn compare_identity(
    previous: &OperationContract,
    current: &OperationContract,
    changes: &mut Vec<CompatibilityChange>,
) {
    if previous.id != current.id {
        changes.push(change(
            CompatibilityImpact::Breaking,
            "id",
            "operation_id_changed",
            format!(
                "operation id changed from {} to {}",
                previous.id, current.id
            ),
        ));
    }
    if previous.summary != current.summary {
        changes.push(change(
            CompatibilityImpact::Metadata,
            "summary",
            "summary_changed",
            "operation summary changed",
        ));
    }
}

fn compare_inputs(
    previous: &OperationContract,
    current: &OperationContract,
    changes: &mut Vec<CompatibilityChange>,
) {
    for old in &previous.inputs {
        let path = input_path(old);
        let Some(new) = current
            .inputs
            .iter()
            .find(|candidate| same_input_key(old, candidate))
        else {
            changes.push(change(
                CompatibilityImpact::Breaking,
                path,
                "input_removed",
                "an accepted operation input was removed",
            ));
            continue;
        };

        match (old.required, new.required) {
            (false, true) => changes.push(change(
                CompatibilityImpact::Breaking,
                format!("{path}.required"),
                "input_became_required",
                "an optional input became required",
            )),
            (true, false) => changes.push(change(
                CompatibilityImpact::NonBreaking,
                format!("{path}.required"),
                "input_became_optional",
                "a required input became optional",
            )),
            _ => {}
        }
        compare_type(
            &old.ty,
            &new.ty,
            &format!("{path}.type"),
            TypeDirection::Input,
            changes,
        );
    }

    for new in &current.inputs {
        if previous
            .inputs
            .iter()
            .any(|candidate| same_input_key(candidate, new))
        {
            continue;
        }
        changes.push(change(
            if new.required {
                CompatibilityImpact::Breaking
            } else {
                CompatibilityImpact::NonBreaking
            },
            input_path(new),
            if new.required {
                "required_input_added"
            } else {
                "optional_input_added"
            },
            if new.required {
                "a new required operation input was added"
            } else {
                "a new optional operation input was added"
            },
        ));
    }
}

fn compare_dependencies(
    previous: &OperationContract,
    current: &OperationContract,
    changes: &mut Vec<CompatibilityChange>,
) {
    for old in &previous.dependencies {
        if !current
            .dependencies
            .iter()
            .any(|new| new.rust_name == old.rust_name)
        {
            changes.push(change(
                CompatibilityImpact::NonBreaking,
                format!("dependencies.{}", old.rust_name),
                "dependency_removed",
                "an operation dependency was removed",
            ));
        }
    }
    for new in &current.dependencies {
        if !previous
            .dependencies
            .iter()
            .any(|old| old.rust_name == new.rust_name)
        {
            changes.push(change(
                CompatibilityImpact::Breaking,
                format!("dependencies.{}", new.rust_name),
                "dependency_added",
                "a new operation dependency must be provided",
            ));
        }
    }
}

fn compare_security(
    previous: &OperationContract,
    current: &OperationContract,
    changes: &mut Vec<CompatibilityChange>,
) {
    for old in &previous.security {
        let Some(new) = current
            .security
            .iter()
            .find(|candidate| candidate.scheme == old.scheme)
        else {
            changes.push(change(
                CompatibilityImpact::NonBreaking,
                format!("security.{}", old.scheme),
                "security_requirement_removed",
                "a security requirement was removed",
            ));
            continue;
        };
        compare_string_set(
            &old.scopes,
            &new.scopes,
            &format!("security.{}.scopes", old.scheme),
            "security_scope",
            CompatibilityImpact::Breaking,
            CompatibilityImpact::NonBreaking,
            changes,
        );
    }
    for new in &current.security {
        if !previous.security.iter().any(|old| old.scheme == new.scheme) {
            changes.push(change(
                CompatibilityImpact::Breaking,
                format!("security.{}", new.scheme),
                "security_requirement_added",
                "a new security requirement was added",
            ));
        }
    }
}

fn compare_responses(
    previous: &OperationContract,
    current: &OperationContract,
    changes: &mut Vec<CompatibilityChange>,
) {
    for old in &previous.responses {
        let path = response_path(old);
        let Some(new) = current
            .responses
            .iter()
            .find(|candidate| same_response_key(old, candidate))
        else {
            changes.push(change(
                CompatibilityImpact::Breaking,
                path,
                "response_removed",
                "a declared operation response was removed",
            ));
            continue;
        };

        match (&old.body, &new.body) {
            (Some(old), Some(new)) => compare_type(
                old,
                new,
                &format!("{path}.body"),
                TypeDirection::Output,
                changes,
            ),
            (Some(_), None) => changes.push(change(
                CompatibilityImpact::Breaking,
                format!("{path}.body"),
                "response_body_removed",
                "a declared response body was removed",
            )),
            (None, Some(_)) => changes.push(change(
                CompatibilityImpact::NonBreaking,
                format!("{path}.body"),
                "response_body_added",
                "a response body was added",
            )),
            (None, None) => {}
        }

        if old.error_message != new.error_message {
            changes.push(change(
                CompatibilityImpact::Metadata,
                format!("{path}.error_message"),
                "error_message_changed",
                "a declared error message changed",
            ));
        }
        compare_response_headers(&old.headers, &new.headers, &path, changes);
    }

    for new in &current.responses {
        if !previous
            .responses
            .iter()
            .any(|candidate| same_response_key(candidate, new))
        {
            changes.push(change(
                CompatibilityImpact::NonBreaking,
                response_path(new),
                "response_added",
                "a new operation response was declared",
            ));
        }
    }
}

fn compare_response_headers(
    previous: &[ResponseHeader],
    current: &[ResponseHeader],
    response_path: &str,
    changes: &mut Vec<CompatibilityChange>,
) {
    for old in previous {
        let path = format!("{response_path}.headers.{}", old.name.to_ascii_lowercase());
        let Some(new) = current
            .iter()
            .find(|header| header.name.eq_ignore_ascii_case(&old.name))
        else {
            changes.push(change(
                CompatibilityImpact::Breaking,
                path,
                "response_header_removed",
                "a declared response header was removed",
            ));
            continue;
        };
        if old.value != new.value {
            changes.push(change(
                CompatibilityImpact::Breaking,
                path,
                "response_header_value_changed",
                "a declared response header value changed",
            ));
        }
    }
    for new in current {
        if !previous
            .iter()
            .any(|header| header.name.eq_ignore_ascii_case(&new.name))
        {
            changes.push(change(
                CompatibilityImpact::NonBreaking,
                format!("{response_path}.headers.{}", new.name.to_ascii_lowercase()),
                "response_header_added",
                "a new response header was declared",
            ));
        }
    }
}

fn compare_agent(
    previous: &OperationContract,
    current: &OperationContract,
    changes: &mut Vec<CompatibilityChange>,
) {
    let previous_risk = risk_tag(previous.agent.risk);
    let current_risk = risk_tag(current.agent.risk);
    if previous_risk != current_risk {
        changes.push(change(
            if current_risk > previous_risk {
                CompatibilityImpact::Breaking
            } else {
                CompatibilityImpact::NonBreaking
            },
            "agent.risk",
            "agent_risk_changed",
            "agent invocation risk changed",
        ));
    }

    if previous.agent.confirmation != current.agent.confirmation {
        changes.push(change(
            if current.agent.confirmation == Confirmation::Required {
                CompatibilityImpact::Breaking
            } else {
                CompatibilityImpact::NonBreaking
            },
            "agent.confirmation",
            "agent_confirmation_changed",
            "agent confirmation policy changed",
        ));
    }

    if previous.agent.idempotent != current.agent.idempotent {
        changes.push(change(
            if previous.agent.idempotent {
                CompatibilityImpact::Breaking
            } else {
                CompatibilityImpact::NonBreaking
            },
            "agent.idempotent",
            "agent_idempotency_changed",
            "agent idempotency guarantee changed",
        ));
    }
}

fn compare_mcp(
    previous: &OperationContract,
    current: &OperationContract,
    changes: &mut Vec<CompatibilityChange>,
) {
    match (&previous.mcp, &current.mcp) {
        (Some(old), Some(new)) => {
            if old.name != new.name {
                changes.push(change(
                    CompatibilityImpact::Breaking,
                    "mcp.name",
                    "mcp_tool_name_changed",
                    "MCP tool name changed",
                ));
            }
            if old.description != new.description {
                changes.push(change(
                    CompatibilityImpact::Metadata,
                    "mcp.description",
                    "mcp_description_changed",
                    "MCP tool description changed",
                ));
            }
            let old_exposure = exposure_tag(old.expose_output);
            let new_exposure = exposure_tag(new.expose_output);
            if old_exposure != new_exposure {
                changes.push(change(
                    if new_exposure > old_exposure {
                        CompatibilityImpact::Breaking
                    } else {
                        CompatibilityImpact::NonBreaking
                    },
                    "mcp.expose_output",
                    "mcp_output_exposure_changed",
                    "MCP output exposure policy changed",
                ));
            }
        }
        (Some(_), None) => changes.push(change(
            CompatibilityImpact::Breaking,
            "mcp",
            "mcp_tool_removed",
            "MCP tool projection was removed",
        )),
        (None, Some(_)) => changes.push(change(
            CompatibilityImpact::NonBreaking,
            "mcp",
            "mcp_tool_added",
            "MCP tool projection was added",
        )),
        (None, None) => {}
    }
}

#[derive(Clone, Copy)]
enum TypeDirection {
    Input,
    Output,
}

fn compare_type(
    previous: &TypeDescriptor,
    current: &TypeDescriptor,
    path: &str,
    direction: TypeDirection,
    changes: &mut Vec<CompatibilityChange>,
) {
    if previous.schema != current.schema {
        changes.push(change(
            CompatibilityImpact::Breaking,
            format!("{path}.schema"),
            "schema_changed",
            "wire schema changed",
        ));
        return;
    }
    if previous.rust_name != current.rust_name {
        changes.push(change(
            CompatibilityImpact::Metadata,
            format!("{path}.rust_name"),
            "rust_type_name_changed",
            "Rust type name changed without changing the wire shape",
        ));
    }

    match (&previous.items, &current.items) {
        (Some(old), Some(new)) => {
            compare_type(old, new, &format!("{path}.items"), direction, changes);
        }
        (Some(_), None) => changes.push(change(
            CompatibilityImpact::Breaking,
            format!("{path}.items"),
            "collection_item_contract_removed",
            "collection item contract precision was removed",
        )),
        (None, Some(_)) => changes.push(change(
            match direction {
                TypeDirection::Input => CompatibilityImpact::Breaking,
                TypeDirection::Output => CompatibilityImpact::NonBreaking,
            },
            format!("{path}.items"),
            "collection_item_contract_added",
            "collection item contract precision was added",
        )),
        (None, None) => {}
    }

    match (&previous.model, &current.model) {
        (Some(old), Some(new)) => compare_model(old, new, path, direction, changes),
        (Some(_), None) => changes.push(change(
            CompatibilityImpact::Breaking,
            format!("{path}.model"),
            "model_contract_removed",
            "model contract precision was removed",
        )),
        (None, Some(_)) => changes.push(change(
            match direction {
                TypeDirection::Input => CompatibilityImpact::Breaking,
                TypeDirection::Output => CompatibilityImpact::NonBreaking,
            },
            format!("{path}.model"),
            "model_contract_added",
            "model contract precision was added",
        )),
        (None, None) => {}
    }
}

fn compare_model(
    previous: &ModelDescriptor,
    current: &ModelDescriptor,
    path: &str,
    direction: TypeDirection,
    changes: &mut Vec<CompatibilityChange>,
) {
    if previous.name != current.name {
        changes.push(change(
            CompatibilityImpact::Metadata,
            format!("{path}.model.name"),
            "model_name_changed",
            "model name changed without changing its wire shape",
        ));
    }

    for old in &previous.fields {
        let field_path = format!("{path}.fields.{}", old.name);
        let Some(new) = current.fields.iter().find(|field| field.name == old.name) else {
            changes.push(change(
                CompatibilityImpact::Breaking,
                field_path,
                "model_field_removed",
                "a model field was removed",
            ));
            continue;
        };

        if old.required != new.required {
            let impact = match (direction, old.required, new.required) {
                (TypeDirection::Input, false, true) | (TypeDirection::Output, true, false) => {
                    CompatibilityImpact::Breaking
                }
                _ => CompatibilityImpact::NonBreaking,
            };
            changes.push(change(
                impact,
                format!("{field_path}.required"),
                "model_field_requiredness_changed",
                "model field requiredness changed",
            ));
        }
        compare_type(
            &old.ty,
            &new.ty,
            &format!("{field_path}.type"),
            direction,
            changes,
        );
        compare_validation(
            &old.validation,
            &new.validation,
            &field_path,
            direction,
            changes,
        );
    }

    for new in &current.fields {
        if previous.fields.iter().any(|field| field.name == new.name) {
            continue;
        }
        let impact = match direction {
            TypeDirection::Input if new.required => CompatibilityImpact::Breaking,
            TypeDirection::Input | TypeDirection::Output => CompatibilityImpact::NonBreaking,
        };
        changes.push(change(
            impact,
            format!("{path}.fields.{}", new.name),
            if new.required {
                "required_model_field_added"
            } else {
                "optional_model_field_added"
            },
            "a model field was added",
        ));
    }
}

fn compare_validation(
    previous: &[ValidationRule],
    current: &[ValidationRule],
    path: &str,
    direction: TypeDirection,
    changes: &mut Vec<CompatibilityChange>,
) {
    if matches!(direction, TypeDirection::Output) {
        if previous != current {
            changes.push(change(
                CompatibilityImpact::Metadata,
                format!("{path}.validation"),
                "output_validation_changed",
                "output model validation metadata changed",
            ));
        }
        return;
    }

    compare_min_length(previous, current, path, changes);
    compare_max_length(previous, current, path, changes);
    let old_email = previous
        .iter()
        .any(|rule| matches!(rule, ValidationRule::Email));
    let new_email = current
        .iter()
        .any(|rule| matches!(rule, ValidationRule::Email));
    if old_email != new_email {
        changes.push(change(
            if new_email {
                CompatibilityImpact::Breaking
            } else {
                CompatibilityImpact::NonBreaking
            },
            format!("{path}.validation.email"),
            "email_validation_changed",
            "email validation changed",
        ));
    }
}

fn compare_min_length(
    previous: &[ValidationRule],
    current: &[ValidationRule],
    path: &str,
    changes: &mut Vec<CompatibilityChange>,
) {
    let old = previous.iter().filter_map(|rule| match rule {
        ValidationRule::MinLength(value) => Some(*value),
        ValidationRule::MaxLength(_) | ValidationRule::Email => None,
    });
    let new = current.iter().filter_map(|rule| match rule {
        ValidationRule::MinLength(value) => Some(*value),
        ValidationRule::MaxLength(_) | ValidationRule::Email => None,
    });
    let old = old.max();
    let new = new.max();
    if old != new {
        changes.push(change(
            if new.unwrap_or(0) > old.unwrap_or(0) {
                CompatibilityImpact::Breaking
            } else {
                CompatibilityImpact::NonBreaking
            },
            format!("{path}.validation.min_length"),
            "minimum_length_changed",
            "minimum accepted length changed",
        ));
    }
}

fn compare_max_length(
    previous: &[ValidationRule],
    current: &[ValidationRule],
    path: &str,
    changes: &mut Vec<CompatibilityChange>,
) {
    let old = previous.iter().filter_map(|rule| match rule {
        ValidationRule::MaxLength(value) => Some(*value),
        ValidationRule::MinLength(_) | ValidationRule::Email => None,
    });
    let new = current.iter().filter_map(|rule| match rule {
        ValidationRule::MaxLength(value) => Some(*value),
        ValidationRule::MinLength(_) | ValidationRule::Email => None,
    });
    let old = old.min();
    let new = new.min();
    if old != new {
        let stricter = match (old, new) {
            (None, Some(_)) => true,
            (Some(old), Some(new)) => new < old,
            (Some(_) | None, None) => false,
        };
        changes.push(change(
            if stricter {
                CompatibilityImpact::Breaking
            } else {
                CompatibilityImpact::NonBreaking
            },
            format!("{path}.validation.max_length"),
            "maximum_length_changed",
            "maximum accepted length changed",
        ));
    }
}

fn compare_string_set(
    previous: &[String],
    current: &[String],
    path: &str,
    code_prefix: &str,
    added_impact: CompatibilityImpact,
    removed_impact: CompatibilityImpact,
    changes: &mut Vec<CompatibilityChange>,
) {
    for old in previous {
        if !current.iter().any(|new| new == old) {
            changes.push(change(
                removed_impact,
                format!("{path}.{old}"),
                format!("{code_prefix}_removed"),
                "a declared value was removed",
            ));
        }
    }
    for new in current {
        if !previous.iter().any(|old| old == new) {
            changes.push(change(
                added_impact,
                format!("{path}.{new}"),
                format!("{code_prefix}_added"),
                "a declared value was added",
            ));
        }
    }
}

fn change(
    impact: CompatibilityImpact,
    path: impl Into<String>,
    code: impl Into<String>,
    message: impl Into<String>,
) -> CompatibilityChange {
    CompatibilityChange::new(impact, path, code, message)
}

fn same_input_key(left: &InputDescriptor, right: &InputDescriptor) -> bool {
    left.source == right.source
        && if left.source == InputSource::Header {
            left.name.eq_ignore_ascii_case(&right.name)
        } else {
            left.name == right.name
        }
}

fn input_path(input: &InputDescriptor) -> String {
    let name = if input.source == InputSource::Header {
        input.name.to_ascii_lowercase()
    } else {
        input.name.clone()
    };
    format!("inputs.{}.{}", input_source_name(input.source), name)
}

fn same_response_key(left: &ResponseDescriptor, right: &ResponseDescriptor) -> bool {
    left.status == right.status && left.error_code == right.error_code
}

fn response_path(response: &ResponseDescriptor) -> String {
    match &response.error_code {
        Some(code) => format!("responses.{}.{}", response.status, code),
        None => format!("responses.{}.success", response.status),
    }
}

const fn input_source_tag(source: InputSource) -> u8 {
    match source {
        InputSource::Path => 0,
        InputSource::Query => 1,
        InputSource::Header => 2,
        InputSource::Cookie => 3,
        InputSource::Json => 4,
        InputSource::Form => 5,
        InputSource::Multipart => 6,
        InputSource::File => 7,
    }
}

const fn input_source_name(source: InputSource) -> &'static str {
    match source {
        InputSource::Path => "path",
        InputSource::Query => "query",
        InputSource::Header => "header",
        InputSource::Cookie => "cookie",
        InputSource::Json => "json",
        InputSource::Form => "form",
        InputSource::Multipart => "multipart",
        InputSource::File => "file",
    }
}

const fn risk_tag(risk: OperationRisk) -> u8 {
    match risk {
        OperationRisk::Read => 0,
        OperationRisk::Write => 1,
        OperationRisk::Destructive => 2,
    }
}

const fn confirmation_tag(confirmation: Confirmation) -> u8 {
    match confirmation {
        Confirmation::Never => 0,
        Confirmation::Required => 1,
    }
}

const fn exposure_tag(exposure: OutputExposure) -> u8 {
    match exposure {
        OutputExposure::Full => 0,
        OutputExposure::SummaryOnly => 1,
        OutputExposure::None => 2,
    }
}

const fn impact_tag(impact: CompatibilityImpact) -> u8 {
    match impact {
        CompatibilityImpact::Breaking => 0,
        CompatibilityImpact::NonBreaking => 1,
        CompatibilityImpact::Metadata => 2,
    }
}

impl From<&str> for TypeDescriptor {
    fn from(value: &str) -> Self {
        Self::new(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AgentPolicy, ApiSchema, Compatibility, Confirmation, DependencyDescriptor, FieldDescriptor,
        InputDescriptor, InputSource, McpToolDescriptor, ModelDescriptor, OperationContract,
        OperationId, OperationRisk, ResponseDescriptor, ResponseHeader, SchemaKind,
        SecurityRequirement, TypeDescriptor, ValidationRule,
    };

    #[test]
    fn operation_ids_are_stable_and_conservative() {
        assert!(OperationId::new("users.create-v2").is_ok());
        assert!(OperationId::new("").is_err());
        assert!(OperationId::new("users/create").is_err());
        assert!(OperationId::new("users create").is_err());
    }

    #[test]
    fn canonical_contract_is_independent_of_declaration_order() {
        let original = sample_contract();
        let mut reordered = original.clone();
        reordered.inputs.reverse();
        reordered.dependencies.reverse();
        reordered.security.reverse();
        reordered.security[0].scopes.reverse();
        reordered.responses.reverse();
        reordered
            .responses
            .iter_mut()
            .find(|response| response.status == 201)
            .unwrap()
            .headers
            .reverse();
        let model = reordered.inputs[1].ty.model.as_mut().unwrap();
        model.fields.reverse();
        model.fields[1].validation.reverse();

        assert_eq!(original.canonical_bytes(), reordered.canonical_bytes());
        assert_eq!(original.fingerprint(), reordered.fingerprint());
    }

    #[test]
    fn deprecated_input_mirror_does_not_change_fingerprint() {
        let original = sample_contract();
        let mut changed = original.clone();
        changed.input = Some(TypeDescriptor::scalar("bool", SchemaKind::Boolean));

        assert_eq!(original.fingerprint(), changed.fingerprint());
        assert!(
            Compatibility::compare(&original, &changed)
                .changes
                .is_empty()
        );
    }

    #[test]
    fn canonical_fingerprint_has_a_golden_value() {
        let fingerprint = sample_contract().fingerprint().to_string();
        assert_eq!(
            fingerprint,
            "blazingly-contract-v1.0-sha256:\
             f930a013b78fd44d7253b2b42e9d168318ac9ec206508f2c98383a0fdbb3c891"
        );
    }

    #[test]
    fn compatibility_classifies_input_and_response_evolution() {
        let original = sample_contract();

        let mut optional_input = original.clone();
        optional_input.inputs.push(InputDescriptor::new(
            "x-request-id",
            InputSource::Header,
            false,
            String::type_descriptor(),
        ));
        assert!(Compatibility::compare(&original, &optional_input).is_backward_compatible());

        let mut required_input = original.clone();
        required_input.inputs.push(InputDescriptor::new(
            "tenant",
            InputSource::Path,
            true,
            String::type_descriptor(),
        ));
        let report = Compatibility::compare(&original, &required_input);
        assert!(!report.is_backward_compatible());
        assert!(
            report
                .breaking_changes()
                .any(|change| change.code == "required_input_added")
        );

        let mut removed_response = original.clone();
        removed_response.responses.pop();
        let report = Compatibility::compare(&original, &removed_response);
        assert!(
            report
                .breaking_changes()
                .any(|change| change.code == "response_removed")
        );
    }

    #[test]
    fn compatibility_catches_stricter_model_agent_and_mcp_policies() {
        let original = sample_contract();
        let mut stricter = original.clone();
        let model = stricter.inputs[0].ty.model.as_mut().unwrap();
        model.fields[0]
            .validation
            .push(ValidationRule::MinLength(12));
        stricter.agent.risk = OperationRisk::Destructive;
        stricter.agent.confirmation = Confirmation::Required;
        stricter.agent.idempotent = false;
        stricter.mcp.as_mut().unwrap().expose_output = super::OutputExposure::None;

        let report = Compatibility::compare(&original, &stricter);
        assert!(!report.is_backward_compatible());
        assert!(
            report
                .breaking_changes()
                .any(|change| change.code == "minimum_length_changed")
        );
        assert!(
            report
                .breaking_changes()
                .any(|change| change.code == "agent_risk_changed")
        );
        assert!(
            report
                .breaking_changes()
                .any(|change| change.code == "mcp_output_exposure_changed")
        );
    }

    #[test]
    fn collection_descriptors_retain_nested_model_contracts() {
        let descriptor = Vec::<TestModel>::type_descriptor();

        assert!(matches!(descriptor.schema, SchemaKind::Array(_)));
        assert_eq!(
            descriptor
                .items
                .as_ref()
                .and_then(|item| item.model.as_ref())
                .map(|model| model.name.as_str()),
            Some("TestModel")
        );
    }

    struct TestModel;

    impl super::ApiModel for TestModel {
        fn model_descriptor() -> ModelDescriptor {
            request_model()
        }

        fn validate(&self) -> Result<(), super::ValidationErrors> {
            Ok(())
        }
    }

    fn sample_contract() -> OperationContract {
        OperationContract::new(
            "users.create",
            "Create one user",
            None,
            vec![
                ResponseDescriptor::success(
                    201,
                    Some(TypeDescriptor::model(ModelDescriptor::new(
                        "CreatedUser",
                        vec![
                            FieldDescriptor::new("id", true, String::type_descriptor(), vec![]),
                            FieldDescriptor::new(
                                "display_name",
                                true,
                                String::type_descriptor(),
                                vec![],
                            ),
                        ],
                    ))),
                )
                .with_headers(vec![
                    ResponseHeader::new("Location", "/users/{id}"),
                    ResponseHeader::new("X-Request-Id", "generated"),
                ]),
                ResponseDescriptor::error(409, "user_exists", "user already exists", None),
            ],
        )
        .unwrap()
        .with_inputs(vec![
            InputDescriptor::new(
                "body",
                InputSource::Json,
                true,
                TypeDescriptor::model(request_model()),
            ),
            InputDescriptor::new(
                "x-tenant",
                InputSource::Header,
                true,
                String::type_descriptor(),
            ),
        ])
        .with_dependencies(vec![
            DependencyDescriptor::new("UserRepository"),
            DependencyDescriptor::new("AuditLog"),
        ])
        .with_security(vec![
            SecurityRequirement::new("bearer")
                .with_scopes(vec!["users:write".to_string(), "profile:write".to_string()]),
            SecurityRequirement::new("tenant"),
        ])
        .with_agent_policy(AgentPolicy {
            risk: OperationRisk::Write,
            confirmation: Confirmation::Never,
            idempotent: true,
        })
        .with_mcp_tool(McpToolDescriptor::new(
            "users_create",
            "Create one user account",
        ))
    }

    fn request_model() -> ModelDescriptor {
        ModelDescriptor::new(
            "TestModel",
            vec![
                FieldDescriptor::new(
                    "email",
                    true,
                    String::type_descriptor(),
                    vec![ValidationRule::Email, ValidationRule::MinLength(3)],
                ),
                FieldDescriptor::new(
                    "display_name",
                    false,
                    String::type_descriptor(),
                    vec![ValidationRule::MaxLength(80)],
                ),
            ],
        )
    }
}
