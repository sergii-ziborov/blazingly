#![forbid(unsafe_code)]
#![doc = include_str!("../README.md")]

// Re-exported, not merely used: the generated loader names it, and a settings
// type declares the same bounds an API model does.
pub use blazingly_contract::ValidationRule;

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

/// Where a settings type reads its values from.
///
/// The process environment is one implementation, not the definition. Reading
/// it is a global side effect, and `std::env::set_var` is unsafe in Rust 2024
/// — which this workspace forbids — so a settings type that could only read the
/// real environment would be a settings type nobody could test. Every loader
/// therefore takes a source.
pub trait ConfigSource {
    fn get(&self, key: &str) -> Option<String>;
}

/// Reads the process environment.
#[derive(Clone, Copy, Debug, Default)]
pub struct Environment;

impl ConfigSource for Environment {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

/// A fixed set of values, for tests and for defaults layered under one.
#[derive(Clone, Debug, Default)]
pub struct MapSource {
    values: BTreeMap<String, String>,
}

impl MapSource {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.values.insert(key.into(), value.into());
        self
    }
}

impl<K: Into<String>, V: Into<String>> FromIterator<(K, V)> for MapSource {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(entries: I) -> Self {
        Self {
            values: entries
                .into_iter()
                .map(|(key, value)| (key.into(), value.into()))
                .collect(),
        }
    }
}

impl ConfigSource for MapSource {
    fn get(&self, key: &str) -> Option<String> {
        self.values.get(key).cloned()
    }
}

/// Reads from the first source that has the key.
pub struct Layered<'sources> {
    sources: Vec<&'sources dyn ConfigSource>,
}

impl<'sources> Layered<'sources> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            sources: Vec::new(),
        }
    }

    /// Adds a source below the ones already added, so earlier layers win.
    #[must_use]
    pub fn under(mut self, source: &'sources dyn ConfigSource) -> Self {
        self.sources.push(source);
        self
    }
}

impl Default for Layered<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigSource for Layered<'_> {
    fn get(&self, key: &str) -> Option<String> {
        self.sources.iter().find_map(|source| source.get(key))
    }
}

/// What went wrong with one setting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigProblem {
    /// The variable is not set and the field has no default.
    Missing,
    /// The value is set but is not a `T`.
    Unparsable {
        value: String,
        expected: &'static str,
    },
    /// The value parsed but broke a declared rule.
    Invalid { value: String, rule: String },
}

impl fmt::Display for ConfigProblem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => formatter.write_str("is not set and has no default"),
            Self::Unparsable { value, expected } => {
                write!(formatter, "is `{value}`, which is not a valid {expected}")
            }
            Self::Invalid { value, rule } => write!(formatter, "is `{value}`, which {rule}"),
        }
    }
}

/// Everything wrong with the configuration, not the first thing wrong with it.
///
/// A container started with three variables missing should learn all three from
/// one failed boot rather than three.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConfigError {
    problems: Vec<(String, ConfigProblem)>,
}

impl ConfigError {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[doc(hidden)]
    pub fn push(&mut self, variable: impl Into<String>, problem: ConfigProblem) {
        self.problems.push((variable.into(), problem));
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.problems.is_empty()
    }

    /// Every problem, as `(variable, problem)`.
    #[must_use]
    pub fn problems(&self) -> &[(String, ConfigProblem)] {
        &self.problems
    }

    /// The variables that were not set at all — what a deployment checklist
    /// wants, separate from the ones that were set wrong.
    pub fn missing(&self) -> impl Iterator<Item = &str> {
        self.problems.iter().filter_map(|(variable, problem)| {
            matches!(problem, ConfigProblem::Missing).then_some(variable.as_str())
        })
    }

    #[doc(hidden)]
    pub fn into_result<T>(self, value: T) -> Result<T, Self> {
        if self.is_empty() {
            Ok(value)
        } else {
            Err(self)
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            formatter,
            "{} setting(s) could not be read:",
            self.problems.len()
        )?;
        for (variable, problem) in &self.problems {
            writeln!(formatter, "  - {variable} {problem}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ConfigError {}

/// A type whose values come from configuration.
///
/// Implemented by `#[settings]`; implementing it by hand is supported but the
/// derive is what keeps the variable names and the field names from drifting.
pub trait Settings: Sized {
    /// Reads every field, collecting every problem before failing.
    ///
    /// # Errors
    ///
    /// Returns every variable that is missing, unparsable, or breaks a rule.
    fn load(source: &dyn ConfigSource) -> Result<Self, ConfigError>;

    /// Reads from the process environment.
    ///
    /// # Errors
    ///
    /// As [`Settings::load`].
    fn from_env() -> Result<Self, ConfigError> {
        Self::load(&Environment)
    }

    /// Every variable this type reads, in declaration order.
    ///
    /// This is what makes a settings type documentable: a deployment can list
    /// what a service expects without running it.
    fn variables() -> Vec<SettingDescriptor>;
}

/// One variable a settings type reads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SettingDescriptor {
    pub variable: String,
    pub field: &'static str,
    pub required: bool,
    pub has_default: bool,
    pub rules: Vec<ValidationRule>,
}

/// A value that can be read from one configuration string.
///
/// Blanket-implemented for everything with a `FromStr`, which covers the
/// numbers, the addresses, and the paths. `bool` and `Vec<T>` are specialised
/// because configuration spells them differently than Rust does.
pub trait FromConfigValue: Sized {
    /// What to call this type when a value does not parse as one.
    const EXPECTED: &'static str;

    /// # Errors
    ///
    /// Returns [`ValueError`], which carries nothing: the caller already has
    /// the variable name, the raw value, and `EXPECTED`, which is the whole
    /// message.
    fn from_config_value(value: &str) -> Result<Self, ValueError>;
}

/// A configuration value that is not what its field needs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ValueError;

impl fmt::Display for ValueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the value is not of the expected type")
    }
}

impl std::error::Error for ValueError {}

macro_rules! from_str_config_value {
    ($($type:ty => $expected:literal),* $(,)?) => {
        $(impl FromConfigValue for $type {
            const EXPECTED: &'static str = $expected;

            fn from_config_value(value: &str) -> Result<Self, ValueError> {
                <$type as FromStr>::from_str(value.trim()).map_err(|_| ValueError)
            }
        })*
    };
}

from_str_config_value! {
    i8 => "8-bit signed integer",
    i16 => "16-bit signed integer",
    i32 => "32-bit signed integer",
    i64 => "64-bit signed integer",
    i128 => "128-bit signed integer",
    isize => "signed integer",
    u8 => "8-bit unsigned integer",
    u16 => "16-bit unsigned integer",
    u32 => "32-bit unsigned integer",
    u64 => "64-bit unsigned integer",
    u128 => "128-bit unsigned integer",
    usize => "unsigned integer",
    f32 => "number",
    f64 => "number",
    std::num::NonZeroUsize => "positive integer",
    std::num::NonZeroU16 => "positive 16-bit integer",
    std::num::NonZeroU32 => "positive 32-bit integer",
    std::num::NonZeroU64 => "positive 64-bit integer",
    std::net::IpAddr => "IP address",
    std::net::SocketAddr => "socket address",
    std::path::PathBuf => "path",
    char => "single character",
}

impl FromConfigValue for String {
    const EXPECTED: &'static str = "string";

    fn from_config_value(value: &str) -> Result<Self, ValueError> {
        Ok(value.to_owned())
    }
}

impl FromConfigValue for bool {
    const EXPECTED: &'static str = "boolean (`true`/`false`, `1`/`0`, `yes`/`no`, `on`/`off`)";

    /// Configuration spells booleans more ways than Rust does, and a deployment
    /// that writes `ENABLED=1` should not be told that is not a boolean.
    fn from_config_value(value: &str) -> Result<Self, ValueError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "y" | "on" => Ok(true),
            "false" | "0" | "no" | "n" | "off" => Ok(false),
            _ => Err(ValueError),
        }
    }
}

impl FromConfigValue for std::time::Duration {
    const EXPECTED: &'static str = "duration (`30s`, `5m`, `2h`, `100ms`, or seconds)";

    fn from_config_value(value: &str) -> Result<Self, ValueError> {
        let value = value.trim();
        let (digits, multiplier_ms) = match value {
            _ if value.ends_with("ms") => (&value[..value.len() - 2], 1),
            _ if value.ends_with('s') => (&value[..value.len() - 1], 1_000),
            _ if value.ends_with('m') => (&value[..value.len() - 1], 60_000),
            _ if value.ends_with('h') => (&value[..value.len() - 1], 3_600_000),
            // A bare number is seconds, which is what most deployments mean.
            _ => (value, 1_000),
        };
        let amount = digits.trim().parse::<u64>().map_err(|_| ValueError)?;
        amount
            .checked_mul(multiplier_ms)
            .map(Self::from_millis)
            .ok_or(ValueError)
    }
}

impl<T: FromConfigValue> FromConfigValue for Vec<T> {
    const EXPECTED: &'static str = T::EXPECTED;

    /// Comma-separated, with surrounding whitespace trimmed and an empty string
    /// reading as an empty list rather than as one empty element.
    fn from_config_value(value: &str) -> Result<Self, ValueError> {
        let value = value.trim();
        if value.is_empty() {
            return Ok(Self::new());
        }
        value
            .split(',')
            .map(|entry| T::from_config_value(entry.trim()))
            .collect()
    }
}

#[doc(hidden)]
pub mod __private {
    use super::{ConfigError, ConfigProblem, ConfigSource, FromConfigValue};

    /// Parses one raw value, recording why it did not parse.
    fn parse<T: FromConfigValue>(
        raw: String,
        variable: &str,
        errors: &mut ConfigError,
    ) -> Option<T> {
        let Ok(value) = T::from_config_value(&raw) else {
            errors.push(
                variable,
                ConfigProblem::Unparsable {
                    value: raw,
                    expected: T::EXPECTED,
                },
            );
            return None;
        };
        Some(value)
    }

    /// Reads one required field, recording the problem instead of returning it
    /// so the caller can go on and find the rest.
    pub fn read<T: FromConfigValue>(
        source: &dyn ConfigSource,
        variable: &str,
        default: Option<&str>,
        errors: &mut ConfigError,
    ) -> Option<T> {
        let Some(raw) = source.get(variable).or_else(|| default.map(str::to_owned)) else {
            errors.push(variable, ConfigProblem::Missing);
            return None;
        };
        parse(raw, variable, errors)
    }

    /// Reads one optional field. An unset variable is `None`; a set variable
    /// that does not parse is still an error, because silently discarding a
    /// value someone deliberately wrote is the failure this crate exists to
    /// prevent.
    pub fn read_optional<T: FromConfigValue>(
        source: &dyn ConfigSource,
        variable: &str,
        errors: &mut ConfigError,
    ) -> Option<T> {
        parse(source.get(variable)?, variable, errors)
    }

    pub fn check_min_length(value: &str, minimum: usize, variable: &str, errors: &mut ConfigError) {
        if value.chars().count() < minimum {
            errors.push(
                variable,
                ConfigProblem::Invalid {
                    value: value.to_owned(),
                    rule: format!("is shorter than the required {minimum} characters"),
                },
            );
        }
    }

    pub fn check_max_length(value: &str, maximum: usize, variable: &str, errors: &mut ConfigError) {
        if value.chars().count() > maximum {
            errors.push(
                variable,
                ConfigProblem::Invalid {
                    value: value.to_owned(),
                    rule: format!("is longer than the permitted {maximum} characters"),
                },
            );
        }
    }
}
