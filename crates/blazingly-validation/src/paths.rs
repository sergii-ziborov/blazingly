//! One field-path syntax shared by decode failures and rule failures.

use blazingly_core::ValidationErrors;

/// Stable violation code reported when a request body cannot be decoded.
pub const DECODE_VIOLATION_CODE: &str = "invalid_value";

/// Rewrites a `serde_path_to_error` path into the framework's field-path syntax.
///
/// Generated model validation reports `items[2].name`, while `serde` reports the
/// same location as `items.2.name`. Normalizing the decode path lets a client
/// parse one syntax for both failure kinds.
#[must_use]
pub fn normalize_field_path(path: &str) -> String {
    let mut normalized = String::with_capacity(path.len() + 2);
    for segment in path.split('.') {
        if segment.is_empty() {
            continue;
        }
        if segment.bytes().all(|byte| byte.is_ascii_digit()) {
            normalized.push('[');
            normalized.push_str(segment);
            normalized.push(']');
        } else {
            if !normalized.is_empty() {
                normalized.push('.');
            }
            normalized.push_str(segment);
        }
    }
    normalized
}

/// Builds the single-violation error set reported for a body decode failure.
///
/// The path is normalized with [`normalize_field_path`] and the violation code
/// is always [`DECODE_VIOLATION_CODE`], so a decode failure and a rule failure
/// present the same `violations` array shape.
#[must_use]
pub fn decode_violations(path: &str, reason: &str) -> ValidationErrors {
    let mut errors = ValidationErrors::new();
    errors.push(normalize_field_path(path), DECODE_VIOLATION_CODE, reason);
    errors
}

#[cfg(test)]
mod tests {
    use super::{DECODE_VIOLATION_CODE, decode_violations, normalize_field_path};

    #[test]
    fn serde_paths_become_dotted_paths_with_bracketed_indices() {
        assert_eq!(normalize_field_path("id"), "id");
        assert_eq!(normalize_field_path("items.2.name"), "items[2].name");
        assert_eq!(normalize_field_path("2.name"), "[2].name");
        assert_eq!(normalize_field_path("a.0.b.11.c"), "a[0].b[11].c");
        assert_eq!(normalize_field_path(""), "");
        assert_eq!(normalize_field_path("."), "");
    }

    #[test]
    fn decode_failures_produce_one_violation_with_a_stable_code() {
        let errors = decode_violations("items.0.id", "invalid Uuid: bad length");
        let violations = errors.violations();
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].field, "items[0].id");
        assert_eq!(violations[0].code, DECODE_VIOLATION_CODE);
        assert_eq!(violations[0].message, "invalid Uuid: bad length");
    }
}
