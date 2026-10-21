# Feedback from loading Uganda's admin units with fhir-pipes

On 2026-10-21 the ICR loaded Uganda's districts into a new country server
(`https://fhir-uga.healthcampaigns.org/fhir`) with fhir-pipes v0.6.0, as a
first real use of `import location-boundaries` and `load` outside Nigeria.
The work is in `recodelabs/icr`, `country/uganda/` (`districts.sql`,
`enrich.py`, `README.md`).

What was loaded: 151 Locations, the country, 4 regions and 146 districts and
cities, matching the Ministry of Health's list one to one. The boundaries come
from Overture Maps (release 2026-10-21.0, OpenStreetMap, ODbL), with two
changes: Entebbe City merged into Wakiso, and Lira City taken from the OCHA
COD-AB (2020) because Overture has no outline for it.

The pipeline was:

1. DuckDB SQL: Overture divisions and division areas for UG, the two fixes,
   P-codes from the COD-AB by name, checks against the ministry's list, written
   as one GeoJSON with a feature per district.
2. `fhir-pipes import location-boundaries --level region=region:region_code
   --level district=district --alias district=district_aliases
   --dissolve-parents`.
3. A Python step adding what the import can't write (see below).
4. `fhir-pipes load` with client credentials through the FHIR Info Gateway.

Steps 2 and 4 worked first time once the input was accepted. `load` with
client credentials, through the gateway, in bundles of 25, was uneventful.

## Problems

1. **`OGC:CRS84` is rejected as not WGS84.** DuckDB/GDAL write
   `"crs": {"name": "urn:ogc:def:crs:OGC::CRS84"}` into GeoJSON by default, and
   `import location-boundaries` exits with "input CRS ... is not WGS84
   (EPSG:4326)". CRS84 is WGS84 with longitude first, which is what GeoJSON
   (RFC 7946) means anyway, so it should be accepted, as should
   `urn:ogc:def:crs:EPSG::4326`. Workaround: write with GDAL's `RFC7946=YES`,
   which leaves `crs` out.

2. **The example profile in the repo doesn't load in the released binary.**
   `examples/profiles/icr.toml` at `main` has `catchment_extension`; v0.6.0
   rejects it as an unknown field and stops. Using `git show
   v0.6.0:examples/profiles/icr.toml` worked. Options: a `min_version` key in
   profiles with a clear message, unknown keys as warnings, or publishing each
   release's profile next to the binary. The profile's comment says ICR's own
   copy lives in `recodelabs/icr`, but no such file is there; a profile URL
   (for example on icr.healthcampaigns.org) would let `--profile` point at
   one place.

3. **No way to write more than one identifier per unit.** Each unit gets one
   identifier under `--code-system`. The Uganda units needed four: the P-code
   (official), the Overture GERS id, the Wikidata id, and ISO 3166 codes for
   the country and regions. `import location-sites` already has
   `--identifier URI=COLUMN`; the same for boundaries, per level, would cover
   it, ideally with a way to mark one `use: official`, for example
   `--identifier district=https://…/pcode=pcode:official`. The parent levels
   need it too (region GERS and Wikidata ids), from properties on the
   features.

4. **A unit with no code gets its own id as its identifier.** With
   `--level district=district` (no code property) every district's identifier
   is `national-admin-code|uga-n-abim`, which repeats the id and claims to be
   a national code. Better to write no identifier for a level without codes,
   or make the fallback opt-in.

5. **No provenance per unit.** The units came from three places (Overture,
   Overture with a merge, COD-AB). Something like `--description PROP`, or a
   `meta.source` from a property, would carry it.

6. **Parent boundaries can only be dissolved.** `--dissolve-parents` gives
   the regions the union of their districts, which is consistent. There is no
   way to give a parent its own authoritative outline from the same source
   (Overture has region polygons) in the same run.

7. **`load --dry-run` reports `create` for resources that already exist.**
   After the load, a second dry run still listed all 151 as `create`. Without
   a snapshot, `load` can't tell new from changed from unchanged. A cheap
   check (`_id` search, `_elements=id,meta`) would let the dry run say what
   will really happen, and a real run skip unchanged resources.

8. **The `updateCreate` warning on every run against HAPI.** HAPI supports it
   but leaves the flag out, and the warning says so. If the server's
   CapabilityStatement says HAPI, the warning could be dropped or shown once.

## Suggestion: an Overture importer

Most of step 1 was generic Overture handling that every country will repeat:

- Read `theme=divisions/type=division` and `type=division_area` for one
  country from the public S3 bucket (anonymous, `country = 'UG'`), for a
  given release, and cache them.
- Build the tree from `parent_division_id` (or `hierarchies`): country →
  region → county (or whichever subtypes are asked for).
- Name each Overture subtype in local terms: in Uganda, Overture's `county`
  is the district (`--level county=district`).
- Write identifiers: the GERS id, Wikidata, ISO 3166-1 for the country and
  ISO 3166-2 (`region`) for regions.
- Take the boundary from the `division_area` (`is_land`), and record the
  release and the licence (ODbL, from `sources`).
- Accept corrections: an override GeoJSON that replaces or adds units, and a
  merge list (`Entebbe City` into `Wakiso`), so a country's differences from
  OSM are data, not code.
- Report what doesn't tile: the sum of the units' areas against the country,
  overlaps, and units without an outline.

With that, Uganda would have been `fhir-pipes import overture --country UG
--level region=region --level county=district --merge "Entebbe City=Wakiso"
--override lira-city.geojson`, then `load`. A matching report against a
reference list (the ministry's names) would also have helped: Overture had
146 units and the list had 146 names, but four names differed and one unit
was the wrong one.

## What Overture has for Uganda (2026-10-21.0)

For reference when deciding what an importer should do below district level:
outlines for the country, 4 regions and 146 districts and cities, which tile
the country with no overlaps; 95 town outlines in 30 districts and 395
neighbourhood outlines in 19, so not usable as a level; about 61,800 village
and hamlet points, which could become settlements.
