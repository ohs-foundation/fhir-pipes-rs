//! The output table: Arrow schema, the row struct pass two fills, and the
//! column builders that turn rows into a RecordBatch.
//!
//! Column names follow the FHIR path they come from. See docs/locations.md
//! "Columns". The built-in columns are base FHIR and what fhir-pipes derives;
//! a profile adds its Location and Organization columns, and the spatial index
//! and overlays columns when it names those extensions (see docs/profiles.md).

use std::collections::HashMap;
use std::sync::Arc;

use arrow_array::builder::{
    BinaryBuilder, Float64Builder, Int32Builder, ListBuilder, StringBuilder, StructBuilder,
};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{DataType, Field, Fields, Schema, SchemaRef};

use crate::index::ADMIN_COLUMNS;
use crate::profile::Profile;

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

/// Built-in column names, always present (the spatial index and overlays
/// columns only with a profile that names their extensions, but their names
/// are reserved). A profile column may not reuse one.
const BUILTIN_COLUMNS: &[&str] = &[
    "id",
    "version_id",
    "last_updated",
    "name",
    "alias",
    "status",
    "description",
    "type",
    "physical_type",
    "part_of",
    "managing_organization",
    "identifier",
    "position_longitude",
    "position_latitude",
    "quadkey",
    "quadkey_level",
    "organization_identifier",
    "depth",
    "admin_level",
    "tier",
    "path",
    "ancestor_ids",
    "overlays_admin_unit_ids",
    "organization_json",
    "country",
    "geom_type",
    "lon",
    "lat",
    GEOMETRY_COLUMN,
    BBOX_COLUMN,
    "fhir_json",
];

/// True for a built-in column name, the adminN ones included.
pub fn is_builtin_column(name: &str) -> bool {
    BUILTIN_COLUMNS.contains(&name)
        || (0..ADMIN_COLUMNS)
            .any(|i| name == format!("admin{i}_name") || name == format!("admin{i}_code"))
}

/// The schema every partition file is written with, for `profile`.
pub fn output_schema(profile: &Profile) -> SchemaRef {
    schema(profile, false)
}

/// The schema for `fhir-pipes transform locations --keep-without-geometry`: the same
/// columns, with `geom_type`, `lon`, `lat` and `bbox` nullable so a
/// Location with no point and no boundary can have a row. `geometry` is
/// nullable in both.
pub fn output_schema_optional_geometry(profile: &Profile) -> SchemaRef {
    schema(profile, true)
}

fn schema(profile: &Profile, optional_geometry: bool) -> SchemaRef {
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
    ];
    fields.extend(profile.location.columns.iter().map(|c| utf8(&c.name)));
    if profile.location.spatial_index_extension.is_some() {
        fields.push(utf8("quadkey"));
        fields.push(Field::new("quadkey_level", DataType::Int32, true));
    }
    fields.push(Field::new(
        "organization_identifier",
        DataType::List(Arc::new(Field::new(
            "item",
            DataType::Struct(identifier_fields()),
            true,
        ))),
        true,
    ));
    fields.extend(profile.organization.columns.iter().map(|c| utf8(&c.name)));
    fields.extend([
        Field::new("depth", DataType::Int32, false),
        Field::new("admin_level", DataType::Int32, true),
        Field::new("tier", DataType::Utf8, false),
        Field::new("path", DataType::Utf8, false),
        list_utf8("ancestor_ids"),
    ]);
    for i in 0..ADMIN_COLUMNS {
        fields.push(utf8(&format!("admin{i}_name")));
    }
    for i in 0..ADMIN_COLUMNS {
        fields.push(utf8(&format!("admin{i}_code")));
    }
    if profile.location.overlays_extension.is_some() {
        fields.push(list_utf8("overlays_admin_unit_ids"));
    }
    fields.extend([
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
    /// The profile's Location columns, in profile order.
    pub location_columns: Vec<Option<String>>,
    /// The deepest quadkey cell on the Location (spatial-index extension);
    /// its prefixes are every coarser tile. Read-only: derived from position.
    pub quadkey: Option<String>,
    pub quadkey_level: Option<i32>,
    pub organization_identifier: Vec<(Option<String>, Option<String>)>,
    /// The profile's Organization columns, in profile order.
    pub organization_columns: Vec<Option<String>>,
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
    location_columns: Vec<StringBuilder>,
    /// With a spatial index extension in the profile.
    quadkey: Option<(StringBuilder, Int32Builder)>,
    organization_identifier: ListBuilder<StructBuilder>,
    organization_columns: Vec<StringBuilder>,
    depth: Int32Builder,
    admin_level: Int32Builder,
    tier: StringBuilder,
    path: StringBuilder,
    ancestor_ids: ListBuilder<StringBuilder>,
    admin_names: Vec<StringBuilder>,
    admin_codes: Vec<StringBuilder>,
    /// With an overlays extension in the profile.
    overlays: Option<ListBuilder<StringBuilder>>,
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

/// One value per builder; a row with fewer values gets nulls for the rest.
fn push_columns(builders: &mut [StringBuilder], values: &[Option<String>]) {
    for (i, b) in builders.iter_mut().enumerate() {
        b.append_option(values.get(i).and_then(|v| v.as_deref()));
    }
}

fn push_list(b: &mut ListBuilder<StringBuilder>, items: &[String]) {
    for item in items {
        b.values().append_value(item);
    }
    b.append(true);
}

impl RowBatch {
    /// Builders for the base output schema (no profile).
    #[cfg(test)]
    pub fn new() -> Self {
        Self::for_profile(&Profile::base(), false)
    }

    /// Builders for `profile`'s schema: [`output_schema`], or
    /// [`output_schema_optional_geometry`] when `optional_geometry`.
    pub fn for_profile(profile: &Profile, optional_geometry: bool) -> Self {
        let schema = schema(profile, optional_geometry);
        let strings = |n: usize| (0..n).map(|_| StringBuilder::new()).collect::<Vec<_>>();
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
            location_columns: strings(profile.location.columns.len()),
            quadkey: profile
                .location
                .spatial_index_extension
                .is_some()
                .then(|| (StringBuilder::new(), Int32Builder::new())),
            organization_identifier: ListBuilder::new(StructBuilder::from_fields(
                identifier_fields(),
                0,
            )),
            organization_columns: strings(profile.organization.columns.len()),
            depth: Int32Builder::new(),
            admin_level: Int32Builder::new(),
            tier: StringBuilder::new(),
            path: StringBuilder::new(),
            ancestor_ids: ListBuilder::new(StringBuilder::new()),
            admin_names: (0..ADMIN_COLUMNS).map(|_| StringBuilder::new()).collect(),
            admin_codes: (0..ADMIN_COLUMNS).map(|_| StringBuilder::new()).collect(),
            overlays: profile
                .location
                .overlays_extension
                .is_some()
                .then(|| ListBuilder::new(StringBuilder::new())),
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
        push_columns(&mut self.location_columns, &r.location_columns);
        if let Some((quadkey, level)) = &mut self.quadkey {
            quadkey.append_option(r.quadkey.as_deref());
            level.append_option(r.quadkey_level);
        }
        push_identifiers(
            &mut self.organization_identifier,
            &r.organization_identifier,
        );
        push_columns(&mut self.organization_columns, &r.organization_columns);
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
        if let Some(overlays) = &mut self.overlays {
            push_list(overlays, &r.overlays_admin_unit_ids);
        }
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
        ];
        for b in &mut self.location_columns {
            cols.push(Arc::new(b.finish()));
        }
        if let Some((quadkey, level)) = &mut self.quadkey {
            cols.push(Arc::new(quadkey.finish()));
            cols.push(Arc::new(level.finish()));
        }
        cols.push(Arc::new(self.organization_identifier.finish()));
        for b in &mut self.organization_columns {
            cols.push(Arc::new(b.finish()));
        }
        cols.extend([
            Arc::new(self.depth.finish()) as ArrayRef,
            Arc::new(self.admin_level.finish()),
            Arc::new(self.tier.finish()),
            Arc::new(self.path.finish()),
            Arc::new(self.ancestor_ids.finish()),
        ]);
        for b in &mut self.admin_names {
            cols.push(Arc::new(b.finish()));
        }
        for b in &mut self.admin_codes {
            cols.push(Arc::new(b.finish()));
        }
        if let Some(overlays) = &mut self.overlays {
            cols.push(Arc::new(overlays.finish()));
        }
        cols.extend([
            Arc::new(self.organization_json.finish()) as ArrayRef,
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

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::Array;

    #[test]
    fn schema_has_the_documented_columns_in_order() {
        let schema = output_schema(&crate::profile::icr_example());
        let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
        assert_eq!(&names[..3], &["id", "version_id", "last_updated"]);
        assert!(names.contains(&"part_of"));
        assert!(names.contains(&"identifier"));
        assert!(names.contains(&"position_longitude"));
        assert!(names.contains(&"admin4_code"));
        let pos = |n: &str| names.iter().position(|x| *x == n).unwrap();
        // The profile's Location columns follow position, in profile order,
        // then the spatial index; its Organization columns follow
        // organization_identifier.
        assert_eq!(pos("pcode"), pos("position_latitude") + 1);
        assert!(pos("pcode") < pos("ownership"));
        assert!(pos("ownership") < pos("quadkey"));
        assert!(pos("quadkey_level") < pos("organization_identifier"));
        assert!(pos("organization_identifier") < pos("nhfr_code"));
        assert!(pos("nhfr_code") < pos("nhfr_uid"));
        assert!(pos("nhfr_uid") < pos("facility_level_text"));
        assert!(pos("facility_level_text") < pos("ownership_text"));
        assert!(pos("overlays_admin_unit_ids") < pos("organization_json"));
        assert!(pos("organization_json") < pos("country"));
        assert_eq!(
            &names[names.len() - 3..],
            &["geometry", "bbox", "fhir_json"]
        );
        let geom = schema.field_with_name(GEOMETRY_COLUMN).unwrap();
        assert_eq!(geom.metadata()["ARROW:extension:name"], "geoarrow.wkb");
        assert!(geom.metadata()["ARROW:extension:metadata"].contains("OGC:CRS84"));
    }

    #[test]
    fn without_a_profile_there_are_no_profile_columns() {
        let schema = output_schema(&Profile::base());
        let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
        for absent in ["pcode", "nhfr_code", "quadkey", "overlays_admin_unit_ids"] {
            assert!(!names.contains(&absent), "{absent}");
        }
        assert!(names.iter().all(|n| is_builtin_column(n)), "{names:?}");
    }

    #[test]
    fn batch_round_trips_a_row() {
        let profile = crate::profile::icr_example();
        let mut batch = RowBatch::for_profile(&profile, false);
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
            organization_columns: vec![Some("05/08".into())],
            organization_identifier: vec![(Some("s".into()), Some("v".into()))],
            organization_json: Some("{}".into()),
            ..Default::default()
        };
        batch.push(&row);
        assert_eq!(batch.rows, 1);
        let rb = batch.finish().unwrap();
        assert_eq!(batch.rows, 0, "finish resets the batch");
        assert_eq!(rb.num_rows(), 1);
        assert_eq!(
            rb.schema().fields().len(),
            output_schema(&profile).fields().len()
        );
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
        let mut batch = RowBatch::for_profile(&Profile::base(), true);
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
        let profile = crate::profile::icr_example();
        let required = output_schema(&profile);
        let optional = output_schema_optional_geometry(&profile);
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
        let mut batch = RowBatch::for_profile(&Profile::base(), true);
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
