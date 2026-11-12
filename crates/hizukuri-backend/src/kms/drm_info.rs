//! Reads the JSON that `drm_info -j` writes into [`DeviceSnapshot`]s.
//!
//! `drm_info` is the usual tool for recording what a DRM device offers, so
//! records made with it (by other people, on other machines) can become
//! inputs for the mock device and the planning tests without running
//! anything on that machine again. `hizukuri kms-info` writes snapshots
//! directly; this covers records that already exist.
//!
//! Format: `drm_info` 2.x, as produced by the version in Fedora 44.

use serde_json::Value;

use super::blob::to_hex;
use super::model::{
    Connector, Crtc, DeviceCaps, DeviceSnapshot, DriverInfo, Encoder, EnumValue, FormatModifier,
    Fourcc, Mode, Plane, Property, PropertyKind,
};

/// Reads every device in a `drm_info -j` document: the device node paths
/// with their snapshots, in the document's order of paths.
pub fn snapshots_from_drm_info(text: &str) -> Result<Vec<(String, DeviceSnapshot)>, String> {
    let document: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let devices = document
        .as_object()
        .ok_or("the document is not an object of devices")?;
    devices
        .iter()
        .map(|(path, device)| {
            snapshot_from_drm_info(device)
                .map(|snapshot| (path.clone(), snapshot))
                .map_err(|e| format!("{path}: {e}"))
        })
        .collect()
}

/// Reads one device of a `drm_info -j` document.
pub fn snapshot_from_drm_info(device: &Value) -> Result<DeviceSnapshot, String> {
    let driver = &device["driver"];
    let caps = &driver["caps"];
    let cap = |name: &str| caps[name].as_u64().unwrap_or(0);
    let version = &driver["version"];

    let mut connectors = Vec::new();
    // The kernel numbers connectors per type, from 1, in creation order,
    // which is the order they are listed in. The record does not carry the
    // number, and the kernel counts across all devices: the second device
    // with a virtual connector has `Virtual-2`, which comes out as
    // `Virtual-1` here.
    let mut type_counts = std::collections::HashMap::<u64, u32>::new();
    for connector in list(device, "connectors")? {
        let kind = uint(connector, "type")?;
        let index = type_counts.entry(kind).or_insert(0);
        *index += 1;
        connectors.push(Connector {
            id: id(connector)?,
            name: format!("{}-{}", connector_type_name(kind), index),
            // 1 connected, 2 disconnected, 3 unknown.
            connected: uint(connector, "status")? == 1,
            physical_size_mm: (
                uint(connector, "phy_width")? as u32,
                uint(connector, "phy_height")? as u32,
            ),
            modes: list(connector, "modes")?
                .iter()
                .map(mode)
                .collect::<Result<_, _>>()?,
            encoders: list(connector, "encoders")?
                .iter()
                .filter_map(|e| e.as_u64().map(|e| e as u32))
                .collect(),
            properties: properties(connector)?,
        });
    }

    let encoders = list(device, "encoders")?
        .iter()
        .map(|encoder| {
            Ok(Encoder {
                id: id(encoder)?,
                possible_crtcs: uint(encoder, "possible_crtcs")? as u32,
            })
        })
        .collect::<Result<_, String>>()?;

    let crtcs = list(device, "crtcs")?
        .iter()
        .map(|crtc| {
            Ok(Crtc {
                id: id(crtc)?,
                properties: properties(crtc)?,
            })
        })
        .collect::<Result<_, String>>()?;

    let mut planes = Vec::new();
    for plane in list(device, "planes")? {
        // `IN_FORMATS` is decoded by drm_info: a list of modifiers, each
        // with the formats it applies to.
        let modifiers = plane["properties"]["IN_FORMATS"]["data"]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .flat_map(|entry| {
                        let modifier = entry["modifier"].as_u64().unwrap_or(0);
                        entry["formats"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_u64)
                            .map(move |format| FormatModifier {
                                format: Fourcc(format as u32),
                                modifier,
                            })
                    })
                    .collect::<Vec<_>>()
            });
        planes.push(Plane {
            id: id(plane)?,
            possible_crtcs: uint(plane, "possible_crtcs")? as u32,
            formats: list(plane, "formats")?
                .iter()
                .filter_map(Value::as_u64)
                .map(|format| Fourcc(format as u32))
                .collect(),
            modifiers,
            properties: properties(plane)?,
        });
    }

    Ok(DeviceSnapshot {
        driver: DriverInfo {
            name: driver["name"].as_str().unwrap_or_default().to_string(),
            description: driver["desc"].as_str().unwrap_or_default().to_string(),
            version: format!(
                "{}.{}.{}",
                version["major"].as_u64().unwrap_or(0),
                version["minor"].as_u64().unwrap_or(0),
                version["patch"].as_u64().unwrap_or(0)
            ),
        },
        caps: DeviceCaps {
            dumb_buffer: cap("DUMB_BUFFER") != 0,
            addfb2_modifiers: cap("ADDFB2_MODIFIERS") != 0,
            async_page_flip: cap("ASYNC_PAGE_FLIP") != 0,
            atomic_async_page_flip: cap("ATOMIC_ASYNC_PAGE_FLIP") != 0,
            cursor_width: cap("CURSOR_WIDTH") as u32,
            cursor_height: cap("CURSOR_HEIGHT") as u32,
            monotonic_timestamp: cap("TIMESTAMP_MONOTONIC") != 0,
        },
        connectors,
        encoders,
        crtcs,
        planes,
    })
}

fn list<'a>(object: &'a Value, key: &str) -> Result<&'a Vec<Value>, String> {
    object[key]
        .as_array()
        .ok_or_else(|| format!("`{key}` is missing or not a list"))
}

fn uint(object: &Value, key: &str) -> Result<u64, String> {
    object[key]
        .as_u64()
        .ok_or_else(|| format!("`{key}` is missing or not a number"))
}

fn id(object: &Value) -> Result<u32, String> {
    uint(object, "id").map(|id| id as u32)
}

fn mode(mode: &Value) -> Result<Mode, String> {
    let field = |key: &str| uint(mode, key);
    Ok(Mode {
        name: mode["name"].as_str().unwrap_or_default().to_string(),
        clock: field("clock")? as u32,
        hdisplay: field("hdisplay")? as u16,
        hsync_start: field("hsync_start")? as u16,
        hsync_end: field("hsync_end")? as u16,
        htotal: field("htotal")? as u16,
        hskew: field("hskew")? as u16,
        vdisplay: field("vdisplay")? as u16,
        vsync_start: field("vsync_start")? as u16,
        vsync_end: field("vsync_end")? as u16,
        vtotal: field("vtotal")? as u16,
        vscan: field("vscan")? as u16,
        vrefresh: field("vrefresh")? as u32,
        flags: field("flags")? as u32,
        kind: field("type")? as u32,
    })
}

// `DRM_MODE_PROP_*`: the legacy type bits and the extended types.
const PROP_RANGE: u64 = 1 << 1;
const PROP_ENUM: u64 = 1 << 3;
const PROP_BLOB: u64 = 1 << 4;
const PROP_BITMASK: u64 = 1 << 5;
const PROP_OBJECT: u64 = 1 << 6;
const PROP_SIGNED_RANGE: u64 = 2 << 6;

fn properties(object: &Value) -> Result<Vec<Property>, String> {
    let map = object["properties"]
        .as_object()
        .ok_or("`properties` is missing or not an object")?;
    let mut out = Vec::new();
    for (name, property) in map {
        let spec = &property["spec"];
        let named_values = || {
            spec.as_array()
                .into_iter()
                .flatten()
                .map(|entry| EnumValue {
                    name: entry["name"].as_str().unwrap_or_default().to_string(),
                    value: entry["value"].as_u64().unwrap_or(0),
                })
                .collect::<Vec<_>>()
        };
        let kind = match uint(property, "type")? {
            PROP_RANGE => PropertyKind::Range {
                min: spec["min"].as_u64().unwrap_or(0),
                max: spec["max"].as_u64().unwrap_or(u64::MAX),
            },
            PROP_SIGNED_RANGE => PropertyKind::SignedRange {
                min: spec["min"].as_i64().unwrap_or(i64::MIN),
                max: spec["max"].as_i64().unwrap_or(i64::MAX),
            },
            PROP_ENUM => PropertyKind::Enum {
                values: named_values(),
            },
            PROP_BITMASK => PropertyKind::Bitmask {
                values: named_values(),
            },
            // Raw blob contents are written as base64 (the EDID is);
            // blobs drm_info decodes (`IN_FORMATS`, modes) are not kept.
            PROP_BLOB => PropertyKind::Blob {
                data: property["data"]
                    .as_str()
                    .and_then(decode_base64)
                    .map(|bytes| to_hex(&bytes)),
            },
            PROP_OBJECT => PropertyKind::Object,
            _ => PropertyKind::Unknown,
        };
        out.push(Property {
            id: id(property)?,
            name: name.clone(),
            value: uint(property, "raw_value")?,
            immutable: property["immutable"].as_bool().unwrap_or(false),
            kind,
        });
    }
    // The document's order of names is not the kernel's; make it stable.
    out.sort_by_key(|p| p.id);
    Ok(out)
}

/// `DRM_MODE_CONNECTOR_*` to the names the kernel uses for connectors.
fn connector_type_name(kind: u64) -> &'static str {
    const NAMES: [&str; 21] = [
        "Unknown",
        "VGA",
        "DVI-I",
        "DVI-D",
        "DVI-A",
        "Composite",
        "SVIDEO",
        "LVDS",
        "Component",
        "DIN",
        "DP",
        "HDMI-A",
        "HDMI-B",
        "TV",
        "eDP",
        "Virtual",
        "DSI",
        "DPI",
        "Writeback",
        "SPI",
        "USB",
    ];
    NAMES.get(kind as usize).copied().unwrap_or("Unknown")
}

/// Decodes standard base64, with or without padding. `None` for anything
/// that is not base64.
fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\n' | b'\r' => continue,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_is_decoded() {
        assert_eq!(decode_base64("").unwrap(), b"");
        assert_eq!(decode_base64("Zg==").unwrap(), b"f");
        assert_eq!(decode_base64("Zm8=").unwrap(), b"fo");
        assert_eq!(decode_base64("Zm9v").unwrap(), b"foo");
        assert_eq!(decode_base64("Zm9vYmFy").unwrap(), b"foobar");
        assert_eq!(decode_base64("AP///w").unwrap(), [0, 255, 255, 255]);
        assert_eq!(decode_base64("not base64!"), None);
    }

    #[test]
    fn a_small_document_is_read() {
        let text = r#"{
          "/dev/dri/card0": {
            "driver": {
              "name": "test", "desc": "A test driver",
              "version": { "major": 1, "minor": 2, "patch": 3 },
              "caps": { "DUMB_BUFFER": 1, "ADDFB2_MODIFIERS": 0, "CURSOR_WIDTH": 64,
                        "CURSOR_HEIGHT": 64, "TIMESTAMP_MONOTONIC": 1 }
            },
            "connectors": [
              { "id": 10, "type": 11, "status": 2, "phy_width": 0, "phy_height": 0,
                "encoders": [20], "modes": [], "properties": {} },
              { "id": 11, "type": 11, "status": 1, "phy_width": 600, "phy_height": 340,
                "encoders": [20],
                "modes": [ { "clock": 148500, "hdisplay": 1920, "hsync_start": 2008,
                             "hsync_end": 2052, "htotal": 2200, "hskew": 0, "vdisplay": 1080,
                             "vsync_start": 1084, "vsync_end": 1089, "vtotal": 1125,
                             "vscan": 0, "vrefresh": 60, "flags": 5, "type": 72,
                             "name": "1920x1080" } ],
                "properties": {
                  "EDID": { "id": 1, "type": 16, "immutable": true, "raw_value": 50,
                            "spec": null, "data": "AP///w==" },
                  "CRTC_ID": { "id": 21, "type": 64, "immutable": false, "raw_value": 30,
                               "spec": 3435973836, "data": null }
                } }
            ],
            "encoders": [ { "id": 20, "possible_crtcs": 1 } ],
            "crtcs": [ { "id": 30, "properties": {
              "ACTIVE": { "id": 22, "type": 2, "immutable": false, "raw_value": 1,
                          "spec": { "min": 0, "max": 1 }, "data": null } } } ],
            "planes": [ { "id": 40, "possible_crtcs": 1, "formats": [875713112],
              "properties": {
                "type": { "id": 9, "type": 8, "immutable": true, "raw_value": 1,
                          "spec": [ { "name": "Overlay", "value": 0 },
                                    { "name": "Primary", "value": 1 } ], "data": null },
                "IN_FENCE_FD": { "id": 19, "type": 128, "immutable": false,
                                 "raw_value": 18446744073709551615,
                                 "spec": { "min": -1, "max": 2147483647 }, "data": null },
                "IN_FORMATS": { "id": 32, "type": 16, "immutable": true, "raw_value": 36,
                                "spec": null,
                                "data": [ { "modifier": 0, "formats": [875713112] },
                                          { "modifier": 72057594037927938,
                                            "formats": [875713112] } ] }
              } } ]
          }
        }"#;
        let devices = snapshots_from_drm_info(text).unwrap();
        assert_eq!(devices.len(), 1);
        let (path, snapshot) = &devices[0];
        assert_eq!(path, "/dev/dri/card0");
        assert_eq!(snapshot.driver.name, "test");
        assert_eq!(snapshot.driver.version, "1.2.3");
        assert!(snapshot.caps.dumb_buffer && !snapshot.caps.addfb2_modifiers);
        // Connectors of one type are numbered in order.
        let names: Vec<_> = snapshot.connectors.iter().map(|c| &c.name).collect();
        assert_eq!(names, ["HDMI-A-1", "HDMI-A-2"]);
        let connector = &snapshot.connectors[1];
        assert!(connector.connected && !snapshot.connectors[0].connected);
        assert_eq!(connector.modes[0].refresh_mhz(), 60_000);
        assert!(connector.modes[0].is_preferred());
        let edid = super::super::model::property(&connector.properties, "EDID").unwrap();
        assert_eq!(
            edid.kind,
            PropertyKind::Blob {
                data: Some("00ffffff".to_string())
            }
        );
        let plane = &snapshot.planes[0];
        assert_eq!(plane.kind(), Some(super::super::model::PlaneKind::Primary));
        assert_eq!(plane.modifiers.as_ref().unwrap().len(), 2);
        let fence = super::super::model::property(&plane.properties, "IN_FENCE_FD").unwrap();
        assert_eq!(fence.value, u64::MAX);
        assert_eq!(
            fence.kind,
            PropertyKind::SignedRange {
                min: -1,
                max: 2147483647
            }
        );
    }

    #[test]
    fn a_malformed_document_is_an_error() {
        assert!(snapshots_from_drm_info("[]").is_err());
        assert!(snapshots_from_drm_info(r#"{"/dev/dri/card0": {}}"#).is_err());
    }
}
