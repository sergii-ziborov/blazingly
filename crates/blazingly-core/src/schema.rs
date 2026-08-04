//! Shared JSON Schema projection of the operation contract.
//!
//! The `OpenAPI` document and the MCP tool-discovery document describe the
//! same operations, so both project the same [`TypeDescriptor`] tree into
//! JSON Schema 2020-12 nodes. Everything the two outputs agree on lives here
//! once, so a keyword one projection learns cannot silently go missing from
//! the other — the constraint carrier itself reached one document a full fix
//! before the other while each kept its own traversal. The few decisions the
//! formats genuinely differ on are supplied by the caller through
//! `SchemaDialect`. The module is `#[doc(hidden)]`, so an intra-doc link out
//! of this header has no page to point at.
//!
//! This module is shared plumbing for the first-party projection crates, not
//! part of the stable public API: it is `#[doc(hidden)]` and may change in
//! any release.

use crate::{FieldMetadata, ModelDescriptor, SchemaKind, TypeDescriptor, ValidationRule};
use blazingly_json::{Map, Value, json};

/// Format decisions that distinguish one JSON Schema output from another.
///
/// The projection is parameterised over exactly the points where `OpenAPI`
/// 3.1 and MCP tool schemas disagree, instead of each format keeping its own
/// copy of the whole traversal.
pub trait SchemaDialect {
    /// The node projected where a model-typed value appears.
    ///
    /// An `OpenAPI` document writes each model once under
    /// `#/components/schemas` and points at it with a `$ref`; an MCP tool
    /// schema travels alone, so its dialect inlines the full object via
    /// [`model_schema`] instead.
    fn model_node(&self, descriptor: &TypeDescriptor, model: &ModelDescriptor) -> Value;

    /// The node projected for a binary payload.
    ///
    /// `OpenAPI` 3.1 spells raw bytes as `format: "binary"`; MCP tool
    /// arguments arrive inside a JSON document, so that dialect asks for
    /// base64 text instead.
    fn binary_node(&self) -> Value;

    /// Projects one opaque custom validator, returning `true` when handled.
    ///
    /// [`apply_validation`] recovers declarative `keyword=value` metadata
    /// itself; every other validator is offered to the dialect — which is
    /// where an optional constraint decoder lives — and recorded under
    /// `x-blazingly-validators` only when this hook declines it.
    fn project_custom_validator(&self, _schema: &mut Value, _validator: &str) -> bool {
        false
    }
}

/// Projects one type, including the rules the type itself declares.
///
/// The recursion is what carries a value type's bounds into a `Vec<Tag>` item
/// and into every deeper nesting: the item is a descriptor of its own, so it
/// is projected by the same code that projects a bare field of that type.
#[must_use]
pub fn schema_value(dialect: &impl SchemaDialect, descriptor: &TypeDescriptor) -> Value {
    let mut value = if let Some(model) = &descriptor.model {
        dialect.model_node(descriptor, model)
    } else {
        let mut value = match (&descriptor.schema, &descriptor.items) {
            (SchemaKind::Array(_), Some(items)) => {
                json!({ "type": "array", "items": schema_value(dialect, items) })
            }
            _ => schema_kind_value(dialect, &descriptor.schema),
        };
        apply_known_string_format(&mut value, &descriptor.rust_name);
        value["x-rust-type"] = Value::String(descriptor.rust_name.clone());
        value
    };
    apply_validation(dialect, &mut value, &descriptor.constraints);
    value
}

/// Projects a bare schema kind, without model or naming information.
#[must_use]
pub fn schema_kind_value(dialect: &impl SchemaDialect, schema: &SchemaKind) -> Value {
    match schema {
        SchemaKind::String => json!({ "type": "string" }),
        SchemaKind::Binary => dialect.binary_node(),
        SchemaKind::Integer => json!({ "type": "integer" }),
        SchemaKind::Number => json!({ "type": "number" }),
        SchemaKind::Boolean => json!({ "type": "boolean" }),
        SchemaKind::Array(item) => {
            json!({ "type": "array", "items": schema_kind_value(dialect, item) })
        }
        SchemaKind::Object => json!({ "type": "object" }),
        SchemaKind::Any => json!({}),
    }
}

/// Projects a model into a full object schema.
#[must_use]
pub fn model_schema(dialect: &impl SchemaDialect, model: &ModelDescriptor) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();

    for field in &model.fields {
        let mut schema = schema_value(dialect, &field.ty);
        apply_validation(dialect, &mut schema, &field.validation);
        properties.insert(field.name.clone(), schema);
        if field.required {
            required.push(field.name.clone());
        }
    }

    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

/// Projects a recovered default, enumeration, or nullability marker.
///
/// JSON Schema 2020-12 — the dialect of both `OpenAPI` 3.1 and MCP tool
/// schemas — has no `nullable` keyword: a value that also accepts `null`
/// widens its own `type` into a union instead. A `$ref` node has no type of
/// its own to widen, so a nullable reference is wrapped in an `anyOf`; a
/// dialect that inlines every model never produces one, and the wrap simply
/// never applies.
///
/// # Panics
///
/// Panics when `schema` is not a JSON object. Every node produced by
/// [`schema_value`] is one.
pub fn apply_field_metadata(schema: &mut Value, metadata: &FieldMetadata) {
    match metadata {
        FieldMetadata::Default(value) => schema["default"] = value.clone(),
        FieldMetadata::Enumeration(values) => {
            schema["enum"] = Value::Array(
                values
                    .iter()
                    .map(|value| Value::String(value.clone()))
                    .collect(),
            );
        }
        FieldMetadata::Nullable => widen_with_null(schema),
    }
}

/// Applies the declared validation rules to a schema node.
///
/// Declarative `keyword=value` metadata is projected as real JSON Schema
/// keywords, so an agent reads the actual bound instead of an opaque string;
/// anything the projection cannot decode itself is offered to
/// [`SchemaDialect::project_custom_validator`] before landing in the
/// `x-blazingly-validators` extension array.
///
/// # Panics
///
/// Panics when `schema` is not a JSON object. Every node produced by
/// [`schema_value`] is one.
pub fn apply_validation(
    dialect: &impl SchemaDialect,
    schema: &mut Value,
    validation: &[ValidationRule],
) {
    for rule in validation {
        match rule {
            ValidationRule::MinLength(value) => schema["minLength"] = json!(value),
            ValidationRule::MaxLength(value) => schema["maxLength"] = json!(value),
            ValidationRule::Email => schema["format"] = json!("email"),
            ValidationRule::Alias(alias) => push_extension(schema, "x-blazingly-aliases", alias),
            ValidationRule::Custom(validator) => {
                if let Some(metadata) = FieldMetadata::parse(validator) {
                    apply_field_metadata(schema, &metadata);
                    continue;
                }
                if dialect.project_custom_validator(schema, validator) {
                    continue;
                }
                push_extension(schema, "x-blazingly-validators", validator);
            }
            ValidationRule::Nested => {
                schema["x-blazingly-nested-validation"] = Value::Bool(true);
            }
        }
    }
}

/// Appends one name to a document extension array, at most once.
///
/// A field declared with a value type is projected twice — once from the
/// type's own constraints, once from the rules the field inherited from it —
/// and a keyword that overwrites is idempotent where an array is not.
///
/// # Panics
///
/// Panics when `schema` is not a JSON object. Every node produced by
/// [`schema_value`] is one.
pub fn push_extension(schema: &mut Value, keyword: &str, name: &str) {
    let names = schema
        .as_object_mut()
        .expect("validation schema must be an object")
        .entry(keyword)
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .expect("a document extension list must be an array");
    if !names.iter().any(|declared| declared.as_str() == Some(name)) {
        names.push(Value::String(name.to_owned()));
    }
}

fn apply_known_string_format(schema: &mut Value, rust_name: &str) {
    let format = match rust_name {
        "Uuid" => "uuid",
        "Url" => "uri",
        "IpAddress" => "ip",
        "Date" => "date",
        "DateTime" => "date-time",
        "Decimal" => "decimal",
        _ => return,
    };
    schema["format"] = Value::String(format.to_owned());
}

fn widen_with_null(schema: &mut Value) {
    let Some(declared) = schema.as_object().map(|object| object.get("type").cloned()) else {
        return;
    };
    match declared {
        Some(Value::String(name)) => schema["type"] = json!([name, "null"]),
        Some(Value::Array(mut names)) => {
            if !names.iter().any(|name| name.as_str() == Some("null")) {
                names.push(Value::String("null".to_owned()));
                schema["type"] = Value::Array(names);
            }
        }
        Some(_) | None => {
            if schema.get("$ref").is_some() {
                let referenced = std::mem::replace(schema, Value::Null);
                *schema = json!({ "anyOf": [referenced, { "type": "null" }] });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FieldDescriptor;

    /// Dialect that inlines models, as the MCP projection does.
    struct Inline;

    impl SchemaDialect for Inline {
        fn model_node(&self, _descriptor: &TypeDescriptor, model: &ModelDescriptor) -> Value {
            model_schema(self, model)
        }

        fn binary_node(&self) -> Value {
            json!({ "type": "string", "contentEncoding": "base64" })
        }
    }

    /// Dialect that references models, as the `OpenAPI` projection does.
    struct Referencing;

    impl SchemaDialect for Referencing {
        fn model_node(&self, _descriptor: &TypeDescriptor, model: &ModelDescriptor) -> Value {
            json!({ "$ref": format!("#/components/schemas/{}", model.name) })
        }

        fn binary_node(&self) -> Value {
            json!({ "type": "string", "format": "binary" })
        }

        fn project_custom_validator(&self, schema: &mut Value, validator: &str) -> bool {
            if validator == "handled" {
                schema["x-handled"] = Value::Bool(true);
                return true;
            }
            false
        }
    }

    fn article() -> ModelDescriptor {
        ModelDescriptor::new(
            "Article",
            vec![FieldDescriptor::new(
                "title",
                true,
                TypeDescriptor::scalar("String", SchemaKind::String),
                Vec::new(),
            )],
        )
    }

    #[test]
    fn the_dialect_decides_how_a_model_appears() {
        let descriptor = TypeDescriptor::model(article());

        let inline = schema_value(&Inline, &descriptor);
        let referenced = schema_value(&Referencing, &descriptor);

        assert_eq!(inline["type"], "object");
        assert_eq!(inline["properties"]["title"]["type"], "string");
        assert_eq!(referenced["$ref"], "#/components/schemas/Article");
    }

    #[test]
    fn the_dialect_decides_how_binary_payloads_are_spelled() {
        let descriptor = TypeDescriptor::scalar("Vec<u8>", SchemaKind::Binary);

        assert_eq!(
            schema_value(&Inline, &descriptor)["contentEncoding"],
            "base64"
        );
        assert_eq!(schema_value(&Referencing, &descriptor)["format"], "binary");
    }

    /// The bounds a type declares survive every nesting the traversal reaches.
    #[test]
    fn type_constraints_recurse_into_items_at_any_depth() {
        let tag = TypeDescriptor::scalar("Tag", SchemaKind::String).with_constraints(vec![
            ValidationRule::MinLength(1),
            ValidationRule::MaxLength(20),
        ]);
        let tags = TypeDescriptor {
            rust_name: "Vec<Vec<Tag>>".to_owned(),
            schema: SchemaKind::Array(Box::new(SchemaKind::Array(Box::new(SchemaKind::String)))),
            model: None,
            items: Some(Box::new(TypeDescriptor {
                rust_name: "Vec<Tag>".to_owned(),
                schema: SchemaKind::Array(Box::new(SchemaKind::String)),
                model: None,
                items: Some(Box::new(tag)),
                constraints: Vec::new(),
            })),
            constraints: Vec::new(),
        };

        for dialect in [
            schema_value(&Inline, &tags),
            schema_value(&Referencing, &tags),
        ] {
            assert_eq!(dialect["items"]["items"]["minLength"], 1, "{dialect}");
            assert_eq!(dialect["items"]["items"]["maxLength"], 20, "{dialect}");
            assert!(dialect["minLength"].is_null(), "{dialect}");
        }
    }

    #[test]
    fn nullability_widens_a_type_and_wraps_a_reference() {
        let mut typed = json!({ "type": "string" });
        apply_field_metadata(&mut typed, &FieldMetadata::Nullable);
        assert_eq!(typed["type"], json!(["string", "null"]));

        apply_field_metadata(&mut typed, &FieldMetadata::Nullable);
        assert_eq!(
            typed["type"],
            json!(["string", "null"]),
            "widening is idempotent"
        );

        let mut referenced = json!({ "$ref": "#/components/schemas/Article" });
        apply_field_metadata(&mut referenced, &FieldMetadata::Nullable);
        assert_eq!(
            referenced["anyOf"][0]["$ref"],
            "#/components/schemas/Article"
        );
        assert_eq!(referenced["anyOf"][1]["type"], "null");
    }

    #[test]
    fn a_custom_validator_is_offered_to_the_dialect_before_the_extension_array() {
        let handled_rule = vec![ValidationRule::Custom("handled".to_owned())];
        let mut handled = json!({ "type": "string" });
        apply_validation(&Referencing, &mut handled, &handled_rule);
        assert_eq!(handled["x-handled"], json!(true));
        assert!(handled["x-blazingly-validators"].is_null());

        let opaque_rule = vec![ValidationRule::Custom("opaque".to_owned())];
        let mut declined = json!({ "type": "string" });
        apply_validation(&Referencing, &mut declined, &opaque_rule);
        assert_eq!(declined["x-blazingly-validators"], json!(["opaque"]));

        apply_validation(&Referencing, &mut declined, &opaque_rule);
        assert_eq!(
            declined["x-blazingly-validators"],
            json!(["opaque"]),
            "a doubly projected validator is recorded once"
        );
    }
}
