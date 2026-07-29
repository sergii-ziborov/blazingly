#![forbid(unsafe_code)]

use blazingly_contract::{InvalidOperationId, OperationContract};
use core::fmt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::future::{Future, poll_fn};
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

pub use blazingly_contract::{
    AgentPolicy, ApiError, ApiModel, ApiSchema, CURRENT_CONTRACT_FORMAT_VERSION, Compatibility,
    CompatibilityChange, CompatibilityImpact, CompatibilityReport, Confirmation,
    ContractFingerprint, ContractFormatVersion, DependencyDescriptor, FieldDescriptor,
    FieldViolation, InputDescriptor, InputSource, McpToolDescriptor, ModelDescriptor,
    OperationFailure, OperationId, OperationRisk, OutputExposure, ResponseBuildError,
    ResponseDescriptor, ResponseHeader, SchemaKind, SecurityLocation, SecurityRequirement,
    SecuritySchemeDescriptor, SecuritySchemeKind, TypeDescriptor, ValidationErrors, ValidationRule,
};

// Hidden rather than documented: the module exists so the OpenAPI and MCP
// projections share one traversal, and hiding it keeps the dialect trait free
// to grow required methods without a major version bump.
#[doc(hidden)]
pub mod schema;

// ---------------------------------------------------------------------------
// Response body sizing
// ---------------------------------------------------------------------------

/// Distinct response shapes tracked per thread before slots start colliding.
const RESPONSE_HINT_SLOTS: usize = 32;
/// Floor for a learned hint, matching what `blazingly_json::to_vec` starts with.
const MIN_RESPONSE_HINT: usize = 128;
/// Ceiling for a learned hint, so one outsized response cannot make every
/// later response of that shape reserve megabytes.
const MAX_RESPONSE_HINT: usize = 1 << 20;

thread_local! {
    /// Last observed encoded size per response shape, direct mapped.
    ///
    /// A JSON body is grown from nothing on every request, which for an 18 KB
    /// listing means about nine reallocations and 32 KB of copying that the
    /// previous request already knew the answer to. The table is advisory: a
    /// stale or colliding entry only changes the initial capacity, never the
    /// bytes produced.
    static RESPONSE_SIZE_HINTS: std::cell::RefCell<[(usize, usize); RESPONSE_HINT_SLOTS]> =
        const { std::cell::RefCell::new([(0, 0); RESPONSE_HINT_SLOTS]) };
}

/// A per-monomorphization key for `T`.
///
/// `type_name` returns a `&'static str` whose address is stable within one
/// monomorphization. Two types that share an address share a size hint, which
/// costs at most one reallocation.
fn response_shape_key<T: ?Sized>() -> usize {
    core::any::type_name::<T>().as_ptr() as usize
}

fn response_hint_slot(key: usize) -> usize {
    (key >> 4) % RESPONSE_HINT_SLOTS
}

/// Returns the capacity to reserve for the next body of shape `T`.
#[must_use]
pub fn response_size_hint<T: ?Sized>() -> usize {
    let key = response_shape_key::<T>();
    RESPONSE_SIZE_HINTS.with_borrow(|hints| {
        let (stored_key, hint) = hints[response_hint_slot(key)];
        if stored_key == key {
            hint.max(MIN_RESPONSE_HINT)
        } else {
            MIN_RESPONSE_HINT
        }
    })
}

/// Records the encoded size of a body of shape `T`.
///
/// The recorded value carries an eighth of headroom so a body that grows
/// slightly from one request to the next still fits the reserved capacity.
pub fn record_response_size<T: ?Sized>(size: usize) {
    let key = response_shape_key::<T>();
    let hint = size
        .saturating_add(size / 8)
        .saturating_add(32)
        .clamp(MIN_RESPONSE_HINT, MAX_RESPONSE_HINT);
    RESPONSE_SIZE_HINTS.with_borrow_mut(|hints| {
        hints[response_hint_slot(key)] = (key, hint);
    });
}

/// A typed JSON request body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Json<T>(pub T);

/// A typed path argument.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Path<T>(pub T);

/// Typed URL query arguments.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Query<T>(pub T);

/// A typed HTTP header argument.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Header<T>(pub T);

/// A typed HTTP cookie argument.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cookie<T>(pub T);

/// A typed `application/x-www-form-urlencoded` request body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Form<T>(pub T);

/// A typed `multipart/form-data` request body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Multipart<T>(pub T);

/// A typed uploaded file argument.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct File<T>(pub T);

/// Runtime-neutral buffered upload metadata.
///
/// Native and Cloudflare adapters may obtain these bytes differently; neither
/// storage nor socket APIs leak into the operation contract.
///
/// # Wire forms
///
/// Deserialization accepts two shapes, because the same type serves two very
/// different transports:
///
/// * the plain object — `field_name`, `file_name`, `content_type`, `bytes` —
///   which is what [`Serialize`] produces and what an MCP client sends;
/// * a one-entry *slot token* the multipart extractor writes in place of the
///   bytes, resolved through [`UploadSlots`] instead of through the document.
///
/// The second form exists so that `Multipart<T>` never has to render megabytes
/// of upload payload as a JSON array of numbers; see [`UploadSlots`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UploadFile {
    pub field_name: String,
    pub file_name: Option<String>,
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
}

impl UploadFile {
    #[must_use]
    pub fn new(field_name: impl Into<String>, bytes: Vec<u8>) -> Self {
        Self {
            field_name: field_name.into(),
            file_name: None,
            content_type: None,
            bytes,
        }
    }

    #[must_use]
    pub fn with_file_name(mut self, file_name: impl Into<String>) -> Self {
        self.file_name = Some(file_name.into());
        self
    }

    #[must_use]
    pub fn with_content_type(mut self, content_type: impl Into<String>) -> Self {
        self.content_type = Some(content_type.into());
        self
    }
}

impl ApiSchema for UploadFile {
    fn type_descriptor() -> TypeDescriptor {
        TypeDescriptor::scalar("UploadFile", SchemaKind::Binary)
    }
}

// ---------------------------------------------------------------------------
// Out-of-band upload transfer
// ---------------------------------------------------------------------------

/// The reserved object key that stands for an upload parked in a slot.
///
/// The `$` prefix and the path separator keep it clear of any identifier a
/// `#[api_model]` field could be named, and of any key an MCP client is likely
/// to send by accident.
const UPLOAD_SLOT_KEY: &str = "$blazingly::upload";

thread_local! {
    /// Uploads parked for the extraction currently running on this thread.
    ///
    /// A typed multipart body is decoded by handing `T` a JSON document built
    /// from the parts. Text parts belong in that document; upload bytes do not.
    /// A five-megabyte image rendered as a JSON array costs one `Value` per
    /// byte — hundreds of megabytes of resident memory and a full
    /// serialize/deserialize round trip — to arrive at the `Vec<u8>` the
    /// extractor already held.
    ///
    /// So the bytes travel beside the document instead of inside it: the
    /// extractor parks each upload here, writes a one-entry token in its place,
    /// and [`UploadFile`]'s deserializer takes the upload back out. The
    /// document stays small enough to be irrelevant, and the bytes are moved,
    /// never re-encoded.
    static UPLOAD_SLOTS: std::cell::RefCell<Vec<Option<UploadFile>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// The extraction scope that owns the uploads parked inside it.
///
/// Slots opened through this guard are released when it is dropped, whether
/// the decode succeeded, failed, or never reached them. The guard restores the
/// table to the length it had on acquisition, so two extractions on one thread
/// — sequential or nested — cannot see each other's slots.
///
/// This is framework plumbing between the executor and [`UploadFile`]'s
/// deserializer; applications never name it.
#[doc(hidden)]
#[derive(Debug)]
pub struct UploadSlots {
    base: usize,
    /// A slot index only means anything on the thread that parked it, so the
    /// guard must not travel to another one.
    thread_bound: PhantomData<*const ()>,
}

impl UploadSlots {
    /// Opens a scope for the uploads of one extraction.
    #[must_use]
    pub fn acquire() -> Self {
        Self {
            base: UPLOAD_SLOTS.with_borrow(Vec::len),
            thread_bound: PhantomData,
        }
    }

    /// Parks `upload` and returns the token that stands for it in the document.
    ///
    /// The token is a one-entry object; the bytes stay in the slot table until
    /// [`UploadFile`]'s deserializer moves them into the decoded value, or
    /// until this guard is dropped.
    #[must_use]
    pub fn park(&self, upload: UploadFile) -> blazingly_json::Value {
        let index = UPLOAD_SLOTS.with_borrow_mut(|slots| {
            slots.push(Some(upload));
            slots.len() - 1
        });
        let mut token = blazingly_json::Map::new();
        token.insert(
            UPLOAD_SLOT_KEY.to_owned(),
            blazingly_json::Value::from(index),
        );
        blazingly_json::Value::Object(token)
    }
}

impl Drop for UploadSlots {
    fn drop(&mut self) {
        UPLOAD_SLOTS.with_borrow_mut(|slots| slots.truncate(self.base));
    }
}

/// Moves the upload parked at `index` out of the slot table.
///
/// Returns `None` for an index that was never parked, was already taken, or
/// belongs to an extraction that has already ended — which is also what a
/// forged token in a request body produces.
fn take_parked_upload(index: usize) -> Option<UploadFile> {
    UPLOAD_SLOTS.with_borrow_mut(|slots| slots.get_mut(index).and_then(Option::take))
}

/// Fields understood by [`UploadFile`]'s deserializer.
enum UploadField {
    Slot,
    FieldName,
    FileName,
    ContentType,
    Bytes,
    Unknown,
}

impl<'de> Deserialize<'de> for UploadField {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FieldVisitor;

        impl serde::de::Visitor<'_> for FieldVisitor {
            type Value = UploadField;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an uploaded file field name")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<UploadField, E> {
                Ok(match value {
                    UPLOAD_SLOT_KEY => UploadField::Slot,
                    "field_name" => UploadField::FieldName,
                    "file_name" => UploadField::FileName,
                    "content_type" => UploadField::ContentType,
                    "bytes" => UploadField::Bytes,
                    _ => UploadField::Unknown,
                })
            }
        }

        deserializer.deserialize_identifier(FieldVisitor)
    }
}

struct UploadFileVisitor;

impl<'de> serde::de::Visitor<'de> for UploadFileVisitor {
    type Value = UploadFile;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an uploaded file")
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<UploadFile, A::Error> {
        use serde::de::Error as _;

        let mut field_name: Option<String> = None;
        let mut file_name: Option<Option<String>> = None;
        let mut content_type: Option<Option<String>> = None;
        let mut bytes: Option<Vec<u8>> = None;

        while let Some(key) = map.next_key::<UploadField>()? {
            match key {
                UploadField::Slot => {
                    let index = map.next_value::<usize>()?;
                    let upload = take_parked_upload(index).ok_or_else(|| {
                        A::Error::custom("uploaded file bytes are no longer available")
                    })?;
                    // Drain the rest so the caller's map access finishes cleanly.
                    while map
                        .next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?
                        .is_some()
                    {}
                    return Ok(upload);
                }
                UploadField::FieldName => {
                    if field_name.is_some() {
                        return Err(A::Error::duplicate_field("field_name"));
                    }
                    field_name = Some(map.next_value()?);
                }
                UploadField::FileName => {
                    if file_name.is_some() {
                        return Err(A::Error::duplicate_field("file_name"));
                    }
                    file_name = Some(map.next_value()?);
                }
                UploadField::ContentType => {
                    if content_type.is_some() {
                        return Err(A::Error::duplicate_field("content_type"));
                    }
                    content_type = Some(map.next_value()?);
                }
                UploadField::Bytes => {
                    if bytes.is_some() {
                        return Err(A::Error::duplicate_field("bytes"));
                    }
                    bytes = Some(map.next_value()?);
                }
                UploadField::Unknown => {
                    map.next_value::<serde::de::IgnoredAny>()?;
                }
            }
        }

        Ok(UploadFile {
            field_name: field_name.ok_or_else(|| A::Error::missing_field("field_name"))?,
            file_name: file_name.unwrap_or_default(),
            content_type: content_type.unwrap_or_default(),
            bytes: bytes.ok_or_else(|| A::Error::missing_field("bytes"))?,
        })
    }
}

impl<'de> Deserialize<'de> for UploadFile {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        const FIELDS: &[&str] = &["field_name", "file_name", "content_type", "bytes"];
        deserializer.deserialize_struct("UploadFile", FIELDS, UploadFileVisitor)
    }
}

/// A JSON response body the operation encoded itself.
///
/// [`Json<T>`] is encoded *after* the operation has returned, so the value it
/// wraps has to own everything it prints. An operation that reads from a lock
/// guard, an arena, or a shared corpus therefore has to clone every string it
/// wants to include, and those clones are freed again a few microseconds later
/// once the body has been written.
///
/// `PreparedJson<T>` moves the encode step inside the operation, where those
/// borrows are still alive, and carries the finished bytes to the transport
/// untouched. A listing operation can build a borrowed view over the store,
/// encode it while it still holds the read guard, and never allocate an owned
/// mirror of the data at all.
///
/// The type parameter carries the *documented* schema and nothing else: the
/// operation still advertises `T` in `OpenAPI`, MCP, and compatibility
/// analysis, exactly as `Json<T>` would.
///
/// # Contract
///
/// The framework does not re-parse the bytes, so it cannot check them against
/// `T`. The operation asserts that the body it encoded is a valid instance of
/// the schema it declares. Use [`PreparedJson::encode`] with a value whose
/// serialized shape matches `T`; [`PreparedJson::from_bytes`] hands the same
/// obligation to the caller for bytes produced some other way.
///
/// # Examples
///
/// ```
/// use blazingly_core::{ApiSchema, PreparedJson, SchemaKind, TypeDescriptor};
/// use serde::Serialize;
///
/// # struct Page;
/// # impl ApiSchema for Page {
/// #     fn type_descriptor() -> TypeDescriptor {
/// #         TypeDescriptor::scalar("Page", SchemaKind::Object)
/// #     }
/// # }
/// #[derive(Serialize)]
/// struct BorrowedPage<'store> {
///     items: Vec<&'store str>,
/// }
///
/// let store = vec![String::from("first"), String::from("second")];
/// let view = BorrowedPage {
///     items: store.iter().map(String::as_str).collect(),
/// };
/// let body = PreparedJson::<Page>::encode(&view).expect("the view encodes");
/// assert_eq!(body.as_bytes(), br#"{"items":["first","second"]}"#);
/// ```
pub struct PreparedJson<T> {
    body: Vec<u8>,
    schema: PhantomData<fn() -> T>,
}

impl<T> PreparedJson<T> {
    /// Adopts bytes the caller has already encoded.
    ///
    /// The bytes are sent verbatim; see the type-level contract note.
    #[must_use]
    pub const fn from_bytes(body: Vec<u8>) -> Self {
        Self {
            body,
            schema: PhantomData,
        }
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.body
    }

    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.body
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.body.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.body.is_empty()
    }

    /// Encodes `value` into the response body now.
    ///
    /// `value` may borrow from anything alive at the call site, which is the
    /// whole point: it is encoded before the borrow ends.
    ///
    /// # Errors
    ///
    /// Returns the `blazingly_json` failure when `value` cannot be encoded, for
    /// instance because a map key is not a string.
    pub fn encode<V>(value: &V) -> Result<Self, blazingly_json::Error>
    where
        V: Serialize + ?Sized,
    {
        let mut body = Vec::with_capacity(response_size_hint::<V>());
        blazingly_json::to_writer(&mut body, value)?;
        record_response_size::<V>(body.len());
        Ok(Self::from_bytes(body))
    }
}

impl<T> fmt::Debug for PreparedJson<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedJson")
            .field("bytes", &self.body.len())
            .finish()
    }
}

impl<T: ApiSchema> ApiSchema for PreparedJson<T> {
    fn type_descriptor() -> TypeDescriptor {
        T::type_descriptor()
    }
}

/// A successful HTTP 201 response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Created<T>(pub T);

/// A successful HTTP 202 response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Accepted<T>(pub T);

/// A successful HTTP 204 response without a body.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NoContent;

/// One failure produced while an HTTP response body is being streamed.
///
/// The error is transport-neutral. Native servers can terminate the wire
/// stream, while in-memory adapters return it directly to tests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BodyStreamError {
    pub code: String,
    pub message: String,
}

impl BodyStreamError {
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for BodyStreamError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for BodyStreamError {}

/// Runtime-neutral, pull-based response byte stream.
///
/// A transport polls the next chunk only after it has capacity to write it.
/// That pull boundary is the framework's backpressure contract; producers do
/// not depend on Tokio, Compio, or a Cloudflare runtime.
pub trait BodyStream: 'static {
    fn poll_next(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Vec<u8>, BodyStreamError>>>;

    /// Offers a spent chunk's buffer back to the producer.
    ///
    /// A consumer that copies chunks out of the stream can return the vector
    /// here once it is done with it; a producer that refills recycled buffers
    /// then moves a long body with a handful of allocations instead of one
    /// per chunk. Purely an optimization hint: the buffer's contents are dead
    /// either way, and the default implementation simply drops it.
    fn recycle(self: Pin<&mut Self>, spent: Vec<u8>) {
        drop(spent);
    }
}

/// Typed streaming HTTP response body.
///
/// `exact_length` is optional. HTTP/1 uses chunked transfer coding when it is
/// absent, while HTTP/2 emits DATA frames without a content-length field.
pub struct StreamingBody {
    stream: Pin<Box<dyn BodyStream>>,
    exact_length: Option<u64>,
}

impl StreamingBody {
    #[must_use]
    pub fn new(stream: impl BodyStream) -> Self {
        Self {
            stream: Box::pin(stream),
            exact_length: None,
        }
    }

    /// Builds a pull stream from already available chunks.
    #[must_use]
    pub fn from_chunks<I, Chunk>(chunks: I) -> Self
    where
        I: IntoIterator<Item = Chunk>,
        I::IntoIter: Unpin + 'static,
        Chunk: Into<Vec<u8>> + 'static,
    {
        Self::new(ChunkIterator {
            chunks: chunks.into_iter(),
        })
    }

    /// Builds a one-chunk stream with a known exact length.
    #[must_use]
    pub fn once(bytes: impl Into<Vec<u8>>) -> Self {
        let bytes = bytes.into();
        let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        Self::from_chunks([bytes]).with_exact_length(length)
    }

    #[must_use]
    pub const fn with_exact_length(mut self, length: u64) -> Self {
        self.exact_length = Some(length);
        self
    }

    #[must_use]
    pub const fn exact_length(&self) -> Option<u64> {
        self.exact_length
    }

    /// Waits until the producer yields one chunk.
    ///
    /// Calling this method is the consumer demand signal. Transports should
    /// not request another chunk until the previous one has been written.
    pub async fn next_chunk(&mut self) -> Option<Result<Vec<u8>, BodyStreamError>> {
        poll_fn(|context| self.stream.as_mut().poll_next(context)).await
    }

    /// Hands a spent chunk's buffer back to the producer for reuse.
    ///
    /// See [`BodyStream::recycle`]; producers that do not reuse buffers
    /// simply drop it.
    pub fn recycle(&mut self, spent: Vec<u8>) {
        self.stream.as_mut().recycle(spent);
    }
}

impl fmt::Debug for StreamingBody {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StreamingBody")
            .field("exact_length", &self.exact_length)
            .finish_non_exhaustive()
    }
}

impl ApiSchema for StreamingBody {
    fn type_descriptor() -> TypeDescriptor {
        TypeDescriptor::scalar("StreamingBody", SchemaKind::Binary)
    }
}

/// A transport error after an HTTP connection has switched protocols.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpgradeIoError {
    pub code: String,
    pub message: String,
}

impl UpgradeIoError {
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for UpgradeIoError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for UpgradeIoError {}

/// Runtime-neutral byte I/O owned after an HTTP protocol upgrade.
///
/// Native and edge adapters implement this trait without exposing their socket
/// or runtime types to handlers.
pub type UpgradeReadFuture<'io> =
    Pin<Box<dyn Future<Output = Result<Option<Vec<u8>>, UpgradeIoError>> + 'io>>;
pub type UpgradeWriteFuture<'io> = Pin<Box<dyn Future<Output = Result<(), UpgradeIoError>> + 'io>>;

pub trait UpgradedIo: 'static {
    fn read(&mut self) -> UpgradeReadFuture<'_>;

    fn write(&mut self, bytes: Vec<u8>) -> UpgradeWriteFuture<'_>;

    fn shutdown(&mut self) -> UpgradeWriteFuture<'_>;
}

pub type UpgradeFuture = Pin<Box<dyn Future<Output = Result<(), UpgradeIoError>> + 'static>>;
pub type UpgradeHandler = Box<dyn FnOnce(Box<dyn UpgradedIo>) -> UpgradeFuture + 'static>;

/// A validated HTTP protocol switch plus its post-handshake session handler.
pub struct HttpUpgrade {
    protocol: &'static str,
    headers: Vec<ResponseHeader>,
    handler: Option<UpgradeHandler>,
}

impl HttpUpgrade {
    #[must_use]
    pub fn new(
        protocol: &'static str,
        headers: Vec<ResponseHeader>,
        handler: impl FnOnce(Box<dyn UpgradedIo>) -> UpgradeFuture + 'static,
    ) -> Self {
        Self {
            protocol,
            headers,
            handler: Some(Box::new(handler)),
        }
    }

    #[must_use]
    pub const fn protocol(&self) -> &'static str {
        self.protocol
    }

    #[must_use]
    pub fn headers(&self) -> &[ResponseHeader] {
        &self.headers
    }

    pub fn extend_headers(&mut self, headers: impl IntoIterator<Item = ResponseHeader>) {
        self.headers.extend(headers);
    }

    /// Runs the one-shot upgraded protocol session.
    ///
    /// # Errors
    ///
    /// Returns the adapter or upgraded protocol error produced by the session.
    pub async fn run(mut self, io: Box<dyn UpgradedIo>) -> Result<(), UpgradeIoError> {
        let handler = self.handler.take().ok_or_else(|| {
            UpgradeIoError::new(
                "upgrade_already_consumed",
                "the protocol upgrade handler has already been consumed",
            )
        })?;
        handler(io).await
    }
}

impl fmt::Debug for HttpUpgrade {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpUpgrade")
            .field("protocol", &self.protocol)
            .field("headers", &self.headers)
            .finish_non_exhaustive()
    }
}

impl ApiSchema for HttpUpgrade {
    fn type_descriptor() -> TypeDescriptor {
        TypeDescriptor::scalar("HttpUpgrade", SchemaKind::Binary)
    }
}

/// A failure produced by work scheduled after an HTTP response is sent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackgroundTaskError {
    pub code: String,
    pub message: String,
}

impl BackgroundTaskError {
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for BackgroundTaskError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for BackgroundTaskError {}

pub type BackgroundFuture =
    Pin<Box<dyn Future<Output = Result<(), BackgroundTaskError>> + 'static>>;

/// One runtime-neutral task that begins after the response body is written.
pub struct BackgroundTask {
    task: Option<Box<dyn FnOnce() -> BackgroundFuture + 'static>>,
}

impl BackgroundTask {
    #[must_use]
    pub fn new<Task, TaskFuture>(task: Task) -> Self
    where
        Task: FnOnce() -> TaskFuture + 'static,
        TaskFuture: Future<Output = Result<(), BackgroundTaskError>> + 'static,
    {
        Self {
            task: Some(Box::new(move || Box::pin(task()))),
        }
    }

    #[must_use]
    pub fn infallible<Task, TaskFuture>(task: Task) -> Self
    where
        Task: FnOnce() -> TaskFuture + 'static,
        TaskFuture: Future<Output = ()> + 'static,
    {
        Self::new(move || async move {
            task().await;
            Ok(())
        })
    }

    /// Runs this task exactly once.
    ///
    /// # Errors
    ///
    /// Returns the task failure or an already-consumed error.
    pub async fn run(mut self) -> Result<(), BackgroundTaskError> {
        let task = self.task.take().ok_or_else(|| {
            BackgroundTaskError::new(
                "background_task_consumed",
                "background task has already been consumed",
            )
        })?;
        task().await
    }
}

impl fmt::Debug for BackgroundTask {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BackgroundTask")
            .finish_non_exhaustive()
    }
}

/// A typed response carrying work that starts after its wire body is sent.
#[derive(Debug)]
pub struct Background<T> {
    response: T,
    tasks: Vec<BackgroundTask>,
}

impl<T> Background<T> {
    #[must_use]
    pub fn new(response: T) -> Self {
        Self {
            response,
            tasks: Vec::new(),
        }
    }

    #[must_use]
    pub fn task(mut self, task: BackgroundTask) -> Self {
        self.tasks.push(task);
        self
    }

    #[must_use]
    pub fn into_parts(self) -> (T, Vec<BackgroundTask>) {
        (self.response, self.tasks)
    }
}

impl<T: ApiSchema> ApiSchema for Background<T> {
    fn type_descriptor() -> TypeDescriptor {
        T::type_descriptor()
    }
}

/// Ergonomic after-response task decoration.
pub trait BackgroundExt: Sized {
    #[must_use]
    fn background(self, task: BackgroundTask) -> Background<Self> {
        Background::new(self).task(task)
    }
}

impl<T> BackgroundExt for T {}

/// Prefixes nested model violations while preserving stable codes/messages.
pub fn merge_validation_errors(
    target: &mut ValidationErrors,
    prefix: &str,
    nested: &ValidationErrors,
) {
    for violation in nested.violations() {
        let field = if violation.field.is_empty() {
            prefix.to_owned()
        } else {
            format!("{prefix}.{}", violation.field)
        };
        target.push(field, violation.code.clone(), violation.message.clone());
    }
}

/// Records a field validator's violations under the field it was declared on.
///
/// A field validator is handed the value alone and cannot see the public name
/// of the field it is checking, yet the obvious way to write one is to name
/// that field anyway. Prefixing unconditionally, the way
/// [`merge_validation_errors`] does for a nested model, turns that into
/// `published_at.published_at`.
///
/// A violation is therefore taken to be about the field itself when its path
/// is empty or is the field, and to be a path inside the value otherwise.
pub fn merge_field_validation_errors(
    target: &mut ValidationErrors,
    field: &str,
    nested: &ValidationErrors,
) {
    for violation in nested.violations() {
        let path = if field.is_empty() {
            violation.field.clone()
        } else if violation.field.is_empty() || violation.field == field {
            field.to_owned()
        } else if is_rooted_at(&violation.field, field) {
            violation.field.clone()
        } else {
            format!("{field}.{}", violation.field)
        };
        target.push(path, violation.code.clone(), violation.message.clone());
    }
}

/// Reports whether `path` already descends from `field`.
fn is_rooted_at(path: &str, field: &str) -> bool {
    path.strip_prefix(field)
        .is_some_and(|rest| rest.starts_with('.') || rest.starts_with('['))
}

/// A value type whose field rules are declared once and reused by name.
///
/// `#[api_model]` implements this for a one-field tuple struct and for a
/// unit-variant enum. A model that carries such a field inherits
/// [`ApiConstrained::constraint_rules`] into its own field descriptor and runs
/// [`ApiConstrained::validate_constraints`] while validating, so a bundle of
/// rules written once applies to every field declared with the type.
pub trait ApiConstrained: ApiSchema {
    /// Rules a model records for every field declared with this type.
    fn constraint_rules() -> Vec<ValidationRule>;

    /// Runs the declared rules against one value.
    ///
    /// # Errors
    ///
    /// Returns the violations found. Their field paths are relative to the
    /// value, so the caller names the field the value was found in.
    fn validate_constraints(&self) -> Result<(), ValidationErrors>;
}

/// Field metadata carried inside [`ValidationRule::Custom`].
///
/// The contract's rule list has no variant for a default, for nullability, or
/// for a string enumeration, and its encoding is frozen. `#[api_model]`
/// therefore writes them as `keyword=value` strings — the channel already used
/// for `pattern` and the numeric bounds — and a schema projection recovers them
/// with [`FieldMetadata::parse`] instead of treating them as opaque validator
/// names.
#[derive(Clone, Debug, PartialEq)]
pub enum FieldMetadata {
    /// Value substituted when the field is absent from the request.
    Default(blazingly_json::Value),
    /// The field accepts `null` in addition to its declared type.
    Nullable,
    /// The complete set of accepted string values, in declaration order.
    Enumeration(Vec<String>),
}

impl FieldMetadata {
    /// Parses the canonical `keyword=value` encoding emitted by `#[api_model]`.
    #[must_use]
    pub fn parse(encoded: &str) -> Option<Self> {
        let (keyword, value) = encoded.split_once('=')?;
        let metadata = match keyword {
            "default" => Self::Default(blazingly_json::from_str(value).ok()?),
            "nullable" if value == "true" => Self::Nullable,
            "enum" if !value.is_empty() => {
                Self::Enumeration(value.split('|').map(str::to_owned).collect())
            }
            _ => return None,
        };
        Some(metadata)
    }

    /// JSON Schema keyword this metadata projects to.
    #[must_use]
    pub const fn keyword(&self) -> &'static str {
        match self {
            Self::Default(_) => "default",
            Self::Nullable => "nullable",
            Self::Enumeration(_) => "enum",
        }
    }

    /// JSON Schema value this metadata projects to.
    #[must_use]
    pub fn schema_value(&self) -> blazingly_json::Value {
        match self {
            Self::Default(value) => value.clone(),
            Self::Nullable => blazingly_json::Value::Bool(true),
            Self::Enumeration(values) => blazingly_json::Value::Array(
                values
                    .iter()
                    .map(|value| blazingly_json::Value::String(value.clone()))
                    .collect(),
            ),
        }
    }

    /// Writes the metadata into a JSON Schema object in place.
    pub fn apply_json_schema(&self, schema: &mut blazingly_json::Value) {
        if let Some(object) = schema.as_object_mut() {
            object.insert(self.keyword().to_owned(), self.schema_value());
        }
    }
}

impl fmt::Display for FieldMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default(value) => write!(formatter, "default={value}"),
            Self::Nullable => formatter.write_str("nullable=true"),
            Self::Enumeration(values) => {
                formatter.write_str("enum=")?;
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        formatter.write_str("|")?;
                    }
                    formatter.write_str(value)?;
                }
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// multipart/form-data syntax
// ---------------------------------------------------------------------------

/// Largest header block accepted for one `multipart/form-data` part.
///
/// The buffered and the streaming readers share this bound, so a body that one
/// accepts is not rejected by the other.
pub const MAX_MULTIPART_HEADER_BYTES: usize = 16 * 1024;

/// Largest number of parts accepted in one `multipart/form-data` body.
pub const MAX_MULTIPART_PARTS: usize = 256;

/// The `Content-Disposition` metadata of one multipart part.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MultipartPartHeaders {
    pub name: String,
    pub file_name: Option<String>,
    pub content_type: Option<String>,
}

/// Extracts the boundary from a `multipart/form-data` `Content-Type`.
///
/// Returns `None` when the media type is something else, when the boundary
/// parameter is absent, or when the boundary is not the 1..=70 printable
/// non-space characters RFC 2046 allows.
#[doc(hidden)]
#[must_use]
pub fn multipart_boundary(content_type: &str) -> Option<String> {
    let mut parameters = header_parameters(content_type);
    if !parameters
        .next()?
        .trim()
        .eq_ignore_ascii_case("multipart/form-data")
    {
        return None;
    }
    for parameter in parameters {
        let (name, value) = parameter.split_once('=')?;
        if name.trim().eq_ignore_ascii_case("boundary") {
            let boundary = unquote_header_value(value.trim())?;
            if boundary.is_empty()
                || boundary.len() > 70
                || boundary.bytes().any(|byte| byte <= b' ' || byte >= 127)
            {
                return None;
            }
            return Some(boundary);
        }
    }
    None
}

/// Parses one multipart part's header block.
///
/// # Errors
///
/// Returns the stable reason string both multipart readers report.
#[doc(hidden)]
pub fn multipart_part_headers(headers: &str) -> Result<MultipartPartHeaders, &'static str> {
    let mut name = None;
    let mut file_name = None;
    let mut content_type = None;
    for line in headers.split("\r\n") {
        let (header_name, value) = line
            .split_once(':')
            .ok_or("multipart part header is malformed")?;
        if header_name.eq_ignore_ascii_case("content-disposition") {
            let mut parameters = header_parameters(value);
            if !parameters
                .next()
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("form-data"))
            {
                return Err("multipart Content-Disposition must be form-data");
            }
            for parameter in parameters {
                let Some((parameter_name, parameter_value)) = parameter.split_once('=') else {
                    continue;
                };
                if parameter_name.trim().eq_ignore_ascii_case("name") {
                    name = unquote_header_value(parameter_value.trim());
                } else if parameter_name.trim().eq_ignore_ascii_case("filename") {
                    file_name = unquote_header_value(parameter_value.trim());
                }
            }
        } else if header_name.eq_ignore_ascii_case("content-type") {
            content_type = Some(value.trim().to_owned());
        }
    }
    let name = name
        .filter(|name| !name.is_empty())
        .ok_or("multipart part has no field name")?;
    Ok(MultipartPartHeaders {
        name,
        file_name,
        content_type,
    })
}

/// Splits a header value on unquoted semicolons.
#[doc(hidden)]
pub fn header_parameters(value: &str) -> impl Iterator<Item = &str> {
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    let mut ranges = Vec::new();
    for (index, character) in value.char_indices() {
        if escaped {
            escaped = false;
        } else if character == '\\' && quoted {
            escaped = true;
        } else if character == '"' {
            quoted = !quoted;
        } else if character == ';' && !quoted {
            ranges.push((start, index));
            start = index + character.len_utf8();
        }
    }
    ranges.push((start, value.len()));
    ranges
        .into_iter()
        .map(move |(start, end)| &value[start..end])
}

/// Removes the quotes and backslash escapes from one header parameter value.
#[doc(hidden)]
#[must_use]
pub fn unquote_header_value(value: &str) -> Option<String> {
    if let Some(value) = value.strip_prefix('"') {
        let value = value.strip_suffix('"')?;
        let mut output = String::with_capacity(value.len());
        let mut escaped = false;
        for character in value.chars() {
            if escaped {
                output.push(character);
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else {
                output.push(character);
            }
        }
        if escaped {
            return None;
        }
        Some(output)
    } else {
        Some(value.to_owned())
    }
}

/// Finds `needle` in `haystack` at or after `from`.
///
/// The scan looks for the first byte and only then compares the rest, so a
/// megabyte of upload costs one pass over the data rather than one comparison
/// per needle byte per position.
#[doc(hidden)]
#[must_use]
pub fn find_bytes(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    let (first, rest) = needle.split_first()?;
    let last_start = haystack.len().checked_sub(needle.len())?;
    let mut position = from;
    while position <= last_start {
        // The first-byte scan runs over every byte of a streamed upload, so
        // it uses `memchr`'s vectorized search rather than a scalar loop.
        let offset = memchr::memchr(*first, haystack.get(position..=last_start)?)?;
        let candidate = position + offset;
        if haystack.get(candidate + 1..candidate + needle.len()) == Some(rest) {
            return Some(candidate);
        }
        position = candidate + 1;
    }
    None
}

// ---------------------------------------------------------------------------
// Streaming multipart request bodies
// ---------------------------------------------------------------------------

/// A failure produced while reading a `multipart/form-data` request body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MultipartError {
    /// The bytes are not a well-formed `multipart/form-data` document.
    ///
    /// The reason strings are the ones the buffered `Multipart<T>` extractor
    /// already reports, and the projected failure carries the same status and
    /// code, so a malformed body looks the same whichever reader saw it.
    Malformed(&'static str),
    /// A handler asked to buffer a part that turned out to be larger than the
    /// limit it named.
    TooLarge { limit: usize },
    /// The transport could not deliver the rest of the body.
    ///
    /// Surfacing this instead of treating it as the end of the body is what
    /// stops a truncated upload from being reported as a complete one.
    Transport(BodyStreamError),
}

const MALFORMED_MULTIPART_MESSAGE: &str = "request body is not valid multipart form data";
const UPLOAD_STREAM_MESSAGE: &str = "the request body could not be read to its end";
const MULTIPART_TOO_LARGE_MESSAGE: &str = "multipart part exceeds the limit the handler set";

impl MultipartError {
    /// The HTTP status this failure projects to.
    #[must_use]
    pub const fn status(&self) -> u16 {
        match self {
            Self::Malformed(_) => 422,
            Self::TooLarge { .. } => 413,
            Self::Transport(_) => 400,
        }
    }

    /// The stable client-visible code this failure projects to.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Malformed(_) => "invalid_multipart",
            Self::TooLarge { .. } => "payload_too_large",
            Self::Transport(_) => "upload_stream_failed",
        }
    }

    /// The stable client-visible message this failure projects to.
    #[must_use]
    pub const fn message(&self) -> &'static str {
        match self {
            Self::Malformed(_) => MALFORMED_MULTIPART_MESSAGE,
            Self::TooLarge { .. } => MULTIPART_TOO_LARGE_MESSAGE,
            Self::Transport(_) => UPLOAD_STREAM_MESSAGE,
        }
    }

    fn details(&self) -> blazingly_json::Value {
        let mut details = blazingly_json::Map::new();
        match self {
            Self::Malformed(reason) => {
                details.insert(
                    "source".to_owned(),
                    blazingly_json::Value::String("multipart".to_owned()),
                );
                details.insert(
                    "reason".to_owned(),
                    blazingly_json::Value::String((*reason).to_owned()),
                );
            }
            Self::TooLarge { limit } => {
                details.insert(
                    "source".to_owned(),
                    blazingly_json::Value::String("multipart".to_owned()),
                );
                details.insert("limit".to_owned(), blazingly_json::Value::from(*limit));
            }
            Self::Transport(error) => {
                details.insert(
                    "source".to_owned(),
                    blazingly_json::Value::String("stream".to_owned()),
                );
                details.insert(
                    "reason".to_owned(),
                    blazingly_json::Value::String(error.code.clone()),
                );
            }
        }
        blazingly_json::Value::Object(details)
    }
}

impl fmt::Display for MultipartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(reason) => formatter.write_str(reason),
            Self::TooLarge { limit } => {
                write!(formatter, "multipart part exceeds the {limit}-byte limit")
            }
            Self::Transport(error) => formatter.write_str(&error.message),
        }
    }
}

impl std::error::Error for MultipartError {}

impl ApiError for MultipartError {
    fn response_descriptors() -> Vec<ResponseDescriptor> {
        vec![
            ResponseDescriptor::error(400, "upload_stream_failed", UPLOAD_STREAM_MESSAGE, None),
            ResponseDescriptor::error(413, "payload_too_large", MULTIPART_TOO_LARGE_MESSAGE, None),
            ResponseDescriptor::error(422, "invalid_multipart", MALFORMED_MULTIPART_MESSAGE, None),
        ]
    }

    fn into_failure(self) -> Result<OperationFailure, ResponseBuildError> {
        let details = blazingly_json::to_vec(&self.details())
            .map_err(|_| ResponseBuildError::serialization_failed())?;
        Ok(OperationFailure::new(self.status(), self.code(), self.message()).with_details(details))
    }
}

/// Where a [`MultipartStream`] currently is in the document.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MultipartState {
    /// Before the opening delimiter.
    Opening,
    /// Between two parts, with a header block still to read.
    Headers,
    /// Inside one part's data.
    Part,
    /// The closing delimiter has been consumed.
    Done,
    /// A failure was already reported; the document cannot be resumed.
    Failed,
}

/// What a scan of the available part data found.
enum MultipartScan {
    /// A closing delimiter begins at this offset.
    Terminator(usize),
    /// This many leading bytes are certainly part data.
    ///
    /// The rest is withheld because it could still turn out to be the start of
    /// a delimiter once more bytes arrive; it is never more than the delimiter
    /// plus four bytes.
    Data(usize),
}

/// A `multipart/form-data` request body read part by part, chunk by chunk.
///
/// The buffered [`Multipart<T>`] extractor materializes every part before the
/// handler starts, which costs resident memory proportional to the requests in
/// flight. This reader instead drives the pull-based request body: it holds one
/// transport chunk plus, at most, a delimiter's worth of look-ahead, whatever
/// the upload's size. A handler that counts bytes and drops them holds nothing.
///
/// Transport limits are not re-implemented here. The stream this reads from is
/// the adapter's, so the decoded-size limit, the chunk-count limit, and the
/// body read deadline apply exactly as they do to [`StreamingBody`] itself, and
/// they apply while the body is arriving rather than after it has all been
/// accepted.
///
/// # Examples
///
/// ```no_run
/// # use blazingly_core::{MultipartError, MultipartStream};
/// # async fn count(mut multipart: MultipartStream) -> Result<usize, MultipartError> {
/// let mut bytes = 0;
/// while let Some(mut field) = multipart.next_field().await? {
///     if field.name() != "file" {
///         continue;
///     }
///     while let Some(chunk) = field.next_chunk().await? {
///         bytes += chunk.len();
///     }
/// }
/// # Ok(bytes)
/// # }
/// ```
#[derive(Debug)]
pub struct MultipartStream {
    body: StreamingBody,
    /// `--` followed by the declared boundary.
    delimiter: Vec<u8>,
    buffer: Vec<u8>,
    cursor: usize,
    state: MultipartState,
    parts: usize,
    bytes_read: u64,
}

impl MultipartStream {
    /// Reads `body` as the `multipart/form-data` document `content_type`
    /// describes.
    ///
    /// # Errors
    ///
    /// Returns [`MultipartError::Malformed`] when `content_type` is not
    /// `multipart/form-data` with a usable boundary.
    pub fn new(body: StreamingBody, content_type: &str) -> Result<Self, MultipartError> {
        let boundary = multipart_boundary(content_type).ok_or(MultipartError::Malformed(
            "multipart boundary is missing or invalid",
        ))?;
        let mut delimiter = Vec::with_capacity(boundary.len() + 2);
        delimiter.extend_from_slice(b"--");
        delimiter.extend_from_slice(boundary.as_bytes());
        Ok(Self {
            body,
            delimiter,
            buffer: Vec::new(),
            cursor: 0,
            state: MultipartState::Opening,
            parts: 0,
            bytes_read: 0,
        })
    }

    /// Bytes pulled from the transport so far, framing included.
    #[must_use]
    pub const fn bytes_read(&self) -> u64 {
        self.bytes_read
    }

    /// Advances to the next part, skipping whatever is left of the current one.
    ///
    /// Returns `None` once the closing delimiter has been read.
    ///
    /// # Errors
    ///
    /// Returns [`MultipartError`] when the document is malformed, when it ends
    /// before its closing delimiter, or when the transport fails.
    pub async fn next_field(&mut self) -> Result<Option<MultipartField<'_>>, MultipartError> {
        match self.state {
            MultipartState::Failed => {
                return Err(MultipartError::Malformed(
                    "multipart body already failed to parse",
                ));
            }
            MultipartState::Done => return Ok(None),
            MultipartState::Opening => self.read_opening().await?,
            MultipartState::Part => self.skip_part().await?,
            MultipartState::Headers => {}
        }
        if self.state == MultipartState::Done {
            return Ok(None);
        }
        let headers = self.read_headers().await?;
        self.parts += 1;
        if self.parts > MAX_MULTIPART_PARTS {
            return Err(self.fail("multipart body contains too many parts"));
        }
        self.state = MultipartState::Part;
        Ok(Some(MultipartField {
            stream: self,
            headers,
        }))
    }

    /// Marks the document unusable and produces the failure to report.
    fn fail(&mut self, reason: &'static str) -> MultipartError {
        self.state = MultipartState::Failed;
        MultipartError::Malformed(reason)
    }

    /// Bytes buffered but not yet consumed.
    const fn available(&self) -> usize {
        self.buffer.len() - self.cursor
    }

    /// Pulls one transport chunk, reporting whether the body had more.
    async fn fill(&mut self) -> Result<bool, MultipartError> {
        loop {
            match self.body.next_chunk().await {
                Some(Ok(chunk)) if chunk.is_empty() => {}
                Some(Ok(chunk)) => {
                    // Dropping the consumed prefix before growing the buffer is
                    // what keeps a five-megabyte upload resident in kilobytes.
                    // The leftover is at most a delimiter's worth of withheld
                    // look-ahead, so the move is a few dozen bytes per refill.
                    if self.cursor > 0 {
                        self.buffer.drain(..self.cursor);
                        self.cursor = 0;
                    }
                    self.bytes_read = self
                        .bytes_read
                        .saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
                    self.buffer.extend_from_slice(&chunk);
                    // The chunk's bytes now live in the window, so its buffer
                    // goes back to the producer to be refilled.
                    self.body.recycle(chunk);
                    return Ok(true);
                }
                Some(Err(error)) => {
                    self.state = MultipartState::Failed;
                    return Err(MultipartError::Transport(error));
                }
                None => return Ok(false),
            }
        }
    }

    /// Consumes the opening delimiter.
    async fn read_opening(&mut self) -> Result<(), MultipartError> {
        while self.available() < self.delimiter.len() {
            if !self.fill().await? {
                return Err(self.fail("multipart body does not start with its declared boundary"));
            }
        }
        if !self.buffer[self.cursor..].starts_with(&self.delimiter) {
            return Err(self.fail("multipart body does not start with its declared boundary"));
        }
        self.cursor += self.delimiter.len();
        self.read_boundary_suffix().await
    }

    /// Consumes the two bytes that follow a delimiter and picks the next state.
    async fn read_boundary_suffix(&mut self) -> Result<(), MultipartError> {
        while self.available() < 2 {
            if !self.fill().await? {
                return Err(self.fail("multipart boundary is malformed"));
            }
        }
        let suffix = [self.buffer[self.cursor], self.buffer[self.cursor + 1]];
        self.cursor += 2;
        match &suffix {
            b"--" => {
                self.state = MultipartState::Done;
                Ok(())
            }
            b"\r\n" => {
                self.state = MultipartState::Headers;
                Ok(())
            }
            _ => Err(self.fail("multipart boundary is malformed")),
        }
    }

    /// Reads one part's header block.
    async fn read_headers(&mut self) -> Result<MultipartPartHeaders, MultipartError> {
        let mut searched = 0;
        let end = loop {
            if let Some(found) = find_bytes(&self.buffer[self.cursor..], b"\r\n\r\n", searched) {
                break found;
            }
            let available = self.available();
            if available > MAX_MULTIPART_HEADER_BYTES {
                return Err(self.fail("multipart part headers exceed the configured limit"));
            }
            searched = available.saturating_sub(3);
            if !self.fill().await? {
                return Err(self.fail("multipart part headers are incomplete"));
            }
        };
        if end > MAX_MULTIPART_HEADER_BYTES {
            return Err(self.fail("multipart part headers exceed the configured limit"));
        }
        let parsed = match std::str::from_utf8(&self.buffer[self.cursor..self.cursor + end]) {
            Ok(headers) => multipart_part_headers(headers),
            Err(_) => Err("multipart part headers are not valid UTF-8"),
        };
        let parsed = parsed.map_err(|reason| self.fail(reason))?;
        self.cursor += end + 4;
        Ok(parsed)
    }

    /// Consumes whatever is left of the current part.
    async fn skip_part(&mut self) -> Result<(), MultipartError> {
        while self.advance_part().await?.is_some() {}
        Ok(())
    }

    /// Advances past the next run of part data.
    ///
    /// The returned pair is a range in `self.buffer` that stays valid until the
    /// next call, which is what lets a chunk be handed out without copying it.
    async fn advance_part(&mut self) -> Result<Option<(usize, usize)>, MultipartError> {
        loop {
            if self.state != MultipartState::Part {
                return Ok(None);
            }
            match scan_multipart_terminator(&self.buffer[self.cursor..], &self.delimiter) {
                MultipartScan::Terminator(offset) => {
                    if offset > 0 {
                        let start = self.cursor;
                        self.cursor += offset;
                        return Ok(Some((start, self.cursor)));
                    }
                    self.cursor += 2 + self.delimiter.len();
                    self.read_boundary_suffix().await?;
                    return Ok(None);
                }
                MultipartScan::Data(safe) => {
                    if safe > 0 {
                        let start = self.cursor;
                        self.cursor += safe;
                        return Ok(Some((start, self.cursor)));
                    }
                    if !self.fill().await? {
                        return Err(self.fail("multipart part has no closing boundary"));
                    }
                }
            }
        }
    }
}

/// One part of a streamed `multipart/form-data` body.
///
/// Dropping a field without reading it to the end is allowed; the next
/// [`MultipartStream::next_field`] call skips whatever is left.
#[derive(Debug)]
pub struct MultipartField<'stream> {
    stream: &'stream mut MultipartStream,
    headers: MultipartPartHeaders,
}

impl MultipartField<'_> {
    /// The part's form field name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.headers.name
    }

    /// The part's declared file name, when it has one.
    #[must_use]
    pub fn file_name(&self) -> Option<&str> {
        self.headers.file_name.as_deref()
    }

    /// The part's declared media type, when it has one.
    #[must_use]
    pub fn content_type(&self) -> Option<&str> {
        self.headers.content_type.as_deref()
    }

    /// Pulls the next run of this part's data.
    ///
    /// The slice borrows the reader's buffer and is valid until the next call,
    /// so nothing is copied on the way to the handler. Returns `None` once the
    /// part's closing delimiter has been reached.
    ///
    /// # Errors
    ///
    /// Returns [`MultipartError`] when the document ends before its closing
    /// delimiter or when the transport fails.
    pub async fn next_chunk(&mut self) -> Result<Option<&[u8]>, MultipartError> {
        match self.stream.advance_part().await? {
            Some((start, end)) => Ok(Some(&self.stream.buffer[start..end])),
            None => Ok(None),
        }
    }

    /// Deliberately buffers the rest of this part, with an explicit limit.
    ///
    /// # Errors
    ///
    /// Returns [`MultipartError::TooLarge`] when the part is longer than
    /// `limit`, or the reader failure that stopped it.
    pub async fn collect(mut self, limit: usize) -> Result<Vec<u8>, MultipartError> {
        let mut bytes = Vec::new();
        while let Some(chunk) = self.next_chunk().await? {
            if bytes.len().saturating_add(chunk.len()) > limit {
                return Err(MultipartError::TooLarge { limit });
            }
            bytes.extend_from_slice(chunk);
        }
        Ok(bytes)
    }

    /// Deliberately buffers the rest of this part as text.
    ///
    /// # Errors
    ///
    /// Returns [`MultipartError::TooLarge`] when the part is longer than
    /// `limit`, [`MultipartError::Malformed`] when it is not UTF-8, or the
    /// reader failure that stopped it.
    pub async fn text(self, limit: usize) -> Result<String, MultipartError> {
        let bytes = self.collect(limit).await?;
        String::from_utf8(bytes)
            .map_err(|_| MultipartError::Malformed("multipart text field is not valid UTF-8"))
    }

    /// Deliberately buffers the rest of this part as an [`UploadFile`].
    ///
    /// This is the bridge to the buffered API: an operation that wants the
    /// bytes after all gets the same value the `File<UploadFile>` extractor
    /// would have produced, having chosen the limit itself.
    ///
    /// # Errors
    ///
    /// Returns [`MultipartError::TooLarge`] when the part is longer than
    /// `limit`, or the reader failure that stopped it.
    pub async fn into_upload(self, limit: usize) -> Result<UploadFile, MultipartError> {
        let field_name = self.headers.name.clone();
        let file_name = self.headers.file_name.clone();
        let content_type = self.headers.content_type.clone();
        let bytes = self.collect(limit).await?;
        Ok(UploadFile {
            field_name,
            file_name,
            content_type,
            bytes,
        })
    }
}

/// Looks for the closing delimiter in the part data available so far.
fn scan_multipart_terminator(data: &[u8], delimiter: &[u8]) -> MultipartScan {
    let mut from = 0;
    while let Some(found) = find_bytes(data, b"\r\n--", from) {
        let start = found + 2;
        let end = start + delimiter.len();
        if end + 2 > data.len() {
            // Undecidable yet. Withhold the candidate only while what did
            // arrive still agrees with the delimiter; otherwise it is data and
            // the scan continues past it.
            let seen = &data[start..];
            let compared = seen.len().min(delimiter.len());
            if seen.get(..compared) == delimiter.get(..compared) {
                return MultipartScan::Data(found);
            }
            from = found + 2;
            continue;
        }
        if data.get(start..end) == Some(delimiter)
            && matches!(data.get(end..end + 2), Some(b"\r\n" | b"--"))
        {
            return MultipartScan::Terminator(found);
        }
        from = found + 2;
    }
    // A trailing `\r`, `\r\n`, or `\r\n-` could still become a delimiter.
    MultipartScan::Data(data.len().saturating_sub(3))
}

struct ChunkIterator<I> {
    chunks: I,
}

impl<I, Chunk> BodyStream for ChunkIterator<I>
where
    I: Iterator<Item = Chunk> + Unpin + 'static,
    Chunk: Into<Vec<u8>> + 'static,
{
    fn poll_next(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Option<Result<Vec<u8>, BodyStreamError>>> {
        Poll::Ready(self.get_mut().chunks.next().map(|chunk| Ok(chunk.into())))
    }
}

/// Overrides the successful status of another typed response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Status<const STATUS: u16, T>(pub T);

/// Adds response headers without changing the typed response body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WithHeaders<T> {
    response: T,
    headers: Vec<ResponseHeader>,
}

impl<T> WithHeaders<T> {
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push(ResponseHeader::new(name, value));
        self
    }

    #[must_use]
    pub fn into_parts(self) -> (T, Vec<ResponseHeader>) {
        (self.response, self.headers)
    }
}

/// Ergonomic response decoration shared by typed success responses.
pub trait ResponseExt: Sized {
    #[must_use]
    fn header(self, name: impl Into<String>, value: impl Into<String>) -> WithHeaders<Self> {
        WithHeaders {
            response: self,
            headers: vec![ResponseHeader::new(name, value)],
        }
    }
}

impl<T> ResponseExt for T {}

/// HTTP methods supported by the operation frontend.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
    Options,
    Trace,
    Connect,
}

impl HttpMethod {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Head => "HEAD",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
            Self::Options => "OPTIONS",
            Self::Trace => "TRACE",
            Self::Connect => "CONNECT",
        }
    }

    #[must_use]
    pub const fn as_openapi_key(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::Head => "head",
            Self::Post => "post",
            Self::Put => "put",
            Self::Patch => "patch",
            Self::Delete => "delete",
            Self::Options => "options",
            Self::Trace => "trace",
            // CONNECT is not a standard OpenAPI Path Item field.
            Self::Connect => "x-blazingly-connect",
        }
    }
}

/// The HTTP projection of a protocol-neutral operation contract.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HttpBinding {
    pub method: HttpMethod,
    pub path: String,
}

impl HttpBinding {
    #[must_use]
    pub fn new(method: HttpMethod, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
        }
    }
}

/// A protocol-neutral operation paired with its HTTP projection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OperationDescriptor {
    pub contract: OperationContract,
    pub http: HttpBinding,
}

impl OperationDescriptor {
    /// Creates an HTTP projection of a protocol-neutral operation.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidOperationId`] when `id` is not a valid stable
    /// operation identity.
    pub fn new(
        method: HttpMethod,
        path: impl Into<String>,
        id: impl Into<String>,
        summary: impl Into<String>,
        input: Option<TypeDescriptor>,
        responses: Vec<ResponseDescriptor>,
    ) -> Result<Self, InvalidOperationId> {
        Ok(Self {
            contract: OperationContract::new(id, summary, input, responses)?,
            http: HttpBinding::new(method, path),
        })
    }

    #[must_use]
    pub fn with_mcp_tool(mut self, tool: McpToolDescriptor, policy: AgentPolicy) -> Self {
        self.contract = self.contract.with_agent_policy(policy).with_mcp_tool(tool);
        self
    }

    #[must_use]
    pub fn mcp_tool(&self) -> Option<&McpToolDescriptor> {
        self.contract.mcp.as_ref()
    }

    #[must_use]
    pub fn with_inputs(mut self, inputs: Vec<InputDescriptor>) -> Self {
        self.contract = self.contract.with_inputs(inputs);
        self
    }

    #[must_use]
    pub fn with_dependencies(mut self, dependencies: Vec<DependencyDescriptor>) -> Self {
        self.contract = self.contract.with_dependencies(dependencies);
        self
    }

    #[must_use]
    pub fn with_security(mut self, requirements: Vec<SecurityRequirement>) -> Self {
        self.contract = self.contract.with_security(requirements);
        self
    }
}

/// A validated, deterministic application description.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AppDefinition {
    operations: Vec<OperationDescriptor>,
    security_schemes: Vec<SecuritySchemeDescriptor>,
}

impl AppDefinition {
    #[must_use]
    pub fn operations(&self) -> &[OperationDescriptor] {
        &self.operations
    }

    #[must_use]
    pub fn security_schemes(&self) -> &[SecuritySchemeDescriptor] {
        &self.security_schemes
    }
}

/// Builder for an application description.
#[derive(Clone, Debug, Default)]
pub struct App {
    operations: Vec<OperationDescriptor>,
    security_schemes: Vec<SecuritySchemeDescriptor>,
}

impl App {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            operations: Vec::new(),
            security_schemes: Vec::new(),
        }
    }

    #[must_use]
    pub fn route(mut self, operation: OperationDescriptor) -> Self {
        self.operations.push(operation);
        self
    }

    #[must_use]
    pub fn routes(mut self, operations: impl IntoIterator<Item = OperationDescriptor>) -> Self {
        self.operations.extend(operations);
        self
    }

    /// Registers a named security scheme referenced by operation contracts.
    #[must_use]
    pub fn security_scheme(mut self, scheme: SecuritySchemeDescriptor) -> Self {
        self.security_schemes.push(scheme);
        self
    }

    /// Validates and deterministically orders the application graph.
    ///
    /// # Errors
    ///
    /// Returns [`BuildError`] when an operation identity or HTTP binding is
    /// registered more than once.
    pub fn build(mut self) -> Result<AppDefinition, BuildError> {
        let mut operation_ids = BTreeSet::new();
        let mut http_bindings = BTreeSet::new();
        let mut route_shapes = BTreeSet::new();
        let mut security_names = BTreeSet::new();

        for scheme in &self.security_schemes {
            if !security_names.insert(scheme.name.clone()) {
                return Err(BuildError::DuplicateSecurityScheme(scheme.name.clone()));
            }
        }

        for operation in &self.operations {
            validate_operation_inputs(operation)?;
            validate_operation_security(operation, &self.security_schemes)?;
            if !operation_ids.insert(operation.contract.id.clone()) {
                return Err(BuildError::DuplicateOperationId(
                    operation.contract.id.clone(),
                ));
            }

            let binding = (operation.http.method, operation.http.path.clone());
            if !http_bindings.insert(binding) {
                return Err(BuildError::DuplicateHttpBinding {
                    method: operation.http.method,
                    path: operation.http.path.clone(),
                });
            }
            if !route_shapes.insert((
                operation.http.method,
                canonical_route_shape(&operation.http.path),
            )) {
                return Err(BuildError::AmbiguousHttpBinding {
                    method: operation.http.method,
                    path: operation.http.path.clone(),
                });
            }
        }

        self.operations.sort_by(|left, right| {
            left.http
                .path
                .cmp(&right.http.path)
                .then(left.http.method.cmp(&right.http.method))
                .then(left.contract.id.cmp(&right.contract.id))
        });
        self.security_schemes
            .sort_by(|left, right| left.name.cmp(&right.name));

        Ok(AppDefinition {
            operations: self.operations,
            security_schemes: self.security_schemes,
        })
    }
}

/// An invalid application graph.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BuildError {
    DuplicateOperationId(OperationId),
    DuplicateHttpBinding {
        method: HttpMethod,
        path: String,
    },
    AmbiguousHttpBinding {
        method: HttpMethod,
        path: String,
    },
    InvalidPathInputs {
        operation: OperationId,
    },
    DuplicateInputName {
        operation: OperationId,
        name: String,
    },
    DuplicateSecurityScheme(String),
    DuplicateSecurityRequirement {
        operation: OperationId,
        scheme: String,
    },
    UnknownSecurityScheme {
        operation: OperationId,
        scheme: String,
    },
    UnknownSecurityScope {
        operation: OperationId,
        scheme: String,
        scope: String,
    },
}

impl fmt::Display for BuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateOperationId(id) => {
                write!(
                    formatter,
                    "operation id {id:?} is registered more than once"
                )
            }
            Self::DuplicateHttpBinding { method, path } => write!(
                formatter,
                "{} {path} is registered more than once",
                method.as_str()
            ),
            Self::AmbiguousHttpBinding { method, path } => write!(
                formatter,
                "{} {path} conflicts with another parameterized route",
                method.as_str()
            ),
            Self::InvalidPathInputs { operation } => write!(
                formatter,
                "operation {operation} path placeholders do not match its Path<T> inputs"
            ),
            Self::DuplicateInputName { operation, name } => write!(
                formatter,
                "operation {operation} exposes input name {name:?} more than once"
            ),
            Self::DuplicateSecurityScheme(name) => {
                write!(
                    formatter,
                    "security scheme {name:?} is registered more than once"
                )
            }
            Self::DuplicateSecurityRequirement { operation, scheme } => write!(
                formatter,
                "operation {operation} requires security scheme {scheme:?} more than once"
            ),
            Self::UnknownSecurityScheme { operation, scheme } => write!(
                formatter,
                "operation {operation} references unknown security scheme {scheme:?}"
            ),
            Self::UnknownSecurityScope {
                operation,
                scheme,
                scope,
            } => write!(
                formatter,
                "operation {operation} requires unknown scope {scope:?} from security scheme {scheme:?}"
            ),
        }
    }
}

impl std::error::Error for BuildError {}

fn canonical_route_shape(path: &str) -> String {
    path.split('/')
        .map(|segment| {
            if segment.starts_with('{') && segment.ends_with('}') {
                "{}"
            } else {
                segment
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn validate_operation_inputs(operation: &OperationDescriptor) -> Result<(), BuildError> {
    let placeholders = operation
        .http
        .path
        .split('/')
        .filter_map(|segment| {
            segment
                .strip_prefix('{')
                .and_then(|segment| segment.strip_suffix('}'))
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
        })
        .collect::<BTreeSet<_>>();
    let path_inputs = operation
        .contract
        .inputs
        .iter()
        .filter(|input| input.source == InputSource::Path)
        .flat_map(input_public_names)
        .collect::<BTreeSet<_>>();
    if placeholders != path_inputs {
        return Err(BuildError::InvalidPathInputs {
            operation: operation.contract.id.clone(),
        });
    }

    let mut names = BTreeSet::new();
    for name in operation
        .contract
        .inputs
        .iter()
        .flat_map(input_public_names)
    {
        if !names.insert(name.clone()) {
            return Err(BuildError::DuplicateInputName {
                operation: operation.contract.id.clone(),
                name,
            });
        }
    }
    Ok(())
}

fn validate_operation_security(
    operation: &OperationDescriptor,
    schemes: &[SecuritySchemeDescriptor],
) -> Result<(), BuildError> {
    let mut required_schemes = BTreeSet::new();
    for requirement in &operation.contract.security {
        if !required_schemes.insert(requirement.scheme.as_str()) {
            return Err(BuildError::DuplicateSecurityRequirement {
                operation: operation.contract.id.clone(),
                scheme: requirement.scheme.clone(),
            });
        }
        let Some(scheme) = schemes
            .iter()
            .find(|scheme| scheme.name == requirement.scheme)
        else {
            return Err(BuildError::UnknownSecurityScheme {
                operation: operation.contract.id.clone(),
                scheme: requirement.scheme.clone(),
            });
        };
        let declared_scopes = match &scheme.kind {
            SecuritySchemeKind::OAuth2 { scopes, .. } => Some(scopes),
            SecuritySchemeKind::ApiKey { .. }
            | SecuritySchemeKind::Http { .. }
            | SecuritySchemeKind::OpenIdConnect { .. }
            | SecuritySchemeKind::MutualTls => None,
        };
        for scope in &requirement.scopes {
            if declared_scopes.is_none_or(|scopes| !scopes.contains(scope)) {
                return Err(BuildError::UnknownSecurityScope {
                    operation: operation.contract.id.clone(),
                    scheme: requirement.scheme.clone(),
                    scope: scope.clone(),
                });
            }
        }
    }
    Ok(())
}

fn input_public_names(input: &InputDescriptor) -> Vec<String> {
    input.ty.model.as_ref().map_or_else(
        || vec![input.name.clone()],
        |model| {
            model
                .fields
                .iter()
                .map(|field| field.name.clone())
                .collect()
        },
    )
}

#[macro_export]
macro_rules! descriptors {
    ($($operation:ident),* $(,)?) => {
        ::std::vec![$($operation::descriptor()),*]
    };
}

#[cfg(test)]
mod tests {
    use super::{
        ApiError, ApiSchema, App, BodyStream, BodyStreamError, BuildError, FieldMetadata,
        HttpMethod, InputDescriptor, InputSource, MAX_MULTIPART_PARTS, MAX_RESPONSE_HINT,
        MIN_RESPONSE_HINT, MultipartError, MultipartStream, OperationDescriptor, PreparedJson,
        ResponseDescriptor, SchemaKind, SecurityRequirement, SecuritySchemeDescriptor,
        SecuritySchemeKind, StreamingBody, TypeDescriptor, UploadFile, UploadSlots,
        merge_field_validation_errors, record_response_size, response_size_hint,
    };
    use crate::ValidationErrors;
    use futures_lite::future::block_on;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    const BOUNDARY: &str = "apibenchcover9f2c41d7e6b3";

    fn content_type() -> String {
        format!("multipart/form-data; boundary={BOUNDARY}")
    }

    /// One part to assemble: field name, file name, media type, and data.
    type TestPart<'data> = (
        &'data str,
        Option<&'data str>,
        Option<&'data str>,
        &'data [u8],
    );

    /// Assembles one `multipart/form-data` document.
    fn multipart_document(parts: &[TestPart<'_>]) -> Vec<u8> {
        let mut body = Vec::new();
        for (name, file_name, media_type, data) in parts {
            body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
            body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"").as_bytes(),
            );
            if let Some(file_name) = file_name {
                body.extend_from_slice(format!("; filename=\"{file_name}\"").as_bytes());
            }
            body.extend_from_slice(b"\r\n");
            if let Some(media_type) = media_type {
                body.extend_from_slice(format!("Content-Type: {media_type}\r\n").as_bytes());
            }
            body.extend_from_slice(b"\r\n");
            body.extend_from_slice(data);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
        body
    }

    /// A body delivered in fixed-size chunks, so a delimiter can be split
    /// across as many transport chunks as the test wants.
    struct SlicedBody {
        bytes: Vec<u8>,
        chunk: usize,
        position: usize,
    }

    impl BodyStream for SlicedBody {
        fn poll_next(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Option<Result<Vec<u8>, BodyStreamError>>> {
            let body = self.get_mut();
            if body.position >= body.bytes.len() {
                return Poll::Ready(None);
            }
            let end = body.bytes.len().min(body.position + body.chunk);
            let chunk = body.bytes[body.position..end].to_vec();
            body.position = end;
            Poll::Ready(Some(Ok(chunk)))
        }
    }

    /// A body that delivers a prefix and then fails, the way a stalled or
    /// aborted upload does.
    struct FailingBody {
        head: Option<Vec<u8>>,
    }

    impl BodyStream for FailingBody {
        fn poll_next(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Option<Result<Vec<u8>, BodyStreamError>>> {
            Poll::Ready(Some(match self.get_mut().head.take() {
                Some(head) => Ok(head),
                None => Err(BodyStreamError::new(
                    "upload_timeout",
                    "request body stalled past the configured deadline",
                )),
            }))
        }
    }

    fn reader(bytes: Vec<u8>, chunk: usize) -> MultipartStream {
        MultipartStream::new(
            StreamingBody::new(SlicedBody {
                bytes,
                chunk,
                position: 0,
            }),
            &content_type(),
        )
        .expect("the declared content type carries a boundary")
    }

    /// Reads every part, returning its name and the bytes it carried.
    async fn read_all(
        stream: &mut MultipartStream,
    ) -> Result<Vec<(String, Vec<u8>)>, MultipartError> {
        let mut parts = Vec::new();
        while let Some(mut field) = stream.next_field().await? {
            let name = field.name().to_owned();
            let mut bytes = Vec::new();
            while let Some(chunk) = field.next_chunk().await? {
                bytes.extend_from_slice(chunk);
            }
            parts.push((name, bytes));
        }
        Ok(parts)
    }

    #[test]
    fn a_streamed_document_yields_every_part_whatever_the_chunk_size() {
        let payload = vec![7_u8; 40_000];
        let document = multipart_document(&[
            ("title", None, None, b"A cover"),
            (
                "file",
                Some("cover.jpg"),
                Some("image/jpeg"),
                payload.as_slice(),
            ),
            ("note", None, None, b""),
        ]);

        // One byte at a time splits the delimiter, the header block, and the
        // CRLF that precedes a boundary across as many chunks as possible.
        for chunk in [1, 2, 3, 7, 64, 8192, usize::MAX] {
            let mut stream = reader(document.clone(), chunk);
            let parts = block_on(read_all(&mut stream)).expect("the document parses");
            assert_eq!(parts.len(), 3, "chunk size {chunk}");
            assert_eq!(parts[0], ("title".to_owned(), b"A cover".to_vec()));
            assert_eq!(parts[1].0, "file");
            assert_eq!(parts[1].1, payload);
            assert_eq!(parts[2], ("note".to_owned(), Vec::new()));
        }
    }

    #[test]
    fn part_metadata_survives_the_streaming_reader() {
        let document =
            multipart_document(&[("file", Some("cover.jpg"), Some("image/jpeg"), b"body")]);
        let mut stream = reader(document, 5);
        block_on(async {
            let field = stream
                .next_field()
                .await
                .expect("the document parses")
                .expect("one part");
            assert_eq!(field.name(), "file");
            assert_eq!(field.file_name(), Some("cover.jpg"));
            assert_eq!(field.content_type(), Some("image/jpeg"));
        });
    }

    #[test]
    fn part_data_that_looks_like_a_boundary_is_still_data() {
        let mut data = Vec::new();
        data.extend_from_slice(b"\r\n--not-the-boundary\r\n");
        data.extend_from_slice(format!("\r\n--{BOUNDARY}x\r\n").as_bytes());
        data.extend_from_slice(format!("\r\n--{BOUNDARY}").as_bytes());
        data.extend_from_slice(b"tail\r\n");
        data.extend_from_slice(b"\r\n-\r\n--\r");
        let document = multipart_document(&[("file", None, None, &data)]);

        for chunk in [1, 4, 17, 8192] {
            let mut stream = reader(document.clone(), chunk);
            let parts = block_on(read_all(&mut stream)).expect("the document parses");
            assert_eq!(parts.len(), 1, "chunk size {chunk}");
            assert_eq!(parts[0].1, data, "chunk size {chunk}");
        }
    }

    #[test]
    fn a_large_upload_never_becomes_resident() {
        // Five mebibytes, the size the upload benchmark sends, delivered in the
        // eight-kibibyte chunks the native adapter uses.
        let payload = vec![9_u8; 5 * 1024 * 1024];
        let document =
            multipart_document(&[("file", Some("cover.jpg"), Some("image/jpeg"), &payload)]);
        let mut stream = reader(document, 8192);

        let (bytes, peak) = block_on(async {
            let mut bytes = 0_usize;
            let mut peak = 0_usize;
            let mut field = stream
                .next_field()
                .await
                .expect("the document parses")
                .expect("one part");
            while let Some(chunk) = field.next_chunk().await.expect("a chunk") {
                bytes += chunk.len();
                peak = peak.max(field.stream.buffer.capacity());
            }
            (bytes, peak)
        });

        assert_eq!(bytes, payload.len());
        // One transport chunk plus a delimiter of look-ahead, not the upload.
        assert!(
            peak < 64 * 1024,
            "a five-megabyte upload held {peak} buffered bytes"
        );
    }

    #[test]
    fn a_skipped_part_does_not_disturb_the_next_one() {
        let document = multipart_document(&[
            ("file", None, None, &[3_u8; 5000]),
            ("note", None, None, b"kept"),
        ]);
        let mut stream = reader(document, 128);
        block_on(async {
            let first = stream
                .next_field()
                .await
                .expect("the document parses")
                .expect("a first part");
            assert_eq!(first.name(), "file");
            drop(first);
            let mut second = stream
                .next_field()
                .await
                .expect("the rest of the document parses")
                .expect("a second part");
            assert_eq!(second.name(), "note");
            let chunk = second
                .next_chunk()
                .await
                .expect("a chunk")
                .expect("the part has data");
            assert_eq!(chunk, b"kept");
        });
    }

    #[test]
    fn a_document_with_no_parts_reads_as_empty() {
        let mut stream = reader(format!("--{BOUNDARY}--").into_bytes(), 3);
        let parts = block_on(read_all(&mut stream)).expect("an empty document parses");
        assert!(parts.is_empty());
    }

    #[test]
    fn a_body_that_ends_before_its_closing_boundary_is_a_failure_not_a_short_read() {
        let mut truncated = multipart_document(&[("file", None, None, &[1_u8; 4096])]);
        truncated.truncate(2048);
        let mut stream = reader(truncated, 512);
        let error = block_on(read_all(&mut stream)).expect_err("a truncated body cannot succeed");
        assert_eq!(
            error,
            MultipartError::Malformed("multipart part has no closing boundary")
        );
    }

    #[test]
    fn a_producer_failure_mid_body_is_reported_not_swallowed() {
        let mut document = multipart_document(&[("file", None, None, &[1_u8; 4096])]);
        document.truncate(1024);
        let mut stream = MultipartStream::new(
            StreamingBody::new(FailingBody {
                head: Some(document),
            }),
            &content_type(),
        )
        .expect("the declared content type carries a boundary");

        let error = block_on(read_all(&mut stream)).expect_err("a failed producer cannot succeed");
        let MultipartError::Transport(transport) = &error else {
            panic!("expected a transport failure, got {error:?}");
        };
        assert_eq!(transport.code, "upload_timeout");
        assert_eq!(error.status(), 400);
        assert_eq!(error.code(), "upload_stream_failed");
    }

    #[test]
    fn a_failed_document_cannot_be_resumed() {
        let mut stream = reader(b"not a multipart body at all".to_vec(), 4);
        block_on(async {
            let first = stream
                .next_field()
                .await
                .expect_err("the body is malformed");
            assert_eq!(
                first,
                MultipartError::Malformed(
                    "multipart body does not start with its declared boundary"
                )
            );
            let second = stream
                .next_field()
                .await
                .expect_err("the reader stays failed");
            assert_eq!(
                second,
                MultipartError::Malformed("multipart body already failed to parse")
            );
        });
    }

    #[test]
    fn a_malformed_document_projects_the_buffered_extractors_failure() {
        let failure = MultipartError::Malformed("multipart boundary is malformed")
            .into_failure()
            .expect("the failure projects");
        assert_eq!(failure.status, 422);
        assert_eq!(failure.code, "invalid_multipart");
        assert_eq!(
            failure.message,
            "request body is not valid multipart form data"
        );
        let details: blazingly_json::Value =
            blazingly_json::from_slice(&failure.details.expect("details")).expect("valid JSON");
        assert_eq!(
            details,
            blazingly_json::json!({
                "source": "multipart",
                "reason": "multipart boundary is malformed"
            })
        );
    }

    #[test]
    fn a_content_type_without_a_usable_boundary_is_rejected() {
        for content_type in [
            "application/json",
            "multipart/form-data",
            "multipart/form-data; boundary=",
            "multipart/form-data; boundary=\"with space\"",
        ] {
            let error = MultipartStream::new(StreamingBody::once(Vec::new()), content_type)
                .err()
                .unwrap_or_else(|| panic!("{content_type} should not carry a boundary"));
            assert_eq!(
                error,
                MultipartError::Malformed("multipart boundary is missing or invalid")
            );
        }
    }

    #[test]
    fn a_document_with_too_many_parts_is_rejected() {
        let empty = Vec::new();
        let parts = (0..=MAX_MULTIPART_PARTS)
            .map(|_| ("field", None, None, empty.as_slice()))
            .collect::<Vec<_>>();
        let mut stream = reader(multipart_document(&parts), 512);
        let error = block_on(read_all(&mut stream)).expect_err("the part count is bounded");
        assert_eq!(
            error,
            MultipartError::Malformed("multipart body contains too many parts")
        );
    }

    #[test]
    fn an_oversized_part_header_block_is_rejected() {
        let mut document = format!("--{BOUNDARY}\r\n").into_bytes();
        document.extend_from_slice(b"Content-Disposition: form-data; name=\"file\"\r\n");
        document.extend_from_slice(b"X-Padding: ");
        document.extend_from_slice(&vec![b'p'; 32 * 1024]);
        document.extend_from_slice(b"\r\n\r\ndata\r\n");
        document.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());

        let mut stream = reader(document, 4096);
        let error = block_on(read_all(&mut stream)).expect_err("the header block is bounded");
        assert_eq!(
            error,
            MultipartError::Malformed("multipart part headers exceed the configured limit")
        );
    }

    #[test]
    fn a_field_can_still_be_buffered_deliberately_with_a_limit() {
        let document = multipart_document(&[
            ("title", None, None, "A cover".as_bytes()),
            ("file", Some("c.png"), Some("image/png"), &[4_u8; 300]),
        ]);
        let mut stream = reader(document, 37);
        block_on(async {
            let title = stream
                .next_field()
                .await
                .expect("the document parses")
                .expect("a first part");
            assert_eq!(title.text(64).await.expect("the text fits"), "A cover");

            let file = stream
                .next_field()
                .await
                .expect("the document parses")
                .expect("a second part");
            let upload = file.into_upload(1024).await.expect("the upload fits");
            assert_eq!(
                upload,
                UploadFile::new("file", vec![4_u8; 300])
                    .with_file_name("c.png")
                    .with_content_type("image/png")
            );
        });
    }

    #[test]
    fn deliberate_buffering_still_honours_the_limit_it_was_given() {
        let document = multipart_document(&[("file", None, None, &[4_u8; 300])]);
        let mut stream = reader(document, 64);
        let error = block_on(async {
            let field = stream
                .next_field()
                .await
                .expect("the document parses")
                .expect("one part");
            field.collect(128).await.expect_err("the part is too large")
        });
        assert_eq!(error, MultipartError::TooLarge { limit: 128 });
        assert_eq!(error.status(), 413);
    }

    struct DocumentedPage;

    impl ApiSchema for DocumentedPage {
        fn type_descriptor() -> TypeDescriptor {
            TypeDescriptor::scalar("DocumentedPage", SchemaKind::Object)
        }
    }

    struct HintProbe;

    #[test]
    fn prepared_json_encodes_a_borrowed_view() {
        let owned = [String::from("alpha"), String::from("beta")];
        let borrowed: Vec<&str> = owned.iter().map(String::as_str).collect();
        let body = PreparedJson::<DocumentedPage>::encode(&borrowed).expect("the view encodes");
        assert_eq!(body.as_bytes(), br#"["alpha","beta"]"#);
        assert_eq!(body.len(), 16);
        assert!(!body.is_empty());
    }

    #[test]
    fn prepared_json_reports_the_declared_schema_not_its_bytes() {
        assert_eq!(
            PreparedJson::<DocumentedPage>::type_descriptor(),
            DocumentedPage::type_descriptor()
        );
    }

    #[test]
    fn prepared_json_carries_adopted_bytes_verbatim() {
        let body =
            PreparedJson::<DocumentedPage>::from_bytes(b"{\"already\":\"encoded\"}".to_vec());
        assert_eq!(body.into_bytes(), b"{\"already\":\"encoded\"}".to_vec());
    }

    #[test]
    fn an_unseen_response_shape_reserves_the_floor() {
        assert_eq!(response_size_hint::<HintProbe>(), MIN_RESPONSE_HINT);
    }

    #[test]
    fn a_recorded_response_shape_reserves_headroom() {
        record_response_size::<(u8, HintProbe)>(8192);
        let hint = response_size_hint::<(u8, HintProbe)>();
        assert!(hint > 8192, "the hint should leave room to grow: {hint}");
        assert!(hint <= MAX_RESPONSE_HINT);
    }

    #[test]
    fn an_outsized_response_cannot_pin_the_hint_above_the_ceiling() {
        record_response_size::<(u16, HintProbe)>(usize::MAX);
        assert_eq!(response_size_hint::<(u16, HintProbe)>(), MAX_RESPONSE_HINT);
    }

    #[test]
    fn a_parked_upload_leaves_only_a_token_in_the_document() {
        let slots = UploadSlots::acquire();
        let token = slots.park(
            UploadFile::new("cover", vec![7; 1 << 20])
                .with_file_name("cover.png")
                .with_content_type("image/png"),
        );

        let encoded = blazingly_json::to_string(&token).expect("the token encodes");
        assert!(
            encoded.len() < 64,
            "a megabyte of upload left {} bytes in the document: {encoded}",
            encoded.len()
        );

        let decoded: UploadFile = blazingly_json::from_value(token).expect("the token resolves");
        assert_eq!(decoded.field_name, "cover");
        assert_eq!(decoded.file_name.as_deref(), Some("cover.png"));
        assert_eq!(decoded.content_type.as_deref(), Some("image/png"));
        assert_eq!(decoded.bytes.len(), 1 << 20);
        assert!(decoded.bytes.iter().all(|byte| *byte == 7));
    }

    #[test]
    fn an_upload_slot_does_not_outlive_its_extraction() {
        let token = {
            let slots = UploadSlots::acquire();
            slots.park(UploadFile::new("gone", vec![1, 2, 3]))
        };
        let error = blazingly_json::from_value::<UploadFile>(token)
            .expect_err("a released slot cannot be resolved");
        assert!(error.to_string().contains("no longer available"), "{error}");
    }

    #[test]
    fn an_upload_slot_can_only_be_taken_once() {
        let slots = UploadSlots::acquire();
        let token = slots.park(UploadFile::new("once", vec![9]));
        let first: UploadFile =
            blazingly_json::from_value(token.clone()).expect("the first take resolves");
        assert_eq!(first.bytes, vec![9]);
        assert!(blazingly_json::from_value::<UploadFile>(token).is_err());
    }

    #[test]
    fn two_extractions_on_one_thread_cannot_see_each_others_slots() {
        let outer = UploadSlots::acquire();
        let outer_token = outer.park(UploadFile::new("outer", vec![1]));
        let inner_token = {
            let inner = UploadSlots::acquire();
            inner.park(UploadFile::new("inner", vec![2]))
        };

        assert!(blazingly_json::from_value::<UploadFile>(inner_token).is_err());
        let resolved: UploadFile =
            blazingly_json::from_value(outer_token).expect("the outer slot survives");
        assert_eq!(resolved.field_name, "outer");
    }

    #[test]
    fn the_object_form_an_mcp_client_sends_still_decodes() {
        let value = blazingly_json::json!({
            "field_name": "avatar",
            "file_name": "a.png",
            "content_type": "image/png",
            "bytes": [1, 2, 3]
        });
        let upload: UploadFile =
            blazingly_json::from_value(value).expect("the object form decodes");
        assert_eq!(
            upload,
            UploadFile::new("avatar", vec![1, 2, 3])
                .with_file_name("a.png")
                .with_content_type("image/png")
        );
    }

    #[test]
    fn an_upload_survives_its_own_serialized_form() {
        let upload = UploadFile::new("report", vec![4, 5, 6]).with_file_name("r.bin");
        let encoded = blazingly_json::to_value(&upload).expect("the upload encodes");
        let decoded: UploadFile = blazingly_json::from_value(encoded).expect("the upload decodes");
        assert_eq!(decoded, upload);
    }

    #[test]
    fn the_object_form_defaults_metadata_and_still_demands_the_rest() {
        let minimal: UploadFile =
            blazingly_json::from_value(blazingly_json::json!({"field_name": "a", "bytes": []}))
                .expect("optional metadata may be absent");
        assert!(minimal.file_name.is_none());
        assert!(minimal.content_type.is_none());

        let error =
            blazingly_json::from_value::<UploadFile>(blazingly_json::json!({"field_name": "a"}))
                .expect_err("bytes are required");
        assert!(error.to_string().contains("bytes"), "{error}");
    }

    fn violations(field: &str, nested: &ValidationErrors) -> Vec<String> {
        let mut merged = ValidationErrors::new();
        merge_field_validation_errors(&mut merged, field, nested);
        merged
            .violations()
            .iter()
            .map(|violation| violation.field.clone())
            .collect()
    }

    #[test]
    fn a_field_validator_that_names_its_own_field_is_not_doubled() {
        let mut reported = ValidationErrors::new();
        reported.push("published_at", "too_far_ahead", "too far ahead");
        assert_eq!(violations("published_at", &reported), ["published_at"]);
    }

    #[test]
    fn a_field_validator_may_still_report_a_path_inside_the_value() {
        let mut reported = ValidationErrors::new();
        reported.push("", "invalid", "invalid");
        reported.push("window.end", "invalid", "invalid");
        reported.push("slots[0]", "invalid", "invalid");
        reported.push("end", "invalid", "invalid");
        assert_eq!(
            violations("window", &reported),
            ["window", "window.end", "window.slots[0]", "window.end"]
        );
    }

    #[test]
    fn an_unnamed_field_leaves_the_reported_path_alone() {
        let mut reported = ValidationErrors::new();
        reported.push("street", "min_length", "too short");
        assert_eq!(violations("", &reported), ["street"]);
    }

    #[test]
    fn field_metadata_round_trips_through_its_encoding() {
        for metadata in [
            FieldMetadata::Default(blazingly_json::json!(20)),
            FieldMetadata::Default(blazingly_json::json!("draft")),
            FieldMetadata::Default(blazingly_json::json!(true)),
            FieldMetadata::Nullable,
            FieldMetadata::Enumeration(vec!["uk".to_owned(), "ru".to_owned()]),
        ] {
            let encoded = metadata.to_string();
            assert_eq!(
                FieldMetadata::parse(&encoded),
                Some(metadata.clone()),
                "{encoded} did not round trip"
            );
        }

        assert_eq!(FieldMetadata::parse("min_items=2"), None);
        assert_eq!(FieldMetadata::parse("validate_code"), None);
        assert_eq!(FieldMetadata::parse("nullable=false"), None);
    }

    #[test]
    fn field_metadata_projects_json_schema_keywords() {
        let mut schema = blazingly_json::json!({ "type": "integer" });
        FieldMetadata::Default(blazingly_json::json!(20)).apply_json_schema(&mut schema);
        FieldMetadata::Nullable.apply_json_schema(&mut schema);
        FieldMetadata::Enumeration(vec!["uk".to_owned()]).apply_json_schema(&mut schema);

        assert_eq!(schema["default"], blazingly_json::json!(20));
        assert_eq!(schema["nullable"], blazingly_json::json!(true));
        assert_eq!(schema["enum"], blazingly_json::json!(["uk"]));
    }

    fn operation(id: &str, method: HttpMethod, path: &str) -> OperationDescriptor {
        OperationDescriptor::new(
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
        .expect("test operation id should be valid")
    }

    #[test]
    fn app_rejects_duplicate_operation_ids() {
        let result = App::new()
            .route(operation("users.read", HttpMethod::Get, "/users/1"))
            .route(operation("users.read", HttpMethod::Get, "/users/2"))
            .build();

        assert!(matches!(result, Err(BuildError::DuplicateOperationId(_))));
    }

    #[test]
    fn app_rejects_duplicate_http_bindings() {
        let result = App::new()
            .route(operation("users.read", HttpMethod::Get, "/users"))
            .route(operation("users.list", HttpMethod::Get, "/users"))
            .build();

        assert!(matches!(
            result,
            Err(BuildError::DuplicateHttpBinding { .. })
        ));
    }

    #[test]
    fn app_rejects_parameter_routes_with_the_same_shape() {
        let result = App::new()
            .route(
                operation("users.by_id", HttpMethod::Get, "/users/{user_id}").with_inputs(vec![
                    InputDescriptor::new(
                        "user_id",
                        InputSource::Path,
                        true,
                        TypeDescriptor::new("u64"),
                    ),
                ]),
            )
            .route(
                operation("users.by_name", HttpMethod::Get, "/users/{user_name}").with_inputs(
                    vec![InputDescriptor::new(
                        "user_name",
                        InputSource::Path,
                        true,
                        TypeDescriptor::new("String"),
                    )],
                ),
            )
            .build();

        assert!(matches!(
            result,
            Err(BuildError::AmbiguousHttpBinding { .. })
        ));
    }

    #[test]
    fn app_order_is_deterministic() {
        let app = App::new()
            .route(operation("users.create", HttpMethod::Post, "/users"))
            .route(operation("health.read", HttpMethod::Get, "/health"))
            .build()
            .expect("application should be valid");

        let ids: Vec<_> = app
            .operations()
            .iter()
            .map(|operation| operation.contract.id.as_str())
            .collect();
        assert_eq!(ids, ["health.read", "users.create"]);
    }

    #[test]
    fn app_validates_and_orders_operation_security() {
        let secured = operation("users.write", HttpMethod::Put, "/users").with_security(vec![
            SecurityRequirement::new("oauth").with_scopes(vec!["users:write".to_owned()]),
        ]);
        let app = App::new()
            .route(secured)
            .security_scheme(SecuritySchemeDescriptor::new(
                "oauth",
                SecuritySchemeKind::OAuth2 {
                    authorization_url: Some("https://auth.example/authorize".to_owned()),
                    token_url: Some("https://auth.example/token".to_owned()),
                    scopes: vec!["users:read".to_owned(), "users:write".to_owned()],
                },
            ))
            .build()
            .expect("registered security requirements should compile");

        assert_eq!(app.security_schemes()[0].name, "oauth");
        assert_eq!(
            app.operations()[0].contract.security[0].scopes,
            ["users:write"]
        );
    }

    #[test]
    fn app_rejects_unknown_security_schemes_and_scopes() {
        let unknown_scheme = App::new()
            .route(
                operation("users.read", HttpMethod::Get, "/users")
                    .with_security(vec![SecurityRequirement::new("missing")]),
            )
            .build();
        assert!(matches!(
            unknown_scheme,
            Err(BuildError::UnknownSecurityScheme { .. })
        ));

        let unknown_scope = App::new()
            .route(
                operation("users.read", HttpMethod::Get, "/users").with_security(vec![
                    SecurityRequirement::new("oauth").with_scopes(vec!["users:write".to_owned()]),
                ]),
            )
            .security_scheme(SecuritySchemeDescriptor::new(
                "oauth",
                SecuritySchemeKind::OAuth2 {
                    authorization_url: None,
                    token_url: Some("https://auth.example/token".to_owned()),
                    scopes: vec!["users:read".to_owned()],
                },
            ))
            .build();
        assert!(matches!(
            unknown_scope,
            Err(BuildError::UnknownSecurityScope { .. })
        ));
    }
}
