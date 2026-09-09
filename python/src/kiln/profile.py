"""ICR Location profile mapping.

Everything profile-specific lives here. Supporting a different Location
profile means editing this module and nothing else.

Profile: https://icr.healthcampaigns.org/StructureDefinition-ICRLocation.html
"""

from __future__ import annotations

import base64
import binascii
from dataclasses import dataclass, field

from kiln.report import Report
from kiln.shape import as_list

BOUNDARY_EXTENSION_URL = (
    "https://icr.healthcampaigns.org/StructureDefinition/location-boundary-geojson"
)
HL7_BOUNDARY_EXTENSION_URL = (
    "http://hl7.org/fhir/StructureDefinition/location-boundary-geojson"
)
# Read set: kiln writes the ICR URL but reads both, so a later IG switch to
# the HL7 canonical strands no data (spec 2026-08-05, extension-URL policy).
BOUNDARY_EXTENSION_URLS = (BOUNDARY_EXTENSION_URL, HL7_BOUNDARY_EXTENSION_URL)
OVERLAYS_EXTENSION_URL = "https://icr.healthcampaigns.org/StructureDefinition/overlays-admin-unit"
SETTLEMENT_TYPE_EXTENSION_URL = (
    "https://icr.healthcampaigns.org/StructureDefinition/settlement-type"
)
DELIVERY_STRATEGY_EXTENSION_URL = (
    "https://icr.healthcampaigns.org/StructureDefinition/delivery-strategy"
)

PCODE_SYSTEM = "https://icr.healthcampaigns.org/identifiers/pcode"
GERS_SYSTEM = "https://icr.healthcampaigns.org/identifiers/overture-gers"

GEOJSON_CONTENT_TYPE = "application/geo+json"

ADMIN_UNIT_TYPE = "admin-unit"

ICR_LOCATION_PROFILE_URL = (
    "https://icr.healthcampaigns.org/StructureDefinition/ICRLocation"
)
LOCATION_TYPE_SYSTEM = "https://icr.healthcampaigns.org/CodeSystem/icr-location-type-cs"
PHYSICAL_TYPE_SYSTEM = "http://terminology.hl7.org/CodeSystem/location-physical-type"
NATIONAL_ADMIN_CODE_SYSTEM = (
    "https://icr.healthcampaigns.org/identifiers/national-admin-code"
)


@dataclass
class BoundaryRef:
    """A boundary attachment: either decoded inline bytes, or a URL to fetch."""

    data: bytes | None = None
    url: str | None = None


@dataclass
class RawLocation:
    """One FHIR Location flattened into the fields kiln cares about."""

    id: str
    name: str | None = None
    status: str | None = None
    loc_type: str | None = None
    physical_type: str | None = None
    pcode: str | None = None
    gers_id: str | None = None
    identifiers: list[dict] = field(default_factory=list)
    parent_id: str | None = None
    overlays_admin_unit_ids: list[str] = field(default_factory=list)
    settlement_type: str | None = None
    delivery_strategy: str | None = None
    facility_level: str | None = None
    ownership: str | None = None
    position: tuple[float, float] | None = None
    boundary: BoundaryRef | None = None
    last_updated: str | None = None


def _first_coding_code(node: dict | list | None) -> str | None:
    """Pull the first coding.code out of a CodeableConcept or list of them."""
    if isinstance(node, list):
        node = node[0] if node else None
    if not isinstance(node, dict):
        return None
    codings = as_list(node.get("coding"))
    if codings and isinstance(codings[0], dict):
        return codings[0].get("code")
    return None


def _coding_code_by_system(node: dict | list | None, system: str) -> str | None:
    """Find the first coding under `system` across a list of CodeableConcepts.

    The read side of the duplicated-classification convention: facility
    level and ownership are authoritative on Organization.type but may be
    copied onto Location.type as extra codings (mCSD allows it) so
    Location-only consumers -- like this exporter -- can see them.
    """
    for concept in as_list(node):
        if not isinstance(concept, dict):
            continue
        for coding in as_list(concept.get("coding")):
            if isinstance(coding, dict) and coding.get("system") == system:
                return coding.get("code")
    return None


def _strip_reference(reference: str | None) -> str | None:
    """`Location/loc-1` -> `loc-1`. Bare ids pass through unchanged."""
    if not reference or not isinstance(reference, str):
        return None
    return reference.rsplit("/", 1)[-1]


def _identifier_value(identifiers: list[dict], system: str) -> str | None:
    for identifier in identifiers:
        if isinstance(identifier, dict) and identifier.get("system") == system:
            return identifier.get("value")
    return None


def _read_boundary(extension: dict, location_id: str, report: Report) -> BoundaryRef | None:
    value_attachment = extension.get("valueAttachment")
    if not isinstance(value_attachment, dict):
        report.add(
            "malformed_field",
            location_id,
            "boundary extension valueAttachment is not a dict",
        )
        return None
    attachment = value_attachment
    content_type = attachment.get("contentType")
    if content_type != GEOJSON_CONTENT_TYPE:
        report.add(
            "boundary_bad_content_type",
            location_id,
            f"expected {GEOJSON_CONTENT_TYPE}, got {content_type!r}",
        )
        return None

    if attachment.get("data"):
        try:
            return BoundaryRef(data=base64.b64decode(attachment["data"], validate=True))
        except (binascii.Error, ValueError, TypeError) as exc:
            report.add("boundary_bad_base64", location_id, str(exc))
            return None

    if attachment.get("url"):
        return BoundaryRef(url=attachment["url"])

    report.add("boundary_empty", location_id, "attachment has neither data nor url")
    return None


def shred(resource: dict, report: Report) -> RawLocation | None:
    """Flatten one FHIR Location resource. Returns None if it has no id.

    `resource` is untrusted: it may have come straight off the wire (a
    Bundle entry) or out of a local NDJSON file nobody has validated, so
    even its top-level shape cannot be assumed.
    """
    if not isinstance(resource, dict):
        report.add("malformed_field", "<unknown>", "Location resource is not a dict")
        return None

    location_id = resource.get("id")
    if not location_id:
        report.add("missing_id", "<unknown>", "Location resource has no id")
        return None
    if not isinstance(location_id, str):
        report.add(
            "malformed_field", "<unknown>", f"Location id is not a string: {location_id!r}"
        )
        return None

    identifiers = resource.get("identifier")
    if identifiers is None:
        identifiers = []
    elif not isinstance(identifiers, list):
        report.add("malformed_field", location_id, "identifier is not a list")
        identifiers = []

    position = None
    if isinstance(resource.get("position"), dict):
        pos = resource["position"]
        longitude, latitude = pos.get("longitude"), pos.get("latitude")
        if longitude is not None and latitude is not None:
            # FHIR is longitude-first. x = longitude, y = latitude.
            try:
                position = (float(longitude), float(latitude))
            except (TypeError, ValueError):
                report.add(
                    "malformed_field",
                    location_id,
                    f"position coordinates not numeric: ({longitude!r}, {latitude!r})",
                )

    # Extract parent_id, guarding against non-dict partOf
    parent_id = None
    part_of = resource.get("partOf")
    if part_of is not None:
        if isinstance(part_of, dict):
            parent_id = _strip_reference(part_of.get("reference"))
        else:
            report.add("malformed_field", location_id, "partOf is not a dict")

    # Extract last_updated, guarding against non-dict meta
    last_updated = None
    meta = resource.get("meta")
    if meta is not None:
        if isinstance(meta, dict):
            last_updated = meta.get("lastUpdated")
        else:
            report.add("malformed_field", location_id, "meta is not a dict")

    # Build identifiers list, guarding against non-dict entries
    identifiers_list = []
    for i, identifier in enumerate(identifiers):
        if not isinstance(identifier, dict):
            report.add(
                "malformed_field",
                location_id,
                f"identifier[{i}] is not a dict",
            )
            continue
        identifiers_list.append(
            {"system": identifier.get("system"), "value": identifier.get("value")}
        )

    raw = RawLocation(
        id=location_id,
        name=resource.get("name"),
        status=resource.get("status"),
        loc_type=_first_coding_code(resource.get("type")),
        physical_type=_first_coding_code(resource.get("physicalType")),
        facility_level=_coding_code_by_system(resource.get("type"), FACILITY_TYPE_SYSTEM),
        ownership=_coding_code_by_system(resource.get("type"), OWNERSHIP_SYSTEM),
        pcode=_identifier_value(identifiers, PCODE_SYSTEM),
        gers_id=_identifier_value(identifiers, GERS_SYSTEM),
        identifiers=identifiers_list,
        parent_id=parent_id,
        position=position,
        last_updated=last_updated,
    )

    extensions = resource.get("extension")
    if extensions is None:
        extensions = []
    elif not isinstance(extensions, list):
        report.add("malformed_field", location_id, "extension is not a list")
        extensions = []

    for extension in extensions:
        if not isinstance(extension, dict):
            report.add("malformed_field", location_id, "extension entry is not a dict")
            continue
        url = extension.get("url")
        if url in BOUNDARY_EXTENSION_URLS and raw.boundary is None:
            raw.boundary = _read_boundary(extension, location_id, report)
        elif url == OVERLAYS_EXTENSION_URL:
            value_reference = extension.get("valueReference")
            if isinstance(value_reference, dict):
                target = _strip_reference(value_reference.get("reference"))
                if target:
                    raw.overlays_admin_unit_ids.append(target)
            elif value_reference is not None:
                report.add(
                    "malformed_field",
                    location_id,
                    "overlays extension valueReference is not a dict",
                )
        elif url == SETTLEMENT_TYPE_EXTENSION_URL:
            raw.settlement_type = extension.get("valueCode")
        elif url == DELIVERY_STRATEGY_EXTENSION_URL:
            raw.delivery_strategy = extension.get("valueCode")

    return raw


def build_location(
    location_id: str,
    name: str,
    *,
    parent_id: str | None = None,
    identifiers: list[tuple[str, str]],
    aliases: list[str] | None = None,
    boundary_geojson: bytes | None = None,
) -> dict:
    """Build one admin-unit Location conforming to ICRLocation.

    The write-side twin of `shred`: bake constructs resources only through
    this function, so the emitted shape lives next to the parsed one. Every
    admin unit must carry >=1 identifier (IG rule icr-loc-admin-id), so an
    empty `identifiers` is a programming error, not a data problem.
    """
    if not identifiers:
        raise ValueError(
            f"admin unit {location_id!r} must carry at least one identifier"
        )

    resource: dict = {
        "resourceType": "Location",
        "id": location_id,
        "meta": {"profile": [ICR_LOCATION_PROFILE_URL]},
        "name": name,
        "status": "active",
        "type": [
            {
                "coding": [
                    {"system": LOCATION_TYPE_SYSTEM, "code": ADMIN_UNIT_TYPE}
                ]
            }
        ],
        "physicalType": {
            "coding": [
                {
                    "system": PHYSICAL_TYPE_SYSTEM,
                    "code": "jdn",
                    "display": "Jurisdiction",
                }
            ]
        },
        "identifier": [
            {"system": system, "value": value} for system, value in identifiers
        ],
    }
    if parent_id:
        resource["partOf"] = {"reference": f"Location/{parent_id}"}
    if aliases:
        resource["alias"] = list(aliases)
    if boundary_geojson is not None:
        attach_boundary(resource, boundary_geojson)
    return resource


ORG_TYPE_SYSTEM = "http://terminology.hl7.org/CodeSystem/organization-type"
FACILITY_TYPE_SYSTEM = "https://icr.healthcampaigns.org/CodeSystem/icr-facility-type-cs"
OWNERSHIP_SYSTEM = "https://icr.healthcampaigns.org/CodeSystem/icr-ownership-cs"
ICR_FACILITY_ORG_PROFILE_URL = (
    "https://icr.healthcampaigns.org/StructureDefinition/ICRFacilityOrganization"
)


def build_facility_organization(
    org_id: str,
    name: str,
    *,
    identifiers: list[tuple[str, str]],
    type_concepts: list[tuple[str, str, str, str | None]],
) -> dict:
    """Build the accountable facility entity of the mCSD pairing.

    Organization.type is the source of truth for facility classification:
    the generic `prov` coding always comes first, then one CodeableConcept
    per (system, code, display, text) in `type_concepts` -- the national
    tier and ownership axes, with the country-specific kind as text. The
    paired Location points here via managingOrganization; registry codes
    (`identifiers`) live on this entity, not on the place.
    """
    types: list[dict] = [
        {
            "coding": [
                {
                    "system": ORG_TYPE_SYSTEM,
                    "code": "prov",
                    "display": "Healthcare Provider",
                }
            ]
        }
    ]
    for system, code, display, text in type_concepts:
        concept: dict = {"coding": [{"system": system, "code": code, "display": display}]}
        if text:
            concept["text"] = text
        types.append(concept)

    resource: dict = {
        "resourceType": "Organization",
        "id": org_id,
        "meta": {"profile": [ICR_FACILITY_ORG_PROFILE_URL]},
        "active": True,
        "name": name,
        "type": types,
    }
    if identifiers:
        resource["identifier"] = [
            {"system": system, "value": value} for system, value in identifiers
        ]
    return resource


def build_point_location(
    location_id: str,
    name: str,
    *,
    type_code: str,
    parent_id: str | None = None,
    identifiers: list[tuple[str, str]],
    position: tuple[float, float] | None = None,
    managing_org_id: str | None = None,
    extra_type_concepts: list[tuple[str, str, str, str | None]] | None = None,
    spatial_cells: list[tuple[str, int, str]] | None = None,
) -> dict:
    """Build one site-shaped Location (facility, school, ...) per ICRLocation.

    The point-feature sibling of `build_location`: type comes from
    ICRLocationTypeCS (`facility`, `school`, ...), physicalType is `si`
    Site (the IG's own convention for service points -- see the Rokupr CHC
    example), geometry is a GPS `position` rather than a boundary, and
    `partOf` hangs the site under its admin parent (typically the ward).
    Sites are not bound by the admin-unit >=1-identifier rule, but callers
    are expected to pass the source's stable identifiers anyway.
    """
    resource: dict = {
        "resourceType": "Location",
        "id": location_id,
        "meta": {"profile": [ICR_LOCATION_PROFILE_URL]},
        "name": name,
        "status": "active",
        "type": _point_types(type_code, extra_type_concepts or []),
        "physicalType": {
            "coding": [
                {"system": PHYSICAL_TYPE_SYSTEM, "code": "si", "display": "Site"}
            ]
        },
    }
    if identifiers:
        resource["identifier"] = [
            {"system": system, "value": value} for system, value in identifiers
        ]
    if parent_id:
        resource["partOf"] = {"reference": f"Location/{parent_id}"}
    if position is not None:
        longitude, latitude = position
        resource["position"] = {"longitude": longitude, "latitude": latitude}
    if managing_org_id:
        resource["managingOrganization"] = {
            "reference": f"Organization/{managing_org_id}"
        }
    if spatial_cells:
        from kiln.spatial import spatial_index_extension

        resource["extension"] = [
            spatial_index_extension(scheme, level, cell) for scheme, level, cell in spatial_cells
        ]
    return resource


def _point_types(
    type_code: str, extra_type_concepts: list[tuple[str, str, str, str | None]]
) -> list[dict]:
    """The ICR functional code first, then any duplicated classification axes.

    Organization.type stays authoritative for facility classification; the
    extras here are the mCSD-sanctioned copy for Location-only consumers
    (documented in the IG's facility-pairing section).
    """
    types: list[dict] = [
        {"coding": [{"system": LOCATION_TYPE_SYSTEM, "code": type_code}]}
    ]
    for system, code, display, text in extra_type_concepts:
        concept: dict = {"coding": [{"system": system, "code": code, "display": display}]}
        if text:
            concept["text"] = text
        types.append(concept)
    return types


def attach_boundary(resource: dict, boundary_geojson: bytes) -> None:
    """Add the boundary extension (write URL, inline base64) to a Location.

    Shared by `build_location` and bake's dissolve-parents pass, so the
    written extension shape stays defined in exactly one place.
    """
    resource.setdefault("extension", []).append(
        {
            "url": BOUNDARY_EXTENSION_URL,
            "valueAttachment": {
                "contentType": GEOJSON_CONTENT_TYPE,
                "data": base64.b64encode(boundary_geojson).decode(),
            },
        }
    )
