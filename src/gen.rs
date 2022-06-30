use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::prelude::*;

use avro_rs::{schema::RecordField, Schema};

use crate::error::{Error, Result};
use crate::templates::*;

/// Represents a schema input source.
pub enum Source<'a> {
    /// An Avro schema enum from `avro-rs` crate.
    Schema(&'a Schema),
    /// An Avro schema string in json format.
    SchemaStr(&'a str),
    /// Pattern for files containing Avro schemas in json format.
    GlobPattern(&'a str),
}

/// The main component of this library.
/// It is stateless and can be reused many times.
pub struct Generator {
    templater: Templater,
}

impl Generator {
    /// Create a new `Generator` through a builder with default config.
    pub fn new() -> Result<Generator> {
        GeneratorBuilder::new().build()
    }

    /// Returns a fluid builder for custom `Generator` instantiation.
    pub fn builder() -> GeneratorBuilder {
        GeneratorBuilder::new()
    }

    /// Generates Rust code from an Avro schema `Source`.
    /// Writes all generated types to the ouput.
    pub fn gen(&self, source: &Source, output: &mut impl Write) -> Result<()> {
        if self.templater.nullable {
            output.write_all(DESER_NULLABLE.as_bytes())?;
        }

        match source {
            Source::Schema(schema) => {
                let mut deps = deps_stack(schema, vec![]);
                self.gen_in_order(&mut deps, output)?;
            }

            Source::SchemaStr(raw_schema) => {
                let schema = Schema::parse_str(raw_schema)?;
                let mut deps = deps_stack(&schema, vec![]);
                self.gen_in_order(&mut deps, output)?;
            }

            Source::GlobPattern(pattern) => {
                let mut raw_schemas = vec![];
                for entry in glob::glob(pattern)? {
                    let path = entry.map_err(|e| e.into_error())?;
                    if !path.is_dir() {
                        raw_schemas.push(fs::read_to_string(path)?);
                    }
                }

                let schemas = &raw_schemas.iter().map(|s| s.as_str()).collect::<Vec<_>>();
                let schemas = Schema::parse_list(schemas)?;
                let mut deps = schemas
                    .iter()
                    .fold(vec![], |deps, schema| deps_stack(schema, deps));

                self.gen_in_order(&mut deps, output)?;
            }
        }

        Ok(())
    }

    /// Given an Avro `schema`:
    /// * Find its ordered, nested dependencies with `deps_stack(schema)`
    /// * Pops sub-schemas and generate appropriate Rust types
    /// * Keeps tracks of nested schema->name with `GenState` mapping
    /// * Appends generated Rust types to the output
    fn gen_in_order(&self, deps: &mut Vec<Schema>, output: &mut impl Write) -> Result<()> {
        let mut gs = GenState::new();

        while let Some(s) = deps.pop() {
            match s {
                // Simply generate code
                Schema::Fixed { .. } => {
                    let code = &self.templater.str_fixed(&s)?;
                    output.write_all(code.as_bytes())?
                }
                Schema::Enum { .. } => {
                    let code = &self.templater.str_enum(&s)?;
                    output.write_all(code.as_bytes())?
                }

                // Generate code with potentially nested types
                Schema::Record { .. } => {
                    let code = &self.templater.str_record(&s, &gs)?;
                    output.write_all(code.as_bytes())?
                }

                // Register inner type for it to be used as a nested type later
                Schema::Array(ref inner) => {
                    let type_str = array_type(inner, &gs)?;
                    gs.put_type(&s, type_str)
                }
                Schema::Map(ref inner) => {
                    let type_str = map_type(inner, &gs)?;
                    gs.put_type(&s, type_str)
                }

                Schema::Union(ref union) => {
                    // Generate custom enum with potentially nested types
                    if (union.is_nullable() && union.variants().len() > 2)
                        || (!union.is_nullable() && union.variants().len() > 1)
                    {
                        let code = &self.templater.str_union_enum(&s, &gs)?;
                        output.write_all(code.as_bytes())?
                    }

                    // Register inner union for it to be used as a nested type later
                    let type_str = union_type(union, &gs, true)?;
                    gs.put_type(&s, type_str)
                }

                _ => return Err(Error::Schema(format!("Not a valid root schema: {:?}", s))),
            }
        }

        Ok(())
    }
}

/// Utility function to find the ordered, nested dependencies of an Avro `schema`.
/// Explores nested `schema`s in a breadth-first fashion, pushing them on a stack
/// at the same time in order to have them ordered.
/// It is similar to traversing the `schema` tree in a post-order fashion.
fn deps_stack(schema: &Schema, mut deps: Vec<Schema>) -> Vec<Schema> {
    fn push_unique(deps: &mut Vec<Schema>, s: Schema) {
        if !deps.contains(&s) {
            deps.push(s);
        }
    }

    let mut q = VecDeque::new();

    q.push_back(schema);
    while !q.is_empty() {
        let s = q.pop_front().unwrap();

        match s {
            // No nested schemas, add them to the result stack
            Schema::Enum { .. } => push_unique(&mut deps, s.clone()),
            Schema::Fixed { .. } => push_unique(&mut deps, s.clone()),
            Schema::Decimal { inner, .. } if matches!(**inner, Schema::Fixed { .. }) => {
                push_unique(&mut deps, s.clone())
            }

            // Explore the record fields for potentially nested schemas
            Schema::Record { fields, .. } => {
                push_unique(&mut deps, s.clone());

                let by_pos = fields
                    .iter()
                    .map(|f| (f.position, f))
                    .collect::<HashMap<_, _>>();
                let mut i = 0;
                while let Some(RecordField { schema: sr, .. }) = by_pos.get(&i) {
                    match sr {
                        // No nested schemas, add them to the result stack
                        Schema::Fixed { .. } => push_unique(&mut deps, sr.clone()),
                        Schema::Enum { .. } => push_unique(&mut deps, sr.clone()),

                        // Push to the exploration queue for further checks
                        Schema::Record { .. } => q.push_back(sr),

                        // Push to the exploration queue, depending on the inner schema format
                        Schema::Map(sc) | Schema::Array(sc) => match &**sc {
                            Schema::Fixed { .. }
                            | Schema::Enum { .. }
                            | Schema::Record { .. }
                            | Schema::Map(..)
                            | Schema::Array(..)
                            | Schema::Union(..) => q.push_back(&**sc),
                            _ => (),
                        },
                        Schema::Union(union) => {
                            if (union.is_nullable() && union.variants().len() > 2)
                                || (!union.is_nullable() && union.variants().len() > 1)
                            {
                                push_unique(&mut deps, sr.clone());
                            }

                            union.variants().iter().for_each(|sc| match sc {
                                Schema::Fixed { .. }
                                | Schema::Enum { .. }
                                | Schema::Record { .. }
                                | Schema::Map(..)
                                | Schema::Array(..)
                                | Schema::Union(..) => {
                                    q.push_back(sc);
                                    push_unique(&mut deps, sc.clone());
                                }

                                _ => (),
                            });
                        }
                        _ => (),
                    };
                    i += 1;
                }
            }

            // Depending on the inner schema type ...
            Schema::Map(sc) | Schema::Array(sc) => match &**sc {
                // ... Needs further checks, push to the exploration queue
                Schema::Fixed { .. }
                | Schema::Enum { .. }
                | Schema::Record { .. }
                | Schema::Map(..)
                | Schema::Array(..)
                | Schema::Union(..) => q.push_back(&**sc),
                // ... Not nested, can be pushed to the result stack
                _ => push_unique(&mut deps, s.clone()),
            },

            Schema::Union(union) => {
                if (union.is_nullable() && union.variants().len() > 2)
                    || (!union.is_nullable() && union.variants().len() > 1)
                {
                    push_unique(&mut deps, s.clone());
                }

                union.variants().iter().for_each(|sc| match sc {
                    // ... Needs further checks, push to the exploration queue
                    Schema::Fixed { .. }
                    | Schema::Enum { .. }
                    | Schema::Record { .. }
                    | Schema::Map(..)
                    | Schema::Array(..)
                    | Schema::Union(..) => q.push_back(sc),
                    // ... Not nested, can be pushed to the result stack
                    _ => push_unique(&mut deps, s.clone()),
                });
            }

            // Ignore all other schema formats
            _ => (),
        }
    }

    deps
}

/// A builder class to customize `Generator`.
pub struct GeneratorBuilder {
    precision: usize,
    nullable: bool,
    use_variant_access: bool,
    use_avro_rs_unions: bool,
    derive_builders: bool,
}

impl Default for GeneratorBuilder {
    fn default() -> Self {
        Self {
            precision: 3,
            nullable: false,
            use_variant_access: false,
            use_avro_rs_unions: false,
            derive_builders: false,
        }
    }
}

impl GeneratorBuilder {
    /// Creates a new `GeneratorBuilder`.
    pub fn new() -> GeneratorBuilder {
        GeneratorBuilder::default()
    }

    /// Sets the precision for default values of f32/f64 fields.
    pub fn precision(mut self, precision: usize) -> GeneratorBuilder {
        self.precision = precision;
        self
    }

    /// Puts default value when deserializing `null` field.
    /// Doesn't apply to union fields ["null", "Foo"], which are `Option<Foo>`.
    pub fn nullable(mut self, nullable: bool) -> GeneratorBuilder {
        self.nullable = nullable;
        self
    }

    /// Adds variant_access_derive to the enums generated from union types.
    pub fn use_variant_access(mut self, use_variant_access: bool) -> GeneratorBuilder {
        self.use_variant_access = use_variant_access;
        self
    }

    /// Adds support for deserializing union types from the `avro-rs` crate.
    /// Only necessary for unions of 3 or more types or 2-type unions without "null".
    /// Note that only int, long, float, double, and boolean values are currently supported.
    pub fn use_avro_rs_unions(mut self, use_avro_rs_unions: bool) -> GeneratorBuilder {
        self.use_avro_rs_unions = use_avro_rs_unions;
        self
    }

    /// Adds support to derive builders using the `rust-derive-builder` crate.
    /// Will derive builders for record structs.
    pub fn derive_builders(mut self, derive_builders: bool) -> GeneratorBuilder {
        self.derive_builders = derive_builders;
        self
    }

    /// Create a `Generator` with the builder parameters.
    pub fn build(self) -> Result<Generator> {
        let mut templater = Templater::new()?;
        templater.precision = self.precision;
        templater.nullable = self.nullable;
        templater.use_variant_access = self.use_variant_access;
        templater.use_avro_rs_unions = self.use_avro_rs_unions;
        templater.derive_builders = self.derive_builders;
        Ok(Generator { templater })
    }
}

#[cfg(test)]
mod tests {
    use avro_rs::schema::Name;

    use super::*;

    macro_rules! assert_schema_gen (
        ($generator:expr, $expected:expr, $raw_schema:expr) => (
            let schema = Schema::parse_str($raw_schema).unwrap();
            let source = Source::Schema(&schema);

            let mut buf = vec![];
            $generator.gen(&source, &mut buf).unwrap();
            let res = String::from_utf8(buf).unwrap();

            assert_eq!($expected, &res);
        );
    );

    #[test]
    fn simple() {
        let raw_schema = r#"
{
  "type": "record",
  "name": "test",
  "fields": [
    {"name": "a", "type": "long", "default": 42},
    {"name": "b", "type": "string"}
  ]
}
"#;

        let expected = r#"
#[derive(Debug, PartialEq, Clone, serde::Deserialize, serde::Serialize)]
pub struct Test {
    #[serde(default = "default_test_a")]
    pub a: i64,
    pub b: String,
}

fn default_test_a() -> i64 { 42 }
"#;

        let g = Generator::new().unwrap();
        assert_schema_gen!(g, expected, raw_schema);
    }

    #[test]
    fn simple_with_builders() {
        let raw_schema = r#"
{
  "type": "record",
  "name": "test",
  "fields": [
    {"name": "a", "type": "long", "default": 42},
    {"name": "b", "type": "string"}
  ]
}
"#;

        let expected = r#"
#[derive(Debug, PartialEq, Clone, serde::Deserialize, serde::Serialize, derive_builder::Builder)]
#[builder(setter(into))]
pub struct Test {
    #[serde(default = "default_test_a")]
    pub a: i64,
    pub b: String,
}

fn default_test_a() -> i64 { 42 }
"#;

        let g = GeneratorBuilder::new()
            .derive_builders(true)
            .build()
            .unwrap();
        assert_schema_gen!(g, expected, raw_schema);
    }

    #[test]
    fn complex() {
        let raw_schema = r#"
{
  "type": "record",
  "name": "User",
  "doc": "Hi there.",
  "fields": [
    {"name": "name", "type": "string", "default": ""},
    {"name": "favorite_number",  "type": "int", "default": 7},
    {"name": "likes_pizza", "type": "boolean", "default": false},
    {"name": "oye", "type": "float", "default": 1.1},
    {"name": "aa-i32",
     "type": {"type": "array", "items": {"type": "array", "items": "int"}},
     "default": [[0], [12, -1]]}
  ]
}
"#;

        let expected = r#"
/// Hi there.
#[derive(Debug, PartialEq, Clone, serde::Deserialize, serde::Serialize)]
pub struct User {
    #[serde(default = "default_user_name")]
    pub name: String,
    #[serde(default = "default_user_favorite_number")]
    pub favorite_number: i32,
    #[serde(default = "default_user_likes_pizza")]
    pub likes_pizza: bool,
    #[serde(default = "default_user_oye")]
    pub oye: f32,
    #[serde(rename = "aa-i32")]
    #[serde(default = "default_user_aa_i32")]
    pub aa_i32: Vec<Vec<i32>>,
}

fn default_user_name() -> String { "".to_owned() }

fn default_user_favorite_number() -> i32 { 7 }

fn default_user_likes_pizza() -> bool { false }

fn default_user_oye() -> f32 { 1.100 }

fn default_user_aa_i32() -> Vec<Vec<i32>> { vec![vec![0], vec![12, -1]] }
"#;

        let g = Generator::new().unwrap();
        assert_schema_gen!(g, expected, raw_schema);
    }

    #[test]
    fn optional_array() {
        let raw_schema = r#"
{
  "name": "Snmp",
  "type": "record",
  "fields": [ {
    "name": "v1",
    "type": [ "null", {
      "name": "V1",
      "type": "record",
      "fields": [ {
        "name": "pdu",
        "type": [ "null", {
          "name": "TrapV1",
          "type": "record",
          "fields": [ {
            "name": "var",
            "type": ["null", {
              "type": "array",
              "items": {
                "name": "Variable",
                "type": "record",
                "fields": [ {
                  "name": "oid",
                  "type": ["null", {
                    "type":"array",
                    "items": "long"
                  } ],
                  "default": null
                }, {
                  "name": "val",
                  "type": ["null", "string"],
                  "default": null
                } ],
                "default": {}
              }
            } ],
            "default": null
          } ]
        } ],
        "default": null
      } ]
    } ],
    "default": null
  } ],
  "default": {}
}
"#;

        let expected = r#"
#[derive(Debug, PartialEq, Clone, serde::Deserialize, serde::Serialize)]
pub struct Variable {
    #[serde(default = "default_variable_oid")]
    pub oid: Option<Vec<i64>>,
    #[serde(default = "default_variable_val")]
    pub val: Option<String>,
}

fn default_variable_oid() -> Option<Vec<i64>> { None }

fn default_variable_val() -> Option<String> { None }

#[derive(Debug, PartialEq, Clone, serde::Deserialize, serde::Serialize)]
pub struct TrapV1 {
    #[serde(default = "default_trapv1_var")]
    pub var: Option<Vec<Variable>>,
}

fn default_trapv1_var() -> Option<Vec<Variable>> { None }

#[derive(Debug, PartialEq, Clone, serde::Deserialize, serde::Serialize)]
pub struct V1 {
    #[serde(default = "default_v1_pdu")]
    pub pdu: Option<TrapV1>,
}

fn default_v1_pdu() -> Option<TrapV1> { None }

#[derive(Debug, PartialEq, Clone, serde::Deserialize, serde::Serialize)]
pub struct Snmp {
    #[serde(default = "default_snmp_v1")]
    pub v1: Option<V1>,
}

fn default_snmp_v1() -> Option<V1> { None }
"#;

        let g = Generator::new().unwrap();
        assert_schema_gen!(g, expected, raw_schema);
    }

    #[test]
    fn optional_arrays() {
        let raw_schema = r#"
{
  "type": "record",
  "name": "KsqlDataSourceSchema",
  "namespace": "io.confluent.ksql.avro_schemas",
  "fields": [ {
    "name": "ID",
    "type": ["null", "string"],
    "default": null
  }, {
    "name": "GROUP_IDS",
    "type": ["null", {
      "type": "array",
      "items": ["null", "string"]
    } ],
    "default": null
  }, {
    "name": "GROUP_NAMES",
    "type": ["null", {
      "type": "array",
      "items": ["null", "string"]
    } ],
    "default": null
  } ]
}
"#;

        let expected = r#"
#[derive(Debug, PartialEq, Clone, serde::Deserialize, serde::Serialize)]
pub struct KsqlDataSourceSchema {
    #[serde(rename = "ID")]
    #[serde(default = "default_ksqldatasourceschema_id")]
    pub id: Option<String>,
    #[serde(rename = "GROUP_IDS")]
    #[serde(default = "default_ksqldatasourceschema_group_ids")]
    pub group_ids: Option<Vec<Option<String>>>,
    #[serde(rename = "GROUP_NAMES")]
    #[serde(default = "default_ksqldatasourceschema_group_names")]
    pub group_names: Option<Vec<Option<String>>>,
}

fn default_ksqldatasourceschema_id() -> Option<String> { None }

fn default_ksqldatasourceschema_group_ids() -> Option<Vec<Option<String>>> { None }

fn default_ksqldatasourceschema_group_names() -> Option<Vec<Option<String>>> { None }
"#;

        let g = Generator::new().unwrap();
        assert_schema_gen!(g, expected, raw_schema);
    }

    #[test]
    fn multi_valued_union() {
        let raw_schema = r#"
{
  "type": "record",
  "name": "Contact",
  "namespace": "com.test",
  "fields": [ {
    "name": "extra",
    "type": "map",
    "values" : [ "null", "string", "long", "double", "boolean" ]
  } ]
}
"#;

        let expected = r#"
/// Auto-generated type for unnamed Avro union variants.
#[derive(Debug, PartialEq, Clone, serde::Deserialize, serde::Serialize)]
pub enum UnionStringLongDoubleBoolean {
    String(String),
    Long(i64),
    Double(f64),
    Boolean(bool),
}

#[derive(Debug, PartialEq, Clone, serde::Deserialize, serde::Serialize)]
pub struct Contact {
    pub extra: ::std::collections::HashMap<String, Option<UnionStringLongDoubleBoolean>>,
}
"#;

        let g = Generator::new().unwrap();
        assert_schema_gen!(g, expected, raw_schema);

        let raw_schema = r#"
{
  "type": "record",
  "name": "AvroFileId",
  "fields": [ {
    "name": "id",
    "type": [
      "string", {
      "type": "record",
      "name": "AvroShortUUID",
      "fields": [ {
        "name": "mostBits",
        "type": "long"
      }, {
        "name": "leastBits",
        "type": "long"
      } ]
    } ]
  } ]
}
"#;

        let expected = r#"
#[derive(Debug, PartialEq, Clone, serde::Deserialize, serde::Serialize)]
pub struct AvroShortUuid {
    #[serde(rename = "mostBits")]
    pub most_bits: i64,
    #[serde(rename = "leastBits")]
    pub least_bits: i64,
}

/// Auto-generated type for unnamed Avro union variants.
#[derive(Debug, PartialEq, Clone, serde::Deserialize, serde::Serialize)]
pub enum UnionStringAvroShortUuid {
    String(String),
    AvroShortUuid(AvroShortUuid),
}

#[derive(Debug, PartialEq, Clone, serde::Deserialize, serde::Serialize)]
pub struct AvroFileId {
    pub id: UnionStringAvroShortUuid,
}
"#;

        let g = Generator::new().unwrap();
        assert_schema_gen!(g, expected, raw_schema);
    }

    #[test]
    fn multi_valued_union_with_variant_access() {
        let raw_schema = r#"
{
  "type": "record",
  "name": "Contact",
  "namespace": "com.test",
  "fields": [ {
    "name": "extra",
    "type": "map",
    "values" : [ "null", "string", "long", "double", "boolean" ]
  } ]
}
"#;

        let expected = r#"
/// Auto-generated type for unnamed Avro union variants.
#[derive(Debug, PartialEq, Clone, serde::Deserialize, serde::Serialize, variant_access_derive::VariantAccess)]
pub enum UnionStringLongDoubleBoolean {
    String(String),
    Long(i64),
    Double(f64),
    Boolean(bool),
}

#[derive(Debug, PartialEq, Clone, serde::Deserialize, serde::Serialize)]
pub struct Contact {
    pub extra: ::std::collections::HashMap<String, Option<UnionStringLongDoubleBoolean>>,
}
"#;

        let g = Generator::builder()
            .use_variant_access(true)
            .build()
            .unwrap();
        assert_schema_gen!(g, expected, raw_schema);

        let raw_schema = r#"
{
  "type": "record",
  "name": "AvroFileId",
  "fields": [ {
    "name": "id",
    "type": [
      "string", {
      "type": "record",
      "name": "AvroShortUUID",
      "fields": [ {
        "name": "mostBits",
        "type": "long"
      }, {
        "name": "leastBits",
        "type": "long"
      } ]
    } ]
  } ]
}
"#;

        let expected = r#"
#[derive(Debug, PartialEq, Clone, serde::Deserialize, serde::Serialize)]
pub struct AvroShortUuid {
    #[serde(rename = "mostBits")]
    pub most_bits: i64,
    #[serde(rename = "leastBits")]
    pub least_bits: i64,
}

/// Auto-generated type for unnamed Avro union variants.
#[derive(Debug, PartialEq, Clone, serde::Deserialize, serde::Serialize, variant_access_derive::VariantAccess)]
pub enum UnionStringAvroShortUuid {
    String(String),
    AvroShortUuid(AvroShortUuid),
}

#[derive(Debug, PartialEq, Clone, serde::Deserialize, serde::Serialize)]
pub struct AvroFileId {
    pub id: UnionStringAvroShortUuid,
}
"#;

        let g = Generator::builder()
            .use_variant_access(true)
            .build()
            .unwrap();
        assert_schema_gen!(g, expected, raw_schema);
    }

    #[test]
    fn multi_valued_union_with_avro_rs_unions() {
        let raw_schema = r#"
{
  "type": "record",
  "name": "Contact",
  "namespace": "com.test",
  "fields": [ {
    "name": "extra",
    "type": "map",
    "values" : [ "null", "string", "long", "double", "boolean" ]
  } ]
}
"#;

        let expected = r#"
/// Auto-generated type for unnamed Avro union variants.
#[derive(Debug, PartialEq, Clone, serde::Serialize)]
pub enum UnionStringLongDoubleBoolean {
    String(String),
    Long(i64),
    Double(f64),
    Boolean(bool),
}

impl<'de> serde::Deserialize<'de> for UnionStringLongDoubleBoolean {
    fn deserialize<D>(deserializer: D) -> Result<UnionStringLongDoubleBoolean, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        /// Serde visitor for the auto-generated unnamed Avro union type.
        struct UnionStringLongDoubleBooleanVisitor;

        impl<'de> serde::de::Visitor<'de> for UnionStringLongDoubleBooleanVisitor {
            type Value = UnionStringLongDoubleBoolean;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a UnionStringLongDoubleBoolean")
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(UnionStringLongDoubleBoolean::Long(value))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(UnionStringLongDoubleBoolean::Double(value))
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(UnionStringLongDoubleBoolean::Boolean(value))
            }
        }

        deserializer.deserialize_any(UnionStringLongDoubleBooleanVisitor)
    }
}

#[derive(Debug, PartialEq, Clone, serde::Deserialize, serde::Serialize)]
pub struct Contact {
    pub extra: ::std::collections::HashMap<String, Option<UnionStringLongDoubleBoolean>>,
}
"#;

        let g = Generator::builder()
            .use_avro_rs_unions(true)
            .build()
            .unwrap();
        assert_schema_gen!(g, expected, raw_schema);

        let raw_schema = r#"
{
  "type": "record",
  "name": "AvroFileId",
  "fields": [ {
    "name": "id",
    "type": [
      "string", {
      "type": "record",
      "name": "AvroShortUUID",
      "fields": [ {
        "name": "mostBits",
        "type": "long"
      }, {
        "name": "leastBits",
        "type": "long"
      } ]
    } ]
  } ]
}
"#;

        let expected = r#"
#[derive(Debug, PartialEq, Clone, serde::Deserialize, serde::Serialize)]
pub struct AvroShortUuid {
    #[serde(rename = "mostBits")]
    pub most_bits: i64,
    #[serde(rename = "leastBits")]
    pub least_bits: i64,
}

/// Auto-generated type for unnamed Avro union variants.
#[derive(Debug, PartialEq, Clone, serde::Serialize)]
pub enum UnionStringAvroShortUuid {
    String(String),
    AvroShortUuid(AvroShortUuid),
}

impl<'de> serde::Deserialize<'de> for UnionStringAvroShortUuid {
    fn deserialize<D>(deserializer: D) -> Result<UnionStringAvroShortUuid, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        /// Serde visitor for the auto-generated unnamed Avro union type.
        struct UnionStringAvroShortUuidVisitor;

        impl<'de> serde::de::Visitor<'de> for UnionStringAvroShortUuidVisitor {
            type Value = UnionStringAvroShortUuid;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a UnionStringAvroShortUuid")
            }
        }

        deserializer.deserialize_any(UnionStringAvroShortUuidVisitor)
    }
}

#[derive(Debug, PartialEq, Clone, serde::Deserialize, serde::Serialize)]
pub struct AvroFileId {
    pub id: UnionStringAvroShortUuid,
}
"#;

        let g = Generator::builder()
            .use_avro_rs_unions(true)
            .build()
            .unwrap();
        assert_schema_gen!(g, expected, raw_schema);
    }

    #[test]
    fn nullable_gen() {
        let raw_schema = r#"
{
  "type": "record",
  "name": "test",
  "fields": [
    {"name": "a", "type": "long", "default": 42},
    {"name": "b-b", "type": "string", "default": "na"},
    {"name": "c", "type": ["null", "int"], "default": null}
  ]
}
"#;

        let expected = r#"
macro_rules! deser(
    ($name:ident, $rtype:ty, $val:expr) => (
        fn $name<'de, D>(deserializer: D) -> Result<$rtype, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            let opt = Option::deserialize(deserializer)?;
            Ok(opt.unwrap_or_else(|| $val))
        }
    );
);

#[derive(Debug, PartialEq, Clone, serde::Deserialize, serde::Serialize)]
pub struct Test {
    #[serde(deserialize_with = "nullable_test_a")]
    #[serde(default = "default_test_a")]
    pub a: i64,
    #[serde(rename = "b-b", deserialize_with = "nullable_test_b_b")]
    #[serde(default = "default_test_b_b")]
    pub b_b: String,
    #[serde(default = "default_test_c")]
    pub c: Option<i32>,
}
deser!(nullable_test_a, i64, 42);
deser!(nullable_test_b_b, String, "na".to_owned());

fn default_test_a() -> i64 { 42 }

fn default_test_b_b() -> String { "na".to_owned() }

fn default_test_c() -> Option<i32> { None }
"#;
        let g = Generator::builder().nullable(true).build().unwrap();
        assert_schema_gen!(g, expected, raw_schema);
    }

    #[test]
    fn nullable_code() {
        use serde::{Deserialize, Deserializer};

        macro_rules! deser(
            ($name:ident, $rtype:ty, $val:expr) => (
                fn $name<'de, D>(deserializer: D) -> std::result::Result<$rtype, D::Error>
                where
                    D: Deserializer<'de>,
                {
                    let opt = Option::deserialize(deserializer)?;
                    Ok(opt.unwrap_or_else(|| $val))
                }
            );
        );

        #[derive(Debug, PartialEq, serde::Deserialize, serde::Serialize)]
        #[serde(default)]
        pub struct Test {
            #[serde(deserialize_with = "nullable_test_a")]
            pub a: i64,
            #[serde(rename = "b-b", deserialize_with = "nullable_test_b_b")]
            pub b_b: String,
            pub c: Option<i32>,
        }
        deser!(nullable_test_a, i64, 42);
        deser!(nullable_test_b_b, String, "na".to_owned());

        impl Default for Test {
            fn default() -> Test {
                Test {
                    a: 42,
                    b_b: "na".to_owned(),
                    c: None,
                }
            }
        }

        let json = r#"{"a": null, "b-b": null, "c": null}"#;
        let res: Test = serde_json::from_str(json).unwrap();
        assert_eq!(Test::default(), res);
    }

    #[test]
    fn deps() {
        let raw_schema = r#"
{
  "type": "record",
  "name": "User",
  "fields": [
    {"name": "name", "type": "string", "default": "unknown"},
    {"name": "address",
     "type": {
       "type": "record",
       "name": "Address",
       "fields": [
         {"name": "city", "type": "string", "default": "unknown"},
         {"name": "country",
          "type": {"type": "enum", "name": "Country", "symbols": ["FR", "JP"]}
         }
       ]
     }
    }
  ]
}
"#;

        let schema = Schema::parse_str(raw_schema).unwrap();
        let mut deps = deps_stack(&schema, vec![]);

        let s = deps.pop().unwrap();
        assert!(matches!(s, Schema::Enum{ name: Name { ref name, ..}, ..} if name == "Country"));

        let s = deps.pop().unwrap();
        assert!(matches!(s, Schema::Record{ name: Name { ref name, ..}, ..} if name == "Address"));

        let s = deps.pop().unwrap();
        assert!(matches!(s, Schema::Record{ name: Name { ref name, ..}, ..} if name == "User"));

        let s = deps.pop();
        assert!(matches!(s, None));
    }

    #[test]
    fn cross_deps() -> std::result::Result<(), Box<dyn std::error::Error>> {
        use std::fs::File;
        use std::io::Write;
        use tempfile::tempdir;

        let dir = tempdir()?;

        let mut schema_a_file = File::create(dir.path().join("schema_a.avsc"))?;
        let schema_a_str = r#"
{
  "name": "A",
  "type": "record",
  "fields": [ {"name": "field_one", "type": "float"} ]
}
"#;
        schema_a_file.write_all(schema_a_str.as_bytes())?;

        let mut schema_b_file = File::create(dir.path().join("schema_b.avsc"))?;
        let schema_b_str = r#"
{
  "name": "B",
  "type": "record",
  "fields": [ {"name": "field_one", "type": "A"} ]
}
"#;
        schema_b_file.write_all(schema_b_str.as_bytes())?;

        let expected = r#"
#[derive(Debug, PartialEq, Clone, serde::Deserialize, serde::Serialize)]
pub struct B {
    pub field_one: A,
}

#[derive(Debug, PartialEq, Clone, serde::Deserialize, serde::Serialize)]
pub struct A {
    pub field_one: f32,
}
"#;

        let pattern = format!("{}/*.avsc", dir.path().display());
        let source = Source::GlobPattern(pattern.as_str());
        let g = Generator::new()?;
        let mut buf = vec![];
        g.gen(&source, &mut buf)?;
        let res = String::from_utf8(buf)?;
        println!("{}", res);

        assert_eq!(expected, res);

        drop(schema_a_file);
        drop(schema_b_file);
        dir.close()?;
        Ok(())
    }
}
