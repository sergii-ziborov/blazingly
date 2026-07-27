#![forbid(unsafe_code)]

//! Jinja-compatible HTML templates as compiled DI state and typed responses.
//!
//! Every template is HTML-escaped regardless of its name; [`EscapeMode::None`]
//! is the only opt-out.

use blazingly_core::{ApiSchema, BackgroundTask, ResponseHeader, SchemaKind, TypeDescriptor};
use blazingly_executor::{ExecutionOutcome, OperationOutput};
use minijinja::{AutoEscape, Environment, ErrorKind};
use serde::Serialize;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path};
use std::sync::Arc;

type EnvironmentSetup = Box<dyn FnOnce(&mut Environment<'static>)>;

/// Output escaping applied to every template of one environment.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum EscapeMode {
    /// HTML escaping for every template, whatever its name.
    #[default]
    Html,
    /// No escaping at all.
    ///
    /// Only for non-HTML output such as plain text or CSV. Interpolated values
    /// are written verbatim, so any untrusted value becomes an injection
    /// vector; never serve the result as `text/html`.
    None,
}

impl EscapeMode {
    const fn auto_escape(self) -> AutoEscape {
        match self {
            Self::Html => AutoEscape::Html,
            Self::None => AutoEscape::None,
        }
    }
}

/// Staged configuration for a [`Templates`] environment.
#[derive(Default)]
pub struct TemplatesBuilder {
    escape: EscapeMode,
    sources: Vec<(String, String)>,
    setup: Vec<EnvironmentSetup>,
}

impl TemplatesBuilder {
    /// Starts from an empty environment that HTML-escapes every template.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Overrides the escaping applied to every template.
    ///
    /// [`EscapeMode::None`] disables the crate's XSS protection; see its
    /// documentation before using it.
    #[must_use]
    pub fn escape(mut self, escape: EscapeMode) -> Self {
        self.escape = escape;
        self
    }

    /// Registers one owned template source.
    #[must_use]
    pub fn add_template(mut self, name: impl Into<String>, source: impl Into<String>) -> Self {
        self.sources.push((name.into(), source.into()));
        self
    }

    /// Registers owned template sources.
    #[must_use]
    pub fn add_templates(mut self, templates: impl IntoIterator<Item = (String, String)>) -> Self {
        self.sources.extend(templates);
        self
    }

    /// Registers every file under `root` whose extension is listed.
    ///
    /// Names are the `/`-joined paths relative to `root`, extension included,
    /// so `root/layouts/base.html` is named `layouts/base.html` and resolves
    /// through `{% extends %}`. Extensions match case-insensitively and may be
    /// written with or without a leading dot. Symbolic links to directories are
    /// not traversed.
    ///
    /// # Errors
    ///
    /// Returns directory traversal, read, or non-UTF-8 path failures.
    pub fn add_directory(
        mut self,
        root: impl AsRef<Path>,
        extensions: &[&str],
    ) -> Result<Self, TemplateError> {
        collect_directory(root.as_ref(), extensions, &mut self.sources)?;
        Ok(self)
    }

    /// Applies caller configuration, such as filters, tests, and globals.
    ///
    /// The escape mode is applied after every callback and cannot be replaced
    /// here.
    #[must_use]
    pub fn configure(mut self, apply: impl FnOnce(&mut Environment<'static>) + 'static) -> Self {
        self.setup.push(Box::new(apply));
        self
    }

    /// Compiles every registered source once.
    ///
    /// # Errors
    ///
    /// Returns the first template syntax error.
    pub fn build(self) -> Result<Templates, TemplateError> {
        let mut environment = Environment::new();
        for setup in self.setup {
            setup(&mut environment);
        }
        let escape = self.escape;
        environment.set_auto_escape_callback(move |_| escape.auto_escape());
        for (name, source) in self.sources {
            environment.add_template_owned(name, source)?;
        }
        Ok(Templates {
            environment: Arc::new(environment),
        })
    }
}

/// Precompiled `MiniJinja` environment suitable for a singleton provider.
#[derive(Clone)]
pub struct Templates {
    environment: Arc<Environment<'static>>,
}

impl Templates {
    /// Starts a staged configuration.
    #[must_use]
    pub fn builder() -> TemplatesBuilder {
        TemplatesBuilder::new()
    }

    /// Compiles owned templates once, HTML-escaping every one of them.
    ///
    /// # Errors
    ///
    /// Returns the first template syntax error.
    pub fn compile(
        templates: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Self, TemplateError> {
        Self::builder().add_templates(templates).build()
    }

    /// Compiles owned templates once under a caller-chosen escape mode.
    ///
    /// [`EscapeMode::None`] emits interpolated values verbatim and must never
    /// be used for output served as `text/html`.
    ///
    /// # Errors
    ///
    /// Returns the first template syntax error.
    pub fn compile_with_escape(
        templates: impl IntoIterator<Item = (String, String)>,
        escape: EscapeMode,
    ) -> Result<Self, TemplateError> {
        Self::builder()
            .escape(escape)
            .add_templates(templates)
            .build()
    }

    /// Compiles every listed extension under `root`, HTML-escaping all of them.
    ///
    /// Names follow [`TemplatesBuilder::add_directory`].
    ///
    /// # Errors
    ///
    /// Returns traversal, read, or template syntax failures.
    pub fn compile_directory(
        root: impl AsRef<Path>,
        extensions: &[&str],
    ) -> Result<Self, TemplateError> {
        Self::builder().add_directory(root, extensions)?.build()
    }

    /// Borrows the compiled environment for inspection.
    #[must_use]
    pub fn environment(&self) -> &Environment<'static> {
        &self.environment
    }

    /// Renders one named template into a `200` response.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::NotFound`] when no template carries `name`, and
    /// [`RenderError::Failed`] when context serialization or rendering fails.
    pub fn render(&self, name: &str, context: impl Serialize) -> Result<Html, RenderError> {
        let template = self
            .environment
            .get_template(name)
            .map_err(|error| RenderError::from_lookup(name, &error))?;
        template
            .render(context)
            .map(Html::new)
            .map_err(|error| RenderError::failed(name, &error))
    }
}

impl fmt::Debug for Templates {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Templates")
            .field("templates", &self.environment.templates().count())
            .finish()
    }
}

/// Typed `text/html` response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Html {
    body: String,
    status: u16,
}

impl Html {
    /// Wraps rendered markup with the default `200` status.
    #[must_use]
    pub fn new(body: impl Into<String>) -> Self {
        Self {
            body: body.into(),
            status: 200,
        }
    }

    /// Replaces the response status.
    #[must_use]
    pub const fn with_status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }

    /// Rendered markup.
    #[must_use]
    pub fn body(&self) -> &str {
        &self.body
    }

    /// Response status.
    #[must_use]
    pub const fn status(&self) -> u16 {
        self.status
    }

    /// Unwraps the rendered markup.
    #[must_use]
    pub fn into_body(self) -> String {
        self.body
    }
}

impl From<String> for Html {
    fn from(body: String) -> Self {
        Self::new(body)
    }
}

impl fmt::Display for Html {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.body)
    }
}

impl ApiSchema for Html {
    fn type_descriptor() -> TypeDescriptor {
        TypeDescriptor::scalar("Html", SchemaKind::String)
    }
}

impl OperationOutput for Html {
    fn into_execution_outcome(self) -> ExecutionOutcome {
        ExecutionOutcome::Success {
            status: self.status,
            headers: vec![ResponseHeader::new(
                "content-type",
                "text/html; charset=utf-8",
            )],
            body: Some(self.body.into_bytes()),
            background: Vec::<BackgroundTask>::new(),
        }
    }
}

/// Category of a [`TemplateError`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TemplateErrorKind {
    /// A template source failed to parse.
    Syntax,
    /// A template could not be read from disk.
    Io,
}

/// Failure raised while loading or compiling templates.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TemplateError {
    kind: TemplateErrorKind,
    message: String,
}

impl TemplateError {
    /// Whether the failure came from parsing or from disk.
    #[must_use]
    pub const fn kind(&self) -> TemplateErrorKind {
        self.kind
    }

    /// Human-readable failure description.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    fn io(path: &Path, error: &io::Error) -> Self {
        Self {
            kind: TemplateErrorKind::Io,
            message: format!("{}: {error}", path.display()),
        }
    }

    fn path(path: &Path) -> Self {
        Self {
            kind: TemplateErrorKind::Io,
            message: format!("non-UTF-8 template path: {}", path.display()),
        }
    }
}

impl From<minijinja::Error> for TemplateError {
    fn from(error: minijinja::Error) -> Self {
        Self {
            kind: TemplateErrorKind::Syntax,
            message: error.to_string(),
        }
    }
}

impl fmt::Display for TemplateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for TemplateError {}

/// Failure raised while rendering a compiled template.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RenderError {
    /// No template is registered under the requested name.
    NotFound {
        /// Requested template name.
        name: String,
    },
    /// The template exists but context serialization or rendering failed.
    Failed {
        /// Requested template name.
        name: String,
        /// Human-readable failure description.
        message: String,
    },
}

impl RenderError {
    /// Requested template name.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::NotFound { name } | Self::Failed { name, .. } => name,
        }
    }

    /// Whether the lookup itself failed.
    #[must_use]
    pub const fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound { .. })
    }

    fn from_lookup(name: &str, error: &minijinja::Error) -> Self {
        if error.kind() == ErrorKind::TemplateNotFound {
            Self::NotFound {
                name: name.to_owned(),
            }
        } else {
            Self::failed(name, error)
        }
    }

    fn failed(name: &str, error: &minijinja::Error) -> Self {
        Self::Failed {
            name: name.to_owned(),
            message: error.to_string(),
        }
    }
}

impl fmt::Display for RenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { name } => write!(formatter, "template `{name}` is not registered"),
            Self::Failed { name, message } => {
                write!(formatter, "template `{name}` failed to render: {message}")
            }
        }
    }
}

impl std::error::Error for RenderError {}

fn collect_directory(
    root: &Path,
    extensions: &[&str],
    sources: &mut Vec<(String, String)>,
) -> Result<(), TemplateError> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let mut entries = Vec::new();
        for entry in
            fs::read_dir(&directory).map_err(|error| TemplateError::io(&directory, &error))?
        {
            let entry = entry.map_err(|error| TemplateError::io(&directory, &error))?;
            let is_directory = entry
                .file_type()
                .map_err(|error| TemplateError::io(&entry.path(), &error))?
                .is_dir();
            entries.push((entry.path(), is_directory));
        }
        entries.sort();
        for (path, is_directory) in entries {
            if is_directory {
                pending.push(path);
            } else if has_extension(&path, extensions) {
                let name = template_name(root, &path)?;
                let source =
                    fs::read_to_string(&path).map_err(|error| TemplateError::io(&path, &error))?;
                sources.push((name, source));
            }
        }
    }
    Ok(())
}

fn has_extension(path: &Path, extensions: &[&str]) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| {
            extensions.iter().any(|allowed| {
                allowed
                    .trim_start_matches('.')
                    .eq_ignore_ascii_case(extension)
            })
        })
}

fn template_name(root: &Path, path: &Path) -> Result<String, TemplateError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| TemplateError::path(path))?;
    let mut name = String::new();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err(TemplateError::path(path));
        };
        let Some(part) = part.to_str() else {
            return Err(TemplateError::path(path));
        };
        if !name.is_empty() {
            name.push('/');
        }
        name.push_str(part);
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempTree {
        root: PathBuf,
    }

    impl TempTree {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let process = std::process::id();
            let root = std::env::temp_dir()
                .join(format!("blazingly-templates-{label}-{process}-{unique}"));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).expect("temp root");
            Self { root }
        }

        fn write(&self, relative: &str, source: &str) {
            let path = self.root.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("temp parent");
            }
            fs::write(path, source).expect("temp file");
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn extensionless_template_names_still_escape_interpolated_values() {
        let templates =
            Templates::compile([("page".to_owned(), "<main>{{ value }}</main>".to_owned())])
                .expect("template");

        let html = templates
            .render("page", BTreeMap::from([("value", "<script>")]))
            .expect("render");

        assert_eq!(html.body(), "<main>&lt;script&gt;</main>");
    }

    #[test]
    fn templates_compile_once_and_render_escaped_html() {
        let templates = Templates::compile([(
            "hello.html".to_owned(),
            "<h1>Hello {{ name }}</h1>".to_owned(),
        )])
        .expect("template");

        let html = templates
            .render("hello.html", BTreeMap::from([("name", "<Blazingly>")]))
            .expect("render");

        assert_eq!(html.body(), "<h1>Hello &lt;Blazingly&gt;</h1>");
    }

    #[test]
    fn explicit_escape_hatch_emits_values_verbatim() {
        let templates = Templates::compile_with_escape(
            [("report.csv".to_owned(), "{{ value }}".to_owned())],
            EscapeMode::None,
        )
        .expect("template");

        let html = templates
            .render("report.csv", BTreeMap::from([("value", "<script>")]))
            .expect("render");

        assert_eq!(html.body(), "<script>");
    }

    #[test]
    fn directory_loader_preserves_relative_names_and_filters_extensions() {
        let tree = TempTree::new("loader");
        tree.write("index.html", "<p>{{ value }}</p>");
        tree.write("pages/detail.html", "<span>{{ value }}</span>");
        tree.write("pages/notes.txt", "ignored");

        let templates = Templates::compile_directory(&tree.root, &["html"]).expect("directory");

        let mut names = templates
            .environment()
            .templates()
            .map(|(name, _)| name.to_owned())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, ["index.html", "pages/detail.html"]);

        let html = templates
            .render("pages/detail.html", BTreeMap::from([("value", "<script>")]))
            .expect("render");
        assert_eq!(html.body(), "<span>&lt;script&gt;</span>");
    }

    #[test]
    fn directory_loader_accepts_dotted_extensions_and_escapes_every_name() {
        let tree = TempTree::new("dotted");
        tree.write("mail/body", "{{ value }}");
        tree.write("mail/body.jinja", "{{ value }}");

        let templates = Templates::compile_directory(&tree.root, &[".JINJA"]).expect("directory");

        let names = templates
            .environment()
            .templates()
            .map(|(name, _)| name.to_owned())
            .collect::<Vec<_>>();
        assert_eq!(names, ["mail/body.jinja"]);

        let html = templates
            .render("mail/body.jinja", BTreeMap::from([("value", "<script>")]))
            .expect("render");
        assert_eq!(html.body(), "&lt;script&gt;");
    }

    #[test]
    fn in_memory_child_resolves_its_parent_template() {
        let templates = Templates::compile([
            (
                "layout".to_owned(),
                "<html><body>{% block content %}{% endblock %}</body></html>".to_owned(),
            ),
            (
                "child".to_owned(),
                "{% extends \"layout\" %}{% block content %}{{ value }}{% endblock %}".to_owned(),
            ),
        ])
        .expect("templates");

        let html = templates
            .render("child", BTreeMap::from([("value", "<script>")]))
            .expect("render");

        assert_eq!(html.body(), "<html><body>&lt;script&gt;</body></html>");
    }

    #[test]
    fn directory_child_resolves_its_parent_template() {
        let tree = TempTree::new("inherit");
        tree.write(
            "layouts/base.html",
            "<html><body>{% block content %}{% endblock %}</body></html>",
        );
        tree.write(
            "pages/index.html",
            "{% extends \"layouts/base.html\" %}{% block content %}{{ value }}{% endblock %}",
        );

        let templates = Templates::compile_directory(&tree.root, &["html"]).expect("directory");

        let html = templates
            .render("pages/index.html", BTreeMap::from([("value", "<script>")]))
            .expect("render");

        assert_eq!(html.body(), "<html><body>&lt;script&gt;</body></html>");
    }

    #[test]
    fn missing_template_is_distinguished_from_render_failure() {
        let templates =
            Templates::compile([("page".to_owned(), "{{ value.missing_method() }}".to_owned())])
                .expect("template");

        let missing = templates
            .render("absent", BTreeMap::<&str, &str>::new())
            .expect_err("missing template");
        assert!(missing.is_not_found());
        assert_eq!(missing.name(), "absent");
        assert_eq!(
            missing,
            RenderError::NotFound {
                name: "absent".to_owned()
            }
        );

        let failed = templates
            .render("page", BTreeMap::from([("value", "text")]))
            .expect_err("render failure");
        assert!(!failed.is_not_found());
        assert!(matches!(failed, RenderError::Failed { .. }));
    }

    #[test]
    fn missing_directory_reports_an_io_error() {
        let tree = TempTree::new("absent");
        let error = Templates::compile_directory(tree.root.join("nope"), &["html"])
            .expect_err("missing directory");

        assert_eq!(error.kind(), TemplateErrorKind::Io);
        assert!(error.message().contains("nope"));
    }

    #[test]
    fn syntax_errors_report_a_syntax_kind() {
        let error = Templates::compile([("page".to_owned(), "{% block %}".to_owned())])
            .expect_err("syntax error");

        assert_eq!(error.kind(), TemplateErrorKind::Syntax);
    }

    #[test]
    fn builder_registers_globals_and_filters_before_compilation() {
        let templates = Templates::builder()
            .configure(|environment| {
                environment.add_global("site", "blazingly");
                environment.add_filter("shout", |value: String| value.to_uppercase());
            })
            .add_template("page", "{{ site }}:{{ value | shout }}")
            .build()
            .expect("templates");

        assert!(templates.environment().get_template("page").is_ok());
        let html = templates
            .render("page", BTreeMap::from([("value", "<hi>")]))
            .expect("render");

        assert_eq!(html.body(), "blazingly:&lt;HI&gt;");
    }

    #[test]
    fn configure_cannot_disable_forced_escaping() {
        let templates = Templates::builder()
            .configure(|environment| {
                environment.set_auto_escape_callback(|_| AutoEscape::None);
            })
            .add_template("page", "{{ value }}")
            .build()
            .expect("templates");

        let html = templates
            .render("page", BTreeMap::from([("value", "<script>")]))
            .expect("render");

        assert_eq!(html.body(), "&lt;script&gt;");
    }

    #[test]
    fn responses_carry_the_chosen_status() {
        let html = Html::new("<p>gone</p>").with_status(404);
        assert_eq!(html.status(), 404);

        let ExecutionOutcome::Success {
            status,
            headers,
            body,
            ..
        } = html.into_execution_outcome()
        else {
            panic!("html outcome");
        };

        assert_eq!(status, 404);
        assert_eq!(headers[0].value, "text/html; charset=utf-8");
        assert_eq!(body.as_deref(), Some(b"<p>gone</p>".as_slice()));
    }

    #[test]
    fn rendered_responses_default_to_status_200() {
        let templates =
            Templates::compile([("page".to_owned(), "<p>ok</p>".to_owned())]).expect("template");

        let html = templates
            .render("page", BTreeMap::<&str, &str>::new())
            .expect("render");

        assert_eq!(html.status(), 200);
        assert_eq!(html.to_string(), "<p>ok</p>");
        assert_eq!(html.into_body(), "<p>ok</p>");
    }
}
