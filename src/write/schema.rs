//! The output table: Arrow schema, the row struct pass two fills, and the
//! column builders that turn rows into a RecordBatch.
//!
//! Column names follow the FHIR path they come from. See README "Columns".

use std::collections::HashMap;
use std::sync::Arc;

use arrow_array::builder::{
    BinaryBuilder, Float64Builder, Int32Builder, ListBuilder, StringBuilder, StructBuilder,
};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{DataType, Field, Fields, Schema, SchemaRef};

use crate::index::ADMIN_COLUMNS;

pub const GEOMETRY_COLUMN: &str = "geometry";
pub const BBOX_COLUMN: &str = "bbox";

fn utf8(name: &str) -> Field {
    Field::new(name, DataType::Utf8, true)
}

fn list_utf8(name: &str) -> Field {
    Field::new(
        name,
        DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
        true,
    )
}

pub fn identifier_fields() -> Fields {
    vec![
        Field::new("system", DataType::Utf8, true),
        Field::new("value", DataType::Utf8, true),
    ]
    .into()
}

pub fn bbox_fields() -> Fields {
    vec![
        Field::new("xmin", DataType::Float64, false),
        Field::new("ymin", DataType::Float64, false),
        Field::new("xmax", DataType::Float64, false),
        Field::new("ymax", DataType::Float64, false),
    ]
    .into()
}

pub fn geometry_field() -> Field {
    let mut meta = HashMap::new();
    meta.insert(
        "ARROW:extension:name".to_string(),
        "geoarrow.wkb".to_string(),
    );
    meta.insert(
        "ARROW:extension:metadata".to_string(),
        r#"{"crs":"OGC:CRS84","crs_type":"authority_code"}"#.to_string(),
    );
    Field::new(GEOMETRY_COLUMN, DataType::Binary, true).with_metadata(meta)
}

/// The schema every partition file is written with.
pub fn output_schema() -> SchemaRef {
    schema(false)
}

/// The schema for `fhir-pipes transform locations --keep-without-geometry`: the same
/// columns, with `geom_type`, `lon`, `lat` and `bbox` nullable so a
/// Location with no point and no boundary can have a row. `geometry` is
/// nullable in both.
pub fn output_schema_optional_geometry() -> SchemaRef {
    schema(true)
}

fn schema(optional_geometry: bool) -> SchemaRef {
    let mut fields = vec![
        Field::new("id", DataType::Utf8, false),
        utf8("version_id"),
        utf8("last_updated"),
        utf8("name"),
        list_utf8("alias"),
        utf8("status"),
        utf8("description"),
        utf8("type"),
        utf8("physical_type"),
        utf8("part_of"),
        utf8("managing_organization"),
        Field::new(
            "identifier",
            DataType::List(Arc::new(Field::new(
                "item",
                DataType::Struct(identifier_fields()),
                true,
            ))),
            true,
        ),
        Field::new("position_longitude", DataType::Float64, true),
        Field::new("position_latitude", DataType::Float64, true),
        utf8("pcode"),
        utf8("gers_id"),
        utf8("settlement_type"),
        utf8("delivery_strategy"),
        utf8("quadkey"),
        Field::new("quadkey_level", DataType::Int32, true),
        utf8("facility_level"),
        utf8("ownership"),
        utf8("nhfr_code"),
        utf8("nhfr_uid"),
        Field::new(
            "organization_identifier",
            DataType::List(Arc::new(Field::new(
                "item",
                DataType::Struct(identifier_fields()),
                true,
            ))),
            true,
        ),
        utf8("facility_level_text"),
        utf8("ownership_text"),
        Field::new("depth", DataType::Int32, false),
        Field::new("admin_level", DataType::Int32, true),
        Field::new("tier", DataType::Utf8, false),
        Field::new("path", DataType::Utf8, false),
        list_utf8("ancestor_ids"),
    ];
    for i in 0..ADMIN_COLUMNS {
        fields.push(utf8(&format!("admin{i}_name")));
    }
    for i in 0..ADMIN_COLUMNS {
        fields.push(utf8(&format!("admin{i}_code")));
    }
    fields.extend([
        list_utf8("overlays_admin_unit_ids"),
        utf8("organization_json"),
        Field::new("country", DataType::Utf8, false),
        Field::new("geom_type", DataType::Utf8, optional_geometry),
        Field::new("lon", DataType::Float64, optional_geometry),
        Field::new("lat", DataType::Float64, optional_geometry),
        geometry_field(),
        Field::new(
            BBOX_COLUMN,
            DataType::Struct(bbox_fields()),
            optional_geometry,
        ),
        Field::new("fhir_json", DataType::Utf8, false),
    ]);
    Arc::new(Schema::new(fields))
}

/// One output row, fully resolved. Built by pass two.
#[derive(Debug, Clone, Default)]
pub struct OutputRow {
    pub id: String,
    pub version_id: Option<String>,
    pub last_updated: Option<String>,
    pub name: Option<String>,
    pub alias: Vec<String>,
    pub status: Option<String>,
    pub description: Option<String>,
    pub type_code: Option<String>,
    pub physical_type: Option<String>,
    pub part_of: Option<String>,
    pub managing_organization: Option<String>,
    /// (system, value) pairs; either side may be absent in the source.
    pub identifier: Vec<(Option<String>, Option<String>)>,
    pub position: Option<(f64, f64)>,
    pub pcode: Option<String>,
    pub gers_id: Option<String>,
    pub settlement_type: Option<String>,
    pub delivery_strategy: Option<String>,
    /// The deepest quadkey cell on the Location (spatial-index extension);
    /// its prefixes are every coarser tile. Read-only: derived from position.
    pub quadkey: Option<String>,
    pub quadkey_level: Option<i32>,
    pub facility_level: Option<String>,
    pub ownership: Option<String>,
    pub nhfr_code: Option<String>,
    pub nhfr_uid: Option<String>,
    pub organization_identifier: Vec<(Option<String>, Option<String>)>,
    pub facility_level_text: Option<String>,
    pub ownership_text: Option<String>,
    pub depth: i32,
    pub admin_level: Option<i32>,
    pub tier: String,
    pub path: String,
    pub ancestor_ids: Vec<String>,
    pub admin_names: [Option<String>; ADMIN_COLUMNS],
    pub admin_codes: [Option<String>; ADMIN_COLUMNS],
    pub overlays_admin_unit_ids: Vec<String>,
    pub organization_json: Option<String>,
    pub country: String,
    /// `None`, with `lon`, `lat` and `bbox`, for a row without geometry;
    /// only the optional-geometry schema accepts that.
    pub geom_type: Option<String>,
    pub lon: Option<f64>,
    pub lat: Option<f64>,
    /// Empty for a row without geometry; written as null.
    pub wkb: Vec<u8>,
    pub bbox: Option<[f64; 4]>,
    pub fhir_json: String,
}

/// Column builders for one row group's worth of rows.
pub struct RowBatch {
    schema: SchemaRef,
    id: StringBuilder,
    version_id: StringBuilder,
    last_updated: StringBuilder,
    name: StringBuilder,
    alias: ListBuilder<StringBuilder>,
    status: StringBuilder,
    description: StringBuilder,
    type_code: StringBuilder,
    physical_type: StringBuilder,
    part_of: StringBuilder,
    managing_organization: StringBuilder,
    identifier: ListBuilder<StructBuilder>,
    position_longitude: Float64Builder,
    position_latitude: Float64Builder,
    pcode: StringBuilder,
    gers_id: StringBuilder,
    settlement_type: StringBuilder,
    delivery_strategy: StringBuilder,
    quadkey: StringBuilder,
    quadkey_level: Int32Builder,
    facility_level: StringBuilder,
    ownership: StringBuilder,
    nhfr_code: StringBuilder,
    nhfr_uid: StringBuilder,
    organization_identifier: ListBuilder<StructBuilder>,
    facility_level_text: StringBuilder,
    ownership_text: StringBuilder,
    depth: Int32Builder,
    admin_level: Int32Builder,
    tier: StringBuilder,
    path: StringBuilder,
    ancestor_ids: ListBuilder<StringBuilder>,
    admin_names: Vec<StringBuilder>,
    admin_codes: Vec<StringBuilder>,
    overlays: ListBuilder<StringBuilder>,
    organization_json: StringBuilder,
    country: StringBuilder,
    geom_type: StringBuilder,
    lon: Float64Builder,
    lat: Float64Builder,
    geometry: BinaryBuilder,
    bbox: StructBuilder,
    fhir_json: StringBuilder,
    pub rows: usize,
}

fn push_identifiers(
    b: &mut ListBuilder<StructBuilder>,
    items: &[(Option<String>, Option<String>)],
) {
    let sb = b.values();
    for (system, value) in items {
        sb.field_builder::<StringBuilder>(0)
            .unwrap()
            .append_option(system.as_deref());
        sb.field_builder::<StringBuilder>(1)
            .unwrap()
            .append_option(value.as_deref());
        sb.append(true);
    }
    b.append(true);
}

fn push_list(b: &mut ListBuilder<StringBuilder>, items: &[String]) {
    for item in items {
        b.values().append_value(item);
    }
    b.append(true);
}

impl RowBatch {
    pub fn new() -> Self {
        Self::with_schema(output_schema())
    }

    /// Builders for `schema`: [`output_schema`] or
    /// [`output_schema_optional_geometry`].
    pub fn with_schema(schema: SchemaRef) -> Self {
        Self {
            schema,
            id: StringBuilder::new(),
            version_id: StringBuilder::new(),
            last_updated: StringBuilder::new(),
            name: StringBuilder::new(),
            alias: ListBuilder::new(StringBuilder::new()),
            status: StringBuilder::new(),
            description: StringBuilder::new(),
            type_code: StringBuilder::new(),
            physical_type: StringBuilder::new(),
            part_of: StringBuilder::new(),
            managing_organization: StringBuilder::new(),
            identifier: ListBuilder::new(StructBuilder::from_fields(identifier_fields(), 0)),
            position_longitude: Float64Builder::new(),
            position_latitude: Float64Builder::new(),
            pcode: StringBuilder::new(),
            gers_id: StringBuilder::new(),
            settlement_type: StringBuilder::new(),
            delivery_strategy: StringBuilder::new(),
            quadkey: StringBuilder::new(),
            quadkey_level: Int32Builder::new(),
            facility_level: StringBuilder::new(),
            ownership: StringBuilder::new(),
            nhfr_code: StringBuilder::new(),
            nhfr_uid: StringBuilder::new(),
            organization_identifier: ListBuilder::new(StructBuilder::from_fields(
                identifier_fields(),
                0,
            )),
            facility_level_text: StringBuilder::new(),
            ownership_text: StringBuilder::new(),
            depth: Int32Builder::new(),
            admin_level: Int32Builder::new(),
            tier: StringBuilder::new(),
            path: StringBuilder::new(),
            ancestor_ids: ListBuilder::new(StringBuilder::new()),
            admin_names: (0..ADMIN_COLUMNS).map(|_| StringBuilder::new()).collect(),
            admin_codes: (0..ADMIN_COLUMNS).map(|_| StringBuilder::new()).collect(),
            overlays: ListBuilder::new(StringBuilder::new()),
            organization_json: StringBuilder::new(),
            country: StringBuilder::new(),
            geom_type: StringBuilder::new(),
            lon: Float64Builder::new(),
            lat: Float64Builder::new(),
            geometry: BinaryBuilder::new(),
            bbox: StructBuilder::from_fields(bbox_fields(), 0),
            fhir_json: StringBuilder::new(),
            rows: 0,
        }
    }

    pub fn push(&mut self, r: &OutputRow) {
        self.id.append_value(&r.id);
        self.version_id.append_option(r.version_id.as_deref());
        self.last_updated.append_option(r.last_updated.as_deref());
        self.name.append_option(r.name.as_deref());
        push_list(&mut self.alias, &r.alias);
        self.status.append_option(r.status.as_deref());
        self.description.append_option(r.description.as_deref());
        self.type_code.append_option(r.type_code.as_deref());
        self.physical_type.append_option(r.physical_type.as_deref());
        self.part_of.append_option(r.part_of.as_deref());
        self.managing_organization
            .append_option(r.managing_organization.as_deref());
        push_identifiers(&mut self.identifier, &r.identifier);
        self.position_longitude
            .append_option(r.position.map(|p| p.0));
        self.position_latitude
            .append_option(r.position.map(|p| p.1));
        self.pcode.append_option(r.pcode.as_deref());
        self.gers_id.append_option(r.gers_id.as_deref());
        self.settlement_type
            .append_option(r.settlement_type.as_deref());
        self.delivery_strategy
            .append_option(r.delivery_strategy.as_deref());
        self.quadkey.append_option(r.quadkey.as_deref());
        self.quadkey_level.append_option(r.quadkey_level);
        self.facility_level
            .append_option(r.facility_level.as_deref());
        self.ownership.append_option(r.ownership.as_deref());
        self.nhfr_code.append_option(r.nhfr_code.as_deref());
        self.nhfr_uid.append_option(r.nhfr_uid.as_deref());
        push_identifiers(
            &mut self.organization_identifier,
            &r.organization_identifier,
        );
        self.facility_level_text
            .append_option(r.facility_level_text.as_deref());
        self.ownership_text
            .append_option(r.ownership_text.as_deref());
        self.depth.append_value(r.depth);
        self.admin_level.append_option(r.admin_level);
        self.tier.append_value(&r.tier);
        self.path.append_value(&r.path);
        push_list(&mut self.ancestor_ids, &r.ancestor_ids);
        for ((name_builder, code_builder), (name, code)) in self
            .admin_names
            .iter_mut()
            .zip(self.admin_codes.iter_mut())
            .zip(r.admin_names.iter().zip(r.admin_codes.iter()))
        {
            name_builder.append_option(name.as_deref());
            code_builder.append_option(code.as_deref());
        }
        push_list(&mut self.overlays, &r.overlays_admin_unit_ids);
        self.organization_json
            .append_option(r.organization_json.as_deref());
        self.country.append_value(&r.country);
        self.geom_type.append_option(r.geom_type.as_deref());
        self.lon.append_option(r.lon);
        self.lat.append_option(r.lat);
        if r.wkb.is_empty() {
            self.geometry.append_null();
        } else {
            self.geometry.append_value(&r.wkb);
        }
        match r.bbox {
            Some(bbox) => {
                for (i, v) in bbox.iter().enumerate() {
                    self.bbox
                        .field_builder::<Float64Builder>(i)
                        .unwrap()
                        .append_value(*v);
                }
                self.bbox.append(true);
            }
            None => {
                // The struct is null; its children need a slot each, masked
                // by the struct's own null.
                for i in 0..4 {
                    self.bbox
                        .field_builder::<Float64Builder>(i)
                        .unwrap()
                        .append_null();
                }
                self.bbox.append(false);
            }
        }
        self.fhir_json.append_value(&r.fhir_json);
        self.rows += 1;
    }

    pub fn finish(&mut self) -> Result<RecordBatch, arrow_schema::ArrowError> {
        let mut cols: Vec<ArrayRef> = vec![
            Arc::new(self.id.finish()),
            Arc::new(self.version_id.finish()),
            Arc::new(self.last_updated.finish()),
            Arc::new(self.name.finish()),
            Arc::new(self.alias.finish()),
            Arc::new(self.status.finish()),
            Arc::new(self.description.finish()),
            Arc::new(self.type_code.finish()),
            Arc::new(self.physical_type.finish()),
            Arc::new(self.part_of.finish()),
            Arc::new(self.managing_organization.finish()),
            Arc::new(self.identifier.finish()),
            Arc::new(self.position_longitude.finish()),
            Arc::new(self.position_latitude.finish()),
            Arc::new(self.pcode.finish()),
            Arc::new(self.gers_id.finish()),
            Arc::new(self.settlement_type.finish()),
            Arc::new(self.delivery_strategy.finish()),
            Arc::new(self.quadkey.finish()),
            Arc::new(self.quadkey_level.finish()),
            Arc::new(self.facility_level.finish()),
            Arc::new(self.ownership.finish()),
            Arc::new(self.nhfr_code.finish()),
            Arc::new(self.nhfr_uid.finish()),
            Arc::new(self.organization_identifier.finish()),
            Arc::new(self.facility_level_text.finish()),
            Arc::new(self.ownership_text.finish()),
            Arc::new(self.depth.finish()),
            Arc::new(self.admin_level.finish()),
            Arc::new(self.tier.finish()),
            Arc::new(self.path.finish()),
            Arc::new(self.ancestor_ids.finish()),
        ];
        for b in &mut self.admin_names {
            cols.push(Arc::new(b.finish()));
        }
        for b in &mut self.admin_codes {
            cols.push(Arc::new(b.finish()));
        }
        cols.extend([
            Arc::new(self.overlays.finish()) as ArrayRef,
            Arc::new(self.organization_json.finish()),
            Arc::new(self.country.finish()),
            Arc::new(self.geom_type.finish()),
            Arc::new(self.lon.finish()),
            Arc::new(self.lat.finish()),
            Arc::new(self.geometry.finish()),
            Arc::new(self.bbox.finish()),
            Arc::new(self.fhir_json.finish()),
        ]);
        self.rows = 0;
        RecordBatch::try_new(self.schema.clone(), cols)
    }
}

impl Default for RowBatch {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::Array;

    #[test]
    fn schema_has_the_documented_columns_in_order() {
        let schema = output_schema();
        let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
        assert_eq!(&names[..3], &["id", "version_id", "last_updated"]);
        assert!(names.contains(&"part_of"));
        assert!(names.contains(&"identifier"));
        assert!(names.contains(&"position_longitude"));
        assert!(names.contains(&"admin4_code"));
        let pos = |n: &str| names.iter().position(|x| *x == n).unwrap();
        assert!(pos("ownership") < pos("nhfr_code"));
        assert!(pos("nhfr_code") < pos("nhfr_uid"));
        assert!(pos("nhfr_uid") < pos("organization_identifier"));
        assert!(pos("organization_identifier") < pos("facility_level_text"));
        assert!(pos("facility_level_text") < pos("ownership_text"));
        assert!(pos("overlays_admin_unit_ids") < pos("organization_json"));
        assert!(pos("organization_json") < pos("country"));
        assert_eq!(
            &names[names.len() - 3..],
            &["geometry", "bbox", "fhir_json"]
        );
        let geom = schema.field_with_name("geometry").unwrap();
        assert_eq!(geom.metadata()["ARROW:extension:name"], "geoarrow.wkb");
        assert!(geom.metadata()["ARROW:extension:metadata"].contains("OGC:CRS84"));
    }

    #[test]
    fn batch_round_trips_a_row() {
        let mut batch = RowBatch::new();
        let row = OutputRow {
            id: "a".into(),
            name: Some("A".into()),
            alias: vec!["x".into()],
            identifier: vec![
                (Some("sys".to_string()), Some("v".to_string())),
                (None, Some("bare".to_string())),
            ],
            depth: 2,
            admin_level: Some(1),
            tier: "1".into(),
            path: "/ng/a".into(),
            ancestor_ids: vec!["ng".into()],
            country: "NG".into(),
            geom_type: Some("point".into()),
            lon: Some(1.0),
            lat: Some(2.0),
            wkb: vec![1, 0, 0, 0, 0],
            bbox: Some([1.0, 2.0, 1.0, 2.0]),
            fhir_json: "{}".into(),
            nhfr_code: Some("05/08".into()),
            organization_identifier: vec![(Some("s".into()), Some("v".into()))],
            organization_json: Some("{}".into()),
            ..Default::default()
        };
        batch.push(&row);
        assert_eq!(batch.rows, 1);
        let rb = batch.finish().unwrap();
        assert_eq!(batch.rows, 0, "finish resets the batch");
        assert_eq!(rb.num_rows(), 1);
        assert_eq!(rb.schema().fields().len(), output_schema().fields().len());
        let text = |name: &str| {
            rb.column_by_name(name)
                .unwrap()
                .as_any()
                .downcast_ref::<arrow_array::StringArray>()
                .unwrap()
                .value(0)
                .to_string()
        };
        assert_eq!(text("nhfr_code"), "05/08");
        assert_eq!(text("organization_json"), "{}");
        let ids = rb.column_by_name("id").unwrap();
        assert_eq!(
            ids.as_any()
                .downcast_ref::<arrow_array::StringArray>()
                .unwrap()
                .value(0),
            "a"
        );
        let level = rb.column_by_name("admin_level").unwrap();
        assert_eq!(
            level
                .as_any()
                .downcast_ref::<arrow_array::Int32Array>()
                .unwrap()
                .value(0),
            1
        );
        let idents = rb.column_by_name("identifier").unwrap();
        let idents = idents
            .as_any()
            .downcast_ref::<arrow_array::ListArray>()
            .unwrap();
        assert_eq!(idents.value(0).len(), 2);
        let structs = idents.value(0);
        let structs = structs
            .as_any()
            .downcast_ref::<arrow_array::StructArray>()
            .unwrap();
        let systems = structs
            .column_by_name("system")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow_array::StringArray>()
            .unwrap();
        assert!(systems.is_null(1), "a missing identifier.system stays null");
        // A second push after finish works, and the reused StructBuilder
        // still produces the right values (not stale ones from the first
        // finish, and not shifted into the wrong slot).
        batch.push(&row);
        let rb2 = batch.finish().unwrap();
        assert_eq!(rb2.num_rows(), 1);
        let idents2 = rb2.column_by_name("identifier").unwrap();
        let idents2 = idents2
            .as_any()
            .downcast_ref::<arrow_array::ListArray>()
            .unwrap();
        let structs2 = idents2.value(0);
        let structs2 = structs2
            .as_any()
            .downcast_ref::<arrow_array::StructArray>()
            .unwrap();
        let systems2 = structs2
            .column_by_name("system")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow_array::StringArray>()
            .unwrap();
        assert_eq!(systems2.value(0), "sys");
    }

    #[test]
    fn a_row_without_geometry_has_null_geometry_columns() {
        let mut batch = RowBatch::with_schema(output_schema_optional_geometry());
        batch.push(&OutputRow {
            id: "ghost".into(),
            country: "NG".into(),
            ..Default::default()
        });
        batch.push(&OutputRow {
            id: "point".into(),
            geom_type: Some("point".into()),
            lon: Some(1.0),
            lat: Some(2.0),
            wkb: vec![1, 0, 0, 0, 0],
            bbox: Some([1.0, 2.0, 1.0, 2.0]),
            ..Default::default()
        });
        let rb = batch.finish().unwrap();
        assert_eq!(rb.num_rows(), 2);
        for name in ["geometry", "geom_type", "lon", "lat", "bbox"] {
            let col = rb.column_by_name(name).unwrap();
            assert!(
                col.is_null(0),
                "{name} is null for the row without geometry"
            );
            assert!(col.is_valid(1), "{name} is set for the row with geometry");
        }
        let country = rb.column_by_name("country").unwrap();
        assert!(country.is_valid(0));
    }

    #[test]
    fn the_default_schema_refuses_a_row_without_geometry() {
        let mut batch = RowBatch::new();
        batch.push(&OutputRow::default());
        assert!(batch.finish().is_err());
    }

    #[test]
    fn only_the_geometry_columns_differ_between_the_two_schemas() {
        let required = output_schema();
        let optional = output_schema_optional_geometry();
        assert_eq!(required.fields().len(), optional.fields().len());
        for (a, b) in required.fields().iter().zip(optional.fields()) {
            assert_eq!(a.name(), b.name());
            assert_eq!(a.data_type(), b.data_type());
            let geometry_column = ["geom_type", "lon", "lat", "bbox"].contains(&a.name().as_str());
            assert_eq!(
                a.is_nullable() != b.is_nullable(),
                geometry_column,
                "{}",
                a.name()
            );
        }
    }

    #[test]
    fn bbox_values_land_in_the_right_slots() {
        let mut batch = RowBatch::with_schema(output_schema_optional_geometry());
        let row = OutputRow {
            bbox: Some([1.0, 2.0, 3.0, 4.0]),
            ..Default::default()
        };
        batch.push(&row);
        let rb = batch.finish().unwrap();
        let bbox = rb
            .column_by_name("bbox")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow_array::StructArray>()
            .unwrap();
        let field = |name: &str| {
            bbox.column_by_name(name)
                .unwrap()
                .as_any()
                .downcast_ref::<arrow_array::Float64Array>()
                .unwrap()
                .value(0)
        };
        assert_eq!(field("xmin"), 1.0);
        assert_eq!(field("ymin"), 2.0);
        assert_eq!(field("xmax"), 3.0);
        assert_eq!(field("ymax"), 4.0);
    }

    #[test]
    fn empty_batch_finishes_to_zero_rows() {
        let mut batch = RowBatch::new();
        let rb = batch.finish().unwrap();
        assert_eq!(rb.num_rows(), 0);
    }
}
