#![forbid(unsafe_code)]

//! Strong string-like API values comparable to common Pydantic field types,
//! plus the runtime checks `#[api_model]` emits for declarative field rules.

mod cache;
mod checks;
mod matcher;
mod paths;

pub use cache::PATTERN_CACHE_CAPACITY;
pub use checks::{
    Constraint, IntoModelViolations, ModelViolation, NumericField, NumericValue,
    UNIQUE_ITEMS_SCAN_LIMIT, check_exclusive_maximum, check_exclusive_minimum, check_max_items,
    check_maximum, check_min_items, check_minimum, check_multiple_of, check_pattern,
    check_unique_items, merge_model_violations, parse_numeric_value,
};
pub use matcher::{
    MAX_CLASS_MEMBERS, MAX_PATTERN_CHARS, MAX_PATTERN_DEPTH, MAX_PATTERN_INSTRUCTIONS, Pattern,
    PatternError, matches_pattern,
};
pub use paths::{DECODE_VIOLATION_CODE, decode_violations, normalize_field_path};

use blazingly_core::{ApiSchema, SchemaKind, TypeDescriptor};
use rust_decimal::Decimal as RustDecimal;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::net::IpAddr;
use std::ops::Deref;
use std::str::FromStr;
use time::format_description::well_known::Rfc3339;
use time::{Date as TimeDate, OffsetDateTime};
use url::Url as ParsedUrl;
use uuid::Uuid as ParsedUuid;

macro_rules! string_value {
    (
        $(#[$meta:meta])*
        $name:ident,
        $inner:ty,
        $schema_name:literal,
        $parse:expr
    ) => {
        $(#[$meta])*
        #[derive(Clone, Debug, Eq, Hash, PartialEq)]
        pub struct $name($inner);

        impl $name {
            #[must_use]
            pub const fn as_inner(&self) -> &$inner {
                &self.0
            }

            #[must_use]
            pub fn into_inner(self) -> $inner {
                self.0
            }
        }

        impl FromStr for $name {
            type Err = ValidationValueError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                ($parse)(value)
                    .map(Self)
                    .map_err(|message| ValidationValueError::new($schema_name, message))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl Deref for $name {
            type Target = $inner;

            fn deref(&self) -> &Self::Target {
                &self.0
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                value.parse().map_err(serde::de::Error::custom)
            }
        }

        impl ApiSchema for $name {
            fn type_descriptor() -> TypeDescriptor {
                TypeDescriptor::scalar($schema_name, SchemaKind::String)
            }
        }
    };
}

string_value!(
    /// RFC 9562 UUID value.
    Uuid,
    ParsedUuid,
    "Uuid",
    |value: &str| ParsedUuid::parse_str(value).map_err(|error| error.to_string())
);

string_value!(
    /// Absolute URL with a parsed scheme, authority, path, query, and fragment.
    Url,
    ParsedUrl,
    "Url",
    |value: &str| ParsedUrl::parse(value).map_err(|error| error.to_string())
);

string_value!(
    /// IPv4 or IPv6 address.
    IpAddress,
    IpAddr,
    "IpAddress",
    |value: &str| value.parse::<IpAddr>().map_err(|error| error.to_string())
);

string_value!(
    /// Calendar date encoded as `YYYY-MM-DD`.
    Date,
    TimeDate,
    "Date",
    |value: &str| TimeDate::parse(
        value,
        time::macros::format_description!("[year]-[month]-[day]")
    )
    .map_err(|error| error.to_string())
);

string_value!(
    /// RFC 3339 date and time with an explicit UTC offset.
    DateTime,
    OffsetDateTime,
    "DateTime",
    |value: &str| OffsetDateTime::parse(value, &Rfc3339).map_err(|error| error.to_string())
);

string_value!(
    /// Arbitrary precision fixed-point decimal serialized without float loss.
    Decimal,
    RustDecimal,
    "Decimal",
    |value: &str| RustDecimal::from_str(value).map_err(|error| error.to_string())
);

/// Stable deserialization failure for a strong API value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidationValueError {
    kind: &'static str,
    message: String,
}

impl ValidationValueError {
    #[must_use]
    pub fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> &'static str {
        self.kind
    }
}

impl fmt::Display for ValidationValueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid {}: {}", self.kind, self.message)
    }
}

impl std::error::Error for ValidationValueError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strong_values_reject_invalid_strings_and_round_trip_json() {
        let uuid: Uuid = "550e8400-e29b-41d4-a716-446655440000"
            .parse()
            .expect("UUID");
        assert_eq!(uuid.to_string(), "550e8400-e29b-41d4-a716-446655440000");
        assert!("not a URL".parse::<Url>().is_err());
        assert!("2025-02-30".parse::<Date>().is_err());
        assert!("999.2500".parse::<Decimal>().is_ok());
    }
}
