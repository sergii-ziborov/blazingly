#![forbid(unsafe_code)]
// The README example carries a `fn main` on purpose: it is copied out of a
// crates.io page into a real file, where the wrapper is not needless.
#![allow(clippy::needless_doctest_main)]
#![doc = include_str!("../README.md")]

use blazingly_contract::OperationFailure;
use core::any::{Any, TypeId};
use core::fmt;
use core::future::Future;
use core::ops::Deref;
use core::pin::Pin;
use std::rc::Rc;

/// A resolved typed dependency passed to a handler or provider.
#[derive(Clone, Debug)]
pub struct Depends<T> {
    value: Rc<T>,
}

impl<T> Depends<T> {
    #[doc(hidden)]
    #[must_use]
    pub const fn from_rc(value: Rc<T>) -> Self {
        Self { value }
    }

    #[must_use]
    pub fn into_inner(self) -> Rc<T> {
        self.value
    }
}

impl<T> Deref for Depends<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

/// The lifetime of a dependency provider.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DependencyLifetime {
    /// Created once while the executable application is compiled.
    Singleton,
    /// Created once for each operation invocation that needs it.
    Request,
    /// Created independently for every injection edge.
    Transient,
}

/// Runtime identity of a typed dependency.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct DependencyKey {
    type_id: TypeId,
    type_name: &'static str,
}

impl DependencyKey {
    #[must_use]
    pub fn of<T: 'static>() -> Self {
        Self {
            type_id: TypeId::of::<T>(),
            type_name: core::any::type_name::<T>(),
        }
    }

    #[must_use]
    pub const fn type_id(self) -> TypeId {
        self.type_id
    }

    #[must_use]
    pub const fn type_name(self) -> &'static str {
        self.type_name
    }
}

impl fmt::Debug for DependencyKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("DependencyKey")
            .field(&self.type_name)
            .finish()
    }
}

/// A dependency required by one operation handler.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DependencyRequest {
    key: DependencyKey,
}

impl DependencyRequest {
    #[must_use]
    pub fn of<T: 'static>() -> Self {
        Self {
            key: DependencyKey::of::<T>(),
        }
    }

    #[must_use]
    pub const fn key(self) -> DependencyKey {
        self.key
    }
}

/// A stable request rejection or an internal dependency failure.
#[derive(Clone, Debug, PartialEq)]
pub enum DependencyError {
    Rejected(OperationFailure),
    Internal { code: &'static str, message: String },
}

impl DependencyError {
    #[must_use]
    pub const fn rejected(failure: OperationFailure) -> Self {
        Self::Rejected(failure)
    }

    #[must_use]
    pub fn internal(code: &'static str, message: impl Into<String>) -> Self {
        Self::Internal {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for DependencyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected(failure) => write!(formatter, "{}", failure.message),
            Self::Internal { message, .. } => formatter.write_str(message),
        }
    }
}

impl std::error::Error for DependencyError {}

#[doc(hidden)]
pub type DependencyValue = Rc<dyn Any>;

/// A numeric source selected by the dependency compiler.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DependencySlot {
    Singleton(usize),
    Request(usize),
}

/// A provider with all of its dependency slots compiled.
#[doc(hidden)]
#[derive(Clone)]
pub struct CompiledProvider {
    runner: ProviderRunner,
    finalizer: Option<ProviderFinalizer>,
}

type SyncProviderRunner = Rc<
    dyn Fn(
        &[Option<DependencyValue>],
        &[Option<DependencyValue>],
    ) -> Result<DependencyValue, DependencyError>,
>;
type ProviderFuture =
    Pin<Box<dyn Future<Output = Result<DependencyValue, DependencyError>> + 'static>>;
type AsyncProviderRunner =
    Rc<dyn Fn(&[Option<DependencyValue>], &[Option<DependencyValue>]) -> ProviderFuture>;

#[derive(Clone)]
enum ProviderRunner {
    Sync(SyncProviderRunner),
    Async(AsyncProviderRunner),
}

/// How the request a scoped dependency was built for ended.
///
/// Request-scoped teardown runs after the handler, and until it could see
/// this it had no way to tell a committed request from a failed one — so the
/// canonical use, a transaction that commits on success and rolls back
/// otherwise, could not be written. This is the argument a `try`/`except`
/// around a `yield` gets in `FastAPI`.
///
/// Deliberately small. It carries what teardown reacts to, not the response:
/// this crate sits below the executor and must not learn its types.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestOutcome<'failure> {
    /// The handler returned a success.
    Succeeded,
    /// The request was answered with an error — a typed `#[api_error]`, a
    /// rejected input, an aborted invocation, or a failing dependency.
    Failed {
        /// The HTTP status the request was answered with.
        status: u16,
        /// The stable error code, the same one in the response body.
        code: &'failure str,
    },
}

impl RequestOutcome<'_> {
    #[must_use]
    pub const fn succeeded(&self) -> bool {
        matches!(self, Self::Succeeded)
    }

    /// The status a failing request was answered with, if it failed.
    #[must_use]
    pub const fn status(&self) -> Option<u16> {
        match self {
            Self::Succeeded => None,
            Self::Failed { status, .. } => Some(*status),
        }
    }

    /// The stable error code a failing request was answered with.
    #[must_use]
    pub const fn code(&self) -> Option<&str> {
        match self {
            Self::Succeeded => None,
            Self::Failed { code, .. } => Some(*code),
        }
    }
}

type SyncProviderFinalizer =
    Rc<dyn Fn(&DependencyValue, RequestOutcome<'_>) -> Result<(), DependencyError>>;
type FinalizerFuture = Pin<Box<dyn Future<Output = Result<(), DependencyError>> + 'static>>;
type AsyncProviderFinalizer = Rc<dyn Fn(&DependencyValue, RequestOutcome<'_>) -> FinalizerFuture>;

#[derive(Clone)]
enum ProviderFinalizer {
    Sync(SyncProviderFinalizer),
    Async(AsyncProviderFinalizer),
}

impl CompiledProvider {
    #[doc(hidden)]
    #[must_use]
    pub fn run(
        &self,
        singletons: &[Option<DependencyValue>],
        requests: &[Option<DependencyValue>],
    ) -> ProviderFuture {
        match &self.runner {
            ProviderRunner::Sync(runner) => {
                let result = runner(singletons, requests);
                Box::pin(async move { result })
            }
            ProviderRunner::Async(runner) => runner(singletons, requests),
        }
    }

    #[doc(hidden)]
    pub fn run_sync(
        &self,
        singletons: &[Option<DependencyValue>],
        requests: &[Option<DependencyValue>],
    ) -> Result<DependencyValue, DependencyError> {
        match &self.runner {
            ProviderRunner::Sync(runner) => runner(singletons, requests),
            ProviderRunner::Async(_) => Err(DependencyError::internal(
                "async_singleton_provider",
                "async providers cannot use the singleton lifetime",
            )),
        }
    }

    #[doc(hidden)]
    pub fn finalize(
        &self,
        value: &DependencyValue,
        outcome: RequestOutcome<'_>,
    ) -> FinalizerFuture {
        match &self.finalizer {
            None => Box::pin(async { Ok(()) }),
            Some(ProviderFinalizer::Sync(finalizer)) => {
                let result = finalizer(value, outcome);
                Box::pin(async move { result })
            }
            Some(ProviderFinalizer::Async(finalizer)) => finalizer(value, outcome),
        }
    }
}

/// Internal compilation error. A valid framework-generated plan never exposes
/// this to request handling.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderCompileError {
    expected: usize,
    actual: usize,
}

impl ProviderCompileError {
    const fn arity(expected: usize, actual: usize) -> Self {
        Self { expected, actual }
    }
}

impl fmt::Display for ProviderCompileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "provider expected {} dependency slots but received {}",
            self.expected, self.actual
        )
    }
}

impl std::error::Error for ProviderCompileError {}

type ProviderCompiler =
    Rc<dyn Fn(&[DependencySlot]) -> Result<CompiledProvider, ProviderCompileError>>;

/// A typed dependency provider registered in a plugin scope.
#[derive(Clone)]
pub struct Provider {
    key: DependencyKey,
    lifetime: DependencyLifetime,
    dependencies: Vec<DependencyKey>,
    compiler: ProviderCompiler,
    finalizer: Option<ProviderFinalizer>,
}

impl Provider {
    /// Registers an already-created singleton value.
    #[must_use]
    pub fn value<T: 'static>(value: T) -> Self {
        let value: DependencyValue = Rc::new(value);
        let compiler = Rc::new(move |slots: &[DependencySlot]| {
            if !slots.is_empty() {
                return Err(ProviderCompileError::arity(0, slots.len()));
            }
            let value = Rc::clone(&value);
            Ok(CompiledProvider {
                runner: ProviderRunner::Sync(Rc::new(move |_, _| Ok(Rc::clone(&value)))),
                finalizer: None,
            })
        });
        Self {
            key: DependencyKey::of::<T>(),
            lifetime: DependencyLifetime::Singleton,
            dependencies: Vec::new(),
            compiler,
            finalizer: None,
        }
    }

    /// Registers an infallible singleton factory.
    #[must_use]
    pub fn singleton<Arguments, Output, Factory>(factory: Factory) -> Self
    where
        Factory: ProviderFactory<Arguments, Output>,
        Output: 'static,
    {
        Self::from_factory(DependencyLifetime::Singleton, factory)
    }

    /// Registers an infallible request-scoped factory.
    #[must_use]
    pub fn request<Arguments, Output, Factory>(factory: Factory) -> Self
    where
        Factory: ProviderFactory<Arguments, Output>,
        Output: 'static,
    {
        Self::from_factory(DependencyLifetime::Request, factory)
    }

    /// Registers an infallible transient factory.
    #[must_use]
    pub fn transient<Arguments, Output, Factory>(factory: Factory) -> Self
    where
        Factory: ProviderFactory<Arguments, Output>,
        Output: 'static,
    {
        Self::from_factory(DependencyLifetime::Transient, factory)
    }

    /// Registers a fallible singleton factory.
    #[must_use]
    pub fn try_singleton<Arguments, Output, Factory>(factory: Factory) -> Self
    where
        Factory: FallibleProviderFactory<Arguments, Output>,
        Output: 'static,
    {
        Self::from_fallible_factory(DependencyLifetime::Singleton, factory)
    }

    /// Registers a fallible request-scoped factory.
    #[must_use]
    pub fn try_request<Arguments, Output, Factory>(factory: Factory) -> Self
    where
        Factory: FallibleProviderFactory<Arguments, Output>,
        Output: 'static,
    {
        Self::from_fallible_factory(DependencyLifetime::Request, factory)
    }

    /// Registers a fallible transient factory.
    #[must_use]
    pub fn try_transient<Arguments, Output, Factory>(factory: Factory) -> Self
    where
        Factory: FallibleProviderFactory<Arguments, Output>,
        Output: 'static,
    {
        Self::from_fallible_factory(DependencyLifetime::Transient, factory)
    }

    /// Registers a request provider with a reverse-order finalizer.
    #[must_use]
    pub fn request_scoped<Arguments, Output, Factory, Finalizer>(
        factory: Factory,
        finalizer: Finalizer,
    ) -> Self
    where
        Factory: ProviderFactory<Arguments, Output>,
        Finalizer: Fn(Depends<Output>, RequestOutcome<'_>) + 'static,
        Output: 'static,
    {
        let mut provider = Self::from_factory(DependencyLifetime::Request, factory);
        provider.finalizer = Some(typed_finalizer(finalizer));
        provider
    }

    /// Registers a fallible request provider with a reverse-order finalizer.
    #[must_use]
    pub fn try_request_scoped<Arguments, Output, Factory, Finalizer>(
        factory: Factory,
        finalizer: Finalizer,
    ) -> Self
    where
        Factory: FallibleProviderFactory<Arguments, Output>,
        Finalizer: Fn(Depends<Output>, RequestOutcome<'_>) + 'static,
        Output: 'static,
    {
        let mut provider = Self::from_fallible_factory(DependencyLifetime::Request, factory);
        provider.finalizer = Some(typed_finalizer(finalizer));
        provider
    }

    /// Registers an async request-scoped factory.
    #[must_use]
    pub fn request_async<Arguments, Output, Factory, FactoryFuture>(factory: Factory) -> Self
    where
        Factory: AsyncProviderFactory<Arguments, Output, FactoryFuture>,
        FactoryFuture: Future<Output = Output> + 'static,
        Output: 'static,
    {
        Self::from_async_factory(DependencyLifetime::Request, factory)
    }

    /// Registers an async transient factory.
    #[must_use]
    pub fn transient_async<Arguments, Output, Factory, FactoryFuture>(factory: Factory) -> Self
    where
        Factory: AsyncProviderFactory<Arguments, Output, FactoryFuture>,
        FactoryFuture: Future<Output = Output> + 'static,
        Output: 'static,
    {
        Self::from_async_factory(DependencyLifetime::Transient, factory)
    }

    /// Registers a fallible async request-scoped factory.
    #[must_use]
    pub fn try_request_async<Arguments, Output, Factory, FactoryFuture>(factory: Factory) -> Self
    where
        Factory: FallibleAsyncProviderFactory<Arguments, Output, FactoryFuture>,
        FactoryFuture: Future<Output = Result<Output, DependencyError>> + 'static,
        Output: 'static,
    {
        Self::from_fallible_async_factory(DependencyLifetime::Request, factory)
    }

    /// Registers a fallible async transient factory.
    #[must_use]
    pub fn try_transient_async<Arguments, Output, Factory, FactoryFuture>(factory: Factory) -> Self
    where
        Factory: FallibleAsyncProviderFactory<Arguments, Output, FactoryFuture>,
        FactoryFuture: Future<Output = Result<Output, DependencyError>> + 'static,
        Output: 'static,
    {
        Self::from_fallible_async_factory(DependencyLifetime::Transient, factory)
    }

    /// Registers an async request provider with an async finalizer.
    #[must_use]
    pub fn request_async_scoped<
        Arguments,
        Output,
        Factory,
        FactoryFuture,
        Finalizer,
        FinalizerFuture,
    >(
        factory: Factory,
        finalizer: Finalizer,
    ) -> Self
    where
        Factory: AsyncProviderFactory<Arguments, Output, FactoryFuture>,
        FactoryFuture: Future<Output = Output> + 'static,
        Finalizer: Fn(Depends<Output>, RequestOutcome<'_>) -> FinalizerFuture + 'static,
        FinalizerFuture: Future<Output = ()> + 'static,
        Output: 'static,
    {
        let mut provider = Self::from_async_factory(DependencyLifetime::Request, factory);
        provider.finalizer = Some(typed_async_finalizer(finalizer));
        provider
    }

    /// Registers a fallible async request provider with an async finalizer.
    #[must_use]
    pub fn try_request_async_scoped<
        Arguments,
        Output,
        Factory,
        FactoryFuture,
        Finalizer,
        FinalizerFuture,
    >(
        factory: Factory,
        finalizer: Finalizer,
    ) -> Self
    where
        Factory: FallibleAsyncProviderFactory<Arguments, Output, FactoryFuture>,
        FactoryFuture: Future<Output = Result<Output, DependencyError>> + 'static,
        Finalizer: Fn(Depends<Output>, RequestOutcome<'_>) -> FinalizerFuture + 'static,
        FinalizerFuture: Future<Output = ()> + 'static,
        Output: 'static,
    {
        let mut provider = Self::from_fallible_async_factory(DependencyLifetime::Request, factory);
        provider.finalizer = Some(typed_async_finalizer(finalizer));
        provider
    }

    fn from_factory<Arguments, Output, Factory>(
        lifetime: DependencyLifetime,
        factory: Factory,
    ) -> Self
    where
        Factory: ProviderFactory<Arguments, Output>,
        Output: 'static,
    {
        let dependencies = Factory::dependency_keys();
        let factory = Rc::new(factory);
        let compiler =
            Rc::new(move |slots: &[DependencySlot]| Factory::compile(Rc::clone(&factory), slots));
        Self {
            key: DependencyKey::of::<Output>(),
            lifetime,
            dependencies,
            compiler,
            finalizer: None,
        }
    }

    fn from_fallible_factory<Arguments, Output, Factory>(
        lifetime: DependencyLifetime,
        factory: Factory,
    ) -> Self
    where
        Factory: FallibleProviderFactory<Arguments, Output>,
        Output: 'static,
    {
        let dependencies = Factory::dependency_keys();
        let factory = Rc::new(factory);
        let compiler =
            Rc::new(move |slots: &[DependencySlot]| Factory::compile(Rc::clone(&factory), slots));
        Self {
            key: DependencyKey::of::<Output>(),
            lifetime,
            dependencies,
            compiler,
            finalizer: None,
        }
    }

    fn from_async_factory<Arguments, Output, Factory, FactoryFuture>(
        lifetime: DependencyLifetime,
        factory: Factory,
    ) -> Self
    where
        Factory: AsyncProviderFactory<Arguments, Output, FactoryFuture>,
        FactoryFuture: Future<Output = Output> + 'static,
        Output: 'static,
    {
        let dependencies = Factory::dependency_keys();
        let factory = Rc::new(factory);
        let compiler =
            Rc::new(move |slots: &[DependencySlot]| Factory::compile(Rc::clone(&factory), slots));
        Self {
            key: DependencyKey::of::<Output>(),
            lifetime,
            dependencies,
            compiler,
            finalizer: None,
        }
    }

    fn from_fallible_async_factory<Arguments, Output, Factory, FactoryFuture>(
        lifetime: DependencyLifetime,
        factory: Factory,
    ) -> Self
    where
        Factory: FallibleAsyncProviderFactory<Arguments, Output, FactoryFuture>,
        FactoryFuture: Future<Output = Result<Output, DependencyError>> + 'static,
        Output: 'static,
    {
        let dependencies = Factory::dependency_keys();
        let factory = Rc::new(factory);
        let compiler =
            Rc::new(move |slots: &[DependencySlot]| Factory::compile(Rc::clone(&factory), slots));
        Self {
            key: DependencyKey::of::<Output>(),
            lifetime,
            dependencies,
            compiler,
            finalizer: None,
        }
    }

    #[must_use]
    pub const fn key(&self) -> DependencyKey {
        self.key
    }

    #[must_use]
    pub const fn lifetime(&self) -> DependencyLifetime {
        self.lifetime
    }

    #[must_use]
    pub fn dependencies(&self) -> &[DependencyKey] {
        &self.dependencies
    }

    #[doc(hidden)]
    pub fn compile(
        &self,
        slots: &[DependencySlot],
    ) -> Result<CompiledProvider, ProviderCompileError> {
        let mut compiled = (self.compiler)(slots)?;
        compiled.finalizer.clone_from(&self.finalizer);
        Ok(compiled)
    }
}

/// Converts an infallible typed closure into a provider factory.
#[doc(hidden)]
pub trait ProviderFactory<Arguments, Output>: 'static {
    fn dependency_keys() -> Vec<DependencyKey>;

    fn compile(
        factory: Rc<Self>,
        slots: &[DependencySlot],
    ) -> Result<CompiledProvider, ProviderCompileError>;
}

/// Converts a fallible typed closure into a provider factory.
#[doc(hidden)]
pub trait FallibleProviderFactory<Arguments, Output>: 'static {
    fn dependency_keys() -> Vec<DependencyKey>;

    fn compile(
        factory: Rc<Self>,
        slots: &[DependencySlot],
    ) -> Result<CompiledProvider, ProviderCompileError>;
}

/// Converts an async typed closure into a provider factory.
#[doc(hidden)]
pub trait AsyncProviderFactory<Arguments, Output, FactoryFuture>: 'static
where
    FactoryFuture: Future<Output = Output> + 'static,
{
    fn dependency_keys() -> Vec<DependencyKey>;

    fn compile(
        factory: Rc<Self>,
        slots: &[DependencySlot],
    ) -> Result<CompiledProvider, ProviderCompileError>;
}

/// Converts a fallible async typed closure into a provider factory.
#[doc(hidden)]
pub trait FallibleAsyncProviderFactory<Arguments, Output, FactoryFuture>: 'static
where
    FactoryFuture: Future<Output = Result<Output, DependencyError>> + 'static,
{
    fn dependency_keys() -> Vec<DependencyKey>;

    fn compile(
        factory: Rc<Self>,
        slots: &[DependencySlot],
    ) -> Result<CompiledProvider, ProviderCompileError>;
}

impl<Factory, Output> ProviderFactory<(), Output> for Factory
where
    Factory: Fn() -> Output + 'static,
    Output: 'static,
{
    fn dependency_keys() -> Vec<DependencyKey> {
        Vec::new()
    }

    fn compile(
        factory: Rc<Self>,
        slots: &[DependencySlot],
    ) -> Result<CompiledProvider, ProviderCompileError> {
        expect_arity(slots, 0)?;
        Ok(CompiledProvider {
            runner: ProviderRunner::Sync(Rc::new(move |_, _| {
                Ok(Rc::new(factory()) as DependencyValue)
            })),
            finalizer: None,
        })
    }
}

impl<Factory, Output, FactoryFuture> AsyncProviderFactory<(), Output, FactoryFuture> for Factory
where
    Factory: Fn() -> FactoryFuture + 'static,
    FactoryFuture: Future<Output = Output> + 'static,
    Output: 'static,
{
    fn dependency_keys() -> Vec<DependencyKey> {
        Vec::new()
    }

    fn compile(
        factory: Rc<Self>,
        slots: &[DependencySlot],
    ) -> Result<CompiledProvider, ProviderCompileError> {
        expect_arity(slots, 0)?;
        Ok(CompiledProvider {
            runner: ProviderRunner::Async(Rc::new(move |_, _| {
                let future = factory();
                Box::pin(async move { Ok(Rc::new(future.await) as DependencyValue) })
            })),
            finalizer: None,
        })
    }
}

impl<Factory, Output, FactoryFuture> FallibleAsyncProviderFactory<(), Output, FactoryFuture>
    for Factory
where
    Factory: Fn() -> FactoryFuture + 'static,
    FactoryFuture: Future<Output = Result<Output, DependencyError>> + 'static,
    Output: 'static,
{
    fn dependency_keys() -> Vec<DependencyKey> {
        Vec::new()
    }

    fn compile(
        factory: Rc<Self>,
        slots: &[DependencySlot],
    ) -> Result<CompiledProvider, ProviderCompileError> {
        expect_arity(slots, 0)?;
        Ok(CompiledProvider {
            runner: ProviderRunner::Async(Rc::new(move |_, _| {
                let future = factory();
                Box::pin(async move { future.await.map(|value| Rc::new(value) as DependencyValue) })
            })),
            finalizer: None,
        })
    }
}

impl<Factory, Output> FallibleProviderFactory<(), Output> for Factory
where
    Factory: Fn() -> Result<Output, DependencyError> + 'static,
    Output: 'static,
{
    fn dependency_keys() -> Vec<DependencyKey> {
        Vec::new()
    }

    fn compile(
        factory: Rc<Self>,
        slots: &[DependencySlot],
    ) -> Result<CompiledProvider, ProviderCompileError> {
        expect_arity(slots, 0)?;
        Ok(CompiledProvider {
            runner: ProviderRunner::Sync(Rc::new(move |_, _| {
                factory().map(|value| Rc::new(value) as DependencyValue)
            })),
            finalizer: None,
        })
    }
}

macro_rules! provider_factory {
    ($(($argument:ident, $value:ident, $index:tt)),+ $(,)?) => {
        impl<Factory, Output, $($argument),+> ProviderFactory<($($argument,)+), Output> for Factory
        where
            Factory: Fn($(Depends<$argument>),+) -> Output + 'static,
            Output: 'static,
            $($argument: 'static),+
        {
            fn dependency_keys() -> Vec<DependencyKey> {
                vec![$(DependencyKey::of::<$argument>()),+]
            }

            fn compile(
                factory: Rc<Self>,
                slots: &[DependencySlot],
            ) -> Result<CompiledProvider, ProviderCompileError> {
                expect_arity(slots, provider_factory!(@count $($argument),+))?;
                $(let $value = slots[$index];)+
                Ok(CompiledProvider {
                    runner: ProviderRunner::Sync(Rc::new(move |singletons, requests| {
                        $(
                            let $value =
                                resolve::<$argument>($value, singletons, requests)?;
                        )+
                        Ok(Rc::new(factory($($value),+)) as DependencyValue)
                    })),
                    finalizer: None,
                })
            }
        }

        impl<Factory, Output, $($argument),+>
            FallibleProviderFactory<($($argument,)+), Output> for Factory
        where
            Factory: Fn($(Depends<$argument>),+) -> Result<Output, DependencyError> + 'static,
            Output: 'static,
            $($argument: 'static),+
        {
            fn dependency_keys() -> Vec<DependencyKey> {
                vec![$(DependencyKey::of::<$argument>()),+]
            }

            fn compile(
                factory: Rc<Self>,
                slots: &[DependencySlot],
            ) -> Result<CompiledProvider, ProviderCompileError> {
                expect_arity(slots, provider_factory!(@count $($argument),+))?;
                $(let $value = slots[$index];)+
                Ok(CompiledProvider {
                    runner: ProviderRunner::Sync(Rc::new(move |singletons, requests| {
                        $(
                            let $value =
                                resolve::<$argument>($value, singletons, requests)?;
                        )+
                        factory($($value),+)
                            .map(|value| Rc::new(value) as DependencyValue)
                    })),
                    finalizer: None,
                })
            }
        }

        impl<Factory, Output, FactoryFuture, $($argument),+>
            AsyncProviderFactory<($($argument,)+), Output, FactoryFuture> for Factory
        where
            Factory: Fn($(Depends<$argument>),+) -> FactoryFuture + 'static,
            FactoryFuture: Future<Output = Output> + 'static,
            Output: 'static,
            $($argument: 'static),+
        {
            fn dependency_keys() -> Vec<DependencyKey> {
                vec![$(DependencyKey::of::<$argument>()),+]
            }

            fn compile(
                factory: Rc<Self>,
                slots: &[DependencySlot],
            ) -> Result<CompiledProvider, ProviderCompileError> {
                expect_arity(slots, provider_factory!(@count $($argument),+))?;
                $(let $value = slots[$index];)+
                Ok(CompiledProvider {
                    runner: ProviderRunner::Async(Rc::new(move |singletons, requests| {
                        $(
                            let $value = match
                                resolve::<$argument>($value, singletons, requests)
                            {
                                Ok(value) => value,
                                Err(error) => return failed_provider_future(error),
                            };
                        )+
                        let future = factory($($value),+);
                        Box::pin(async move {
                            Ok(Rc::new(future.await) as DependencyValue)
                        })
                    })),
                    finalizer: None,
                })
            }
        }

        impl<Factory, Output, FactoryFuture, $($argument),+>
            FallibleAsyncProviderFactory<($($argument,)+), Output, FactoryFuture> for Factory
        where
            Factory: Fn($(Depends<$argument>),+) -> FactoryFuture + 'static,
            FactoryFuture: Future<Output = Result<Output, DependencyError>> + 'static,
            Output: 'static,
            $($argument: 'static),+
        {
            fn dependency_keys() -> Vec<DependencyKey> {
                vec![$(DependencyKey::of::<$argument>()),+]
            }

            fn compile(
                factory: Rc<Self>,
                slots: &[DependencySlot],
            ) -> Result<CompiledProvider, ProviderCompileError> {
                expect_arity(slots, provider_factory!(@count $($argument),+))?;
                $(let $value = slots[$index];)+
                Ok(CompiledProvider {
                    runner: ProviderRunner::Async(Rc::new(move |singletons, requests| {
                        $(
                            let $value = match
                                resolve::<$argument>($value, singletons, requests)
                            {
                                Ok(value) => value,
                                Err(error) => return failed_provider_future(error),
                            };
                        )+
                        let future = factory($($value),+);
                        Box::pin(async move {
                            future
                                .await
                                .map(|value| Rc::new(value) as DependencyValue)
                        })
                    })),
                    finalizer: None,
                })
            }
        }
    };
    (@count $($argument:ident),+) => {
        <[()]>::len(&[$(provider_factory!(@unit $argument)),+])
    };
    (@unit $argument:ident) => { () };
}

provider_factory!((A, a, 0));
provider_factory!((A, a, 0), (B, b, 1));
provider_factory!((A, a, 0), (B, b, 1), (C, c, 2));
provider_factory!((A, a, 0), (B, b, 1), (C, c, 2), (D, d, 3));
provider_factory!((A, a, 0), (B, b, 1), (C, c, 2), (D, d, 3), (E, e, 4));
provider_factory!(
    (A, a, 0),
    (B, b, 1),
    (C, c, 2),
    (D, d, 3),
    (E, e, 4),
    (F, f, 5)
);
provider_factory!(
    (A, a, 0),
    (B, b, 1),
    (C, c, 2),
    (D, d, 3),
    (E, e, 4),
    (F, f, 5),
    (G, g, 6)
);
provider_factory!(
    (A, a, 0),
    (B, b, 1),
    (C, c, 2),
    (D, d, 3),
    (E, e, 4),
    (F, f, 5),
    (G, g, 6),
    (H, h, 7)
);

fn typed_finalizer<T, Finalizer>(finalizer: Finalizer) -> ProviderFinalizer
where
    Finalizer: Fn(Depends<T>, RequestOutcome<'_>) + 'static,
    T: 'static,
{
    ProviderFinalizer::Sync(Rc::new(move |value, outcome| {
        let dependency = Rc::clone(value).downcast::<T>().map_err(|_| {
            DependencyError::internal(
                "dependency_type_mismatch",
                "compiled finalizer received an unexpected dependency type",
            )
        })?;
        finalizer(Depends::from_rc(dependency), outcome);
        Ok(())
    }))
}

fn typed_async_finalizer<T, Finalizer, FinalizerOutput>(finalizer: Finalizer) -> ProviderFinalizer
where
    Finalizer: Fn(Depends<T>, RequestOutcome<'_>) -> FinalizerOutput + 'static,
    FinalizerOutput: Future<Output = ()> + 'static,
    T: 'static,
{
    ProviderFinalizer::Async(Rc::new(move |value, outcome| {
        let Ok(dependency) = Rc::clone(value).downcast::<T>() else {
            return Box::pin(async {
                Err(DependencyError::internal(
                    "dependency_type_mismatch",
                    "compiled async finalizer received an unexpected dependency type",
                ))
            });
        };
        let future = finalizer(Depends::from_rc(dependency), outcome);
        Box::pin(async move {
            future.await;
            Ok(())
        })
    }))
}

fn failed_provider_future(error: DependencyError) -> ProviderFuture {
    Box::pin(async move { Err(error) })
}

fn expect_arity(slots: &[DependencySlot], expected: usize) -> Result<(), ProviderCompileError> {
    if slots.len() == expected {
        Ok(())
    } else {
        Err(ProviderCompileError::arity(expected, slots.len()))
    }
}

/// Typed access to one provider invocation's compiled slots.
///
/// The macro-generated body of a request-aware provider reads its `Depends`
/// arguments and its decoded request inputs through this reader, so the
/// container stays a plain slice of erased values and never learns what an
/// extractor is. Positions mirror the declaration: dependencies first, in
/// argument order, then request inputs.
#[doc(hidden)]
pub struct SlotReader<'values> {
    singletons: &'values [Option<DependencyValue>],
    requests: &'values [Option<DependencyValue>],
    slots: &'values [DependencySlot],
    depends_count: usize,
}

impl SlotReader<'_> {
    /// The `position`-th declared `Depends<T>` argument.
    ///
    /// # Errors
    ///
    /// Returns an internal dependency error when the compiled plan and the
    /// generated code disagree, which a framework-generated pair never does.
    pub fn depends<T: 'static>(&self, position: usize) -> Result<Depends<T>, DependencyError> {
        let slot = self.slots.get(position).copied().ok_or_else(|| {
            DependencyError::internal(
                "invalid_dependency_slot",
                "compiled provider requested an undeclared dependency position",
            )
        })?;
        resolve(slot, self.singletons, self.requests)
    }

    /// The `position`-th declared request input, decoded before the provider
    /// chain ran.
    ///
    /// # Errors
    ///
    /// Returns an internal dependency error when the compiled plan and the
    /// generated code disagree, which a framework-generated pair never does.
    pub fn input<T: 'static>(&self, position: usize) -> Result<Rc<T>, DependencyError> {
        let slot = self
            .slots
            .get(self.depends_count + position)
            .copied()
            .ok_or_else(|| {
                DependencyError::internal(
                    "invalid_dependency_slot",
                    "compiled provider requested an undeclared input position",
                )
            })?;
        let value = match slot {
            DependencySlot::Singleton(index) => self.singletons.get(index),
            DependencySlot::Request(index) => self.requests.get(index),
        }
        .and_then(Option::as_ref)
        .ok_or_else(|| {
            DependencyError::internal(
                "invalid_dependency_slot",
                "compiled request input slot was not initialized",
            )
        })?;
        Rc::clone(value).downcast::<T>().map_err(|_| {
            DependencyError::internal(
                "dependency_type_mismatch",
                "compiled request input slot contained an unexpected type",
            )
        })
    }
}

/// A synchronous slot-based provider body, generated by `#[provider]`.
#[doc(hidden)]
pub type SlotFactory<T> = Rc<dyn Fn(&SlotReader<'_>) -> Result<T, DependencyError>>;

/// An asynchronous slot-based provider body, generated by `#[provider]`.
///
/// The closure reads everything it needs through the reader before returning
/// the future, so the future owns its captures and outlives the borrow.
#[doc(hidden)]
pub type AsyncSlotFactory<T> = Rc<
    dyn Fn(&SlotReader<'_>) -> Pin<Box<dyn Future<Output = Result<T, DependencyError>> + 'static>>,
>;

impl Provider {
    fn from_slots_inner<T: 'static>(
        lifetime: DependencyLifetime,
        dependencies: Vec<DependencyKey>,
        input_count: usize,
        runner_of: impl Fn(Rc<Vec<DependencySlot>>, usize) -> ProviderRunner + 'static,
    ) -> Self {
        let depends_count = dependencies.len();
        let expected = depends_count + input_count;
        let compiler = Rc::new(move |slots: &[DependencySlot]| {
            expect_arity(slots, expected)?;
            let slots = Rc::new(slots.to_vec());
            Ok(CompiledProvider {
                runner: runner_of(slots, depends_count),
                finalizer: None,
            })
        });
        Self {
            key: DependencyKey::of::<T>(),
            lifetime,
            dependencies,
            compiler,
            finalizer: None,
        }
    }

    /// A request-lifetime provider over compiled slots: dependencies first,
    /// then decoded request inputs. Macro plumbing, not application API.
    #[doc(hidden)]
    #[must_use]
    pub fn request_from_slots<T: 'static>(
        dependencies: Vec<DependencyKey>,
        input_count: usize,
        factory: SlotFactory<T>,
    ) -> Self {
        Self::slot_provider::<T>(
            DependencyLifetime::Request,
            dependencies,
            input_count,
            factory,
        )
    }

    /// The transient twin of [`Provider::request_from_slots`].
    #[doc(hidden)]
    #[must_use]
    pub fn transient_from_slots<T: 'static>(
        dependencies: Vec<DependencyKey>,
        input_count: usize,
        factory: SlotFactory<T>,
    ) -> Self {
        Self::slot_provider::<T>(
            DependencyLifetime::Transient,
            dependencies,
            input_count,
            factory,
        )
    }

    /// The asynchronous twin of [`Provider::request_from_slots`].
    #[doc(hidden)]
    #[must_use]
    pub fn request_from_slots_async<T: 'static>(
        dependencies: Vec<DependencyKey>,
        input_count: usize,
        factory: AsyncSlotFactory<T>,
    ) -> Self {
        Self::async_slot_provider::<T>(
            DependencyLifetime::Request,
            dependencies,
            input_count,
            factory,
        )
    }

    /// The asynchronous transient twin of [`Provider::request_from_slots`].
    #[doc(hidden)]
    #[must_use]
    pub fn transient_from_slots_async<T: 'static>(
        dependencies: Vec<DependencyKey>,
        input_count: usize,
        factory: AsyncSlotFactory<T>,
    ) -> Self {
        Self::async_slot_provider::<T>(
            DependencyLifetime::Transient,
            dependencies,
            input_count,
            factory,
        )
    }

    fn slot_provider<T: 'static>(
        lifetime: DependencyLifetime,
        dependencies: Vec<DependencyKey>,
        input_count: usize,
        factory: SlotFactory<T>,
    ) -> Self {
        Self::from_slots_inner::<T>(
            lifetime,
            dependencies,
            input_count,
            move |slots, depends| {
                let factory = Rc::clone(&factory);
                ProviderRunner::Sync(Rc::new(move |singletons, requests| {
                    let reader = SlotReader {
                        singletons,
                        requests,
                        slots: &slots,
                        depends_count: depends,
                    };
                    factory(&reader).map(|value| Rc::new(value) as DependencyValue)
                }))
            },
        )
    }

    fn async_slot_provider<T: 'static>(
        lifetime: DependencyLifetime,
        dependencies: Vec<DependencyKey>,
        input_count: usize,
        factory: AsyncSlotFactory<T>,
    ) -> Self {
        Self::from_slots_inner::<T>(
            lifetime,
            dependencies,
            input_count,
            move |slots, depends| {
                let factory = Rc::clone(&factory);
                ProviderRunner::Async(Rc::new(move |singletons, requests| {
                    let reader = SlotReader {
                        singletons,
                        requests,
                        slots: &slots,
                        depends_count: depends,
                    };
                    let future = factory(&reader);
                    Box::pin(
                        async move { future.await.map(|value| Rc::new(value) as DependencyValue) },
                    )
                }))
            },
        )
    }
}

fn resolve<T: 'static>(
    slot: DependencySlot,
    singletons: &[Option<DependencyValue>],
    requests: &[Option<DependencyValue>],
) -> Result<Depends<T>, DependencyError> {
    let value = match slot {
        DependencySlot::Singleton(index) => singletons.get(index),
        DependencySlot::Request(index) => requests.get(index),
    }
    .and_then(Option::as_ref)
    .ok_or_else(|| {
        DependencyError::internal(
            "invalid_dependency_slot",
            "compiled dependency slot was not initialized",
        )
    })?;
    Rc::clone(value)
        .downcast::<T>()
        .map(Depends::from_rc)
        .map_err(|_| {
            DependencyError::internal(
                "dependency_type_mismatch",
                "compiled dependency slot contained an unexpected type",
            )
        })
}
