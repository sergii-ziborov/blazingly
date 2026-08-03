#![forbid(unsafe_code)]

use serde::Deserialize;
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const HELP: &str = "\
cargo blazingly — Blazingly application CLI

USAGE:
    cargo blazingly <COMMAND> [OPTIONS] [-- APP_ARGS...]

COMMANDS:
    new         Generate a minimal runnable Blazingly project
    dev         Build an app, run it, and rebuild it when sources change
    run         Build the release binary and launch it directly
    build       Build the selected app in release mode
    check       Type-check the selected app
    openapi     Build the app and print its OpenAPI document
    routes      Build the app and print its operation table
    discover    List discoverable Blazingly binary targets
    doctor      Verify Cargo, Rust, config, and app discovery

OPTIONS:
    -p, --package <NAME>       Select a workspace package
        --bin <NAME>           Select a binary target
        --example <NAME>       Select an example target
    -F, --features <LIST>      Enable Cargo features (repeatable, comma separated)
        --all-features         Enable every Cargo feature
        --no-default-features  Disable default Cargo features
        --address <ADDR>       Set BLAZINGLY_LISTEN_ADDRESS for the app
        --watch <PATH>         Add an autoreload path (repeatable)
        --no-reload            Run dev without watching files
        --no-build             Launch the existing binary without building (run)
        --debug                Use the debug profile for run/build
        --out <FILE>           Write openapi/routes output to a file
        --framework-path <DIR> Local Blazingly checkout for new (path dependency)

`openapi` and `routes` build the debug profile, then run the binary with
BLAZINGLY_EMIT=openapi|routes and forward its stdout. The framework prints the
document during server construction and exits before serving; the run also sets
BLAZINGLY_LISTEN_ADDRESS=127.0.0.1:0 and BLAZINGLY_WORKERS=1 so it cannot race
a serving instance for the listen port.

`new <name>` scaffolds Cargo.toml, src/main.rs, and .gitignore in ./<name>.
With `--framework-path` the project uses a path dependency on that checkout;
without it, the crates.io release matching this CLI's version.

Blazingly.toml:
    [app]
    package = \"api\"
    bin = \"api\"
    address = \"127.0.0.1:8000\"
    features = [\"native\"]
    all-features = false
    no-default-features = false
    watch = [\"src\", \"templates\"]

    [env]
    RUST_LOG = \"info\"

`address` sets BLAZINGLY_LISTEN_ADDRESS, the variable the scaffolded
application main reads and the generated Deployment manifest sets.
";

const ADDRESS_VARIABLE: &str = "BLAZINGLY_LISTEN_ADDRESS";
// Mirrors `blazingly_http::EMIT_VARIABLE`; `HttpApp::new` prints the requested
// document and exits when it is set, which is the whole `openapi`/`routes`
// contract.
const EMIT_VARIABLE: &str = "BLAZINGLY_EMIT";
const WORKERS_VARIABLE: &str = "BLAZINGLY_WORKERS";
const EMIT_ADDRESS: &str = "127.0.0.1:0";
const FRAMEWORK_GIT_URL: &str = "https://github.com/sergii-ziborov/blazingly";
// A generated project pins the framework version this CLI was built against,
// so `cargo install cargo-blazingly` and the project it scaffolds cannot
// disagree.
const FRAMEWORK_VERSION: &str = env!("CARGO_PKG_VERSION");

fn registry_dependency_note() -> String {
    format!(
        "# To track unreleased work on `main` instead:\n\
         # blazingly = {{ git = \"{FRAMEWORK_GIT_URL}\", features = [\"native\"] }}\n"
    )
}
const STAGE_DIRECTORY: &str = "blazingly-dev";
const STAGE_ATTEMPTS: u32 = 10;
const POLL_INTERVAL: Duration = Duration::from_millis(200);
const QUIET_PERIOD: Duration = Duration::from_millis(400);
const STOP_GRACE: Duration = Duration::from_secs(5);
const STOP_POLL: Duration = Duration::from_millis(50);

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<u8, CliError> {
    let mut arguments = env::args().skip(1).collect::<Vec<_>>();
    if arguments
        .first()
        .is_some_and(|argument| argument == "blazingly")
    {
        arguments.remove(0);
    }
    let Some(command) = arguments.first().cloned() else {
        print!("{HELP}");
        return Ok(0);
    };
    if matches!(command.as_str(), "-h" | "--help" | "help") {
        print!("{HELP}");
        return Ok(0);
    }
    if matches!(command.as_str(), "-V" | "--version" | "version") {
        println!("cargo-blazingly {}", env!("CARGO_PKG_VERSION"));
        return Ok(0);
    }
    // `new` runs before metadata discovery: it is the one command that works
    // outside a Cargo workspace.
    if command == "new" {
        return new_project(&arguments[1..]);
    }

    let options = CliOptions::parse(&arguments[1..])?;
    let metadata = cargo_metadata()?;
    let config = FileConfig::load(&metadata.workspace_root)?;
    let candidates = discover_apps(&metadata);

    match command.as_str() {
        "discover" => {
            print_candidates(&candidates, &metadata.workspace_root);
            Ok(0)
        }
        "doctor" => doctor(&metadata, &config, &options, &candidates),
        "dev" | "run" | "build" | "check" | "openapi" | "routes" => {
            let session = Session {
                metadata: &metadata,
                config: &config,
                options: &options,
                app: select_app(&candidates, &config, &options)?,
            };
            match command.as_str() {
                "dev" => session.dev(),
                "run" => session.run_app(),
                "build" => session.cargo_status("build"),
                "check" => session.cargo_status("check"),
                "openapi" | "routes" => session.emit(&command),
                _ => unreachable!(),
            }
        }
        unknown => Err(CliError::Usage(format!(
            "unknown command `{unknown}`\n\n{HELP}"
        ))),
    }
}

#[derive(Clone, Debug, Default)]
struct FeatureSelection {
    names: Vec<String>,
    all: bool,
    no_default: bool,
}

impl FeatureSelection {
    fn push(&mut self, value: &str) {
        self.names.extend(
            value
                .split([',', ' '])
                .filter(|feature| !feature.is_empty())
                .map(str::to_owned),
        );
    }

    fn apply(&self, command: &mut Command) {
        if self.all {
            command.arg("--all-features");
        }
        if self.no_default {
            command.arg("--no-default-features");
        }
        if !self.names.is_empty() {
            command.arg("--features").arg(self.names.join(","));
        }
    }
}

#[derive(Default)]
struct CliOptions {
    package: Option<String>,
    target: Option<TargetSelection>,
    address: Option<String>,
    features: FeatureSelection,
    watch: Vec<PathBuf>,
    reload: bool,
    debug: bool,
    no_build: bool,
    out: Option<PathBuf>,
    app_arguments: Vec<String>,
}

impl CliOptions {
    fn parse(arguments: &[String]) -> Result<Self, CliError> {
        let mut options = Self {
            reload: true,
            ..Self::default()
        };
        let mut index = 0;
        while index < arguments.len() {
            match arguments[index].as_str() {
                "-h" | "--help" => return Err(CliError::Usage(HELP.to_owned())),
                "-p" | "--package" => {
                    options.package = Some(option_value(arguments, &mut index, "--package")?);
                }
                "--bin" => {
                    let name = option_value(arguments, &mut index, "--bin")?;
                    set_target(&mut options.target, TargetKind::Bin, name)?;
                }
                "--example" => {
                    let name = option_value(arguments, &mut index, "--example")?;
                    set_target(&mut options.target, TargetKind::Example, name)?;
                }
                "-F" | "--features" => {
                    let value = option_value(arguments, &mut index, "--features")?;
                    options.features.push(&value);
                }
                "--all-features" => options.features.all = true,
                "--no-default-features" => options.features.no_default = true,
                "--address" => {
                    options.address = Some(option_value(arguments, &mut index, "--address")?);
                }
                "--watch" => {
                    options.watch.push(PathBuf::from(option_value(
                        arguments, &mut index, "--watch",
                    )?));
                }
                "--no-reload" => options.reload = false,
                "--no-build" => options.no_build = true,
                "--debug" => options.debug = true,
                "--out" => {
                    options.out =
                        Some(PathBuf::from(option_value(arguments, &mut index, "--out")?));
                }
                "--" => {
                    options
                        .app_arguments
                        .extend_from_slice(&arguments[index + 1..]);
                    break;
                }
                argument if argument.starts_with('-') => {
                    return Err(CliError::Usage(format!("unknown option `{argument}`")));
                }
                argument => {
                    return Err(CliError::Usage(format!(
                        "unexpected argument `{argument}`; app arguments belong after `--`"
                    )));
                }
            }
            index += 1;
        }
        Ok(options)
    }
}

fn option_value(arguments: &[String], index: &mut usize, name: &str) -> Result<String, CliError> {
    *index += 1;
    arguments
        .get(*index)
        .cloned()
        .ok_or_else(|| CliError::Usage(format!("`{name}` requires a value")))
}

fn set_target(
    target: &mut Option<TargetSelection>,
    kind: TargetKind,
    name: String,
) -> Result<(), CliError> {
    if target.is_some() {
        return Err(CliError::Usage(
            "only one of `--bin` and `--example` may be selected".to_owned(),
        ));
    }
    *target = Some(TargetSelection { kind, name });
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Default)]
struct FileConfig {
    #[serde(default)]
    app: AppConfig,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct AppConfig {
    package: Option<String>,
    bin: Option<String>,
    example: Option<String>,
    address: Option<String>,
    #[serde(default)]
    features: Vec<String>,
    #[serde(default)]
    all_features: bool,
    #[serde(default)]
    no_default_features: bool,
    #[serde(default)]
    watch: Vec<PathBuf>,
}

impl FileConfig {
    fn load(workspace_root: &Path) -> Result<Self, CliError> {
        let path = workspace_root.join("Blazingly.toml");
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(&path).map_err(|error| CliError::Io(path.clone(), error))?;
        toml::from_str(&text).map_err(|error| CliError::Config(path, error.to_string()))
    }

    fn target(&self) -> Result<Option<TargetSelection>, CliError> {
        match (&self.app.bin, &self.app.example) {
            (Some(_), Some(_)) => Err(CliError::Config(
                PathBuf::from("Blazingly.toml"),
                "`app.bin` and `app.example` are mutually exclusive".to_owned(),
            )),
            (Some(name), None) => Ok(Some(TargetSelection {
                kind: TargetKind::Bin,
                name: name.clone(),
            })),
            (None, Some(name)) => Ok(Some(TargetSelection {
                kind: TargetKind::Example,
                name: name.clone(),
            })),
            (None, None) => Ok(None),
        }
    }
}

#[derive(Deserialize)]
struct CargoMetadata {
    workspace_root: PathBuf,
    target_directory: PathBuf,
    packages: Vec<MetadataPackage>,
    workspace_members: Vec<String>,
}

#[derive(Deserialize)]
struct MetadataPackage {
    id: String,
    name: String,
    manifest_path: PathBuf,
    dependencies: Vec<MetadataDependency>,
    targets: Vec<MetadataTarget>,
}

#[derive(Deserialize)]
struct MetadataDependency {
    name: String,
    #[serde(default)]
    path: Option<PathBuf>,
}

#[derive(Deserialize)]
struct MetadataTarget {
    name: String,
    kind: Vec<String>,
    src_path: PathBuf,
}

fn cargo_metadata() -> Result<CargoMetadata, CliError> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args(["metadata", "--format-version=1", "--no-deps"])
        .output()
        .map_err(|error| CliError::Process("could not execute `cargo metadata`", error))?;
    if !output.status.success() {
        return Err(CliError::CommandFailed {
            command: "cargo metadata".to_owned(),
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    blazingly_json::from_slice(&output.stdout)
        .map_err(|error| CliError::Metadata(error.to_string()))
}

#[derive(Deserialize)]
struct ArtifactMessage {
    reason: String,
    target: ArtifactTarget,
    #[serde(default)]
    executable: Option<PathBuf>,
}

#[derive(Deserialize)]
struct ArtifactTarget {
    name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum TargetKind {
    Bin,
    Example,
}

impl TargetKind {
    const fn cargo_flag(&self) -> &'static str {
        match self {
            Self::Bin => "--bin",
            Self::Example => "--example",
        }
    }

    const fn label(&self) -> &'static str {
        match self {
            Self::Bin => "bin",
            Self::Example => "example",
        }
    }
}

#[derive(Clone, Debug)]
struct TargetSelection {
    kind: TargetKind,
    name: String,
}

#[derive(Clone, Debug)]
struct DiscoveredApp {
    package: String,
    target: TargetSelection,
    manifest_path: PathBuf,
    source_path: PathBuf,
    direct_blazingly_dependency: bool,
}

fn discover_apps(metadata: &CargoMetadata) -> Vec<DiscoveredApp> {
    let workspace_members = metadata
        .workspace_members
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let mut applications = metadata
        .packages
        .iter()
        .filter(|package| workspace_members.contains(package.id.as_str()))
        .filter(|package| package.name != env!("CARGO_PKG_NAME"))
        .flat_map(|package| {
            let direct_blazingly_dependency = package
                .dependencies
                .iter()
                .any(|dependency| dependency.name == "blazingly");
            package.targets.iter().filter_map(move |target| {
                let kind = if target.kind.iter().any(|kind| kind == "bin") {
                    TargetKind::Bin
                } else if target.kind.iter().any(|kind| kind == "example") {
                    TargetKind::Example
                } else {
                    return None;
                };
                Some(DiscoveredApp {
                    package: package.name.clone(),
                    target: TargetSelection {
                        kind,
                        name: target.name.clone(),
                    },
                    manifest_path: package.manifest_path.clone(),
                    source_path: target.src_path.clone(),
                    direct_blazingly_dependency,
                })
            })
        })
        .collect::<Vec<_>>();
    applications.sort_by(|left, right| {
        right
            .direct_blazingly_dependency
            .cmp(&left.direct_blazingly_dependency)
            .then_with(|| left.package.cmp(&right.package))
            .then_with(|| left.target.name.cmp(&right.target.name))
    });
    applications
}

fn select_app(
    applications: &[DiscoveredApp],
    config: &FileConfig,
    options: &CliOptions,
) -> Result<DiscoveredApp, CliError> {
    let package = options.package.as_ref().or(config.app.package.as_ref());
    let configured_target = config.target()?;
    let target = options.target.as_ref().or(configured_target.as_ref());
    let mut matching = applications
        .iter()
        .filter(|app| package.is_none_or(|package| app.package == *package))
        .filter(|app| {
            target.is_none_or(|target| {
                app.target.kind == target.kind && app.target.name == target.name
            })
        })
        .cloned()
        .collect::<Vec<_>>();

    if matching.len() > 1 {
        let blazingly = matching
            .iter()
            .filter(|app| app.direct_blazingly_dependency)
            .cloned()
            .collect::<Vec<_>>();
        if blazingly.len() == 1 {
            matching = blazingly;
        }
    }
    match matching.as_slice() {
        [selected] => Ok(selected.clone()),
        [] => Err(CliError::Discovery(
            "no matching binary target was found; run `cargo blazingly discover`".to_owned(),
        )),
        _ => Err(CliError::Discovery(format!(
            "multiple app targets match; select one with `--package` and `--bin`/`--example`:\n{}",
            candidates_text(&matching)
        ))),
    }
}

fn print_candidates(applications: &[DiscoveredApp], workspace_root: &Path) {
    if applications.is_empty() {
        println!("No binary or example targets found.");
        return;
    }
    println!("PACKAGE\tKIND\tTARGET\tBLAZINGLY\tSOURCE");
    for application in applications {
        println!(
            "{}\t{}\t{}\t{}\t{}",
            application.package,
            application.target.kind.label(),
            application.target.name,
            if application.direct_blazingly_dependency {
                "yes"
            } else {
                "no"
            },
            display_relative(workspace_root, &application.source_path)
        );
    }
}

fn candidates_text(applications: &[DiscoveredApp]) -> String {
    applications
        .iter()
        .map(|app| {
            format!(
                "  -p {} {} {}",
                app.package,
                app.target.kind.cargo_flag(),
                app.target.name
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn doctor(
    metadata: &CargoMetadata,
    config: &FileConfig,
    options: &CliOptions,
    candidates: &[DiscoveredApp],
) -> Result<u8, CliError> {
    let rustc = Command::new("rustc")
        .arg("--version")
        .output()
        .map_err(|error| CliError::Process("could not execute rustc", error))?;
    if !rustc.status.success() {
        return Err(CliError::Discovery("rustc is not healthy".to_owned()));
    }
    println!("ok  {}", String::from_utf8_lossy(&rustc.stdout).trim());
    println!("ok  workspace {}", metadata.workspace_root.display());
    println!("ok  {} discoverable target(s)", candidates.len());
    let session = Session {
        metadata,
        config,
        options,
        app: select_app(candidates, config, options)?,
    };
    println!(
        "ok  selected {} {} {}",
        session.app.package,
        session.app.target.kind.label(),
        session.app.target.name
    );
    let features = session.features();
    if !features.names.is_empty() {
        println!("ok  features {}", features.names.join(","));
    }
    if features.all {
        println!("ok  all Cargo features enabled");
    }
    if features.no_default {
        println!("ok  default Cargo features disabled");
    }
    if let Some(address) = effective_address(config, options) {
        println!("ok  {ADDRESS_VARIABLE}={address}");
    }
    let binary = session.binary_path(!options.debug);
    if binary.exists() {
        println!("ok  binary {}", binary.display());
    } else {
        println!("ok  binary {} (not built yet)", binary.display());
    }
    println!("ok  {} watched path(s)", session.watch_roots().len());
    Ok(0)
}

fn effective_address<'a>(config: &'a FileConfig, options: &'a CliOptions) -> Option<&'a str> {
    options.address.as_deref().or(config.app.address.as_deref())
}

/// Generates a minimal runnable project in `./<name>` from the shared
/// `blazingly-docs` scaffold.
fn new_project(arguments: &[String]) -> Result<u8, CliError> {
    let mut name: Option<String> = None;
    let mut framework_path: Option<PathBuf> = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "-h" | "--help" => return Err(CliError::Usage(HELP.to_owned())),
            "--framework-path" => {
                framework_path = Some(PathBuf::from(option_value(
                    arguments,
                    &mut index,
                    "--framework-path",
                )?));
            }
            argument if argument.starts_with('-') => {
                return Err(CliError::Usage(format!("unknown option `{argument}`")));
            }
            argument => {
                if name.is_some() {
                    return Err(CliError::Usage("`new` accepts one project name".to_owned()));
                }
                name = Some(argument.to_owned());
            }
        }
        index += 1;
    }
    let Some(name) = name else {
        return Err(CliError::Usage(
            "`new` requires a project name: cargo blazingly new <name>".to_owned(),
        ));
    };
    validate_package_name(&name)?;
    let dependency = framework_dependency(framework_path.as_deref())?;
    let root = env::current_dir()
        .map_err(|error| CliError::Process("could not read the working directory", error))?
        .join(&name);
    if root.exists() {
        return Err(CliError::Usage(format!(
            "{} already exists",
            root.display()
        )));
    }
    let files = project_files(&name, &dependency, framework_path.is_none());
    for (relative, contents) in &files {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| CliError::Io(parent.to_owned(), error))?;
        }
        fs::write(&path, contents).map_err(|error| CliError::Io(path.clone(), error))?;
    }
    println!("Created {}", root.display());
    for relative in files.keys() {
        println!("  {relative}");
    }
    println!("Next: run `cargo blazingly dev` inside {name}");
    Ok(0)
}

fn validate_package_name(name: &str) -> Result<(), CliError> {
    let valid_start = name
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_');
    let valid_rest = name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'));
    if valid_start && valid_rest {
        Ok(())
    } else {
        Err(CliError::Usage(format!(
            "`{name}` is not a valid package name; use ASCII letters, digits, `-`, and `_`, \
             starting with a letter or `_`"
        )))
    }
}

/// The Cargo dependency expression for `blazingly`: a path dependency on a
/// local checkout, or the registry release matching this CLI.
fn framework_dependency(framework_path: Option<&Path>) -> Result<String, CliError> {
    let Some(path) = framework_path else {
        return Ok(format!(
            "{{ version = \"{FRAMEWORK_VERSION}\", features = [\"native\"] }}"
        ));
    };
    let crate_directory = resolve_framework_crate(path)?;
    Ok(format!(
        "{{ path = \"{}\", features = [\"native\"] }}",
        toml_path_text(&crate_directory)
    ))
}

/// Accepts either the workspace root of a Blazingly checkout or the
/// `blazingly` crate directory itself.
fn resolve_framework_crate(path: &Path) -> Result<PathBuf, CliError> {
    let absolute =
        std::path::absolute(path).map_err(|error| CliError::Io(path.to_owned(), error))?;
    let nested = absolute.join("crates").join("blazingly");
    let candidate = if nested.join("Cargo.toml").is_file() {
        nested
    } else {
        absolute
    };
    let manifest = candidate.join("Cargo.toml");
    let text =
        fs::read_to_string(&manifest).map_err(|error| CliError::Io(manifest.clone(), error))?;
    if !text.contains("name = \"blazingly\"") {
        return Err(CliError::Usage(format!(
            "`--framework-path` does not reach the `blazingly` crate: {} declares no `name = \"blazingly\"`",
            manifest.display()
        )));
    }
    Ok(candidate)
}

/// A path in a Cargo manifest string; forward slashes work on every platform
/// and need no TOML escaping.
fn toml_path_text(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

fn project_files(
    name: &str,
    dependency: &str,
    registry_dependency: bool,
) -> BTreeMap<String, String> {
    let config = blazingly_docs::ScaffoldConfig::new(name)
        .with_dependency(dependency)
        .without_kubernetes();
    let mut files = blazingly_docs::scaffold(&config).into_files();
    if registry_dependency && let Some(cargo_toml) = files.get_mut("Cargo.toml") {
        *cargo_toml = cargo_toml.replacen(
            "\nblazingly = ",
            &format!("\n{}blazingly = ", registry_dependency_note()),
            1,
        );
    }
    files.insert(".gitignore".to_owned(), "/target\n".to_owned());
    files
}

struct Session<'a> {
    metadata: &'a CargoMetadata,
    config: &'a FileConfig,
    options: &'a CliOptions,
    app: DiscoveredApp,
}

impl Session<'_> {
    fn features(&self) -> FeatureSelection {
        let mut selection = FeatureSelection {
            names: self.config.app.features.clone(),
            all: self.config.app.all_features || self.options.features.all,
            no_default: self.config.app.no_default_features || self.options.features.no_default,
        };
        selection
            .names
            .extend(self.options.features.names.iter().cloned());
        let mut seen = BTreeSet::new();
        selection.names.retain(|name| seen.insert(name.clone()));
        selection
    }

    fn cargo(&self, subcommand: &str, release: bool) -> Command {
        let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let mut command = Command::new(cargo);
        command
            .current_dir(&self.metadata.workspace_root)
            .arg(subcommand)
            .args(["--package", &self.app.package])
            .arg(self.app.target.kind.cargo_flag())
            .arg(&self.app.target.name);
        if release {
            command.arg("--release");
        }
        self.features().apply(&mut command);
        command.envs(&self.config.env);
        command
    }

    fn cargo_status(&self, subcommand: &str) -> Result<u8, CliError> {
        let release = subcommand == "build" && !self.options.debug;
        let status = self
            .cargo(subcommand, release)
            .status()
            .map_err(|error| CliError::Process("could not execute Cargo", error))?;
        Ok(exit_code(status.code()))
    }

    fn build(&self, release: bool) -> Result<BuildOutcome, CliError> {
        let mut command = self.cargo("build", release);
        command
            .arg("--message-format=json-render-diagnostics")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = command
            .spawn()
            .map_err(|error| CliError::Process("could not execute Cargo", error))?;
        let mut messages = String::new();
        if let Some(stdout) = child.stdout.as_mut() {
            stdout
                .read_to_string(&mut messages)
                .map_err(|error| CliError::Process("could not read Cargo output", error))?;
        }
        let status = child
            .wait()
            .map_err(|error| CliError::Process("could not wait for Cargo", error))?;
        Ok(BuildOutcome {
            code: exit_code(status.code()),
            executable: self.artifact_path(&messages),
        })
    }

    fn artifact_path(&self, messages: &str) -> Option<PathBuf> {
        messages
            .lines()
            .filter_map(|line| blazingly_json::from_str::<ArtifactMessage>(line).ok())
            .filter(|message| message.reason == "compiler-artifact")
            .filter(|message| message.target.name == self.app.target.name)
            .filter_map(|message| message.executable)
            .next_back()
    }

    fn binary_path(&self, release: bool) -> PathBuf {
        let mut path = self.metadata.target_directory.clone();
        path.push(if release { "release" } else { "debug" });
        if matches!(self.app.target.kind, TargetKind::Example) {
            path.push("examples");
        }
        path.push(format!(
            "{}{}",
            self.app.target.name,
            env::consts::EXE_SUFFIX
        ));
        path
    }

    fn executable(&self, outcome: BuildOutcome, release: bool) -> PathBuf {
        outcome
            .executable
            .unwrap_or_else(|| self.binary_path(release))
    }

    fn apply_env(&self, command: &mut Command) {
        command.envs(&self.config.env);
        if let Some(address) = effective_address(self.config, self.options) {
            command.env(ADDRESS_VARIABLE, address);
        }
    }

    fn app_command(&self, binary: &Path) -> Command {
        let mut command = Command::new(binary);
        command
            .current_dir(&self.metadata.workspace_root)
            .args(&self.options.app_arguments);
        self.apply_env(&mut command);
        command
    }

    fn run_app(&self) -> Result<u8, CliError> {
        let release = !self.options.debug;
        let binary = if self.options.no_build {
            self.binary_path(release)
        } else {
            let outcome = self.build(release)?;
            if !outcome.succeeded() {
                return Ok(outcome.code);
            }
            self.executable(outcome, release)
        };
        if !binary.exists() {
            return Err(CliError::Discovery(format!(
                "{} does not exist; build the app before using `--no-build`",
                binary.display()
            )));
        }
        let status = self
            .app_command(&binary)
            .status()
            .map_err(|error| CliError::Process("could not run application", error))?;
        Ok(exit_code(status.code()))
    }

    /// The application run for `openapi` and `routes`: the binary prints one
    /// introspection document during server construction and exits.
    fn emit_command(&self, binary: &Path, mode: &str) -> Command {
        let mut command = self.app_command(binary);
        command
            .env(EMIT_VARIABLE, mode)
            .env(ADDRESS_VARIABLE, EMIT_ADDRESS)
            .env(WORKERS_VARIABLE, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        command
    }

    /// Builds the debug profile so `openapi`/`routes` share the `dev` build
    /// cache, runs the binary as an emit run, and forwards its stdout.
    fn emit(&self, mode: &str) -> Result<u8, CliError> {
        let outcome = self.build(false)?;
        if !outcome.succeeded() {
            return Ok(outcome.code);
        }
        let binary = self.executable(outcome, false);
        let output = self
            .emit_command(&binary, mode)
            .output()
            .map_err(|error| CliError::Process("could not run application", error))?;
        if !output.status.success() {
            return Ok(exit_code(output.status.code()));
        }
        if let Some(path) = self.options.out.as_deref() {
            fs::write(path, &output.stdout)
                .map_err(|error| CliError::Io(path.to_owned(), error))?;
        } else {
            let mut stdout = std::io::stdout().lock();
            stdout
                .write_all(&output.stdout)
                .and_then(|()| stdout.flush())
                .map_err(|error| {
                    CliError::Process("could not forward application output", error)
                })?;
        }
        Ok(0)
    }

    fn spawn(&self, binary: &Path) -> Result<Child, CliError> {
        self.app_command(binary)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| CliError::Process("could not start development app", error))
    }

    // Development runs a copy of the artifact. Windows locks a running
    // executable, so building before stopping the previous process only works
    // when that process does not hold the path Cargo relinks.
    fn stage_binary(&self, binary: &Path) -> Result<PathBuf, CliError> {
        let Some(name) = binary.file_name() else {
            return Ok(binary.to_owned());
        };
        let directory = self.metadata.target_directory.join(STAGE_DIRECTORY);
        fs::create_dir_all(&directory).map_err(|error| CliError::Io(directory.clone(), error))?;
        let staged = directory.join(name);
        for _ in 1..STAGE_ATTEMPTS {
            if fs::copy(binary, &staged).is_ok() {
                return Ok(staged);
            }
            thread::sleep(STOP_POLL);
        }
        fs::copy(binary, &staged).map_err(|error| CliError::Io(staged.clone(), error))?;
        Ok(staged)
    }

    fn spawn_staged(&self, binary: &Path) -> Result<Child, CliError> {
        let staged = self.stage_binary(binary)?;
        self.spawn(&staged)
    }

    fn dev(&self) -> Result<u8, CliError> {
        let stopped = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&stopped);
        ctrlc::set_handler(move || signal.store(true, Ordering::Release))
            .map_err(|error| CliError::Signal(error.to_string()))?;

        println!(
            "Blazingly dev: {} {} {}",
            self.app.package,
            self.app.target.kind.label(),
            self.app.target.name
        );
        let roots = self.watch_roots();
        let mut quiet = QuietPeriod::new(QUIET_PERIOD);
        if self.options.reload {
            println!("Watching {} path(s); Ctrl-C stops the app.", roots.len());
        }

        let outcome = self.build(false)?;
        if !outcome.succeeded() && !self.options.reload {
            return Ok(outcome.code);
        }
        // Snapshot after the first build so that a lock file Cargo has just
        // written does not read as a source change.
        let mut digest = tree_digest(&roots);
        let mut child = if outcome.succeeded() {
            Some(self.spawn_staged(&self.executable(outcome, false))?)
        } else {
            println!("Build failed; waiting for a source change.");
            None
        };

        while !stopped.load(Ordering::Acquire) {
            if let Some(process) = child.as_mut()
                && let Some(status) = process
                    .try_wait()
                    .map_err(|error| CliError::Process("could not poll application", error))?
            {
                println!(
                    "Application exited with {status}; waiting for {}.",
                    if self.options.reload {
                        "a source change"
                    } else {
                        "Ctrl-C"
                    }
                );
                child = None;
            }
            if self.options.reload {
                let next = tree_digest(&roots);
                let changed = next != digest;
                digest = next;
                if quiet.observe(changed, Instant::now()) {
                    child = self.rebuild(child)?;
                }
            }
            thread::sleep(POLL_INTERVAL);
        }
        if let Some(mut process) = child {
            stop_child(&mut process)?;
        }
        Ok(0)
    }

    fn rebuild(&self, current: Option<Child>) -> Result<Option<Child>, CliError> {
        println!("Source changed; building.");
        let outcome = self.build(false)?;
        if !outcome.succeeded() {
            println!(
                "Build failed; {}.",
                if current.is_some() {
                    "keeping the running application"
                } else {
                    "no application is running"
                }
            );
            return Ok(current);
        }
        let binary = self.executable(outcome, false);
        if let Some(mut process) = current {
            stop_child(&mut process)?;
        }
        println!("Build succeeded; restarting.");
        Ok(Some(self.spawn_staged(&binary)?))
    }

    fn watch_roots(&self) -> Vec<PathBuf> {
        let workspace_root = &self.metadata.workspace_root;
        let mut roots = vec![
            workspace_root.join("Cargo.toml"),
            workspace_root.join("Cargo.lock"),
            workspace_root.join("Blazingly.toml"),
        ];
        if let Some(directory) = self.app.source_path.parent() {
            roots.push(directory.to_owned());
        }
        roots.extend(self.package_roots());
        roots.extend(
            self.config
                .app
                .watch
                .iter()
                .chain(&self.options.watch)
                .map(|path| {
                    if path.is_absolute() {
                        path.clone()
                    } else {
                        workspace_root.join(path)
                    }
                }),
        );
        roots.sort();
        roots.dedup();
        roots
    }

    fn package_roots(&self) -> Vec<PathBuf> {
        let by_directory = self
            .metadata
            .packages
            .iter()
            .filter_map(|package| Some((package.manifest_path.parent()?, package)))
            .collect::<BTreeMap<_, _>>();
        let mut roots = Vec::new();
        let mut seen = BTreeSet::new();
        let mut pending = vec![
            self.app
                .manifest_path
                .parent()
                .unwrap_or(&self.metadata.workspace_root)
                .to_owned(),
        ];
        while let Some(directory) = pending.pop() {
            if !seen.insert(directory.clone()) {
                continue;
            }
            roots.push(directory.join("src"));
            roots.push(directory.join("Cargo.toml"));
            let Some(package) = by_directory.get(directory.as_path()) else {
                continue;
            };
            for dependency in &package.dependencies {
                let Some(path) = dependency.path.as_ref() else {
                    continue;
                };
                if path.starts_with(&self.metadata.workspace_root) {
                    pending.push(path.clone());
                }
            }
        }
        roots
    }
}

struct BuildOutcome {
    code: u8,
    executable: Option<PathBuf>,
}

impl BuildOutcome {
    const fn succeeded(&self) -> bool {
        self.code == 0
    }
}

struct QuietPeriod {
    duration: Duration,
    pending: Option<Instant>,
}

impl QuietPeriod {
    const fn new(duration: Duration) -> Self {
        Self {
            duration,
            pending: None,
        }
    }

    fn observe(&mut self, changed: bool, now: Instant) -> bool {
        if changed {
            self.pending = Some(now);
            return false;
        }
        let Some(since) = self.pending else {
            return false;
        };
        if now.duration_since(since) < self.duration {
            return false;
        }
        self.pending = None;
        true
    }
}

fn stop_child(child: &mut Child) -> Result<(), CliError> {
    if reap(child)? {
        return Ok(());
    }
    if request_stop(child.id()) {
        let deadline = Instant::now() + STOP_GRACE;
        while Instant::now() < deadline {
            if reap(child)? {
                return Ok(());
            }
            thread::sleep(STOP_POLL);
        }
    }
    force_stop(child)?;
    child
        .wait()
        .map_err(|error| CliError::Process("could not reap child process", error))?;
    Ok(())
}

fn reap(child: &mut Child) -> Result<bool, CliError> {
    let exited = child
        .try_wait()
        .map_err(|error| CliError::Process("could not poll child process", error))?
        .is_some();
    if exited {
        child
            .wait()
            .map_err(|error| CliError::Process("could not reap child process", error))?;
    }
    Ok(exited)
}

// Best effort stop request. Windows console applications usually refuse the
// non-forced `taskkill`, so the escalation below runs immediately for them;
// `GenerateConsoleCtrlEvent` would need `unsafe`, which this workspace forbids.
fn request_stop(pid: u32) -> bool {
    stop_request_command(pid)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(windows)]
fn stop_request_command(pid: u32) -> Command {
    let mut command = Command::new("taskkill");
    command.arg("/PID").arg(pid.to_string()).arg("/T");
    command
}

#[cfg(not(windows))]
fn stop_request_command(pid: u32) -> Command {
    let mut command = Command::new("kill");
    command.arg("-TERM").arg(pid.to_string());
    command
}

#[cfg(windows)]
fn force_stop(child: &mut Child) -> Result<(), CliError> {
    let terminated = Command::new("taskkill")
        .arg("/PID")
        .arg(child.id().to_string())
        .arg("/T")
        .arg("/F")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if terminated {
        return Ok(());
    }
    kill_direct(child)
}

#[cfg(not(windows))]
fn force_stop(child: &mut Child) -> Result<(), CliError> {
    kill_direct(child)
}

fn kill_direct(child: &mut Child) -> Result<(), CliError> {
    match child.kill() {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => Ok(()),
        Err(error) => Err(CliError::Process("could not stop child process", error)),
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TreeDigest {
    files: u64,
    hash: u64,
}

fn tree_digest(roots: &[PathBuf]) -> TreeDigest {
    let mut digest = TreeDigest::default();
    let mut visited = BTreeSet::new();
    for root in roots {
        accumulate_digest(root, &mut visited, &mut digest);
    }
    digest
}

fn accumulate_digest(path: &Path, visited: &mut BTreeSet<PathBuf>, digest: &mut TreeDigest) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if metadata.file_type().is_symlink() {
        return;
    }
    if metadata.is_file() {
        if watched_file(path) {
            let mut hasher = DefaultHasher::new();
            path.hash(&mut hasher);
            metadata.len().hash(&mut hasher);
            metadata.modified().ok().hash(&mut hasher);
            digest.files = digest.files.wrapping_add(1);
            digest.hash = digest.hash.wrapping_add(hasher.finish());
        }
        return;
    }
    if !visited.insert(path.to_owned()) {
        return;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let child = entry.path();
        if child
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| matches!(name, ".git" | "target" | "node_modules"))
        {
            continue;
        }
        accumulate_digest(&child, visited, digest);
    }
}

fn watched_file(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| matches!(name, "Cargo.toml" | "Cargo.lock" | "Blazingly.toml"))
        || path
            .extension()
            .and_then(OsStr::to_str)
            .is_some_and(|extension| {
                matches!(
                    extension,
                    "rs" | "toml" | "json" | "yaml" | "yml" | "html" | "css" | "js" | "ts" | "sql"
                )
            })
}

fn display_relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn exit_code(code: Option<i32>) -> u8 {
    code.and_then(|code| u8::try_from(code).ok()).unwrap_or(1)
}

#[derive(Debug)]
enum CliError {
    Usage(String),
    Discovery(String),
    Metadata(String),
    Config(PathBuf, String),
    Io(PathBuf, std::io::Error),
    Process(&'static str, std::io::Error),
    CommandFailed {
        command: String,
        status: Option<i32>,
        stderr: String,
    },
    Signal(String),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(message) | Self::Discovery(message) | Self::Metadata(message) => {
                formatter.write_str(message)
            }
            Self::Config(path, message) => {
                write!(formatter, "invalid {}: {message}", path.display())
            }
            Self::Io(path, error) => write!(formatter, "{}: {error}", path.display()),
            Self::Process(context, error) => write!(formatter, "{context}: {error}"),
            Self::CommandFailed {
                command,
                status,
                stderr,
            } => write!(
                formatter,
                "`{command}` failed with status {}{}",
                status.map_or_else(|| "unknown".to_owned(), |status| status.to_string()),
                if stderr.is_empty() {
                    String::new()
                } else {
                    format!(": {stderr}")
                }
            ),
            Self::Signal(message) => {
                write!(formatter, "could not install Ctrl-C handler: {message}")
            }
        }
    }
}

impl std::error::Error for CliError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata() -> CargoMetadata {
        CargoMetadata {
            workspace_root: PathBuf::from("/workspace"),
            target_directory: PathBuf::from("/workspace/target"),
            workspace_members: vec![
                "path+file:///workspace#api@0.1.0".to_owned(),
                "path+file:///workspace#helper@0.1.0".to_owned(),
            ],
            packages: vec![
                MetadataPackage {
                    id: "path+file:///workspace#api@0.1.0".to_owned(),
                    name: "api".to_owned(),
                    manifest_path: PathBuf::from("/workspace/api/Cargo.toml"),
                    dependencies: vec![
                        MetadataDependency {
                            name: "blazingly".to_owned(),
                            path: None,
                        },
                        MetadataDependency {
                            name: "helper".to_owned(),
                            path: Some(PathBuf::from("/workspace/helper")),
                        },
                        MetadataDependency {
                            name: "vendored".to_owned(),
                            path: Some(PathBuf::from("/elsewhere/vendored")),
                        },
                    ],
                    targets: vec![MetadataTarget {
                        name: "server".to_owned(),
                        kind: vec!["bin".to_owned()],
                        src_path: PathBuf::from("/workspace/api/src/main.rs"),
                    }],
                },
                MetadataPackage {
                    id: "path+file:///workspace#helper@0.1.0".to_owned(),
                    name: "helper".to_owned(),
                    manifest_path: PathBuf::from("/workspace/helper/Cargo.toml"),
                    dependencies: Vec::new(),
                    targets: vec![MetadataTarget {
                        name: "tool".to_owned(),
                        kind: vec!["bin".to_owned()],
                        src_path: PathBuf::from("/workspace/helper/src/main.rs"),
                    }],
                },
            ],
        }
    }

    fn application(kind: TargetKind) -> DiscoveredApp {
        DiscoveredApp {
            package: "api".to_owned(),
            target: TargetSelection {
                kind,
                name: "server".to_owned(),
            },
            manifest_path: PathBuf::from("/workspace/api/Cargo.toml"),
            source_path: PathBuf::from("/workspace/api/src/main.rs"),
            direct_blazingly_dependency: true,
        }
    }

    fn arguments(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn discovery_prefers_the_single_direct_framework_application() {
        let applications = discover_apps(&metadata());
        let selected = select_app(
            &applications,
            &FileConfig::default(),
            &CliOptions::default(),
        )
        .expect("single Blazingly app should be preferred");
        assert_eq!(selected.package, "api");
        assert_eq!(selected.target.name, "server");
    }

    #[test]
    fn arguments_keep_application_arguments_after_separator() {
        let options = CliOptions::parse(&arguments(&[
            "--package",
            "api",
            "--bin",
            "server",
            "--",
            "--tenant",
            "one",
        ]))
        .expect("arguments");
        assert_eq!(options.package.as_deref(), Some("api"));
        assert_eq!(options.app_arguments, ["--tenant", "one"]);
    }

    #[test]
    fn traceable_source_extensions_exclude_build_outputs() {
        assert!(watched_file(Path::new("src/main.rs")));
        assert!(watched_file(Path::new("templates/index.html")));
        assert!(!watched_file(Path::new("target/debug/api.exe")));
    }

    #[test]
    fn feature_options_merge_configuration_and_command_line() {
        let metadata = metadata();
        let config = FileConfig {
            app: AppConfig {
                features: vec!["native".to_owned()],
                ..AppConfig::default()
            },
            ..FileConfig::default()
        };
        let options = CliOptions::parse(&arguments(&[
            "--features",
            "native,tracing",
            "-F",
            "extra",
            "--no-default-features",
        ]))
        .expect("arguments");
        let session = Session {
            metadata: &metadata,
            config: &config,
            options: &options,
            app: application(TargetKind::Bin),
        };
        let command = session.cargo("build", true);
        let forwarded = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(forwarded.contains(&"--release".to_owned()));
        assert!(forwarded.contains(&"--no-default-features".to_owned()));
        assert!(forwarded.contains(&"--features".to_owned()));
        assert!(forwarded.contains(&"native,tracing,extra".to_owned()));
        assert!(!forwarded.contains(&"--all-features".to_owned()));
    }

    #[test]
    fn all_features_flag_reaches_cargo() {
        let metadata = metadata();
        let config = FileConfig::default();
        let options = CliOptions::parse(&arguments(&["--all-features"])).expect("arguments");
        let session = Session {
            metadata: &metadata,
            config: &config,
            options: &options,
            app: application(TargetKind::Bin),
        };
        let forwarded = session
            .cargo("check", false)
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(forwarded.first().map(String::as_str), Some("check"));
        assert!(forwarded.contains(&"--all-features".to_owned()));
    }

    #[test]
    fn address_option_sets_the_documented_listen_variable() {
        let metadata = metadata();
        let config = FileConfig::default();
        let options =
            CliOptions::parse(&arguments(&["--address", "127.0.0.1:8000"])).expect("arguments");
        let session = Session {
            metadata: &metadata,
            config: &config,
            options: &options,
            app: application(TargetKind::Bin),
        };
        let command = session.app_command(Path::new("/workspace/target/debug/server"));
        let variables = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect::<Vec<_>>();
        assert!(variables.iter().any(|(key, value)| {
            key == ADDRESS_VARIABLE && value.as_deref() == Some("127.0.0.1:8000")
        }));
    }

    #[test]
    fn emit_runs_set_the_introspection_variables() {
        let metadata = metadata();
        let config = FileConfig::default();
        let options =
            CliOptions::parse(&arguments(&["--address", "127.0.0.1:8000"])).expect("arguments");
        let session = Session {
            metadata: &metadata,
            config: &config,
            options: &options,
            app: application(TargetKind::Bin),
        };
        let command = session.emit_command(Path::new("/workspace/target/debug/server"), "openapi");
        let variables = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect::<Vec<_>>();
        let value_of = |name: &str| {
            variables
                .iter()
                .find(|(key, _)| key == name)
                .and_then(|(_, value)| value.clone())
        };
        assert_eq!(value_of(EMIT_VARIABLE).as_deref(), Some("openapi"));
        // An emit run must never race a serving instance for the listen port.
        assert_eq!(value_of(ADDRESS_VARIABLE).as_deref(), Some(EMIT_ADDRESS));
        assert_eq!(value_of(WORKERS_VARIABLE).as_deref(), Some("1"));
    }

    #[test]
    fn generated_projects_default_to_the_matching_registry_release() {
        let dependency = framework_dependency(None).expect("registry dependency");
        assert!(dependency.contains(&format!("version = \"{FRAMEWORK_VERSION}\"")));
        assert!(dependency.contains("features = [\"native\"]"));
        // A CLI and the project it scaffolds must not disagree about the
        // framework version.
        assert_eq!(FRAMEWORK_VERSION, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn framework_path_resolves_the_workspace_checkout() {
        let root =
            env::temp_dir().join(format!("cargo-blazingly-framework-{}", std::process::id()));
        let crate_directory = root.join("crates").join("blazingly");
        fs::create_dir_all(&crate_directory).expect("temporary directory");
        fs::write(
            crate_directory.join("Cargo.toml"),
            "[package]\nname = \"blazingly\"\n",
        )
        .expect("manifest");
        let dependency = framework_dependency(Some(&root)).expect("path dependency");
        assert!(dependency.starts_with("{ path = \""));
        assert!(dependency.contains("crates/blazingly"));
        assert!(dependency.contains("features = [\"native\"]"));
        assert!(!dependency.contains('\\'));
        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn generated_project_annotates_the_registry_dependency() {
        let registry = framework_dependency(None).expect("registry dependency");
        let files = project_files("demo", &registry, true);
        let cargo_toml = files.get("Cargo.toml").expect("manifest");
        assert!(cargo_toml.contains("name = \"demo\""));
        assert!(cargo_toml.contains(&registry_dependency_note()));
        assert!(cargo_toml.contains(FRAMEWORK_GIT_URL));
        assert!(cargo_toml.contains(&format!("blazingly = {registry}")));
        assert!(
            files.get("src/main.rs").is_some_and(|main| {
                main.contains("MulticoreServer") && main.contains("#[get(")
            })
        );
        assert!(
            files
                .get(".gitignore")
                .is_some_and(|ignore| ignore.contains("/target"))
        );
    }

    #[test]
    fn path_projects_keep_the_dependency_unannotated() {
        let files = project_files("demo", "{ path = \"../blazingly\" }", false);
        let cargo_toml = files.get("Cargo.toml").expect("manifest");
        assert!(!cargo_toml.contains(&registry_dependency_note()));
        assert!(cargo_toml.contains("blazingly = { path = \"../blazingly\" }"));
    }

    #[test]
    fn package_names_reject_cargo_incompatible_input() {
        assert!(validate_package_name("api").is_ok());
        assert!(validate_package_name("users_api-2").is_ok());
        assert!(validate_package_name("_internal").is_ok());
        assert!(validate_package_name("").is_err());
        assert!(validate_package_name("1api").is_err());
        assert!(validate_package_name("api server").is_err());
        assert!(validate_package_name("api/server").is_err());
    }

    #[test]
    fn artifact_messages_locate_the_selected_executable() {
        let metadata = metadata();
        let config = FileConfig::default();
        let options = CliOptions::default();
        let session = Session {
            metadata: &metadata,
            config: &config,
            options: &options,
            app: application(TargetKind::Bin),
        };
        let messages = concat!(
            "{\"reason\":\"compiler-artifact\",\"target\":{\"name\":\"helper\"},\
             \"executable\":\"/workspace/target/debug/helper\"}\n",
            "{\"reason\":\"compiler-artifact\",\"target\":{\"name\":\"server\"},\
             \"executable\":null}\n",
            "{\"reason\":\"compiler-artifact\",\"target\":{\"name\":\"server\"},\
             \"executable\":\"/workspace/target/debug/server\"}\n",
            "{\"reason\":\"build-finished\",\"success\":true}\n",
        );
        assert_eq!(
            session.artifact_path(messages),
            Some(PathBuf::from("/workspace/target/debug/server"))
        );
        assert_eq!(session.artifact_path("not json\n"), None);
    }

    #[test]
    fn binary_paths_follow_the_profile_and_target_kind() {
        let metadata = metadata();
        let config = FileConfig::default();
        let options = CliOptions::default();
        let session = Session {
            metadata: &metadata,
            config: &config,
            options: &options,
            app: application(TargetKind::Bin),
        };
        assert_eq!(
            session.binary_path(true),
            PathBuf::from(format!(
                "/workspace/target/release/server{}",
                env::consts::EXE_SUFFIX
            ))
        );
        let example = Session {
            metadata: &metadata,
            config: &config,
            options: &options,
            app: application(TargetKind::Example),
        };
        assert_eq!(
            example.binary_path(false),
            PathBuf::from(format!(
                "/workspace/target/debug/examples/server{}",
                env::consts::EXE_SUFFIX
            ))
        );
    }

    #[test]
    fn staging_runs_a_copy_of_the_cargo_artifact() {
        let root = env::temp_dir().join(format!("cargo-blazingly-stage-{}", std::process::id()));
        let output = root.join("target").join("debug");
        fs::create_dir_all(&output).expect("temporary directory");
        let artifact = output.join("server");
        fs::write(&artifact, b"first").expect("write artifact");
        let metadata = CargoMetadata {
            target_directory: root.join("target"),
            ..metadata()
        };
        let config = FileConfig::default();
        let options = CliOptions::default();
        let session = Session {
            metadata: &metadata,
            config: &config,
            options: &options,
            app: application(TargetKind::Bin),
        };
        let staged = session.stage_binary(&artifact).expect("stage artifact");
        assert_eq!(
            staged,
            root.join("target").join(STAGE_DIRECTORY).join("server")
        );
        assert_eq!(fs::read(&staged).expect("read staged"), b"first");
        fs::write(&artifact, b"second").expect("rewrite artifact");
        assert_eq!(
            fs::read(session.stage_binary(&artifact).expect("restage")).expect("read staged"),
            b"second"
        );
        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn watch_roots_cover_workspace_path_dependencies() {
        let metadata = metadata();
        let config = FileConfig::default();
        let options = CliOptions::default();
        let session = Session {
            metadata: &metadata,
            config: &config,
            options: &options,
            app: application(TargetKind::Bin),
        };
        let roots = session.watch_roots();
        assert!(roots.contains(&PathBuf::from("/workspace/api/src")));
        assert!(roots.contains(&PathBuf::from("/workspace/api/Cargo.toml")));
        assert!(roots.contains(&PathBuf::from("/workspace/helper/src")));
        assert!(roots.contains(&PathBuf::from("/workspace/helper/Cargo.toml")));
        assert!(roots.contains(&PathBuf::from("/workspace/Cargo.lock")));
        assert!(!roots.contains(&PathBuf::from("/elsewhere/vendored/src")));
    }

    #[test]
    fn quiet_period_waits_for_changes_to_settle() {
        let start = Instant::now();
        let mut quiet = QuietPeriod::new(Duration::from_millis(400));
        assert!(!quiet.observe(true, start));
        assert!(!quiet.observe(false, start + Duration::from_millis(300)));
        assert!(!quiet.observe(true, start + Duration::from_millis(350)));
        assert!(!quiet.observe(false, start + Duration::from_millis(600)));
        assert!(quiet.observe(false, start + Duration::from_millis(800)));
        assert!(!quiet.observe(false, start + Duration::from_millis(1600)));
    }

    #[test]
    fn tree_digest_tracks_watched_files() {
        let root = env::temp_dir().join(format!("cargo-blazingly-{}", std::process::id()));
        let source = root.join("src");
        fs::create_dir_all(&source).expect("temporary directory");
        let file = source.join("main.rs");
        fs::write(&file, b"fn main() {}").expect("write source");
        let roots = vec![root.clone()];
        let first = tree_digest(&roots);
        assert_eq!(first.files, 1);
        assert_eq!(tree_digest(&roots), first);
        fs::write(&file, b"fn main() { println!(); }").expect("rewrite source");
        assert_ne!(tree_digest(&roots), first);
        fs::write(source.join("notes.txt"), b"ignored").expect("write ignored file");
        assert_eq!(tree_digest(&roots).files, 1);
        fs::remove_dir_all(&root).expect("cleanup");
    }
}
