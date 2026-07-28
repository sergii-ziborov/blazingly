use crate::{ContentBlock, McpProtocolError};
use blazingly_json::{Map, Value};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

type ResourceFuture =
    Pin<Box<dyn Future<Output = Result<ResourceContent, McpProtocolError>> + 'static>>;
type ResourceReader = Rc<dyn Fn() -> ResourceFuture>;
type PromptFuture =
    Pin<Box<dyn Future<Output = Result<Vec<PromptMessage>, McpProtocolError>> + 'static>>;
type PromptRenderer = Rc<dyn Fn(Map<String, Value>) -> PromptFuture>;

/// Metadata returned from `resources/list`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceDescriptor {
    pub uri: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

impl ResourceDescriptor {
    #[must_use]
    pub fn new(uri: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            name: name.into(),
            title: None,
            description: None,
            mime_type: None,
            size: None,
        }
    }

    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    #[must_use]
    pub fn with_mime_type(mut self, mime_type: impl Into<String>) -> Self {
        self.mime_type = Some(mime_type.into());
        self
    }

    #[must_use]
    pub const fn with_size(mut self, size: u64) -> Self {
        self.size = Some(size);
        self
    }
}

/// Text or base64-encoded contents returned from `resources/read`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceContent {
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}

impl ResourceContent {
    #[must_use]
    pub fn text(
        uri: impl Into<String>,
        mime_type: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        Self {
            uri: uri.into(),
            mime_type: Some(mime_type.into()),
            text: Some(text.into()),
            blob: None,
        }
    }

    #[must_use]
    pub fn blob(
        uri: impl Into<String>,
        mime_type: impl Into<String>,
        base64: impl Into<String>,
    ) -> Self {
        Self {
            uri: uri.into(),
            mime_type: Some(mime_type.into()),
            text: None,
            blob: Some(base64.into()),
        }
    }
}

/// One statically or dynamically readable MCP resource.
#[derive(Clone)]
pub struct McpResource {
    descriptor: ResourceDescriptor,
    reader: ResourceReader,
}

impl McpResource {
    #[must_use]
    pub fn text(descriptor: ResourceDescriptor, text: impl Into<String>) -> Self {
        let uri = descriptor.uri.clone();
        let mime_type = descriptor
            .mime_type
            .clone()
            .unwrap_or_else(|| "text/plain".to_owned());
        let text = text.into();
        Self::dynamic(descriptor, move || {
            let content = ResourceContent::text(uri.clone(), mime_type.clone(), text.clone());
            async move { Ok(content) }
        })
    }

    #[must_use]
    pub fn dynamic<Reader, ReaderFuture>(descriptor: ResourceDescriptor, reader: Reader) -> Self
    where
        Reader: Fn() -> ReaderFuture + 'static,
        ReaderFuture: Future<Output = Result<ResourceContent, McpProtocolError>> + 'static,
    {
        Self {
            descriptor,
            reader: Rc::new(move || Box::pin(reader())),
        }
    }

    #[must_use]
    pub const fn descriptor(&self) -> &ResourceDescriptor {
        &self.descriptor
    }

    pub(crate) async fn read(&self) -> Result<ResourceContent, McpProtocolError> {
        (self.reader)().await
    }
}

/// Metadata for one prompt argument.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PromptArgument {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub required: bool,
}

impl PromptArgument {
    #[must_use]
    pub fn required(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            required: true,
        }
    }

    #[must_use]
    pub fn optional(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            required: false,
        }
    }

    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// Metadata returned from `prompts/list`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PromptDescriptor {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<PromptArgument>,
}

impl PromptDescriptor {
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            title: None,
            description: None,
            arguments: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    #[must_use]
    pub fn argument(mut self, argument: PromptArgument) -> Self {
        self.arguments.push(argument);
        self
    }
}

/// Role attached to a rendered MCP prompt message.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptRole {
    User,
    Assistant,
}

/// One message returned from `prompts/get`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PromptMessage {
    pub role: PromptRole,
    pub content: ContentBlock,
}

impl PromptMessage {
    #[must_use]
    pub fn text(role: PromptRole, text: impl Into<String>) -> Self {
        Self {
            role,
            content: ContentBlock::Text { text: text.into() },
        }
    }
}

/// One dynamically rendered MCP prompt.
#[derive(Clone)]
pub struct McpPrompt {
    descriptor: PromptDescriptor,
    renderer: PromptRenderer,
}

impl McpPrompt {
    #[must_use]
    pub fn dynamic<Renderer, RendererFuture>(
        descriptor: PromptDescriptor,
        renderer: Renderer,
    ) -> Self
    where
        Renderer: Fn(Map<String, Value>) -> RendererFuture + 'static,
        RendererFuture: Future<Output = Result<Vec<PromptMessage>, McpProtocolError>> + 'static,
    {
        Self {
            descriptor,
            renderer: Rc::new(move |arguments| Box::pin(renderer(arguments))),
        }
    }

    /// Creates a user prompt from a `{{name}}` template.
    #[must_use]
    pub fn template(descriptor: PromptDescriptor, template: impl Into<String>) -> Self {
        let arguments = descriptor.arguments.clone();
        let template = template.into();
        Self::dynamic(descriptor, move |values| {
            let arguments = arguments.clone();
            let mut rendered = template.clone();
            async move {
                for argument in &arguments {
                    let value = values.get(&argument.name).and_then(Value::as_str);
                    if argument.required && value.is_none() {
                        return Err(McpProtocolError {
                            code: -32_602,
                            message: format!(
                                "required prompt argument `{}` is missing",
                                argument.name
                            ),
                        });
                    }
                    rendered = rendered.replace(
                        &format!("{{{{{}}}}}", argument.name),
                        value.unwrap_or_default(),
                    );
                }
                Ok(vec![PromptMessage::text(PromptRole::User, rendered)])
            }
        })
    }

    #[must_use]
    pub const fn descriptor(&self) -> &PromptDescriptor {
        &self.descriptor
    }

    pub(crate) async fn render(
        &self,
        arguments: Map<String, Value>,
    ) -> Result<Vec<PromptMessage>, McpProtocolError> {
        (self.renderer)(arguments).await
    }
}

/// Deterministic resource and prompt registry shared by MCP transports.
#[derive(Clone, Default)]
pub struct McpRegistry {
    resources: BTreeMap<String, McpResource>,
    prompts: BTreeMap<String, McpPrompt>,
}

impl McpRegistry {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            resources: BTreeMap::new(),
            prompts: BTreeMap::new(),
        }
    }

    /// Registers a resource by URI.
    ///
    /// # Errors
    ///
    /// Returns an error for empty or duplicate URIs.
    pub fn register_resource(&mut self, resource: McpResource) -> Result<(), RegistryError> {
        let uri = resource.descriptor.uri.clone();
        if uri.is_empty() {
            return Err(RegistryError::EmptyResourceUri);
        }
        if self.resources.insert(uri.clone(), resource).is_some() {
            return Err(RegistryError::DuplicateResource { uri });
        }
        Ok(())
    }

    /// Registers a prompt by name.
    ///
    /// # Errors
    ///
    /// Returns an error for empty or duplicate names.
    pub fn register_prompt(&mut self, prompt: McpPrompt) -> Result<(), RegistryError> {
        let name = prompt.descriptor.name.clone();
        if name.is_empty() {
            return Err(RegistryError::EmptyPromptName);
        }
        if self.prompts.insert(name.clone(), prompt).is_some() {
            return Err(RegistryError::DuplicatePrompt { name });
        }
        Ok(())
    }

    pub fn resources(&self) -> impl Iterator<Item = &McpResource> {
        self.resources.values()
    }

    pub fn prompts(&self) -> impl Iterator<Item = &McpPrompt> {
        self.prompts.values()
    }

    pub(crate) fn resource(&self, uri: &str) -> Option<&McpResource> {
        self.resources.get(uri)
    }

    pub(crate) fn prompt(&self, name: &str) -> Option<&McpPrompt> {
        self.prompts.get(name)
    }

    pub(crate) fn has_resources(&self) -> bool {
        !self.resources.is_empty()
    }

    pub(crate) fn has_prompts(&self) -> bool {
        !self.prompts.is_empty()
    }
}

/// Invalid MCP registry declaration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RegistryError {
    EmptyResourceUri,
    EmptyPromptName,
    DuplicateResource { uri: String },
    DuplicatePrompt { name: String },
}

impl fmt::Display for RegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyResourceUri => formatter.write_str("resource URI cannot be empty"),
            Self::EmptyPromptName => formatter.write_str("prompt name cannot be empty"),
            Self::DuplicateResource { uri } => write!(formatter, "duplicate resource URI `{uri}`"),
            Self::DuplicatePrompt { name } => write!(formatter, "duplicate prompt name `{name}`"),
        }
    }
}

impl std::error::Error for RegistryError {}
