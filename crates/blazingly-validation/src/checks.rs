//! Runtime helpers emitted by `#[api_model]` for declarative field constraints.

use crate::matcher::Pattern;
use blazingly_core::{FieldViolation, ValidationErrors};
use core::cmp::Ordering;
use core::fmt;
use serde_json::{Value, json};

/// Largest collection length for which uniqueness is verified in place.
///
/// Longer collections are rejected outright so an attacker-supplied array
/// cannot force a quadratic comparison scan.
pub const UNIQUE_ITEMS_SCAN_LIMIT: usize = 1024;

/// A numeric bound or field value normalized for constraint comparison.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NumericValue {
    /// An exact integral value.
    Integer(i128),
    /// A binary floating-point value.
    Float(f64),
}

impl NumericValue {
    /// Widens the value to `f64` for mixed integer and float comparison.
    // Mixed integer and float comparison follows JSON Schema's double semantics.
    #[allow(clippy::cast_precision_loss)]
    #[must_use]
    pub fn as_f64(self) -> f64 {
        match self {
            Self::Integer(value) => value as f64,
            Self::Float(value) => value,
        }
    }

    fn compare(self, other: Self) -> Option<Ordering> {
        match (self, other) {
            (Self::Integer(left), Self::Integer(right)) => Some(left.cmp(&right)),
            _ => self.as_f64().partial_cmp(&other.as_f64()),
        }
    }

    fn is_multiple_of(self, factor: Self) -> bool {
        if let (Self::Integer(value), Self::Integer(factor)) = (self, factor) {
            return factor != 0 && !(value == i128::MIN && factor == -1) && value % factor == 0;
        }
        let factor = factor.as_f64();
        let value = self.as_f64();
        if !factor.is_normal() || !value.is_finite() {
            return false;
        }
        let quotient = value / factor;
        let tolerance = f64::EPSILON * 8.0 * quotient.abs().max(1.0);
        (quotient - quotient.round()).abs() <= tolerance
    }

    fn as_json(self) -> Value {
        match self {
            Self::Integer(value) => {
                i64::try_from(value).map_or_else(|_| json!(self.as_f64()), |value| json!(value))
            }
            Self::Float(value) => json!(value),
        }
    }
}

impl fmt::Display for NumericValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Integer(value) => write!(formatter, "{value}"),
            Self::Float(value) if value.fract() == 0.0 && value.is_finite() => {
                write!(formatter, "{value:.1}")
            }
            Self::Float(value) => write!(formatter, "{value}"),
        }
    }
}

/// Parses the canonical textual encoding of a numeric bound.
#[must_use]
pub fn parse_numeric_value(encoded: &str) -> Option<NumericValue> {
    if encoded.contains(['.', 'e', 'E', 'n', 'i']) {
        let value = encoded.parse::<f64>().ok()?;
        return value.is_finite().then_some(NumericValue::Float(value));
    }
    encoded.parse::<i128>().ok().map(NumericValue::Integer)
}

/// Numeric field types that carry declarative range constraints.
pub trait NumericField: Copy {
    /// Normalizes the value for constraint comparison.
    fn numeric_value(self) -> NumericValue;
}

macro_rules! integer_field {
    ($($ty:ty),* $(,)?) => {
        $(
            impl NumericField for $ty {
                fn numeric_value(self) -> NumericValue {
                    NumericValue::Integer(i128::from(self))
                }
            }
        )*
    };
}

integer_field!(i8, i16, i32, i64, i128, u8, u16, u32, u64);

impl NumericField for isize {
    fn numeric_value(self) -> NumericValue {
        NumericValue::Integer(self as i128)
    }
}

impl NumericField for usize {
    fn numeric_value(self) -> NumericValue {
        NumericValue::Integer(self as i128)
    }
}

impl NumericField for u128 {
    fn numeric_value(self) -> NumericValue {
        NumericValue::Integer(i128::try_from(self).unwrap_or(i128::MAX))
    }
}

impl NumericField for f32 {
    fn numeric_value(self) -> NumericValue {
        NumericValue::Float(f64::from(self))
    }
}

impl NumericField for f64 {
    fn numeric_value(self) -> NumericValue {
        NumericValue::Float(self)
    }
}

/// Records a `minimum` violation when the value is below the inclusive bound.
pub fn check_minimum<T: NumericField>(
    errors: &mut ValidationErrors,
    field: &str,
    value: T,
    bound: NumericValue,
) {
    if matches!(
        value.numeric_value().compare(bound),
        Some(Ordering::Less) | None
    ) {
        errors.push(
            field,
            "minimum",
            format!("must be greater than or equal to {bound}"),
        );
    }
}

/// Records a `maximum` violation when the value is above the inclusive bound.
pub fn check_maximum<T: NumericField>(
    errors: &mut ValidationErrors,
    field: &str,
    value: T,
    bound: NumericValue,
) {
    if matches!(
        value.numeric_value().compare(bound),
        Some(Ordering::Greater) | None
    ) {
        errors.push(
            field,
            "maximum",
            format!("must be less than or equal to {bound}"),
        );
    }
}

/// Records an `exclusive_minimum` violation when the value reaches the bound.
pub fn check_exclusive_minimum<T: NumericField>(
    errors: &mut ValidationErrors,
    field: &str,
    value: T,
    bound: NumericValue,
) {
    if !matches!(
        value.numeric_value().compare(bound),
        Some(Ordering::Greater)
    ) {
        errors.push(
            field,
            "exclusive_minimum",
            format!("must be greater than {bound}"),
        );
    }
}

/// Records an `exclusive_maximum` violation when the value reaches the bound.
pub fn check_exclusive_maximum<T: NumericField>(
    errors: &mut ValidationErrors,
    field: &str,
    value: T,
    bound: NumericValue,
) {
    if !matches!(value.numeric_value().compare(bound), Some(Ordering::Less)) {
        errors.push(
            field,
            "exclusive_maximum",
            format!("must be less than {bound}"),
        );
    }
}

/// Records a `multiple_of` violation when the value is not an exact multiple.
pub fn check_multiple_of<T: NumericField>(
    errors: &mut ValidationErrors,
    field: &str,
    value: T,
    factor: NumericValue,
) {
    if !value.numeric_value().is_multiple_of(factor) {
        errors.push(
            field,
            "multiple_of",
            format!("must be a multiple of {factor}"),
        );
    }
}

/// Records a `min_items` violation when the collection is too short.
pub fn check_min_items<T>(
    errors: &mut ValidationErrors,
    field: &str,
    values: &[T],
    minimum: usize,
) {
    if values.len() < minimum {
        errors.push(
            field,
            "min_items",
            format!("must contain at least {minimum} items"),
        );
    }
}

/// Records a `max_items` violation when the collection is too long.
pub fn check_max_items<T>(
    errors: &mut ValidationErrors,
    field: &str,
    values: &[T],
    maximum: usize,
) {
    if values.len() > maximum {
        errors.push(
            field,
            "max_items",
            format!("must contain at most {maximum} items"),
        );
    }
}

/// Records a `unique_items` violation when the collection repeats an element.
///
/// Collections longer than [`UNIQUE_ITEMS_SCAN_LIMIT`] are rejected without
/// being scanned, which keeps the check linear in the accepted input size.
pub fn check_unique_items<T: PartialEq>(errors: &mut ValidationErrors, field: &str, values: &[T]) {
    if values.len() > UNIQUE_ITEMS_SCAN_LIMIT {
        errors.push(
            field,
            "unique_items",
            format!(
                "must contain at most {UNIQUE_ITEMS_SCAN_LIMIT} items to be checked for uniqueness"
            ),
        );
        return;
    }
    for (index, value) in values.iter().enumerate() {
        if values.iter().skip(index + 1).any(|other| other == value) {
            errors.push(field, "unique_items", "must not contain duplicate items");
            return;
        }
    }
}

/// Records a `pattern` violation when the value does not satisfy the pattern.
pub fn check_pattern(errors: &mut ValidationErrors, field: &str, value: &str, pattern: &str) {
    match Pattern::compile(pattern) {
        Ok(compiled) => {
            if !compiled.matches(value) {
                errors.push(
                    field,
                    "pattern",
                    format!("must match the pattern {pattern}"),
                );
            }
        }
        Err(error) => errors.push(
            field,
            "pattern",
            format!("the declared pattern {pattern} is unusable: {error}"),
        ),
    }
}

/// A declarative constraint carried inside `ValidationRule::Custom`.
///
/// `#[api_model]` encodes constraints that predate a dedicated contract variant
/// as `keyword=value` strings so schema projections can recover them.
#[derive(Clone, Debug, PartialEq)]
pub enum Constraint {
    /// Inclusive lower bound for a numeric field.
    Minimum(NumericValue),
    /// Inclusive upper bound for a numeric field.
    Maximum(NumericValue),
    /// Exclusive lower bound for a numeric field.
    ExclusiveMinimum(NumericValue),
    /// Exclusive upper bound for a numeric field.
    ExclusiveMaximum(NumericValue),
    /// Required divisor for a numeric field.
    MultipleOf(NumericValue),
    /// Required pattern for a string field.
    Pattern(String),
    /// Minimum accepted collection length.
    MinItems(usize),
    /// Maximum accepted collection length.
    MaxItems(usize),
    /// Requires every collection element to be distinct.
    UniqueItems,
}

impl Constraint {
    /// Parses the canonical `keyword=value` encoding emitted by `#[api_model]`.
    #[must_use]
    pub fn parse(encoded: &str) -> Option<Self> {
        let (keyword, value) = encoded.split_once('=')?;
        let constraint = match keyword {
            "minimum" => Self::Minimum(parse_numeric_value(value)?),
            "maximum" => Self::Maximum(parse_numeric_value(value)?),
            "exclusive_minimum" => Self::ExclusiveMinimum(parse_numeric_value(value)?),
            "exclusive_maximum" => Self::ExclusiveMaximum(parse_numeric_value(value)?),
            "multiple_of" => Self::MultipleOf(parse_numeric_value(value)?),
            "pattern" => Self::Pattern(value.to_owned()),
            "min_items" => Self::MinItems(value.parse().ok()?),
            "max_items" => Self::MaxItems(value.parse().ok()?),
            "unique_items" if value == "true" => Self::UniqueItems,
            _ => return None,
        };
        Some(constraint)
    }

    /// JSON Schema keyword this constraint projects to.
    #[must_use]
    pub const fn keyword(&self) -> &'static str {
        match self {
            Self::Minimum(_) => "minimum",
            Self::Maximum(_) => "maximum",
            Self::ExclusiveMinimum(_) => "exclusiveMinimum",
            Self::ExclusiveMaximum(_) => "exclusiveMaximum",
            Self::MultipleOf(_) => "multipleOf",
            Self::Pattern(_) => "pattern",
            Self::MinItems(_) => "minItems",
            Self::MaxItems(_) => "maxItems",
            Self::UniqueItems => "uniqueItems",
        }
    }

    /// JSON Schema value this constraint projects to.
    #[must_use]
    pub fn schema_value(&self) -> Value {
        match self {
            Self::Minimum(value)
            | Self::Maximum(value)
            | Self::ExclusiveMinimum(value)
            | Self::ExclusiveMaximum(value)
            | Self::MultipleOf(value) => value.as_json(),
            Self::Pattern(value) => json!(value),
            Self::MinItems(value) | Self::MaxItems(value) => json!(value),
            Self::UniqueItems => json!(true),
        }
    }

    /// Writes the constraint into a JSON Schema object in place.
    pub fn apply_json_schema(&self, schema: &mut Value) {
        if let Some(object) = schema.as_object_mut() {
            object.insert(self.keyword().to_owned(), self.schema_value());
        }
    }
}

impl fmt::Display for Constraint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Minimum(value) => write!(formatter, "minimum={value}"),
            Self::Maximum(value) => write!(formatter, "maximum={value}"),
            Self::ExclusiveMinimum(value) => write!(formatter, "exclusive_minimum={value}"),
            Self::ExclusiveMaximum(value) => write!(formatter, "exclusive_maximum={value}"),
            Self::MultipleOf(value) => write!(formatter, "multiple_of={value}"),
            Self::Pattern(value) => write!(formatter, "pattern={value}"),
            Self::MinItems(value) => write!(formatter, "min_items={value}"),
            Self::MaxItems(value) => write!(formatter, "max_items={value}"),
            Self::UniqueItems => formatter.write_str("unique_items=true"),
        }
    }
}

/// A single cross-field failure produced by a model-level validator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelViolation {
    field: String,
    code: String,
    message: String,
}

impl ModelViolation {
    /// Builds a violation attached to one field path.
    #[must_use]
    pub fn field(
        field: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            field: field.into(),
            code: code.into(),
            message: message.into(),
        }
    }

    /// Builds a model-wide violation with an empty field path.
    #[must_use]
    pub fn model(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::field(String::new(), code, message)
    }
}

/// Converts a model-level validator failure into collected field violations.
pub trait IntoModelViolations {
    /// Produces the violations recorded for this failure.
    fn into_model_violations(self) -> ValidationErrors;
}

impl IntoModelViolations for ValidationErrors {
    fn into_model_violations(self) -> ValidationErrors {
        self
    }
}

impl IntoModelViolations for ModelViolation {
    fn into_model_violations(self) -> ValidationErrors {
        let mut errors = ValidationErrors::new();
        errors.push(self.field, self.code, self.message);
        errors
    }
}

impl IntoModelViolations for FieldViolation {
    fn into_model_violations(self) -> ValidationErrors {
        let mut errors = ValidationErrors::new();
        errors.push(self.field, self.code, self.message);
        errors
    }
}

impl IntoModelViolations for String {
    fn into_model_violations(self) -> ValidationErrors {
        ModelViolation::model("model", self).into_model_violations()
    }
}

impl IntoModelViolations for &str {
    fn into_model_violations(self) -> ValidationErrors {
        ModelViolation::model("model", self).into_model_violations()
    }
}

/// Records a model-level validator failure beside the collected field failures.
pub fn merge_model_violations<E: IntoModelViolations>(errors: &mut ValidationErrors, failure: E) {
    for violation in failure.into_model_violations().violations() {
        errors.push(
            violation.field.clone(),
            violation.code.clone(),
            violation.message.clone(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Constraint, ModelViolation, NumericValue, check_exclusive_maximum, check_exclusive_minimum,
        check_max_items, check_maximum, check_min_items, check_minimum, check_multiple_of,
        check_pattern, check_unique_items, merge_model_violations, parse_numeric_value,
    };
    use blazingly_core::ValidationErrors;
    use serde_json::json;

    fn codes(errors: &ValidationErrors) -> Vec<&str> {
        errors
            .violations()
            .iter()
            .map(|violation| violation.code.as_str())
            .collect()
    }

    #[test]
    fn integer_bounds_are_compared_without_precision_loss() {
        let mut errors = ValidationErrors::new();
        let bound = NumericValue::Integer(9_007_199_254_740_993);
        check_minimum(&mut errors, "count", 9_007_199_254_740_992_i64, bound);
        assert_eq!(codes(&errors), ["minimum"]);

        let mut errors = ValidationErrors::new();
        check_minimum(&mut errors, "count", 9_007_199_254_740_993_i64, bound);
        assert!(errors.is_empty());
    }

    #[test]
    fn inclusive_and_exclusive_bounds_disagree_only_on_the_boundary() {
        let bound = NumericValue::Integer(10);
        let mut errors = ValidationErrors::new();
        check_maximum(&mut errors, "value", 10_u8, bound);
        check_minimum(&mut errors, "value", 10_u8, bound);
        assert!(errors.is_empty());

        let mut errors = ValidationErrors::new();
        check_exclusive_maximum(&mut errors, "value", 10_u8, bound);
        check_exclusive_minimum(&mut errors, "value", 10_u8, bound);
        assert_eq!(codes(&errors), ["exclusive_maximum", "exclusive_minimum"]);
    }

    #[test]
    fn multiple_of_handles_integers_floats_and_degenerate_factors() {
        let mut errors = ValidationErrors::new();
        check_multiple_of(&mut errors, "value", 9_i32, NumericValue::Integer(3));
        check_multiple_of(&mut errors, "value", 0.75_f64, NumericValue::Float(0.25));
        assert!(errors.is_empty());

        let mut errors = ValidationErrors::new();
        check_multiple_of(&mut errors, "value", 10_i32, NumericValue::Integer(3));
        check_multiple_of(&mut errors, "value", 1.0_f64, NumericValue::Float(0.0));
        check_multiple_of(&mut errors, "value", f64::NAN, NumericValue::Float(0.5));
        assert_eq!(
            codes(&errors),
            ["multiple_of", "multiple_of", "multiple_of"]
        );
    }

    #[test]
    fn nan_fails_every_range_constraint() {
        let mut errors = ValidationErrors::new();
        let bound = NumericValue::Float(1.0);
        check_minimum(&mut errors, "value", f64::NAN, bound);
        check_maximum(&mut errors, "value", f64::NAN, bound);
        check_exclusive_minimum(&mut errors, "value", f64::NAN, bound);
        check_exclusive_maximum(&mut errors, "value", f64::NAN, bound);
        assert_eq!(
            codes(&errors),
            [
                "minimum",
                "maximum",
                "exclusive_minimum",
                "exclusive_maximum"
            ]
        );
    }

    #[test]
    fn collection_constraints_report_bounds_and_duplicates() {
        let mut errors = ValidationErrors::new();
        check_min_items(&mut errors, "tags", &[1_u8], 2);
        check_max_items(&mut errors, "tags", &[1_u8, 2, 3], 2);
        check_unique_items(&mut errors, "tags", &[1_u8, 2, 1]);
        assert_eq!(codes(&errors), ["min_items", "max_items", "unique_items"]);

        let mut errors = ValidationErrors::new();
        check_unique_items(&mut errors, "tags", &[1_u8, 2, 3]);
        assert!(errors.is_empty());
    }

    #[test]
    fn oversized_collections_are_rejected_instead_of_scanned() {
        let values = vec![0_u8; super::UNIQUE_ITEMS_SCAN_LIMIT + 1];
        let mut errors = ValidationErrors::new();
        check_unique_items(&mut errors, "tags", &values);
        assert_eq!(codes(&errors), ["unique_items"]);
    }

    #[test]
    fn pattern_failures_and_unusable_patterns_both_report_one_violation() {
        let mut errors = ValidationErrors::new();
        check_pattern(&mut errors, "slug", "Bad Slug", "^[a-z-]+$");
        assert_eq!(codes(&errors), ["pattern"]);

        let mut errors = ValidationErrors::new();
        check_pattern(&mut errors, "slug", "anything", "^a{2}$");
        assert_eq!(codes(&errors), ["pattern"]);

        let mut errors = ValidationErrors::new();
        check_pattern(&mut errors, "slug", "good-slug", "^[a-z-]+$");
        assert!(errors.is_empty());
    }

    #[test]
    fn constraints_round_trip_through_their_canonical_encoding() {
        let samples = [
            Constraint::Minimum(NumericValue::Integer(-3)),
            Constraint::Maximum(NumericValue::Float(2.5)),
            Constraint::ExclusiveMinimum(NumericValue::Float(0.0)),
            Constraint::ExclusiveMaximum(NumericValue::Integer(9)),
            Constraint::MultipleOf(NumericValue::Integer(2)),
            Constraint::Pattern("^a=b$".to_owned()),
            Constraint::MinItems(1),
            Constraint::MaxItems(4),
            Constraint::UniqueItems,
        ];
        for sample in samples {
            let encoded = sample.to_string();
            assert_eq!(Constraint::parse(&encoded), Some(sample), "{encoded}");
        }
        assert_eq!(Constraint::parse("validate_code"), None);
        assert_eq!(Constraint::parse("unique_items=false"), None);
    }

    #[test]
    fn constraints_project_into_json_schema_keywords() {
        let mut schema = json!({ "type": "integer" });
        Constraint::Minimum(NumericValue::Integer(1)).apply_json_schema(&mut schema);
        Constraint::MultipleOf(NumericValue::Float(0.5)).apply_json_schema(&mut schema);
        assert_eq!(schema["minimum"], json!(1));
        assert_eq!(schema["multipleOf"], json!(0.5));
    }

    #[test]
    fn numeric_encoding_distinguishes_integers_from_whole_floats() {
        assert_eq!(NumericValue::Integer(5).to_string(), "5");
        assert_eq!(NumericValue::Float(5.0).to_string(), "5.0");
        assert_eq!(
            parse_numeric_value("5"),
            Some(NumericValue::Integer(5)),
            "integers stay exact"
        );
        assert_eq!(parse_numeric_value("5.0"), Some(NumericValue::Float(5.0)));
        assert_eq!(parse_numeric_value("inf"), None);
        assert_eq!(parse_numeric_value("NaN"), None);
    }

    #[test]
    fn model_validator_failures_are_merged_beside_field_failures() {
        let mut errors = ValidationErrors::new();
        errors.push("start", "minimum", "must be greater than or equal to 0");
        merge_model_violations(
            &mut errors,
            ModelViolation::field("end", "range", "must be after start"),
        );
        merge_model_violations(&mut errors, "the window is inconsistent");
        let fields = errors
            .violations()
            .iter()
            .map(|violation| violation.field.as_str())
            .collect::<Vec<_>>();
        assert_eq!(fields, ["start", "end", ""]);
        assert_eq!(codes(&errors), ["minimum", "range", "model"]);
    }
}
