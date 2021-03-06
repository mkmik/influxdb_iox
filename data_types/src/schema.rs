//! This module contains the schema definiton for IOx
use snafu::{ResultExt, Snafu};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    convert::{TryFrom, TryInto},
    fmt,
    sync::Arc,
};

use arrow_deps::arrow::datatypes::{
    DataType as ArrowDataType, Field as ArrowField, Schema as ArrowSchema,
    SchemaRef as ArrowSchemaRef,
};

pub const TIME_COLUMN_NAME: &str = "time";

pub mod builder;

/// Database schema creation / validation errors.
#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Error validating schema: '{}' is both a field and a tag", column_name,))]
    BothFieldAndTag { column_name: String },

    #[snafu(display("Error: Duplicate column name found in schema: '{}'", column_name,))]
    DuplicateColumnName { column_name: String },

    #[snafu(display(
        "Error: Incompatible metadata type found in schema for column '{}'. Metadata specified {:?} which is incompatible with actual type {:?}",
        column_name, influxdb_column_type, actual_type
    ))]
    IncompatibleMetadata {
        column_name: String,
        influxdb_column_type: InfluxColumnType,
        actual_type: ArrowDataType,
    },

    #[snafu(display(
        "Duplicate column name: '{}' was specified to be {:?} as well as timestamp",
        column_name,
        existing_type
    ))]
    InvalidTimestamp {
        column_name: String,
        existing_type: InfluxColumnType,
    },

    #[snafu(display(
        "Schema Merge Error: Incompatible measurement names. Existing measurement name '{}', new measurement name '{}'",
        existing_measurement, new_measurement
    ))]
    TryMergeDifferentMeasurementNames {
        existing_measurement: String,
        new_measurement: String,
    },

    #[snafu(display(
        "Schema Merge Error: Incompatible column type for '{}'. Existing type {:?}, new type {:?}",
        field_name,
        influx_column_type,
        existing_influx_column_type
    ))]
    TryMergeBadColumnType {
        field_name: String,
        existing_influx_column_type: Option<InfluxColumnType>,
        influx_column_type: Option<InfluxColumnType>,
    },

    #[snafu(display(
        "Schema Merge Error: Incompatible data type for '{}'. Existing type {:?}, new type {:?}",
        field_name,
        existing_data_type,
        new_data_type
    ))]
    TryMergeBadArrowType {
        field_name: String,
        existing_data_type: ArrowDataType,
        new_data_type: ArrowDataType,
    },

    #[snafu(display(
        "Schema Merge Error: Incompatible nullability for '{}'. Existing field {}, new field {}",
        field_name, nullable_to_str(*existing_nullability), nullable_to_str(*new_nullability)
    ))]
    TryMergeBadNullability {
        field_name: String,
        existing_nullability: bool,
        new_nullability: bool,
    },

    #[snafu(display("Schema Merge: Error merging underlying schema: {}", source))]
    MergingSchemas {
        source: arrow_deps::arrow::error::ArrowError,
    },
}

fn nullable_to_str(nullability: bool) -> &'static str {
    if nullability {
        "can be null"
    } else {
        "can not be null"
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Schema for an IOx table.
///
/// This struct is a wrapper around an Arrow `SchemaRef` that knows
/// how to create and interpret the "user defined metadata" added to that schema
/// by IOx.
///
/// The metadata can be used to map back and forth to the InfluxDB
/// data model, which is described in the
/// [documentation](https://docs.influxdata.com/influxdb/v2.0/reference/syntax/line-protocol/).
///
/// Specifically, each column in the Arrow schema has a corresponding
/// InfluxDB data model type of Tag, Field or Timestamp which is stored in
/// the metadata field of the ArrowSchemaRef
#[derive(Debug, Clone, PartialEq)]
pub struct Schema {
    /// All the actual data lives on the metadata structure in
    /// `ArrowSchemaRef` and this structure knows how to access that
    /// metadata
    inner: ArrowSchemaRef,
}

impl From<Schema> for ArrowSchemaRef {
    fn from(s: Schema) -> Self {
        s.inner
    }
}

impl From<&Schema> for ArrowSchemaRef {
    fn from(s: &Schema) -> Self {
        s.inner.clone()
    }
}

impl TryFrom<ArrowSchemaRef> for Schema {
    type Error = Error;

    fn try_from(value: ArrowSchemaRef) -> Result<Self, Self::Error> {
        Self::try_from_arrow(value)
    }
}

const MEASUREMENT_METADATA_KEY: &str = "iox::measurement::name";

impl Schema {
    /// Create a new Schema wrapper over the schema
    ///
    /// All metadata validation is done on creation (todo maybe offer
    /// a fallable version where the checks are done on access)?
    fn try_from_arrow(inner: ArrowSchemaRef) -> Result<Self> {
        // All column names must be unique
        let mut field_names = BTreeSet::new();
        for f in inner.fields() {
            if field_names.contains(f.name()) {
                return DuplicateColumnName {
                    column_name: f.name(),
                }
                .fail();
            }
            field_names.insert(f.name());
        }

        let schema = Self { inner };

        // for each field, ensure any type specified by the metadata
        // is compatible with the actual type of the field
        for (influxdb_column_type, field) in schema.iter() {
            if let Some(influxdb_column_type) = influxdb_column_type {
                let actual_type = field.data_type();
                if !influxdb_column_type.valid_arrow_type(actual_type) {
                    return IncompatibleMetadata {
                        column_name: field.name(),
                        influxdb_column_type,
                        actual_type: actual_type.clone(),
                    }
                    .fail();
                }
            }
        }
        Ok(schema)
    }

    /// Create and validate a new Schema, creating metadata to
    /// represent the the various parts. This method is intended to be
    /// used only by the SchemaBuilder.
    ///
    /// fields: the column definitions, in order
    ///
    /// tag columns: names of any columns which are tags
    ///
    /// field columns: names of any columns which are fields, and
    /// their associated InfluxDB data model types
    pub(crate) fn new_from_parts(
        measurement: Option<String>,
        fields: Vec<ArrowField>,
        tag_cols: HashSet<String>,
        field_cols: HashMap<String, InfluxColumnType>,
        time_col: Option<String>,
    ) -> Result<Self> {
        let mut metadata = HashMap::new();

        for tag_name in tag_cols.into_iter() {
            metadata.insert(tag_name, InfluxColumnType::Tag.to_string());
        }

        // Ensure we don't have columns that were specified to be both fields and tags
        for (column_name, influxdb_column_type) in field_cols.into_iter() {
            if metadata.get(&column_name).is_some() {
                return BothFieldAndTag { column_name }.fail();
            }
            metadata.insert(column_name, influxdb_column_type.to_string());
        }

        // Ensure we didn't ask the field to be both a timestamp and a field or tag
        if let Some(column_name) = time_col {
            if let Some(existing_type) = metadata.get(&column_name) {
                let existing_type: InfluxColumnType = existing_type.as_str().try_into().unwrap();
                return InvalidTimestamp {
                    column_name,
                    existing_type,
                }
                .fail();
            }
            metadata.insert(column_name, InfluxColumnType::Timestamp.to_string());
        }

        if let Some(measurement) = measurement {
            metadata.insert(MEASUREMENT_METADATA_KEY.to_string(), measurement);
        }

        // Call new_from_arrow to do normal, additional validation
        // (like dupe column detection)
        ArrowSchemaRef::new(ArrowSchema::new_with_metadata(fields, metadata)).try_into()
    }

    /// Provide a reference to the underlying Arrow Schema object
    pub fn inner(&self) -> &ArrowSchemaRef {
        &self.inner
    }

    /// Return the InfluxDB data model type, if any, and underlying arrow
    /// schema field for the column at index `idx`. Panics if `idx` is
    /// greater than or equal to self.len()
    ///
    /// if there is no corresponding influx metadata,
    /// returns None for the influxdb_column_type
    pub fn field(&self, idx: usize) -> (Option<InfluxColumnType>, &ArrowField) {
        let field = self.inner.field(idx);

        // Lookup and translate metadata type, if present
        // invalid metadata was detected and reported as part of the constructor
        let influxdb_column_type = self
            .inner
            .metadata()
            .get(field.name())
            .and_then(|influxdb_column_type_str| influxdb_column_type_str.as_str().try_into().ok());

        (influxdb_column_type, field)
    }

    /// Find the index of the column with the given name, if any.
    pub fn find_index_of(&self, name: &str) -> Option<usize> {
        self.inner.index_of(name).ok()
    }

    /// Provides the InfluxDB data model measurement name for this schema, if
    /// any
    pub fn measurement(&self) -> Option<&String> {
        self.inner.metadata().get(MEASUREMENT_METADATA_KEY)
    }

    /// Returns the number of columns defined in this schema
    pub fn len(&self) -> usize {
        self.inner.fields().len()
    }

    /// Returns the number of columns defined in this schema
    pub fn is_empty(&self) -> bool {
        self.inner.fields().is_empty()
    }

    /// Returns an iterator over all the columns of this schema, in order
    pub fn iter(&self) -> SchemaIter<'_> {
        SchemaIter {
            schema: self,
            idx: 0,
        }
    }

    /// Merges any new columns from new_schema, consuming self. If the
    /// column already exists, self is unchanged. If the column
    /// definition conflicts with a prior definition, an error is
    /// returned.
    pub fn try_merge(self, other: Self) -> Result<Self> {
        // Optimize for the common case of the same schema
        let mut need_merge = false;

        // Do our own pre-checks of the new fields to make nicer error messages
        if let (Some(existing_measurement), Some(new_measurement)) =
            (self.measurement(), other.measurement())
        {
            if existing_measurement != new_measurement {
                return TryMergeDifferentMeasurementNames {
                    existing_measurement,
                    new_measurement,
                }
                .fail();
            }
        }

        // if one side has a measurement and the other doesn't need to merge
        if self.measurement() != other.measurement() {
            need_merge = true;
        }

        other
            .iter()
            .filter_map(|(influx_column_type, field)| {
                if let Some(idx) = self.find_index_of(field.name()) {
                    let (existing_influx_column_type, existing_field) = self.field(idx);
                    Some((
                        existing_influx_column_type,
                        existing_field,
                        influx_column_type,
                        field,
                    ))
                } else {
                    // new field
                    need_merge = true;
                    None
                }
            })
            .try_for_each(
                |(existing_influx_column_type, existing_field, influx_column_type, field)| {
                    let field_name = field.name();

                    // for now, insist the types are exactly the same
                    // (e.g. None and Some(..) don't match). We could
                    // consider relaxing this constrait
                    if existing_influx_column_type != influx_column_type {
                        TryMergeBadColumnType {
                            field_name,
                            existing_influx_column_type,
                            influx_column_type,
                        }
                        .fail()
                    } else if field.data_type() != existing_field.data_type() {
                        TryMergeBadArrowType {
                            field_name,
                            existing_data_type: existing_field.data_type().clone(),
                            new_data_type: field.data_type().clone(),
                        }
                        .fail()
                    } else if field.is_nullable() != existing_field.is_nullable() {
                        TryMergeBadNullability {
                            field_name,
                            existing_nullability: existing_field.is_nullable(),
                            new_nullability: field.is_nullable(),
                        }
                        .fail()
                    } else {
                        Ok(())
                    }
                },
            )?;

        let new_self = if need_merge {
            // Delegate the rest of the actual work to arrow schema
            let new_schema = ArrowSchema::try_merge(&[
                self.unwrap_to_inner_owned(),
                other.unwrap_to_inner_owned(),
            ])
            .context(MergingSchemas)?;
            Self {
                inner: Arc::new(new_schema),
            }
        } else {
            self
        };

        Ok(new_self)
    }

    fn unwrap_to_inner_owned(self) -> ArrowSchema {
        // try and avoid a clone if possible, but it might be required if the Arc is
        // shared
        match Arc::try_unwrap(self.inner) {
            Ok(schema) => schema,
            Err(schema_arc) => schema_arc.as_ref().clone(),
        }
    }

    /// Resort order of our columns lexographically by name
    pub fn sort_fields_by_name(self) -> Self {
        // pairs of (orig_index, field_ref)
        let mut sorted_fields: Vec<(usize, &ArrowField)> =
            self.inner.fields().iter().enumerate().collect();
        sorted_fields.sort_by(|a, b| a.1.name().cmp(b.1.name()));

        let is_sorted = sorted_fields
            .iter()
            .enumerate()
            .all(|(index, pair)| index == pair.0);

        if is_sorted {
            self
        } else {
            // No way at present to destructure an existing Schema so
            // we have to copy :(
            let new_fields: Vec<ArrowField> =
                sorted_fields.iter().map(|pair| pair.1).cloned().collect();

            let new_meta = self.inner.metadata().clone();
            let new_schema = ArrowSchema::new_with_metadata(new_fields, new_meta);

            Self {
                inner: Arc::new(new_schema),
            }
        }
    }
}

/// Valid types for InfluxDB data model, as defined in [the documentation]
///
/// [the documentation]: https://docs.influxdata.com/influxdb/v2.0/reference/syntax/line-protocol/
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum InfluxFieldType {
    /// 64-bit floating point number (TDB if NULLs / Nans are allowed)
    Float,
    /// 64-bit signed integer
    Integer,
    /// Unsigned 64-bit integers. Trailing u on the number specifies an unsigned
    /// integer.
    UInteger,
    /// UTF-8 encoded string
    String,
    /// true or false
    Boolean,
}

impl From<InfluxFieldType> for ArrowDataType {
    fn from(t: InfluxFieldType) -> Self {
        match t {
            InfluxFieldType::Float => Self::Float64,
            InfluxFieldType::Integer => Self::Int64,
            InfluxFieldType::UInteger => Self::UInt64,
            InfluxFieldType::String => Self::Utf8,
            InfluxFieldType::Boolean => Self::Boolean,
        }
    }
}

impl TryFrom<ArrowDataType> for InfluxFieldType {
    type Error = &'static str;

    fn try_from(value: ArrowDataType) -> Result<Self, Self::Error> {
        match value {
            ArrowDataType::Float64 => Ok(Self::Float),
            ArrowDataType::Int64 => Ok(Self::Integer),
            ArrowDataType::UInt64 => Ok(Self::UInteger),
            ArrowDataType::Utf8 => Ok(Self::String),
            ArrowDataType::Boolean => Ok(Self::Boolean),
            _ => Err("No corresponding type in the InfluxDB data model"),
        }
    }
}

/// Valid types for fields in the InfluxDB data model, as described in the
/// [documentation](https://docs.influxdata.com/influxdb/v2.0/reference/syntax/line-protocol/).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum InfluxColumnType {
    /// Tag
    ///
    /// Note: tags are always stored as a Utf8, but eventually this
    /// should allow for both Utf8 and Dictionary
    Tag,

    /// Field: Data of type in InfluxDB Data model
    Field(InfluxFieldType),

    /// Timestamp
    ///
    /// 64 bit timestamp "UNIX timestamps" representing nanosecods
    /// since the UNIX epoch (00:00:00 UTC on 1 January 1970).
    Timestamp,
}

impl InfluxColumnType {
    /// returns true if `arrow_type` can validly store this column type
    pub fn valid_arrow_type(&self, data_type: &ArrowDataType) -> bool {
        // Note this function is forward looking and imagines the day
        // when types like `Tag` can be stored as Utf8 or various
        // StringDictionary types.
        let default_type: ArrowDataType = self.into();
        data_type == &default_type
    }
}

/// "serialization" to strings that are stored in arrow metadata
impl From<&InfluxColumnType> for &'static str {
    fn from(t: &InfluxColumnType) -> Self {
        match t {
            InfluxColumnType::Tag => "iox::column_type::tag",
            InfluxColumnType::Field(InfluxFieldType::Float) => "iox::column_type::field::float",
            InfluxColumnType::Field(InfluxFieldType::Integer) => "iox::column_type::field::integer",
            InfluxColumnType::Field(InfluxFieldType::UInteger) => {
                "iox::column_type::field::uinteger"
            }
            InfluxColumnType::Field(InfluxFieldType::String) => "iox::column_type::field::string",
            InfluxColumnType::Field(InfluxFieldType::Boolean) => "iox::column_type::field::boolean",
            InfluxColumnType::Timestamp => "iox::column_type::timestamp",
        }
    }
}

impl ToString for InfluxColumnType {
    fn to_string(&self) -> String {
        let s: &str = self.into();
        s.into()
    }
}

/// "deserialization" from strings that are stored in arrow metadata
impl TryFrom<&str> for InfluxColumnType {
    type Error = String;
    /// this is the inverse of converting to &str
    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "iox::column_type::tag" => Ok(Self::Tag),
            "iox::column_type::field::float" => Ok(Self::Field(InfluxFieldType::Float)),
            "iox::column_type::field::integer" => Ok(Self::Field(InfluxFieldType::Integer)),
            "iox::column_type::field::uinteger" => Ok(Self::Field(InfluxFieldType::UInteger)),
            "iox::column_type::field::string" => Ok(Self::Field(InfluxFieldType::String)),
            "iox::column_type::field::boolean" => Ok(Self::Field(InfluxFieldType::Boolean)),
            "iox::column_type::timestamp" => Ok(Self::Timestamp),
            _ => Err(format!("Unknown column type in metadata: {:?}", s)),
        }
    }
}

impl From<&InfluxColumnType> for ArrowDataType {
    /// What arrow type is used for this column type?
    fn from(t: &InfluxColumnType) -> Self {
        match t {
            InfluxColumnType::Tag => Self::Utf8,
            InfluxColumnType::Field(influxdb_field_type) => (*influxdb_field_type).into(),
            InfluxColumnType::Timestamp => Self::Int64,
        }
    }
}

/// Thing that implements iterator over a Schema's columns.
pub struct SchemaIter<'a> {
    schema: &'a Schema,
    idx: usize,
}

impl<'a> fmt::Debug for SchemaIter<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SchemaIter<{}>", self.idx)
    }
}

impl<'a> Iterator for SchemaIter<'a> {
    type Item = (Option<InfluxColumnType>, &'a ArrowField);

    fn next(&mut self) -> Option<Self::Item> {
        if self.idx < self.schema.len() {
            let ret = self.schema.field(self.idx);
            self.idx += 1;
            Some(ret)
        } else {
            None
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, Some(self.schema.len()))
    }
}

/// Asserts that the result of calling Schema:field(i) is as expected:
///
/// example
///   assert_column_eq!(schema, 0, InfluxColumnType::Tag, "host");
#[macro_export]
macro_rules! assert_column_eq {
    ($schema:expr, $i:expr, $expected_influxdb_column_type:expr, $expected_field_name:expr) => {
        let (influxdb_column_type, arrow_field) = $schema.field($i);
        assert_eq!(
            influxdb_column_type,
            Some($expected_influxdb_column_type),
            "Line protocol column mismatch for column {}, field {:?}, in schema {:#?}",
            $i,
            arrow_field,
            $schema
        );
        assert_eq!(
            arrow_field.name(),
            $expected_field_name,
            "expected field name mismatch for column {}, field {:?}, in schema {:#?}",
            $i,
            arrow_field,
            $schema
        )
    };
}

#[cfg(test)]
mod test {
    use super::{builder::SchemaBuilder, *};
    use InfluxColumnType::*;
    use InfluxFieldType::*;

    #[test]
    fn new_from_arrow_no_metadata() {
        let arrow_schema = ArrowSchemaRef::new(ArrowSchema::new(vec![
            ArrowField::new("col1", ArrowDataType::Int64, false),
            ArrowField::new("col2", ArrowDataType::Utf8, false),
        ]));

        // Given a schema created from arrow record batch with no metadata
        let schema: Schema = arrow_schema.clone().try_into().unwrap();
        assert_eq!(schema.len(), 2);

        // It still works, but has no lp column types
        let (influxdb_column_type, field) = schema.field(0);
        assert_eq!(field.name(), "col1");
        assert_eq!(field, arrow_schema.field(0));
        assert_eq!(influxdb_column_type, None);

        let (influxdb_column_type, field) = schema.field(1);
        assert_eq!(field.name(), "col2");
        assert_eq!(field, arrow_schema.field(1));
        assert_eq!(influxdb_column_type, None);
    }

    #[test]
    fn new_from_arrow_metadata_good() {
        let fields = vec![
            ArrowField::new("tag_col", ArrowDataType::Utf8, false),
            ArrowField::new("int_col", ArrowDataType::Int64, false),
            ArrowField::new("uint_col", ArrowDataType::UInt64, false),
            ArrowField::new("float_col", ArrowDataType::Float64, false),
            ArrowField::new("str_col", ArrowDataType::Utf8, false),
            ArrowField::new("bool_col", ArrowDataType::Boolean, false),
            ArrowField::new("time_col", ArrowDataType::Int64, false),
        ];

        let metadata: HashMap<_, _> = vec![
            ("tag_col", "iox::column_type::tag"),
            ("int_col", "iox::column_type::field::integer"),
            ("uint_col", "iox::column_type::field::uinteger"),
            ("float_col", "iox::column_type::field::float"),
            ("str_col", "iox::column_type::field::string"),
            ("bool_col", "iox::column_type::field::boolean"),
            ("time_col", "iox::column_type::timestamp"),
            ("iox::measurement::name", "the_measurement"),
        ]
        .into_iter()
        .map(|i| (i.0.to_string(), i.1.to_string()))
        .collect();

        let arrow_schema = ArrowSchemaRef::new(ArrowSchema::new_with_metadata(fields, metadata));

        let schema: Schema = arrow_schema.try_into().unwrap();
        assert_column_eq!(schema, 0, Tag, "tag_col");
        assert_column_eq!(schema, 1, Field(Integer), "int_col");
        assert_column_eq!(schema, 2, Field(UInteger), "uint_col");
        assert_column_eq!(schema, 3, Field(Float), "float_col");
        assert_column_eq!(schema, 4, Field(String), "str_col");
        assert_column_eq!(schema, 5, Field(Boolean), "bool_col");
        assert_column_eq!(schema, 6, Timestamp, "time_col");
        assert_eq!(schema.len(), 7);

        assert_eq!(schema.measurement().unwrap(), "the_measurement");
    }

    #[test]
    fn new_from_arrow_metadata_extra() {
        let fields = vec![
            ArrowField::new("tag_col", ArrowDataType::Utf8, false),
            ArrowField::new("int_col", ArrowDataType::Int64, false),
        ];

        // This metadata models metadata that was not created by this
        // rust module itself
        let metadata: HashMap<_, _> = vec![
            ("tag_col", "something_other_than_iox"),
            ("int_col", "iox::column_type::field::some_new_exotic_type"),
            ("non_existent_col", "iox::column_type::field::float"),
            ("iox::some::new::key", "foo"),
        ]
        .into_iter()
        .map(|i| (i.0.to_string(), i.1.to_string()))
        .collect();

        let arrow_schema = ArrowSchemaRef::new(ArrowSchema::new_with_metadata(fields, metadata));

        // Having this succeed is the primary test
        let schema: Schema = arrow_schema.try_into().unwrap();

        let (influxdb_column_type, field) = schema.field(0);
        assert_eq!(field.name(), "tag_col");
        assert_eq!(influxdb_column_type, None);

        let (influxdb_column_type, field) = schema.field(1);
        assert_eq!(field.name(), "int_col");
        assert_eq!(influxdb_column_type, None);
    }

    // mismatched metadata / arrow types
    #[test]
    fn new_from_arrow_metadata_mismatched_tag() {
        let fields = vec![
            ArrowField::new("tag_col", ArrowDataType::Int64, false), // not a valid tag type
        ];

        let metadata: HashMap<_, _> = vec![
            ("tag_col", "iox::column_type::tag"), /* claims that tag_col is a tag, but it is an
                                                   * integer */
        ]
        .into_iter()
        .map(|i| (i.0.to_string(), i.1.to_string()))
        .collect();

        let arrow_schema = ArrowSchemaRef::new(ArrowSchema::new_with_metadata(fields, metadata));

        let res = Schema::try_from_arrow(arrow_schema);
        assert_eq!(res.unwrap_err().to_string(), "Error: Incompatible metadata type found in schema for column 'tag_col'. Metadata specified Tag which is incompatible with actual type Int64");
    }

    // mismatched metadata / arrow types
    #[test]
    fn new_from_arrow_metadata_mismatched_field() {
        let fields = vec![ArrowField::new("int_col", ArrowDataType::Int64, false)];

        let metadata: HashMap<_, _> = vec![
            ("int_col", "iox::column_type::field::float"), // metadata claims it is a float
        ]
        .into_iter()
        .map(|i| (i.0.to_string(), i.1.to_string()))
        .collect();

        let arrow_schema = ArrowSchemaRef::new(ArrowSchema::new_with_metadata(fields, metadata));

        let res = Schema::try_from_arrow(arrow_schema);
        assert_eq!(res.unwrap_err().to_string(), "Error: Incompatible metadata type found in schema for column 'int_col'. Metadata specified Field(Float) which is incompatible with actual type Int64");
    }

    // mismatched metadata / arrow types
    #[test]
    fn new_from_arrow_metadata_mismatched_timestamp() {
        let fields = vec![
            ArrowField::new("time", ArrowDataType::Utf8, false), // timestamp can't be strings
        ];

        let metadata: HashMap<_, _> = vec![
            ("time", "iox::column_type::timestamp"), // metadata claims it is a timstam
        ]
        .into_iter()
        .map(|i| (i.0.to_string(), i.1.to_string()))
        .collect();

        let arrow_schema = ArrowSchemaRef::new(ArrowSchema::new_with_metadata(fields, metadata));

        let res = Schema::try_from_arrow(arrow_schema);
        assert_eq!(res.unwrap_err().to_string(), "Error: Incompatible metadata type found in schema for column 'time'. Metadata specified Timestamp which is incompatible with actual type Utf8");
    }

    #[test]
    fn new_from_arrow_replicated_columns() {
        // arrow allows duplicated colum names
        let fields = vec![
            ArrowField::new("the_column", ArrowDataType::Utf8, false),
            ArrowField::new("another_column", ArrowDataType::Utf8, false),
            ArrowField::new("the_column", ArrowDataType::Utf8, false),
        ];

        let arrow_schema = ArrowSchemaRef::new(ArrowSchema::new(fields));

        let res = Schema::try_from_arrow(arrow_schema);
        assert_eq!(
            res.unwrap_err().to_string(),
            "Error: Duplicate column name found in schema: 'the_column'"
        );
    }

    #[test]
    fn test_round_trip() {
        let schema1 = SchemaBuilder::new()
            .influx_field("the_field", String)
            .tag("the_tag")
            .timestamp()
            .measurement("the_measurement")
            .build()
            .unwrap();

        // Make a new schema via ArrowSchema (serialized metadata) to ensure that
        // the metadata makes it through a round trip

        let arrow_schema_1: ArrowSchemaRef = schema1.clone().into();
        let schema2 = Schema::try_from_arrow(arrow_schema_1).unwrap();

        for s in &[schema1, schema2] {
            assert_eq!(s.measurement().unwrap(), "the_measurement");
            assert_column_eq!(s, 0, Field(String), "the_field");
            assert_column_eq!(s, 1, Tag, "the_tag");
            assert_column_eq!(s, 2, Timestamp, "time");
            assert_eq!(3, s.len());
        }
    }

    #[test]
    fn test_iter() {
        let schema = SchemaBuilder::new()
            .influx_field("the_field", String)
            .tag("the_tag")
            .timestamp()
            .measurement("the_measurement")
            .build()
            .unwrap();

        // test schema iterator and field accessor match up
        for (i, (iter_col_type, iter_field)) in schema.iter().enumerate() {
            let (col_type, field) = schema.field(i);
            assert_eq!(iter_col_type, col_type);
            assert_eq!(iter_field, field);
        }
        assert_eq!(schema.iter().count(), 3);
    }

    #[test]
    fn test_merge_compatible_schema() {
        let schema1 = SchemaBuilder::new()
            .tag("the_tag")
            .influx_field("int_field", Integer)
            .build()
            .unwrap();

        // has some of the same and some new, different fields
        let schema2 = SchemaBuilder::new()
            .measurement("my_measurement")
            .tag("the_other_tag")
            .influx_field("int_field", Integer)
            .influx_field("another_field", Integer)
            .build()
            .unwrap();

        let merged_schema = schema1.try_merge(schema2).unwrap();

        let expected_schema = SchemaBuilder::new()
            .measurement("my_measurement")
            .tag("the_tag")
            .influx_field("int_field", Integer)
            .tag("the_other_tag")
            .influx_field("another_field", Integer)
            .build()
            .unwrap();

        assert_eq!(
            expected_schema, merged_schema,
            "\nExpected:\n{:#?}\nActual:\n{:#?}",
            expected_schema, merged_schema
        );
    }

    #[test]
    fn test_merge_compatible_schema_no_names() {
        let schema1 = SchemaBuilder::new().tag("the_tag").build().unwrap();

        // has some of the same and some new, different fields
        let schema2 = SchemaBuilder::new().tag("the_other_tag").build().unwrap();

        // ensure the merge is not optimized away
        let merged_schema = schema1.try_merge(schema2).unwrap();

        let expected_schema = SchemaBuilder::new()
            .tag("the_tag")
            .tag("the_other_tag")
            .build()
            .unwrap();

        assert_eq!(
            expected_schema, merged_schema,
            "\nExpected:\n{:#?}\nActual:\n{:#?}",
            expected_schema, merged_schema
        );
    }

    #[test]
    fn test_merge_compatible_schema_only_measurement() {
        let schema1 = SchemaBuilder::new()
            .tag("the_tag")
            .measurement("the_measurement")
            .build()
            .unwrap();

        // schema has same fields but not measurement name
        let schema2 = SchemaBuilder::new().tag("the_tag").build().unwrap();

        // ensure the merge is not optimized away
        let merged_schema = schema1.try_merge(schema2).unwrap();

        let expected_schema = SchemaBuilder::new()
            .tag("the_tag")
            .measurement("the_measurement")
            .build()
            .unwrap();

        assert_eq!(
            expected_schema, merged_schema,
            "\nExpected:\n{:#?}\nActual:\n{:#?}",
            expected_schema, merged_schema
        );
    }

    #[test]
    fn test_merge_measurement_names() {
        let schema1 = SchemaBuilder::new().tag("the_tag").build().unwrap();

        // has some of the same and some different fields
        let schema2 = SchemaBuilder::new()
            .measurement("my_measurement")
            .build()
            .unwrap();

        let merged_schema = schema1.try_merge(schema2).unwrap();

        let expected_schema = SchemaBuilder::new()
            .measurement("my_measurement")
            .tag("the_tag")
            .build()
            .unwrap();

        assert_eq!(
            expected_schema, merged_schema,
            "\nExpected:\n{:#?}\nActual:\n{:#?}",
            expected_schema, merged_schema
        );
    }

    #[test]
    fn test_merge_incompatible_schema_measurement_names() {
        let schema1 = SchemaBuilder::new()
            .tag("the_tag")
            .measurement("measurement1")
            .build()
            .unwrap();

        // different measurement name, same otherwise
        let schema2 = SchemaBuilder::new()
            .tag("the_tag")
            .measurement("measurement2")
            .build()
            .unwrap();

        let merged_schema_error = schema1.try_merge(schema2).unwrap_err();

        assert_eq!(
            merged_schema_error.to_string(),
            "Schema Merge Error: Incompatible measurement names. Existing measurement name 'measurement1', new measurement name 'measurement2'"
        );
    }

    #[test]
    fn test_merge_incompatible_data_types() {
        // same field name with different type
        let schema1 = SchemaBuilder::new()
            .field("the_field", ArrowDataType::Int16)
            .build()
            .unwrap();

        // same field name with different type
        let schema2 = SchemaBuilder::new()
            .field("the_field", ArrowDataType::Int8)
            .build()
            .unwrap();

        let merged_schema_error = schema1.try_merge(schema2).unwrap_err();

        assert_eq!(merged_schema_error.to_string(), "Schema Merge Error: Incompatible data type for 'the_field'. Existing type Int16, new type Int8");
    }

    #[test]
    fn test_merge_incompatible_column_types() {
        let schema1 = SchemaBuilder::new().tag("the_tag").build().unwrap();

        // same field name with different type
        let schema2 = SchemaBuilder::new()
            .influx_field("the_tag", Integer)
            .build()
            .unwrap();

        let merged_schema_error = schema1.try_merge(schema2).unwrap_err();

        assert_eq!(merged_schema_error.to_string(), "Schema Merge Error: Incompatible column type for 'the_tag'. Existing type Some(Field(Integer)), new type Some(Tag)");
    }

    #[test]
    fn test_merge_incompatible_schema_nullability() {
        let schema1 = SchemaBuilder::new()
            .non_null_field("int_field", ArrowDataType::Int64)
            .build()
            .unwrap();

        // same field name with different nullability
        let schema2 = SchemaBuilder::new()
            .field("int_field", ArrowDataType::Int64)
            .build()
            .unwrap();

        let merged_schema_error = schema1.try_merge(schema2).unwrap_err();

        assert_eq!(merged_schema_error.to_string(), "Schema Merge Error: Incompatible nullability for 'int_field'. Existing field can not be null, new field can be null");
    }

    #[test]
    fn test_sort_fields_by_name_already_sorted() {
        let schema = SchemaBuilder::new()
            .field("field_a", ArrowDataType::Int64)
            .field("field_b", ArrowDataType::Int64)
            .field("field_c", ArrowDataType::Int64)
            .build()
            .unwrap();

        let sorted_schema = schema.clone().sort_fields_by_name();

        assert_eq!(
            schema, sorted_schema,
            "\nExpected:\n{:#?}\nActual:\n{:#?}",
            schema, sorted_schema
        );
    }

    #[test]
    fn test_sort_fields_by_name() {
        let schema = SchemaBuilder::new()
            .field("field_b", ArrowDataType::Int64)
            .field("field_a", ArrowDataType::Int64)
            .field("field_c", ArrowDataType::Int64)
            .build()
            .unwrap();

        let sorted_schema = schema.sort_fields_by_name();

        let expected_schema = SchemaBuilder::new()
            .field("field_a", ArrowDataType::Int64)
            .field("field_b", ArrowDataType::Int64)
            .field("field_c", ArrowDataType::Int64)
            .build()
            .unwrap();

        assert_eq!(
            expected_schema, sorted_schema,
            "\nExpected:\n{:#?}\nActual:\n{:#?}",
            expected_schema, sorted_schema
        );
    }
}
