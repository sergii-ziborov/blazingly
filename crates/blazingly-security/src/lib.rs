#![forbid(unsafe_code)]

//! Runtime-neutral authentication, authorization, and session middleware.
//!
//! Security descriptors remain the canonical contract. This crate attaches
//! concrete verifiers to those named schemes and enforces every requirement
//! before request bodies are parsed or handlers are invoked.
//!
//! [`SessionLayer`] adds the write half of a session: a handler mutates
//! [`Session`] and the layer emits the resulting `Set-Cookie` header. The
//! default backend is a stateless signed cookie; attaching a [`SessionStore`]
//! moves the state server side so a session can be revoked before it expires.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use blazingly_core::{
    OperationDescriptor, SecurityLocation, SecurityRequirement, SecuritySchemeDescriptor,
    SecuritySchemeKind,
};
use blazingly_executor::{ExecutableApp, FromInvocation, InputRejection, InvocationInput};
use blazingly_http::{HttpMiddleware, HttpRequestContext, HttpRequestView, Response};
use blazingly_json::{Map, Value, json};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::any::TypeId;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::rc::Rc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

/// Minimum length of a machine-generated shared secret.
pub const MINIMUM_SECRET_BYTES: usize = 32;

/// Minimum length of a human-chosen HTTP Basic password.
pub const MINIMUM_PASSWORD_BYTES: usize = 8;

/// Default session cookie name used by [`SessionLayer`].
pub const DEFAULT_SESSION_COOKIE: &str = "blazingly_session";

/// Default session lifetime in seconds.
pub const DEFAULT_SESSION_TTL_SECONDS: u64 = 86_400;

const DEFAULT_REALM: &str = "api";

/// One identity authenticated through a named contract security scheme.
#[derive(Clone, Debug, PartialEq)]
pub struct AuthenticatedIdentity {
    pub scheme: String,
    pub subject: Option<String>,
    pub scopes: Vec<String>,
    pub claims: Value,
}

impl AuthenticatedIdentity {
    #[must_use]
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|candidate| candidate == scope)
    }
}

/// All identities required by an operation.
///
/// Blazingly's current contract projects the requirements as one `OpenAPI`
/// security object, so multiple entries have AND semantics. A context with no
/// identity is an anonymous request served through an optional scheme.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SecurityContext {
    identities: Vec<AuthenticatedIdentity>,
}

impl SecurityContext {
    #[must_use]
    pub fn identities(&self) -> &[AuthenticatedIdentity] {
        &self.identities
    }

    #[must_use]
    pub fn identity(&self, scheme: &str) -> Option<&AuthenticatedIdentity> {
        self.identities
            .iter()
            .find(|identity| identity.scheme == scheme)
    }

    #[must_use]
    pub fn primary(&self) -> Option<&AuthenticatedIdentity> {
        self.identities.first()
    }

    /// Returns whether any credential was accepted for this request.
    #[must_use]
    pub fn is_authenticated(&self) -> bool {
        !self.identities.is_empty()
    }
}

impl FromInvocation for SecurityContext {
    fn from_invocation(
        input: &InvocationInput<'_>,
        _name: &str,
        _required: bool,
    ) -> Result<Self, InputRejection> {
        let InvocationInput::Http(request) = input else {
            return Err(identity_rejection());
        };
        request
            .extension(TypeId::of::<Self>())
            .and_then(|value| value.downcast_ref::<Self>())
            .cloned()
            .ok_or_else(identity_rejection)
    }
}

impl FromInvocation for AuthenticatedIdentity {
    fn from_invocation(
        input: &InvocationInput<'_>,
        name: &str,
        required: bool,
    ) -> Result<Self, InputRejection> {
        let context = SecurityContext::from_invocation(input, name, required)?;
        context.primary().cloned().ok_or_else(identity_rejection)
    }
}

fn identity_rejection() -> InputRejection {
    InputRejection::new(
        401,
        "authentication_required",
        "authenticated identity is required",
    )
}

/// Failure returned by concrete credential verifiers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthenticationError {
    Missing,
    Invalid(&'static str),
    Internal(&'static str),
}

impl fmt::Display for AuthenticationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => formatter.write_str("credentials are missing"),
            Self::Invalid(reason) => write!(formatter, "credentials are invalid: {reason}"),
            Self::Internal(reason) => write!(formatter, "credential verifier failed: {reason}"),
        }
    }
}

impl std::error::Error for AuthenticationError {}

/// Verifies credentials for one named contract security scheme.
pub trait CredentialVerifier {
    /// Verifies request credentials and returns the authenticated identity.
    ///
    /// # Errors
    ///
    /// Returns whether credentials are absent, invalid, or the verifier itself
    /// could not complete.
    fn verify(
        &self,
        context: &HttpRequestContext<'_>,
        requirement: &SecurityRequirement,
        descriptor: &SecuritySchemeDescriptor,
    ) -> Result<AuthenticatedIdentity, AuthenticationError>;
}

/// Whether a declared scheme rejects an unauthenticated request.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AuthMode {
    /// Missing or invalid credentials end the request with a challenge.
    #[default]
    Required,
    /// Missing or invalid credentials leave the request anonymous and the
    /// operation still runs, mirroring `auto_error=False`.
    Optional,
}

struct SchemeVerifier {
    verifier: Box<dyn CredentialVerifier>,
    mode: AuthMode,
}

enum SchemeOutcome {
    Authenticated(Box<AuthenticatedIdentity>),
    Anonymous,
    Rejected(Box<Response>),
}

/// Enforces the security requirements already stored in each operation
/// descriptor.
#[derive(Default)]
pub struct Security {
    verifiers: BTreeMap<String, SchemeVerifier>,
    realm: Option<String>,
}

impl Security {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            verifiers: BTreeMap::new(),
            realm: None,
        }
    }

    /// Sets the protection space reported in every `WWW-Authenticate` value.
    #[must_use]
    pub fn realm(mut self, realm: impl Into<String>) -> Self {
        self.realm = Some(realm.into());
        self
    }

    /// Attaches a verifier that must succeed for every declared use.
    #[must_use]
    pub fn verifier(
        mut self,
        scheme: impl Into<String>,
        verifier: impl CredentialVerifier + 'static,
    ) -> Self {
        self.verifiers.insert(
            scheme.into(),
            SchemeVerifier {
                verifier: Box::new(verifier),
                mode: AuthMode::Required,
            },
        );
        self
    }

    /// Attaches a verifier whose failure leaves the request anonymous.
    #[must_use]
    pub fn optional_verifier(
        mut self,
        scheme: impl Into<String>,
        verifier: impl CredentialVerifier + 'static,
    ) -> Self {
        self.verifiers.insert(
            scheme.into(),
            SchemeVerifier {
                verifier: Box::new(verifier),
                mode: AuthMode::Optional,
            },
        );
        self
    }

    /// Checks the attached verifiers against the declared contract schemes.
    ///
    /// # Errors
    ///
    /// Returns [`SecurityConfigError::MissingVerifier`] for a declared scheme
    /// without a verifier and [`SecurityConfigError::UnknownScheme`] for a
    /// verifier that no scheme declares.
    pub fn build(self, schemes: &[SecuritySchemeDescriptor]) -> Result<Self, SecurityConfigError> {
        for scheme in self.verifiers.keys() {
            if !schemes.iter().any(|declared| &declared.name == scheme) {
                return Err(SecurityConfigError::UnknownScheme {
                    scheme: scheme.clone(),
                });
            }
        }
        for declared in schemes {
            if !self.verifiers.contains_key(&declared.name) {
                return Err(SecurityConfigError::MissingVerifier {
                    scheme: declared.name.clone(),
                });
            }
        }
        Ok(self)
    }

    /// Checks the attached verifiers against a compiled application.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::build`].
    pub fn build_for(self, app: &ExecutableApp) -> Result<Self, SecurityConfigError> {
        self.build(app.definition().security_schemes())
    }

    fn protection_space(&self) -> &str {
        self.realm.as_deref().unwrap_or(DEFAULT_REALM)
    }

    fn authenticate(
        &self,
        context: &HttpRequestContext<'_>,
        requirement: &SecurityRequirement,
        descriptor: &SecuritySchemeDescriptor,
        entry: &SchemeVerifier,
    ) -> SchemeOutcome {
        let mut identity = match entry.verifier.verify(context, requirement, descriptor) {
            Ok(identity) => identity,
            Err(AuthenticationError::Internal(_)) => {
                return SchemeOutcome::Rejected(Box::new(server_security_error(
                    "credential verifier failed",
                )));
            }
            Err(error) => {
                return if entry.mode == AuthMode::Optional {
                    SchemeOutcome::Anonymous
                } else {
                    SchemeOutcome::Rejected(Box::new(self.challenge_response(descriptor, &error)))
                };
            }
        };
        if requirement
            .scopes
            .iter()
            .any(|scope| !identity.has_scope(scope))
        {
            return if entry.mode == AuthMode::Optional {
                SchemeOutcome::Anonymous
            } else {
                SchemeOutcome::Rejected(Box::new(
                    self.scope_response(descriptor, &requirement.scopes),
                ))
            };
        }
        identity.scheme.clone_from(&requirement.scheme);
        SchemeOutcome::Authenticated(Box::new(identity))
    }

    fn challenge_response(
        &self,
        descriptor: &SecuritySchemeDescriptor,
        error: &AuthenticationError,
    ) -> Response {
        let (message, failure) = match error {
            // RFC 6750 section 3: a request that carried no credential at all
            // must not receive an error code.
            AuthenticationError::Missing | AuthenticationError::Internal(_) => {
                ("credentials are required", None)
            }
            AuthenticationError::Invalid(reason) => (
                "credentials are invalid",
                Some(ChallengeFailure {
                    error: ChallengeError::InvalidToken,
                    description: reason,
                    scopes: &[],
                }),
            ),
        };
        json_error(401, "authentication_required", message).with_header(
            "www-authenticate",
            authenticate_header(descriptor, self.protection_space(), failure.as_ref()),
        )
    }

    fn scope_response(&self, descriptor: &SecuritySchemeDescriptor, scopes: &[String]) -> Response {
        let failure = ChallengeFailure {
            error: ChallengeError::InsufficientScope,
            description: "the request requires higher privileges",
            scopes,
        };
        json_error(
            403,
            "insufficient_scope",
            "authenticated identity lacks a required scope",
        )
        .with_header(
            "www-authenticate",
            authenticate_header(descriptor, self.protection_space(), Some(&failure)),
        )
    }
}

impl HttpMiddleware for Security {
    fn on_operation(
        &self,
        context: &mut HttpRequestContext<'_>,
        operation: &OperationDescriptor,
        security_schemes: &[SecuritySchemeDescriptor],
    ) -> Option<Response> {
        if operation.contract.security.is_empty() {
            return None;
        }
        let mut identities = Vec::with_capacity(operation.contract.security.len());
        for requirement in &operation.contract.security {
            let Some(descriptor) = security_schemes
                .iter()
                .find(|descriptor| descriptor.name == requirement.scheme)
            else {
                return Some(server_security_error("security scheme is not registered"));
            };
            let Some(entry) = self.verifiers.get(&requirement.scheme) else {
                return Some(server_security_error(
                    "security scheme has no runtime verifier",
                ));
            };
            match self.authenticate(context, requirement, descriptor, entry) {
                SchemeOutcome::Authenticated(identity) => identities.push(*identity),
                SchemeOutcome::Anonymous => {}
                SchemeOutcome::Rejected(response) => return Some(*response),
            }
        }
        context.insert_extension(SecurityContext { identities });
        None
    }
}

/// RFC 6750 section 3 error code reported in a `WWW-Authenticate` challenge.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChallengeError {
    InvalidRequest,
    InvalidToken,
    InsufficientScope,
}

impl ChallengeError {
    /// Returns the registered error code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::InvalidToken => "invalid_token",
            Self::InsufficientScope => "insufficient_scope",
        }
    }
}

struct ChallengeFailure<'failure> {
    error: ChallengeError,
    description: &'failure str,
    scopes: &'failure [String],
}

/// Verifies a bearer token independently from its transport extraction.
pub trait TokenVerifier {
    /// Validates a bearer token and decodes its identity.
    ///
    /// # Errors
    ///
    /// Returns an authentication error for malformed, expired, untrusted, or
    /// otherwise invalid tokens.
    fn verify_token(&self, token: &str) -> Result<VerifiedToken, AuthenticationError>;
}

/// Transport-independent result returned by JWT or opaque-token verification.
#[derive(Clone, Debug, PartialEq)]
pub struct VerifiedToken {
    pub subject: Option<String>,
    pub scopes: Vec<String>,
    pub claims: Value,
}

/// `Authorization: Bearer` credential verifier.
#[derive(Clone, Debug)]
pub struct BearerToken<V> {
    verifier: V,
}

impl<V> BearerToken<V> {
    #[must_use]
    pub const fn new(verifier: V) -> Self {
        Self { verifier }
    }
}

impl<V: TokenVerifier> CredentialVerifier for BearerToken<V> {
    fn verify(
        &self,
        context: &HttpRequestContext<'_>,
        _requirement: &SecurityRequirement,
        _descriptor: &SecuritySchemeDescriptor,
    ) -> Result<AuthenticatedIdentity, AuthenticationError> {
        let header = context
            .request()
            .header_value("authorization", 0)
            .ok_or(AuthenticationError::Missing)?;
        let (scheme, token) = header.split_once(' ').ok_or(AuthenticationError::Invalid(
            "malformed authorization header",
        ))?;
        if !scheme.eq_ignore_ascii_case("bearer") || token.is_empty() {
            return Err(AuthenticationError::Invalid(
                "authorization scheme is not bearer",
            ));
        }
        let token = self.verifier.verify_token(token)?;
        Ok(AuthenticatedIdentity {
            scheme: String::new(),
            subject: token.subject,
            scopes: token.scopes,
            claims: token.claims,
        })
    }
}

/// `OAuth2` access-token verifier. Scope enforcement is performed by
/// [`Security`] from the operation contract.
#[derive(Clone, Debug)]
pub struct OAuth2Bearer<V>(BearerToken<V>);

impl<V> OAuth2Bearer<V> {
    #[must_use]
    pub const fn new(verifier: V) -> Self {
        Self(BearerToken::new(verifier))
    }
}

impl<V: TokenVerifier> CredentialVerifier for OAuth2Bearer<V> {
    fn verify(
        &self,
        context: &HttpRequestContext<'_>,
        requirement: &SecurityRequirement,
        descriptor: &SecuritySchemeDescriptor,
    ) -> Result<AuthenticatedIdentity, AuthenticationError> {
        self.0.verify(context, requirement, descriptor)
    }
}

/// Verifies one username and password pair for HTTP Basic.
pub trait PasswordVerifier {
    /// Validates a decoded credential pair.
    ///
    /// # Errors
    ///
    /// Returns an authentication error for an unknown user or a password
    /// mismatch.
    fn verify_password(
        &self,
        username: &str,
        password: &[u8],
    ) -> Result<VerifiedToken, AuthenticationError>;
}

#[derive(Clone)]
struct PasswordEntry {
    username: String,
    password: Vec<u8>,
    scopes: Vec<String>,
}

/// Constant-time static credential table for HTTP Basic.
#[derive(Clone, Default)]
pub struct StaticPasswords {
    entries: Vec<PasswordEntry>,
}

impl fmt::Debug for StaticPasswords {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StaticPasswords")
            .field("entries", &self.entries.len())
            .finish()
    }
}

impl StaticPasswords {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers one credential pair.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty user name or a password shorter than
    /// [`MINIMUM_PASSWORD_BYTES`].
    pub fn user(
        self,
        username: impl Into<String>,
        password: impl AsRef<[u8]>,
    ) -> Result<Self, SecurityConfigError> {
        self.user_with_scopes(username, password, Vec::new())
    }

    /// Registers one credential pair that grants the supplied scopes.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty user name or a password shorter than
    /// [`MINIMUM_PASSWORD_BYTES`].
    pub fn user_with_scopes(
        mut self,
        username: impl Into<String>,
        password: impl AsRef<[u8]>,
        scopes: Vec<String>,
    ) -> Result<Self, SecurityConfigError> {
        let username = username.into();
        let password = password.as_ref();
        if username.is_empty() {
            return Err(SecurityConfigError::EmptyUsername);
        }
        if password.len() < MINIMUM_PASSWORD_BYTES {
            return Err(SecurityConfigError::WeakPassword { username });
        }
        self.entries.push(PasswordEntry {
            username,
            password: password.to_vec(),
            scopes,
        });
        Ok(self)
    }
}

impl PasswordVerifier for StaticPasswords {
    fn verify_password(
        &self,
        username: &str,
        password: &[u8],
    ) -> Result<VerifiedToken, AuthenticationError> {
        let mut matched = None;
        for entry in &self.entries {
            // Both comparisons run for every entry so neither the set of known
            // user names nor a password prefix is observable through timing.
            let hit = constant_time_eq(entry.username.as_bytes(), username.as_bytes())
                & constant_time_eq(&entry.password, password);
            if hit {
                matched = Some(entry);
            }
        }
        let entry = matched.ok_or(AuthenticationError::Invalid(
            "basic credentials do not match",
        ))?;
        Ok(VerifiedToken {
            subject: Some(entry.username.clone()),
            scopes: entry.scopes.clone(),
            claims: json!({"credential": "basic", "sub": entry.username}),
        })
    }
}

/// `Authorization: Basic` credential verifier.
#[derive(Clone, Debug)]
pub struct BasicAuth<V> {
    verifier: V,
}

impl<V> BasicAuth<V> {
    #[must_use]
    pub const fn new(verifier: V) -> Self {
        Self { verifier }
    }
}

impl<V: PasswordVerifier> CredentialVerifier for BasicAuth<V> {
    fn verify(
        &self,
        context: &HttpRequestContext<'_>,
        _requirement: &SecurityRequirement,
        descriptor: &SecuritySchemeDescriptor,
    ) -> Result<AuthenticatedIdentity, AuthenticationError> {
        if !is_basic_scheme(&descriptor.kind) {
            return Err(AuthenticationError::Internal(
                "basic verifier attached to incompatible scheme",
            ));
        }
        let header = context
            .request()
            .header_value("authorization", 0)
            .ok_or(AuthenticationError::Missing)?;
        let (scheme, encoded) = header.split_once(' ').ok_or(AuthenticationError::Invalid(
            "malformed authorization header",
        ))?;
        if !scheme.eq_ignore_ascii_case("basic") {
            return Err(AuthenticationError::Invalid(
                "authorization scheme is not basic",
            ));
        }
        let decoded = decode_basic(encoded.trim())?;
        let decoded = String::from_utf8(decoded)
            .map_err(|_| AuthenticationError::Invalid("basic credentials are not UTF-8"))?;
        let (username, password) = decoded.split_once(':').ok_or(AuthenticationError::Invalid(
            "basic credentials are missing a separator",
        ))?;
        let token = self
            .verifier
            .verify_password(username, password.as_bytes())?;
        Ok(AuthenticatedIdentity {
            scheme: String::new(),
            subject: token.subject,
            scopes: token.scopes,
            claims: token.claims,
        })
    }
}

fn decode_basic(encoded: &str) -> Result<Vec<u8>, AuthenticationError> {
    STANDARD.decode(encoded).map_or_else(
        |_| {
            STANDARD_NO_PAD
                .decode(encoded)
                .map_err(|_| AuthenticationError::Invalid("basic credentials are not base64"))
        },
        Ok,
    )
}

fn is_basic_scheme(kind: &SecuritySchemeKind) -> bool {
    matches!(kind, SecuritySchemeKind::Http { scheme, .. } if scheme.eq_ignore_ascii_case("basic"))
}

/// Validation policy for HMAC-SHA256 JWTs.
#[derive(Clone, Debug)]
pub struct JwtValidation {
    /// Permitted `iss` values. An empty set accepts any issuer.
    pub issuers: BTreeSet<String>,
    /// Permitted `aud` values. An empty set accepts any audience.
    pub audiences: BTreeSet<String>,
    pub leeway_seconds: u64,
    pub require_expiration: bool,
    pub require_not_before: bool,
    pub require_subject: bool,
}

impl Default for JwtValidation {
    fn default() -> Self {
        Self {
            issuers: BTreeSet::new(),
            audiences: BTreeSet::new(),
            leeway_seconds: 30,
            require_expiration: true,
            require_not_before: false,
            require_subject: true,
        }
    }
}

impl JwtValidation {
    /// Adds one permitted issuer.
    #[must_use]
    pub fn issuer(mut self, issuer: impl Into<String>) -> Self {
        self.issuers.insert(issuer.into());
        self
    }

    /// Adds one permitted audience.
    #[must_use]
    pub fn audience(mut self, audience: impl Into<String>) -> Self {
        self.audiences.insert(audience.into());
        self
    }
}

/// Ready-to-use constant-time HS256 JWT verifier and encoder.
#[derive(Clone)]
pub struct JwtHs256 {
    key: Vec<u8>,
    validation: JwtValidation,
}

impl fmt::Debug for JwtHs256 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JwtHs256")
            .field("key", &"<redacted>")
            .field("validation", &self.validation)
            .finish()
    }
}

impl JwtHs256 {
    /// Creates a verifier with a minimum 256-bit secret.
    ///
    /// # Errors
    ///
    /// Returns an error when the HMAC key is shorter than
    /// [`MINIMUM_SECRET_BYTES`].
    pub fn new(key: impl AsRef<[u8]>) -> Result<Self, SecurityConfigError> {
        let key = key.as_ref();
        if key.len() < MINIMUM_SECRET_BYTES {
            return Err(SecurityConfigError::WeakHmacKey);
        }
        Ok(Self {
            key: key.to_vec(),
            validation: JwtValidation::default(),
        })
    }

    #[must_use]
    pub fn validation(mut self, validation: JwtValidation) -> Self {
        self.validation = validation;
        self
    }

    /// Encodes standard claims and additional application claims.
    ///
    /// # Errors
    ///
    /// Returns an error if claims cannot be serialized or the HMAC key cannot
    /// be initialized.
    pub fn encode(&self, claims: &JwtClaims) -> Result<String, AuthenticationError> {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let payload = blazingly_json::to_vec(&claims.as_value())
            .map_err(|_| AuthenticationError::Internal("claims serialization failed"))?;
        let payload = URL_SAFE_NO_PAD.encode(payload);
        let signing_input = format!("{header}.{payload}");
        let mut mac = HmacSha256::new_from_slice(&self.key)
            .map_err(|_| AuthenticationError::Internal("invalid HMAC key"))?;
        mac.update(signing_input.as_bytes());
        let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
        Ok(format!("{signing_input}.{signature}"))
    }
}

impl TokenVerifier for JwtHs256 {
    fn verify_token(&self, token: &str) -> Result<VerifiedToken, AuthenticationError> {
        let mut segments = token.split('.');
        let header = segments
            .next()
            .ok_or(AuthenticationError::Invalid("JWT header is missing"))?;
        let payload = segments
            .next()
            .ok_or(AuthenticationError::Invalid("JWT payload is missing"))?;
        let signature = segments
            .next()
            .ok_or(AuthenticationError::Invalid("JWT signature is missing"))?;
        if segments.next().is_some() {
            return Err(AuthenticationError::Invalid("JWT has too many segments"));
        }
        let header_value: Value = decode_json_segment(header)?;
        if header_value.get("alg").and_then(Value::as_str) != Some("HS256") {
            return Err(AuthenticationError::Invalid("JWT algorithm is not HS256"));
        }
        let signature = URL_SAFE_NO_PAD
            .decode(signature)
            .map_err(|_| AuthenticationError::Invalid("JWT signature is not base64url"))?;
        let mut mac = HmacSha256::new_from_slice(&self.key)
            .map_err(|_| AuthenticationError::Internal("invalid HMAC key"))?;
        mac.update(format!("{header}.{payload}").as_bytes());
        mac.verify_slice(&signature)
            .map_err(|_| AuthenticationError::Invalid("JWT signature does not match"))?;
        let claims: Value = decode_json_segment(payload)?;
        validate_claims(&claims, &self.validation)?;
        Ok(VerifiedToken {
            subject: claims
                .get("sub")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            scopes: claim_scopes(&claims),
            claims,
        })
    }
}

/// Standard claims accepted by [`JwtHs256::encode`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct JwtClaims {
    pub subject: Option<String>,
    pub issuer: Option<String>,
    pub audience: Vec<String>,
    pub expires_at: Option<u64>,
    pub not_before: Option<u64>,
    pub issued_at: Option<u64>,
    pub scopes: Vec<String>,
    pub additional: Map<String, Value>,
}

impl JwtClaims {
    #[must_use]
    pub fn new(subject: impl Into<String>, expires_at: u64) -> Self {
        Self {
            subject: Some(subject.into()),
            expires_at: Some(expires_at),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn scope(mut self, scope: impl Into<String>) -> Self {
        self.scopes.push(scope.into());
        self
    }

    fn as_value(&self) -> Value {
        let mut value = self.additional.clone();
        insert_optional(&mut value, "sub", self.subject.as_ref());
        insert_optional(&mut value, "iss", self.issuer.as_ref());
        if !self.audience.is_empty() {
            value.insert("aud".to_owned(), json!(self.audience));
        }
        insert_optional(&mut value, "exp", self.expires_at.as_ref());
        insert_optional(&mut value, "nbf", self.not_before.as_ref());
        insert_optional(&mut value, "iat", self.issued_at.as_ref());
        if !self.scopes.is_empty() {
            value.insert("scope".to_owned(), Value::String(self.scopes.join(" ")));
        }
        Value::Object(value)
    }
}

/// `SameSite` cookie policy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SameSite {
    Strict,
    #[default]
    Lax,
    None,
}

impl SameSite {
    /// Returns the attribute value written to the cookie.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Strict => "Strict",
            Self::Lax => "Lax",
            Self::None => "None",
        }
    }
}

/// Attributes applied to every issued session cookie.
///
/// The defaults are `Path=/; HttpOnly; Secure; SameSite=Lax`.
#[derive(Clone, Debug)]
pub struct CookieOptions {
    name: String,
    domain: Option<String>,
    path: String,
    same_site: SameSite,
    secure: bool,
    http_only: bool,
    max_age_seconds: Option<u64>,
}

impl CookieOptions {
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            domain: None,
            path: "/".to_owned(),
            same_site: SameSite::Lax,
            secure: true,
            http_only: true,
            max_age_seconds: None,
        }
    }

    /// Restricts the cookie to one domain and its subdomains.
    #[must_use]
    pub fn domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = Some(domain.into());
        self
    }

    /// Restricts the cookie to one path prefix.
    #[must_use]
    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.path = path.into();
        self
    }

    /// Sets the cross-site policy.
    #[must_use]
    pub const fn same_site(mut self, same_site: SameSite) -> Self {
        self.same_site = same_site;
        self
    }

    /// Sets whether the cookie is restricted to TLS connections.
    #[must_use]
    pub const fn secure(mut self, secure: bool) -> Self {
        self.secure = secure;
        self
    }

    /// Sets whether scripts can read the cookie.
    #[must_use]
    pub const fn http_only(mut self, http_only: bool) -> Self {
        self.http_only = http_only;
        self
    }

    /// Sets an explicit `Max-Age`, overriding the session lifetime.
    #[must_use]
    pub const fn max_age(mut self, seconds: u64) -> Self {
        self.max_age_seconds = Some(seconds);
        self
    }

    /// Emits a browser-session cookie with no `Max-Age`.
    #[must_use]
    pub const fn without_max_age(mut self) -> Self {
        self.max_age_seconds = None;
        self
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Checks that the attributes form a cookie a browser will accept.
    ///
    /// # Errors
    ///
    /// Returns [`SecurityConfigError::InsecureSameSiteNone`] for
    /// `SameSite=None` without `Secure`, and
    /// [`SecurityConfigError::InvalidCookieAttribute`] for a name, path, or
    /// domain that cannot be serialized safely.
    pub fn validate(&self) -> Result<(), SecurityConfigError> {
        if self.same_site == SameSite::None && !self.secure {
            return Err(SecurityConfigError::InsecureSameSiteNone);
        }
        if self.name.is_empty() || self.name.bytes().any(is_invalid_cookie_name_byte) {
            return Err(SecurityConfigError::InvalidCookieAttribute { attribute: "name" });
        }
        if self.path.bytes().any(is_invalid_cookie_attribute_byte) {
            return Err(SecurityConfigError::InvalidCookieAttribute { attribute: "path" });
        }
        if self
            .domain
            .as_ref()
            .is_some_and(|domain| domain.bytes().any(is_invalid_cookie_attribute_byte))
        {
            return Err(SecurityConfigError::InvalidCookieAttribute {
                attribute: "domain",
            });
        }
        Ok(())
    }

    fn format(&self, value: &str, max_age_seconds: Option<u64>) -> String {
        let mut cookie = format!("{}={value}", self.name);
        if let Some(domain) = &self.domain {
            cookie.push_str("; Domain=");
            cookie.push_str(domain);
        }
        cookie.push_str("; Path=");
        cookie.push_str(&self.path);
        if let Some(max_age) = max_age_seconds {
            cookie.push_str("; Max-Age=");
            cookie.push_str(&max_age.to_string());
        }
        if self.http_only {
            cookie.push_str("; HttpOnly");
        }
        if self.secure {
            cookie.push_str("; Secure");
        }
        cookie.push_str("; SameSite=");
        cookie.push_str(self.same_site.as_str());
        cookie
    }

    fn clear(&self) -> String {
        self.format("", Some(0))
    }
}

fn is_invalid_cookie_name_byte(byte: u8) -> bool {
    byte.is_ascii_whitespace() || byte.is_ascii_control() || b"();,=\"\\/[]?{}:@".contains(&byte)
}

fn is_invalid_cookie_attribute_byte(byte: u8) -> bool {
    byte == b';' || byte == b',' || byte.is_ascii_control()
}

/// Pending session write decided by a handler.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SessionStatus {
    #[default]
    Unchanged,
    Modified,
    Cleared,
}

#[derive(Debug, Default)]
struct SessionState {
    id: Option<String>,
    subject: Option<String>,
    scopes: Vec<String>,
    data: Map<String, Value>,
    status: SessionStatus,
}

/// Request-scoped session state shared between a handler and [`SessionLayer`].
///
/// Every mutation takes `&self` so a handler can log a user in without owning
/// the response; the layer serializes the result into `Set-Cookie` after the
/// handler returns.
#[derive(Clone, Debug, Default)]
pub struct Session {
    state: Rc<RefCell<SessionState>>,
}

impl Session {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the server-side session id, when the layer keeps one.
    #[must_use]
    pub fn id(&self) -> Option<String> {
        self.state.borrow().id.clone()
    }

    /// Returns the authenticated subject stored in the session.
    #[must_use]
    pub fn subject(&self) -> Option<String> {
        self.state.borrow().subject.clone()
    }

    /// Logs a subject in and schedules a new cookie.
    pub fn set_subject(&self, subject: impl Into<String>) {
        let mut state = self.state.borrow_mut();
        state.subject = Some(subject.into());
        state.status = SessionStatus::Modified;
    }

    /// Returns the scopes granted to this session.
    #[must_use]
    pub fn scopes(&self) -> Vec<String> {
        self.state.borrow().scopes.clone()
    }

    /// Replaces the granted scopes.
    pub fn set_scopes(&self, scopes: impl IntoIterator<Item = impl Into<String>>) {
        let mut state = self.state.borrow_mut();
        state.scopes = scopes.into_iter().map(Into::into).collect();
        state.status = SessionStatus::Modified;
    }

    /// Grants one additional scope.
    pub fn grant_scope(&self, scope: impl Into<String>) {
        let mut state = self.state.borrow_mut();
        state.scopes.push(scope.into());
        state.status = SessionStatus::Modified;
    }

    /// Reads one application value.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<Value> {
        self.state.borrow().data.get(key).cloned()
    }

    /// Writes one application value.
    pub fn insert(&self, key: impl Into<String>, value: impl Into<Value>) {
        let mut state = self.state.borrow_mut();
        state.data.insert(key.into(), value.into());
        state.status = SessionStatus::Modified;
    }

    /// Removes one application value.
    pub fn remove(&self, key: &str) {
        let mut state = self.state.borrow_mut();
        if state.data.remove(key).is_some() {
            state.status = SessionStatus::Modified;
        }
    }

    /// Logs the subject out and schedules cookie and store removal.
    pub fn clear(&self) {
        let mut state = self.state.borrow_mut();
        state.subject = None;
        state.scopes.clear();
        state.data.clear();
        state.status = SessionStatus::Cleared;
    }

    /// Returns the write scheduled for the response.
    #[must_use]
    pub fn status(&self) -> SessionStatus {
        self.state.borrow().status
    }

    /// Returns whether the session carries a subject.
    #[must_use]
    pub fn is_authenticated(&self) -> bool {
        let state = self.state.borrow();
        state.status != SessionStatus::Cleared && state.subject.is_some()
    }

    fn restored(id: Option<String>, record: SessionRecord) -> Self {
        Self {
            state: Rc::new(RefCell::new(SessionState {
                id,
                subject: record.subject,
                scopes: record.scopes,
                data: record.data,
                status: SessionStatus::Unchanged,
            })),
        }
    }

    fn identity(&self) -> Option<AuthenticatedIdentity> {
        let state = self.state.borrow();
        if state.status == SessionStatus::Cleared {
            return None;
        }
        let subject = state.subject.clone()?;
        let mut claims = Map::new();
        claims.insert("credential".to_owned(), Value::String("session".to_owned()));
        claims.insert("sub".to_owned(), Value::String(subject.clone()));
        if let Some(id) = &state.id {
            claims.insert("sid".to_owned(), Value::String(id.clone()));
        }
        if !state.data.is_empty() {
            claims.insert("dat".to_owned(), Value::Object(state.data.clone()));
        }
        Some(AuthenticatedIdentity {
            scheme: String::new(),
            subject: Some(subject),
            scopes: state.scopes.clone(),
            claims: Value::Object(claims),
        })
    }
}

impl FromInvocation for Session {
    fn from_invocation(
        input: &InvocationInput<'_>,
        _name: &str,
        _required: bool,
    ) -> Result<Self, InputRejection> {
        let InvocationInput::Http(request) = input else {
            return Err(session_rejection());
        };
        request
            .extension(TypeId::of::<Self>())
            .and_then(|value| value.downcast_ref::<Self>())
            .cloned()
            .ok_or_else(session_rejection)
    }
}

fn session_rejection() -> InputRejection {
    InputRejection::new(
        500,
        "session_layer_missing",
        "the session layer is not installed",
    )
}

/// Server-side session data addressed by an opaque session id.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionRecord {
    pub subject: Option<String>,
    pub scopes: Vec<String>,
    pub data: Map<String, Value>,
    pub expires_at: Option<u64>,
}

/// Server-side session storage. A store makes revocation before `exp`
/// possible because the cookie carries only an opaque id.
pub trait SessionStore: Send + Sync {
    /// Reads one stored session.
    fn load(&self, id: &str) -> Option<SessionRecord>;

    /// Writes or replaces one stored session.
    fn save(&self, id: &str, record: SessionRecord);

    /// Revokes one stored session immediately.
    fn remove(&self, id: &str);
}

/// Process-local [`SessionStore`] backed by a mutex-guarded map.
#[derive(Debug, Default)]
pub struct MemorySessionStore {
    sessions: Mutex<HashMap<String, SessionRecord>>,
}

impl MemorySessionStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns every live session id, newest ordering unspecified.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        self.lock().keys().cloned().collect()
    }

    /// Drops every session that expired at or before `now`.
    pub fn purge_expired(&self, now: u64) {
        self.lock()
            .retain(|_, record| record.expires_at.is_none_or(|expiry| expiry > now));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, SessionRecord>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl SessionStore for MemorySessionStore {
    fn load(&self, id: &str) -> Option<SessionRecord> {
        let record = self.lock().get(id).cloned()?;
        let expired = match (record.expires_at, unix_time().ok()) {
            (Some(expiry), Some(now)) => expiry <= now,
            (Some(_), None) => true,
            (None, _) => false,
        };
        if expired {
            self.remove(id);
            return None;
        }
        Some(record)
    }

    fn save(&self, id: &str, record: SessionRecord) {
        self.lock().insert(id.to_owned(), record);
    }

    fn remove(&self, id: &str) {
        self.lock().remove(id);
    }
}

/// Reads, writes, and clears the session cookie around every request.
///
/// Register the layer as middleware so it can install [`Session`] before the
/// handler runs and emit `Set-Cookie` afterwards. The same value also
/// implements [`CredentialVerifier`], so a contract scheme can be backed by
/// the session it manages.
#[derive(Clone)]
pub struct SessionLayer {
    jwt: JwtHs256,
    cookie: CookieOptions,
    ttl_seconds: u64,
    store: Option<Arc<dyn SessionStore>>,
}

impl fmt::Debug for SessionLayer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionLayer")
            .field("jwt", &self.jwt)
            .field("cookie", &self.cookie)
            .field("ttl_seconds", &self.ttl_seconds)
            .field("store", &self.store.is_some())
            .finish()
    }
}

impl SessionLayer {
    /// Creates a stateless signed-cookie session layer.
    #[must_use]
    pub fn new(mut jwt: JwtHs256) -> Self {
        jwt.validation.require_subject = false;
        Self {
            jwt,
            cookie: CookieOptions::new(DEFAULT_SESSION_COOKIE),
            ttl_seconds: DEFAULT_SESSION_TTL_SECONDS,
            store: None,
        }
    }

    /// Replaces the cookie attributes.
    ///
    /// # Errors
    ///
    /// Returns an error for attributes a browser would reject, such as
    /// `SameSite=None` without `Secure`.
    pub fn cookie(mut self, options: CookieOptions) -> Result<Self, SecurityConfigError> {
        options.validate()?;
        self.cookie = options;
        Ok(self)
    }

    /// Sets the session lifetime in seconds.
    #[must_use]
    pub const fn ttl(mut self, seconds: u64) -> Self {
        self.ttl_seconds = seconds;
        self
    }

    /// Moves session state into a server-side store so it can be revoked.
    #[must_use]
    pub fn store(mut self, store: Arc<dyn SessionStore>) -> Self {
        self.store = Some(store);
        self
    }

    fn load(&self, context: &HttpRequestContext<'_>) -> Session {
        let Some(token) = cookie_value(context.request(), self.cookie.name()) else {
            return Session::new();
        };
        let Ok(verified) = self.jwt.verify_token(token) else {
            return Session::new();
        };
        let id = verified
            .claims
            .get("sid")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let Some(store) = &self.store else {
            return Session::restored(
                id,
                SessionRecord {
                    subject: verified.subject,
                    scopes: verified.scopes,
                    data: session_data(&verified.claims),
                    expires_at: verified.claims.get("exp").and_then(Value::as_u64),
                },
            );
        };
        let Some(id) = id else {
            return Session::new();
        };
        // A revoked or expired server-side session leaves the request
        // anonymous even though the cookie signature still verifies.
        store
            .load(&id)
            .map_or_else(Session::new, |record| Session::restored(Some(id), record))
    }

    fn write(&self, session: &Session, response: &mut Response) {
        match session.status() {
            SessionStatus::Unchanged => {}
            SessionStatus::Cleared => {
                if let Some(store) = &self.store
                    && let Some(id) = session.id()
                {
                    store.remove(&id);
                }
                response.set_header("set-cookie", self.cookie.clear());
            }
            SessionStatus::Modified => {
                if let Some(cookie) = self.issue(session) {
                    response.set_header("set-cookie", cookie);
                } else {
                    *response = server_security_error("session cookie could not be issued");
                }
            }
        }
    }

    fn issue(&self, session: &Session) -> Option<String> {
        let now = unix_time().ok()?;
        let expires_at = now.saturating_add(self.ttl_seconds);
        let state = session.state.borrow();
        let mut additional = Map::new();
        let claims = if let Some(store) = &self.store {
            let id = state.id.clone().unwrap_or_else(new_session_id);
            store.save(
                &id,
                SessionRecord {
                    subject: state.subject.clone(),
                    scopes: state.scopes.clone(),
                    data: state.data.clone(),
                    expires_at: Some(expires_at),
                },
            );
            additional.insert("sid".to_owned(), Value::String(id));
            JwtClaims {
                expires_at: Some(expires_at),
                issued_at: Some(now),
                additional,
                ..JwtClaims::default()
            }
        } else {
            if let Some(id) = &state.id {
                additional.insert("sid".to_owned(), Value::String(id.clone()));
            }
            if !state.data.is_empty() {
                additional.insert("dat".to_owned(), Value::Object(state.data.clone()));
            }
            JwtClaims {
                subject: state.subject.clone(),
                expires_at: Some(expires_at),
                issued_at: Some(now),
                scopes: state.scopes.clone(),
                additional,
                ..JwtClaims::default()
            }
        };
        drop(state);
        let token = self.jwt.encode(&claims).ok()?;
        let max_age = self.cookie.max_age_seconds.unwrap_or(self.ttl_seconds);
        Some(self.cookie.format(&token, Some(max_age)))
    }
}

impl HttpMiddleware for SessionLayer {
    fn on_request(&self, context: &mut HttpRequestContext<'_>) -> Option<Response> {
        let session = self.load(context);
        context.insert_extension(session);
        None
    }

    fn on_response(
        &self,
        context: &HttpRequestContext<'_>,
        _operation: Option<&OperationDescriptor>,
        response: &mut Response,
    ) {
        if let Some(session) = context.extension::<Session>().cloned() {
            self.write(&session, response);
        }
    }
}

impl CredentialVerifier for SessionLayer {
    fn verify(
        &self,
        context: &HttpRequestContext<'_>,
        _requirement: &SecurityRequirement,
        _descriptor: &SecuritySchemeDescriptor,
    ) -> Result<AuthenticatedIdentity, AuthenticationError> {
        let session = context
            .extension::<Session>()
            .ok_or(AuthenticationError::Internal(
                "session layer is not installed",
            ))?;
        session.identity().ok_or(AuthenticationError::Missing)
    }
}

fn session_data(claims: &Value) -> Map<String, Value> {
    claims
        .get("dat")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

fn new_session_id() -> String {
    Uuid::new_v4().simple().to_string()
}

/// Signed JWT stored in an HTTP cookie and exposed as a security verifier.
///
/// This is the read-only half of a session. Use [`SessionLayer`] when the
/// application also needs to create, mutate, or revoke sessions.
#[derive(Clone, Debug)]
pub struct SignedSession {
    cookie: CookieOptions,
    jwt: JwtHs256,
}

impl SignedSession {
    #[must_use]
    pub fn new(cookie_name: impl Into<String>, jwt: JwtHs256) -> Self {
        Self {
            cookie: CookieOptions::new(cookie_name),
            jwt,
        }
    }

    /// Replaces the cookie attributes used when issuing this session.
    ///
    /// # Errors
    ///
    /// Returns an error for attributes a browser would reject, such as
    /// `SameSite=None` without `Secure`.
    pub fn cookie_options(mut self, options: CookieOptions) -> Result<Self, SecurityConfigError> {
        options.validate()?;
        self.cookie = options;
        Ok(self)
    }

    /// Creates a secure `Set-Cookie` value for the supplied claims.
    ///
    /// # Errors
    ///
    /// Returns an error if token encoding fails.
    pub fn cookie(
        &self,
        claims: &JwtClaims,
        max_age_seconds: u64,
    ) -> Result<String, AuthenticationError> {
        let token = self.jwt.encode(claims)?;
        Ok(self.cookie.format(&token, Some(max_age_seconds)))
    }

    #[must_use]
    pub fn clear_cookie(&self) -> String {
        self.cookie.clear()
    }
}

impl CredentialVerifier for SignedSession {
    fn verify(
        &self,
        context: &HttpRequestContext<'_>,
        _requirement: &SecurityRequirement,
        _descriptor: &SecuritySchemeDescriptor,
    ) -> Result<AuthenticatedIdentity, AuthenticationError> {
        let token = cookie_value(context.request(), self.cookie.name())
            .ok_or(AuthenticationError::Missing)?;
        let token = self.jwt.verify_token(token)?;
        Ok(AuthenticatedIdentity {
            scheme: String::new(),
            subject: token.subject,
            scopes: token.scopes,
            claims: token.claims,
        })
    }
}

/// Constant-time static API-key verifier for header, query, or cookie schemes.
#[derive(Clone)]
pub struct ApiKey {
    expected: Vec<u8>,
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ApiKey")
            .field("expected", &"<redacted>")
            .finish()
    }
}

impl ApiKey {
    /// Creates a verifier for one machine-generated key.
    ///
    /// # Errors
    ///
    /// Returns an error when the key is shorter than [`MINIMUM_SECRET_BYTES`].
    pub fn new(expected: impl AsRef<[u8]>) -> Result<Self, SecurityConfigError> {
        let expected = expected.as_ref();
        if expected.len() < MINIMUM_SECRET_BYTES {
            return Err(SecurityConfigError::WeakApiKey);
        }
        Ok(Self {
            expected: expected.to_vec(),
        })
    }
}

impl CredentialVerifier for ApiKey {
    fn verify(
        &self,
        context: &HttpRequestContext<'_>,
        _requirement: &SecurityRequirement,
        descriptor: &SecuritySchemeDescriptor,
    ) -> Result<AuthenticatedIdentity, AuthenticationError> {
        let SecuritySchemeKind::ApiKey { location, name } = &descriptor.kind else {
            return Err(AuthenticationError::Internal(
                "API-key verifier attached to incompatible scheme",
            ));
        };
        let supplied = match location {
            SecurityLocation::Header => context.request().header_value(name, 0),
            SecurityLocation::Query => query_value(context.request().target(), name),
            SecurityLocation::Cookie => cookie_value(context.request(), name),
        }
        .ok_or(AuthenticationError::Missing)?;
        if !constant_time_eq(supplied.as_bytes(), &self.expected) {
            return Err(AuthenticationError::Invalid("API key does not match"));
        }
        Ok(AuthenticatedIdentity {
            scheme: String::new(),
            subject: None,
            scopes: Vec::new(),
            claims: json!({"credential": "api_key"}),
        })
    }
}

/// Static misconfiguration detected before any request is served.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SecurityConfigError {
    WeakHmacKey,
    WeakApiKey,
    WeakPassword { username: String },
    EmptyUsername,
    InsecureSameSiteNone,
    InvalidCookieAttribute { attribute: &'static str },
    MissingVerifier { scheme: String },
    UnknownScheme { scheme: String },
}

impl fmt::Display for SecurityConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WeakHmacKey => write!(
                formatter,
                "HS256 key must contain at least {MINIMUM_SECRET_BYTES} bytes"
            ),
            Self::WeakApiKey => write!(
                formatter,
                "API key must contain at least {MINIMUM_SECRET_BYTES} bytes"
            ),
            Self::WeakPassword { username } => write!(
                formatter,
                "password for `{username}` must contain at least {MINIMUM_PASSWORD_BYTES} bytes"
            ),
            Self::EmptyUsername => formatter.write_str("user name must not be empty"),
            Self::InsecureSameSiteNone => {
                formatter.write_str("SameSite=None requires the Secure attribute")
            }
            Self::InvalidCookieAttribute { attribute } => {
                write!(
                    formatter,
                    "cookie {attribute} contains an invalid character"
                )
            }
            Self::MissingVerifier { scheme } => write!(
                formatter,
                "security scheme `{scheme}` has no attached verifier"
            ),
            Self::UnknownScheme { scheme } => write!(
                formatter,
                "verifier `{scheme}` does not match a declared security scheme"
            ),
        }
    }
}

impl std::error::Error for SecurityConfigError {}

fn decode_json_segment(segment: &str) -> Result<Value, AuthenticationError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(segment)
        .map_err(|_| AuthenticationError::Invalid("JWT segment is not base64url"))?;
    blazingly_json::from_slice(&bytes)
        .map_err(|_| AuthenticationError::Invalid("JWT segment is not valid JSON"))
}

fn validate_claims(claims: &Value, validation: &JwtValidation) -> Result<(), AuthenticationError> {
    let now = unix_time()?;
    let leeway = validation.leeway_seconds;
    match numeric_claim(claims, "exp")? {
        Some(expiration) if now > expiration.saturating_add(leeway) => {
            return Err(AuthenticationError::Invalid("JWT has expired"));
        }
        None if validation.require_expiration => {
            return Err(AuthenticationError::Invalid("JWT expiration is required"));
        }
        _ => {}
    }
    match numeric_claim(claims, "nbf")? {
        Some(not_before) if now.saturating_add(leeway) < not_before => {
            return Err(AuthenticationError::Invalid("JWT is not active yet"));
        }
        None if validation.require_not_before => {
            return Err(AuthenticationError::Invalid("JWT nbf is required"));
        }
        _ => {}
    }
    if validation.require_subject && claims.get("sub").and_then(Value::as_str).is_none() {
        return Err(AuthenticationError::Invalid("JWT subject is required"));
    }
    if !validation.issuers.is_empty()
        && !claims
            .get("iss")
            .and_then(Value::as_str)
            .is_some_and(|issuer| validation.issuers.contains(issuer))
    {
        return Err(AuthenticationError::Invalid("JWT issuer is not permitted"));
    }
    if !validation.audiences.is_empty()
        && !claim_audiences(claims)
            .iter()
            .any(|audience| validation.audiences.contains(*audience))
    {
        return Err(AuthenticationError::Invalid(
            "JWT audience is not permitted",
        ));
    }
    Ok(())
}

fn numeric_claim(claims: &Value, name: &str) -> Result<Option<u64>, AuthenticationError> {
    match claims.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .ok_or(AuthenticationError::Invalid(
                "JWT time claim is not a whole number of seconds",
            ))
            .map(Some),
    }
}

fn unix_time() -> Result<u64, AuthenticationError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| AuthenticationError::Internal("system clock is before Unix epoch"))
}

fn claim_audiences(claims: &Value) -> Vec<&str> {
    match claims.get("aud") {
        Some(Value::String(value)) => vec![value],
        Some(Value::Array(values)) => values.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

fn claim_scopes(claims: &Value) -> Vec<String> {
    match claims.get("scope").or_else(|| claims.get("scp")) {
        Some(Value::String(value)) => value
            .split_ascii_whitespace()
            .map(ToOwned::to_owned)
            .collect(),
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

fn insert_optional<T: Serialize>(map: &mut Map<String, Value>, name: &str, value: Option<&T>) {
    if let Some(value) = value
        && let Ok(value) = blazingly_json::to_value(value)
    {
        map.insert(name.to_owned(), value);
    }
}

fn query_value<'target>(target: &'target str, name: &str) -> Option<&'target str> {
    target.split_once('?')?.1.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        (key == name).then_some(value)
    })
}

/// Reads one cookie across every `cookie` header field, because a client may
/// split its cookies over several fields.
fn cookie_value<'request>(
    request: &'request dyn HttpRequestView,
    name: &str,
) -> Option<&'request str> {
    let mut index = 0;
    while let Some(header) = request.header_value("cookie", index) {
        for cookie in header.split(';') {
            if let Some((candidate, value)) = cookie.trim().split_once('=')
                && candidate.trim() == name
            {
                return Some(value.trim());
            }
        }
        index += 1;
    }
    None
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        let left = left.get(index).copied().unwrap_or_default();
        let right = right.get(index).copied().unwrap_or_default();
        difference |= usize::from(left ^ right);
    }
    difference == 0
}

fn authenticate_header(
    descriptor: &SecuritySchemeDescriptor,
    realm: &str,
    failure: Option<&ChallengeFailure<'_>>,
) -> String {
    let mut value = challenge_scheme(descriptor);
    value.push_str(" realm=\"");
    push_quoted(&mut value, realm);
    value.push('"');
    if is_basic_scheme(&descriptor.kind) {
        value.push_str(", charset=\"UTF-8\"");
    }
    let Some(failure) = failure.filter(|_| supports_error_parameters(&descriptor.kind)) else {
        return value;
    };
    value.push_str(", error=\"");
    value.push_str(failure.error.code());
    value.push_str("\", error_description=\"");
    push_quoted(&mut value, failure.description);
    value.push('"');
    if failure.error == ChallengeError::InsufficientScope && !failure.scopes.is_empty() {
        value.push_str(", scope=\"");
        push_quoted(&mut value, &failure.scopes.join(" "));
        value.push('"');
    }
    value
}

fn challenge_scheme(descriptor: &SecuritySchemeDescriptor) -> String {
    match &descriptor.kind {
        SecuritySchemeKind::Http { scheme, .. } => capitalize(scheme),
        SecuritySchemeKind::OAuth2 { .. } | SecuritySchemeKind::OpenIdConnect { .. } => {
            "Bearer".to_owned()
        }
        SecuritySchemeKind::ApiKey { .. } => "ApiKey".to_owned(),
        SecuritySchemeKind::MutualTls => "MutualTLS".to_owned(),
    }
}

fn supports_error_parameters(kind: &SecuritySchemeKind) -> bool {
    match kind {
        SecuritySchemeKind::OAuth2 { .. } | SecuritySchemeKind::OpenIdConnect { .. } => true,
        SecuritySchemeKind::Http { scheme, .. } => scheme.eq_ignore_ascii_case("bearer"),
        SecuritySchemeKind::ApiKey { .. } | SecuritySchemeKind::MutualTls => false,
    }
}

fn capitalize(value: &str) -> String {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return "Bearer".to_owned();
    };
    let mut capitalized = first.to_ascii_uppercase().to_string();
    capitalized.push_str(&characters.as_str().to_ascii_lowercase());
    capitalized
}

fn push_quoted(target: &mut String, value: &str) {
    for character in value.chars() {
        if character == '"' || character == '\\' {
            target.push('\\');
        }
        target.push(character);
    }
}

fn server_security_error(message: &str) -> Response {
    json_error(500, "security_configuration_error", message)
}

fn json_error(status: u16, code: &str, message: &str) -> Response {
    Response::from_bytes(
        status,
        blazingly_json::to_vec(&json!({
            "error": {
                "code": code,
                "message": message,
            }
        }))
        .unwrap_or_else(|_| b"{\"error\":{\"code\":\"security_error\"}}".to_vec()),
    )
    .with_header("content-type", "application/json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use blazingly_core::{HttpMethod, ResponseDescriptor, TypeDescriptor};
    use blazingly_executor::{ExecutableOperation, ExecutionOutcome, OperationFuture};
    use blazingly_http::{Request, TestApp};
    use futures_lite::future;

    const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";
    const PASSWORD: &str = "correct horse battery staple";

    struct SplitCookieRequest {
        cookies: Vec<String>,
    }

    impl HttpRequestView for SplitCookieRequest {
        fn method(&self) -> HttpMethod {
            HttpMethod::Get
        }

        fn target(&self) -> &'static str {
            "/"
        }

        fn header_value(&self, name: &str, index: usize) -> Option<&str> {
            if !name.eq_ignore_ascii_case("cookie") {
                return None;
            }
            self.cookies.get(index).map(String::as_str)
        }

        fn body(&self) -> &[u8] {
            &[]
        }
    }

    fn jwt() -> JwtHs256 {
        JwtHs256::new(KEY).expect("strong key")
    }

    fn operation(
        method: HttpMethod,
        path: &str,
        id: &str,
        security: Vec<SecurityRequirement>,
        handler: impl Fn(&InvocationInput<'_>) -> Result<Value, InputRejection> + 'static,
    ) -> ExecutableOperation {
        let descriptor = OperationDescriptor::new(
            method,
            path,
            id,
            id,
            None,
            vec![ResponseDescriptor::success(
                200,
                Some(TypeDescriptor::new("Output")),
            )],
        )
        .expect("operation id is valid")
        .with_security(security);
        ExecutableOperation::typed(descriptor, move |input| {
            let value = handler(&input)?;
            let body = blazingly_json::to_vec(&value).expect("test payload serializes");
            let future: OperationFuture = Box::pin(async move {
                ExecutionOutcome::Success {
                    status: 200,
                    headers: Vec::new(),
                    body: Some(body),
                    background: Vec::new(),
                }
            });
            Ok(future)
        })
    }

    fn session_scheme() -> SecuritySchemeDescriptor {
        SecuritySchemeDescriptor::new(
            "session",
            SecuritySchemeKind::ApiKey {
                location: SecurityLocation::Cookie,
                name: DEFAULT_SESSION_COOKIE.to_owned(),
            },
        )
    }

    fn oauth_scheme() -> SecuritySchemeDescriptor {
        SecuritySchemeDescriptor::new(
            "oauth",
            SecuritySchemeKind::OAuth2 {
                authorization_url: None,
                token_url: Some("/token".to_owned()),
                scopes: vec!["orders:read".to_owned()],
            },
        )
    }

    fn basic_scheme() -> SecuritySchemeDescriptor {
        SecuritySchemeDescriptor::new(
            "basic",
            SecuritySchemeKind::Http {
                scheme: "basic".to_owned(),
                bearer_format: None,
            },
        )
    }

    fn login_operation() -> ExecutableOperation {
        operation(
            HttpMethod::Post,
            "/login",
            "session.login",
            Vec::new(),
            |input| {
                let session = Session::from_invocation(input, "session", true)?;
                session.set_subject("user-42");
                session.insert("theme", "dark");
                Ok(json!("ok"))
            },
        )
    }

    fn logout_operation() -> ExecutableOperation {
        operation(
            HttpMethod::Post,
            "/logout",
            "session.logout",
            Vec::new(),
            |input| {
                let session = Session::from_invocation(input, "session", true)?;
                session.clear();
                Ok(json!("ok"))
            },
        )
    }

    fn subject_operation(path: &str, id: &str, scheme: &str) -> ExecutableOperation {
        operation(
            HttpMethod::Get,
            path,
            id,
            vec![SecurityRequirement::new(scheme)],
            |input| {
                let context = SecurityContext::from_invocation(input, "security", true)?;
                Ok(json!(
                    context
                        .primary()
                        .and_then(|identity| identity.subject.clone())
                        .unwrap_or_else(|| "anonymous".to_owned())
                ))
            },
        )
    }

    fn theme_operation() -> ExecutableOperation {
        operation(
            HttpMethod::Get,
            "/theme",
            "session.theme",
            Vec::new(),
            |input| {
                let session = Session::from_invocation(input, "session", true)?;
                Ok(session.get("theme").unwrap_or(Value::Null))
            },
        )
    }

    fn cookie_pair(response: &Response) -> String {
        response
            .get_header("set-cookie")
            .expect("session cookie")
            .split(';')
            .next()
            .expect("cookie pair")
            .to_owned()
    }

    #[test]
    fn jwt_round_trip_validates_signature_and_scopes() {
        let jwt = jwt();
        let claims =
            JwtClaims::new("agent-7", unix_time().expect("time") + 60).scope("orders:read");
        let token = jwt.encode(&claims).expect("encode");
        let verified = jwt.verify_token(&token).expect("verify");
        assert_eq!(verified.subject.as_deref(), Some("agent-7"));
        assert_eq!(verified.scopes, ["orders:read"]);
    }

    #[test]
    fn jwt_rejects_signature_changes() {
        let jwt = jwt();
        let claims = JwtClaims::new("agent-7", unix_time().expect("time") + 60);
        let mut token = jwt.encode(&claims).expect("encode");
        token.push('x');
        assert!(matches!(
            jwt.verify_token(&token),
            Err(AuthenticationError::Invalid(_))
        ));
    }

    #[test]
    fn jwt_accepts_a_permitted_issuer_and_audience_set() {
        let jwt = jwt().validation(
            JwtValidation::default()
                .issuer("https://issuer.one")
                .issuer("https://issuer.two")
                .audience("orders")
                .audience("billing"),
        );
        let accepted = JwtClaims {
            issuer: Some("https://issuer.two".to_owned()),
            audience: vec!["billing".to_owned()],
            ..JwtClaims::new("agent-7", unix_time().expect("time") + 60)
        };
        let token = jwt.encode(&accepted).expect("encode");
        assert!(jwt.verify_token(&token).is_ok());

        let rejected = JwtClaims {
            issuer: Some("https://issuer.three".to_owned()),
            audience: vec!["billing".to_owned()],
            ..JwtClaims::new("agent-7", unix_time().expect("time") + 60)
        };
        let token = jwt.encode(&rejected).expect("encode");
        assert_eq!(
            jwt.verify_token(&token),
            Err(AuthenticationError::Invalid("JWT issuer is not permitted"))
        );
    }

    #[test]
    fn jwt_rejects_a_token_that_is_not_active_yet() {
        let jwt = jwt();
        let claims = JwtClaims {
            not_before: Some(unix_time().expect("time") + 600),
            ..JwtClaims::new("agent-7", unix_time().expect("time") + 900)
        };
        let token = jwt.encode(&claims).expect("encode");
        assert_eq!(
            jwt.verify_token(&token),
            Err(AuthenticationError::Invalid("JWT is not active yet"))
        );
    }

    #[test]
    fn cookies_are_read_across_split_header_fields() {
        let request = SplitCookieRequest {
            cookies: vec![
                "theme=dark".to_owned(),
                "blazingly_session=token-value; other=1".to_owned(),
            ],
        };
        assert_eq!(
            cookie_value(&request, DEFAULT_SESSION_COOKIE),
            Some("token-value")
        );
        assert_eq!(cookie_value(&request, "missing"), None);
    }

    #[test]
    fn cookie_attributes_are_configurable_and_validated() {
        let options = CookieOptions::new("sid")
            .domain("example.test")
            .path("/app")
            .same_site(SameSite::Strict)
            .max_age(120);
        assert_eq!(
            options.format("value", options.max_age_seconds),
            "sid=value; Domain=example.test; Path=/app; Max-Age=120; HttpOnly; Secure; \
             SameSite=Strict"
        );
        assert_eq!(
            CookieOptions::new("sid")
                .same_site(SameSite::None)
                .secure(false)
                .validate(),
            Err(SecurityConfigError::InsecureSameSiteNone)
        );
        assert!(
            CookieOptions::new("sid")
                .same_site(SameSite::None)
                .validate()
                .is_ok()
        );
        assert_eq!(
            CookieOptions::new("bad name").validate(),
            Err(SecurityConfigError::InvalidCookieAttribute { attribute: "name" })
        );
    }

    #[test]
    fn session_cookie_is_secure_by_default() {
        let session = SignedSession::new(DEFAULT_SESSION_COOKIE, jwt());
        let claims = JwtClaims::new("user-1", unix_time().expect("time") + 60);
        let cookie = session.cookie(&claims, 60).expect("cookie");
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("Secure"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(session.clear_cookie().contains("Max-Age=0"));
    }

    #[test]
    fn weak_secrets_are_rejected_at_construction() {
        assert_eq!(
            JwtHs256::new(b"short").err(),
            Some(SecurityConfigError::WeakHmacKey)
        );
        assert_eq!(
            ApiKey::new(b"").err(),
            Some(SecurityConfigError::WeakApiKey)
        );
        assert_eq!(
            ApiKey::new(b"short-key").err(),
            Some(SecurityConfigError::WeakApiKey)
        );
        assert!(ApiKey::new(KEY).is_ok());
        assert_eq!(
            StaticPasswords::new().user("ada", "short").err(),
            Some(SecurityConfigError::WeakPassword {
                username: "ada".to_owned()
            })
        );
        assert_eq!(
            StaticPasswords::new().user("", PASSWORD).err(),
            Some(SecurityConfigError::EmptyUsername)
        );
    }

    #[test]
    fn missing_verifiers_are_detected_before_the_first_request() {
        let schemes = [oauth_scheme(), session_scheme()];
        assert_eq!(
            Security::new()
                .verifier("oauth", OAuth2Bearer::new(jwt()))
                .build(&schemes)
                .err(),
            Some(SecurityConfigError::MissingVerifier {
                scheme: "session".to_owned()
            })
        );
        assert_eq!(
            Security::new()
                .verifier("typo", OAuth2Bearer::new(jwt()))
                .build(&schemes)
                .err(),
            Some(SecurityConfigError::UnknownScheme {
                scheme: "typo".to_owned()
            })
        );
        assert!(
            Security::new()
                .verifier("oauth", OAuth2Bearer::new(jwt()))
                .verifier("session", SessionLayer::new(jwt()))
                .build(&schemes)
                .is_ok()
        );
    }

    #[test]
    fn session_created_in_a_handler_round_trips_through_set_cookie() {
        let sessions = SessionLayer::new(jwt());
        let executable = ExecutableApp::with_security_schemes(
            vec![
                login_operation(),
                logout_operation(),
                theme_operation(),
                subject_operation("/profile", "session.profile", "session"),
            ],
            [session_scheme()],
        )
        .expect("session app compiles");
        let security = Security::new()
            .verifier("session", sessions.clone())
            .build_for(&executable)
            .expect("every declared scheme has a verifier");
        let app = TestApp::new(&executable)
            .with_middleware(sessions)
            .with_middleware(security);

        let anonymous = future::block_on(app.call(Request::get("/profile")));
        assert_eq!(anonymous.status(), 401);

        let login = future::block_on(app.call(Request::post("/login")));
        assert_eq!(login.status(), 200);
        let issued = login.get_header("set-cookie").expect("session cookie");
        assert!(issued.starts_with("blazingly_session="));
        assert!(issued.contains("HttpOnly"));
        assert!(issued.contains("Secure"));
        assert!(issued.contains("SameSite=Lax"));
        let cookie = cookie_pair(&login);

        let profile =
            future::block_on(app.call(Request::get("/profile").header("cookie", cookie.clone())));
        assert_eq!(profile.status(), 200);
        assert_eq!(profile.json::<String>().expect("subject"), "user-42");

        let theme =
            future::block_on(app.call(Request::get("/theme").header("cookie", cookie.clone())));
        assert_eq!(theme.json::<String>().expect("theme"), "dark");

        let logout =
            future::block_on(app.call(Request::post("/logout").header("cookie", cookie.clone())));
        assert!(
            logout
                .get_header("set-cookie")
                .expect("cleared cookie")
                .contains("Max-Age=0")
        );

        let unchanged =
            future::block_on(app.call(Request::get("/profile").header("cookie", cookie)));
        assert_eq!(unchanged.status(), 200);
        assert_eq!(unchanged.get_header("set-cookie"), None);
    }

    #[test]
    fn store_backed_sessions_can_be_revoked_before_expiry() {
        let store = Arc::new(MemorySessionStore::new());
        let sessions = SessionLayer::new(jwt()).store(store.clone());
        let executable = ExecutableApp::with_security_schemes(
            vec![
                login_operation(),
                subject_operation("/profile", "session.profile", "session"),
            ],
            [session_scheme()],
        )
        .expect("session app compiles");
        let app = TestApp::new(&executable)
            .with_middleware(sessions.clone())
            .with_middleware(Security::new().verifier("session", sessions));

        let login = future::block_on(app.call(Request::post("/login")));
        let cookie = cookie_pair(&login);
        assert_eq!(store.ids().len(), 1);
        // The opaque id is the only session material the client ever holds.
        assert!(!cookie.contains("user-42"));

        let allowed =
            future::block_on(app.call(Request::get("/profile").header("cookie", cookie.clone())));
        assert_eq!(allowed.status(), 200);
        assert_eq!(allowed.json::<String>().expect("subject"), "user-42");

        for id in store.ids() {
            store.remove(&id);
        }
        let revoked = future::block_on(app.call(Request::get("/profile").header("cookie", cookie)));
        assert_eq!(revoked.status(), 401);
    }

    #[test]
    fn optional_authentication_serves_an_anonymous_request() {
        let executable = ExecutableApp::with_security_schemes(
            vec![subject_operation("/feed", "feed.read", "oauth")],
            [oauth_scheme()],
        )
        .expect("optional app compiles");
        let app = TestApp::new(&executable)
            .with_middleware(Security::new().optional_verifier("oauth", OAuth2Bearer::new(jwt())));

        let anonymous = future::block_on(app.call(Request::get("/feed")));
        assert_eq!(anonymous.status(), 200);
        assert_eq!(anonymous.json::<String>().expect("body"), "anonymous");

        let garbage =
            future::block_on(app.call(Request::get("/feed").header("authorization", "Bearer bad")));
        assert_eq!(garbage.status(), 200);
        assert_eq!(garbage.json::<String>().expect("body"), "anonymous");

        let token = jwt()
            .encode(&JwtClaims::new("agent-1", unix_time().expect("time") + 60))
            .expect("token");
        let identified = future::block_on(
            app.call(Request::get("/feed").header("authorization", format!("Bearer {token}"))),
        );
        assert_eq!(identified.status(), 200);
        assert_eq!(identified.json::<String>().expect("body"), "agent-1");
    }

    #[test]
    fn challenges_follow_rfc_6750() {
        let executable = ExecutableApp::with_security_schemes(
            vec![operation(
                HttpMethod::Get,
                "/orders",
                "orders.read",
                vec![SecurityRequirement::new("oauth").with_scopes(vec!["orders:read".to_owned()])],
                |_input| Ok(json!("ok")),
            )],
            [oauth_scheme()],
        )
        .expect("scoped app compiles");
        let app = TestApp::new(&executable).with_middleware(
            Security::new()
                .realm("orders")
                .verifier("oauth", OAuth2Bearer::new(jwt())),
        );

        let missing = future::block_on(app.call(Request::get("/orders")));
        assert_eq!(missing.status(), 401);
        assert_eq!(
            missing.get_header("www-authenticate"),
            Some(r#"Bearer realm="orders""#)
        );

        let invalid = future::block_on(
            app.call(Request::get("/orders").header("authorization", "Bearer nonsense")),
        );
        assert_eq!(invalid.status(), 401);
        assert_eq!(
            invalid.get_header("www-authenticate"),
            Some(
                r#"Bearer realm="orders", error="invalid_token", error_description="JWT payload is missing""#
            )
        );

        let token = jwt()
            .encode(&JwtClaims::new("agent-1", unix_time().expect("time") + 60))
            .expect("token");
        let forbidden = future::block_on(
            app.call(Request::get("/orders").header("authorization", format!("Bearer {token}"))),
        );
        assert_eq!(forbidden.status(), 403);
        assert_eq!(
            forbidden.get_header("www-authenticate"),
            Some(
                r#"Bearer realm="orders", error="insufficient_scope", error_description="the request requires higher privileges", scope="orders:read""#
            )
        );
    }

    #[test]
    fn basic_authentication_accepts_and_rejects_credentials() {
        let passwords = StaticPasswords::new()
            .user("ada", PASSWORD)
            .expect("credentials");
        let executable = ExecutableApp::with_security_schemes(
            vec![subject_operation("/admin", "admin.read", "basic")],
            [basic_scheme()],
        )
        .expect("basic app compiles");
        let app = TestApp::new(&executable)
            .with_middleware(Security::new().verifier("basic", BasicAuth::new(passwords)));

        let accepted = future::block_on(app.call(Request::get("/admin").header(
            "authorization",
            format!("Basic {}", STANDARD.encode(format!("ada:{PASSWORD}"))),
        )));
        assert_eq!(accepted.status(), 200);
        assert_eq!(accepted.json::<String>().expect("subject"), "ada");

        let unpadded = future::block_on(app.call(Request::get("/admin").header(
            "authorization",
            format!(
                "Basic {}",
                STANDARD_NO_PAD.encode(format!("ada:{PASSWORD}"))
            ),
        )));
        assert_eq!(unpadded.status(), 200);

        let wrong_password = future::block_on(app.call(Request::get("/admin").header(
            "authorization",
            format!("Basic {}", STANDARD.encode("ada:wrong password here")),
        )));
        assert_eq!(wrong_password.status(), 401);
        assert_eq!(
            wrong_password.get_header("www-authenticate"),
            Some(r#"Basic realm="api", charset="UTF-8""#)
        );

        let malformed =
            future::block_on(app.call(Request::get("/admin").header("authorization", "Basic %%%")));
        assert_eq!(malformed.status(), 401);

        let missing = future::block_on(app.call(Request::get("/admin")));
        assert_eq!(missing.status(), 401);
    }
}
