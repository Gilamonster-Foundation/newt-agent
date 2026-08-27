//! The versioned type tag, as a TYPE.
//!
//! A `schema: String` field lets a record claim any tag, and — more to the
//! point for the slice that publishes a cross-language contract — it makes
//! the generated JSON Schema say `{"type": "string"}`. A foreign
//! implementor validating a definition record against `response.schema.json`
//! would PASS, while our decoder classifies it unknown. That is not a
//! description of "response v1"; it is a description of "any record with
//! some string in its schema field".
//!
//! Each record therefore carries a tag type that serializes and
//! deserializes exactly one value, and whose generated schema is a
//! `const`. The outer PROBE stays permissive on purpose — it reads the tag
//! as a plain string and dispatches to the strict deserializer only on an
//! exact match, which is what lets an unknown version be preserved rather
//! than rejected as malformed.

use serde::de::{Error as _, Unexpected};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Define a tag type that is exactly one string on the wire.
macro_rules! schema_tag {
    ($(#[$meta:meta])* $name:ident, $value:path) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
        pub struct $name;

        impl $name {
            /// The one value this tag takes on the wire.
            #[must_use]
            pub fn as_str(&self) -> &'static str {
                $value
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str($value)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let text = String::deserialize(deserializer)?;
                if text == $value {
                    Ok(Self)
                } else {
                    Err(D::Error::invalid_value(
                        Unexpected::Str(&text),
                        &$value,
                    ))
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str($value)
            }
        }

        #[cfg(feature = "schema")]
        impl schemars::JsonSchema for $name {
            fn schema_name() -> String {
                stringify!($name).to_string()
            }

            fn json_schema(_: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
                let mut schema = schemars::schema::SchemaObject {
                    instance_type: Some(schemars::schema::InstanceType::String.into()),
                    ..Default::default()
                };
                schema.const_value = Some(serde_json::Value::String($value.to_string()));
                schema.into()
            }
        }
    };
}

schema_tag!(
    /// The tag every v1 definition carries, and the only value it accepts.
    DefinitionTag,
    crate::definition::DEFINITION_SCHEMA_V1
);
schema_tag!(
    /// The tag every v1 instance carries.
    InstanceTag,
    crate::instance::INSTANCE_SCHEMA_V1
);
schema_tag!(
    /// The tag every v1 response carries.
    ResponseTag,
    crate::response::RESPONSE_SCHEMA_V1
);

/// Publish a constrained string schema for a validated scalar newtype.
///
/// A derived `JsonSchema` on these would emit a bare `{"type": "string"}`,
/// which permits values the decoder refuses — the same false-guarantee
/// defect as a doc comment claiming a validation the wire does not
/// perform. A schema is a published contract; it has to state the rule.
#[cfg(feature = "schema")]
#[macro_export]
macro_rules! string_scalar_schema {
    ($name:ident, $pattern:expr) => {
        impl schemars::JsonSchema for $name {
            fn schema_name() -> String {
                stringify!($name).to_string()
            }

            fn json_schema(_: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
                let mut schema = schemars::schema::SchemaObject {
                    instance_type: Some(schemars::schema::InstanceType::String.into()),
                    ..Default::default()
                };
                schema.string().min_length = Some(1);
                if let Some(pattern) = $pattern {
                    schema.string().pattern = Some(pattern.to_string());
                }
                schema.into()
            }
        }
    };
}

/// Schema for an author-assigned name: non-empty, ASCII alphanumeric plus
/// `-` and `_` — the rule [`crate::ids::ControlId::new`] enforces.
///
/// Published rather than merely enforced, because a schema that permits
/// any string while the decoder refuses most of them is the same
/// false-guarantee defect as a doc comment that claims a validation the
/// wire does not perform.
#[cfg(feature = "schema")]
#[must_use]
pub fn name_schema(_: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
    let mut schema = schemars::schema::SchemaObject {
        instance_type: Some(schemars::schema::InstanceType::String.into()),
        ..Default::default()
    };
    schema.string().min_length = Some(1);
    schema.string().pattern = Some("^[A-Za-z0-9_-]+$".to_string());
    schema.into()
}

/// Schema for a scalar whose only rule is non-emptiness.
#[cfg(feature = "schema")]
#[must_use]
pub fn non_empty_schema(_: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
    let mut schema = schemars::schema::SchemaObject {
        instance_type: Some(schemars::schema::InstanceType::String.into()),
        ..Default::default()
    };
    schema.string().min_length = Some(1);
    schema.into()
}
