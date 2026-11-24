//! A plain-data description of a DRM/KMS device: what `drmModeGetResources`
//! and friends report, in a form that can be written to and read from a
//! file.
//!
//! Decisions (which connector, CRTC, plane and mode to use; how to assemble
//! an atomic commit) are made from a [`DeviceSnapshot`], never by asking the
//! device directly. That keeps them pure, and lets tests feed them the
//! snapshot of a real machine saved as JSON (`proto-compositor kms-info`).

use serde::{Deserialize, Serialize};

/// `DRM_MODE_TYPE_PREFERRED`.
pub const MODE_TYPE_PREFERRED: u32 = 1 << 3;
/// `DRM_MODE_FLAG_INTERLACE`.
pub const MODE_FLAG_INTERLACE: u32 = 1 << 4;
/// `DRM_MODE_FLAG_DBLSCAN`.
pub const MODE_FLAG_DBLSCAN: u32 = 1 << 5;

/// DRM fourcc `XRGB8888`.
pub const FOURCC_XRGB8888: Fourcc = Fourcc::from_bytes(*b"XR24");
/// DRM fourcc `ARGB8888`.
pub const FOURCC_ARGB8888: Fourcc = Fourcc::from_bytes(*b"AR24");
/// `DRM_FORMAT_MOD_LINEAR`.
pub const MODIFIER_LINEAR: u64 = 0;
/// `DRM_FORMAT_MOD_INVALID`: "no modifier given".
pub const MODIFIER_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// A DRM pixel format code. Written to JSON as its four characters
/// (`"XR24"`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Fourcc(pub u32);

impl Fourcc {
    pub const fn from_bytes(bytes: [u8; 4]) -> Self {
        Self(u32::from_le_bytes(bytes))
    }
}

impl std::fmt::Debug for Fourcc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self}")
    }
}

impl std::fmt::Display for Fourcc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let bytes = self.0.to_le_bytes();
        if bytes.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
            write!(f, "{}", String::from_utf8_lossy(&bytes))
        } else {
            write!(f, "{:#010x}", self.0)
        }
    }
}

impl Serialize for Fourcc {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Fourcc {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        if let Some(hex) = text.strip_prefix("0x") {
            return u32::from_str_radix(hex, 16)
                .map(Fourcc)
                .map_err(serde::de::Error::custom);
        }
        let bytes: [u8; 4] = text
            .as_bytes()
            .try_into()
            .map_err(|_| serde::de::Error::custom(format!("fourcc {text:?} is not 4 bytes")))?;
        Ok(Fourcc::from_bytes(bytes))
    }
}

/// Everything the planning code needs to know about a device.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceSnapshot {
    pub driver: DriverInfo,
    pub caps: DeviceCaps,
    pub connectors: Vec<Connector>,
    pub encoders: Vec<Encoder>,
    /// In the order the kernel lists them: bit `n` of a `possible_crtcs`
    /// mask refers to the `n`th entry.
    pub crtcs: Vec<Crtc>,
    pub planes: Vec<Plane>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriverInfo {
    pub name: String,
    pub description: String,
    pub version: String,
}

/// Driver capabilities (`DRM_CAP_*`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCaps {
    pub dumb_buffer: bool,
    /// Framebuffers can be created with format modifiers.
    pub addfb2_modifiers: bool,
    pub async_page_flip: bool,
    /// Tearing page flips through the atomic API.
    pub atomic_async_page_flip: bool,
    pub cursor_width: u32,
    pub cursor_height: u32,
    pub monotonic_timestamp: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Connector {
    pub id: u32,
    /// Type and index as the kernel names them, e.g. `DP-1`, `Virtual-1`.
    pub name: String,
    pub connected: bool,
    pub physical_size_mm: (u32, u32),
    pub modes: Vec<Mode>,
    pub encoders: Vec<u32>,
    pub properties: Vec<Property>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Encoder {
    pub id: u32,
    pub possible_crtcs: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Crtc {
    pub id: u32,
    pub properties: Vec<Property>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plane {
    pub id: u32,
    pub possible_crtcs: u32,
    pub formats: Vec<Fourcc>,
    /// The format and modifier pairs from the `IN_FORMATS` property. `None`
    /// when the plane has no such property: the driver takes no modifiers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modifiers: Option<Vec<FormatModifier>>,
    pub properties: Vec<Property>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormatModifier {
    pub format: Fourcc,
    pub modifier: u64,
}

/// The kind of a plane, from its `type` property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaneKind {
    Overlay,
    Primary,
    Cursor,
}

/// A display mode: the fields of `drm_mode_modeinfo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mode {
    pub name: String,
    /// Pixel clock in kHz.
    pub clock: u32,
    pub hdisplay: u16,
    pub hsync_start: u16,
    pub hsync_end: u16,
    pub htotal: u16,
    pub hskew: u16,
    pub vdisplay: u16,
    pub vsync_start: u16,
    pub vsync_end: u16,
    pub vtotal: u16,
    pub vscan: u16,
    /// Refresh rate in Hz as the kernel rounds it.
    pub vrefresh: u32,
    pub flags: u32,
    /// `DRM_MODE_TYPE_*` bits.
    pub kind: u32,
}

impl Mode {
    pub fn size(&self) -> (u32, u32) {
        (self.hdisplay as u32, self.vdisplay as u32)
    }

    pub fn is_preferred(&self) -> bool {
        self.kind & MODE_TYPE_PREFERRED != 0
    }

    /// Refresh rate in mHz, computed from the timings (the `vrefresh` field
    /// is rounded to whole Hz). Falls back to `vrefresh` when the timings
    /// are missing.
    pub fn refresh_mhz(&self) -> u32 {
        let total = self.htotal as u64 * self.vtotal as u64;
        if total == 0 || self.clock == 0 {
            return self.vrefresh * 1000;
        }
        let mut numerator = self.clock as u64 * 1_000_000;
        let mut denominator = total;
        if self.flags & MODE_FLAG_INTERLACE != 0 {
            numerator *= 2;
        }
        if self.flags & MODE_FLAG_DBLSCAN != 0 {
            denominator *= 2;
        }
        if self.vscan > 1 {
            denominator *= self.vscan as u64;
        }
        ((numerator + denominator / 2) / denominator) as u32
    }

    /// A mode with CVT-like blanking, for tests and synthetic devices.
    pub fn synthetic(width: u16, height: u16, refresh_hz: u32, preferred: bool) -> Mode {
        let htotal = width + 160;
        let vtotal = height + 45;
        Mode {
            name: format!("{width}x{height}"),
            clock: (htotal as u32 * vtotal as u32 * refresh_hz).div_ceil(1000),
            hdisplay: width,
            hsync_start: width + 48,
            hsync_end: width + 80,
            htotal,
            hskew: 0,
            vdisplay: height,
            vsync_start: height + 3,
            vsync_end: height + 8,
            vtotal,
            vscan: 0,
            vrefresh: refresh_hz,
            flags: 0,
            kind: if preferred { MODE_TYPE_PREFERRED } else { 0 },
        }
    }
}

/// A KMS property of an object, with its current value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Property {
    pub id: u32,
    pub name: String,
    pub value: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub immutable: bool,
    pub kind: PropertyKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PropertyKind {
    Range {
        min: u64,
        max: u64,
    },
    SignedRange {
        min: i64,
        max: i64,
    },
    Enum {
        values: Vec<EnumValue>,
    },
    Bitmask {
        values: Vec<EnumValue>,
    },
    /// A blob. `data` holds the contents as hex when they were captured.
    Blob {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<String>,
    },
    /// A reference to another KMS object (framebuffer, CRTC, ...).
    Object,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnumValue {
    pub name: String,
    pub value: u64,
}

impl Connector {
    /// A writeback connector: it captures what a CRTC shows instead of
    /// showing it. The kernel only lists these to clients that ask.
    pub fn is_writeback(&self) -> bool {
        self.name.starts_with("Writeback-")
    }

    /// The `non-desktop` property: a display that is not meant to show a
    /// desktop (a VR headset, a drawing tablet's control screen).
    pub fn is_non_desktop(&self) -> bool {
        property(&self.properties, "non-desktop").is_some_and(|p| p.value != 0)
    }
}

/// Finds a property by name.
pub fn property<'a>(properties: &'a [Property], name: &str) -> Option<&'a Property> {
    properties.iter().find(|p| p.name == name)
}

impl Plane {
    pub fn kind(&self) -> Option<PlaneKind> {
        let property = property(&self.properties, "type")?;
        let PropertyKind::Enum { values } = &property.kind else {
            return None;
        };
        let name = &values.iter().find(|v| v.value == property.value)?.name;
        match name.as_str() {
            "Overlay" => Some(PlaneKind::Overlay),
            "Primary" => Some(PlaneKind::Primary),
            "Cursor" => Some(PlaneKind::Cursor),
            _ => None,
        }
    }

    /// The modifiers the plane takes for a format. `None` when the plane
    /// does not report modifiers at all.
    pub fn modifiers_for(&self, format: Fourcc) -> Option<Vec<u64>> {
        self.modifiers.as_ref().map(|list| {
            list.iter()
                .filter(|m| m.format == format)
                .map(|m| m.modifier)
                .collect()
        })
    }
}

impl DeviceSnapshot {
    pub fn connector(&self, id: u32) -> Option<&Connector> {
        self.connectors.iter().find(|c| c.id == id)
    }

    pub fn connector_by_name(&self, name: &str) -> Option<&Connector> {
        self.connectors.iter().find(|c| c.name == name)
    }

    pub fn crtc(&self, id: u32) -> Option<&Crtc> {
        self.crtcs.iter().find(|c| c.id == id)
    }

    pub fn plane(&self, id: u32) -> Option<&Plane> {
        self.planes.iter().find(|p| p.id == id)
    }

    /// Position of a CRTC in the kernel's list, which is its bit in
    /// `possible_crtcs` masks.
    pub fn crtc_index(&self, id: u32) -> Option<usize> {
        self.crtcs.iter().position(|c| c.id == id)
    }

    /// The CRTCs a `possible_crtcs` mask allows.
    pub fn crtcs_in_mask(&self, mask: u32) -> Vec<u32> {
        self.crtcs
            .iter()
            .enumerate()
            .filter(|(index, _)| *index < 32 && mask & (1 << index) != 0)
            .map(|(_, crtc)| crtc.id)
            .collect()
    }

    /// The properties of any object, by its id.
    pub fn object_properties(&self, id: u32) -> Option<&[Property]> {
        if let Some(connector) = self.connector(id) {
            return Some(&connector.properties);
        }
        if let Some(crtc) = self.crtc(id) {
            return Some(&crtc.properties);
        }
        self.plane(id).map(|plane| plane.properties.as_slice())
    }
}

/// One property assignment of an atomic commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PropertyChange {
    pub object: u32,
    pub property: u32,
    pub value: u64,
}

/// The property assignments of an atomic commit.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AtomicRequest {
    pub changes: Vec<PropertyChange>,
}

impl AtomicRequest {
    pub fn set(&mut self, object: u32, property: u32, value: u64) {
        self.changes.push(PropertyChange {
            object,
            property,
            value,
        });
    }

    /// The value this request assigns to a property, if it does.
    pub fn value(&self, object: u32, property: u32) -> Option<u64> {
        self.changes
            .iter()
            .rev()
            .find(|c| c.object == object && c.property == property)
            .map(|c| c.value)
    }
}

/// `DRM_MODE_ATOMIC_*` / `DRM_MODE_PAGE_FLIP_*` flags of a commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CommitFlags {
    /// Check the request without applying it.
    pub test_only: bool,
    /// Allow changes that need a full modeset.
    pub allow_modeset: bool,
    /// Return before the change is on screen.
    pub nonblock: bool,
    /// Send a page flip event when the change is on screen.
    pub page_flip_event: bool,
    /// Flip immediately instead of at vblank (tearing).
    pub page_flip_async: bool,
}

impl CommitFlags {
    /// A blocking full modeset that reports its completion.
    pub const MODESET: CommitFlags = CommitFlags {
        test_only: false,
        allow_modeset: true,
        nonblock: false,
        page_flip_event: true,
        page_flip_async: false,
    };

    /// A page flip at the next vblank, reported with an event.
    pub const FLIP: CommitFlags = CommitFlags {
        test_only: false,
        allow_modeset: false,
        nonblock: true,
        page_flip_event: true,
        page_flip_async: false,
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fourcc_round_trips_through_json_as_text() {
        let json = serde_json::to_string(&FOURCC_XRGB8888).unwrap();
        assert_eq!(json, "\"XR24\"");
        assert_eq!(
            serde_json::from_str::<Fourcc>(&json).unwrap(),
            FOURCC_XRGB8888
        );
        let odd = Fourcc(0x0000_0001);
        let json = serde_json::to_string(&odd).unwrap();
        assert_eq!(json, "\"0x00000001\"");
        assert_eq!(serde_json::from_str::<Fourcc>(&json).unwrap(), odd);
        assert!(serde_json::from_str::<Fourcc>("\"TOOLONG\"").is_err());
    }

    #[test]
    fn refresh_is_computed_from_the_timings() {
        // CTA-861 1920x1080@60: 148.5 MHz, 2200 x 1125.
        let mode = Mode {
            name: "1920x1080".into(),
            clock: 148_500,
            hdisplay: 1920,
            hsync_start: 2008,
            hsync_end: 2052,
            htotal: 2200,
            hskew: 0,
            vdisplay: 1080,
            vsync_start: 1084,
            vsync_end: 1089,
            vtotal: 1125,
            vscan: 0,
            vrefresh: 60,
            flags: 0,
            kind: MODE_TYPE_PREFERRED,
        };
        assert_eq!(mode.refresh_mhz(), 60_000);
        assert!(mode.is_preferred());
        assert_eq!(mode.size(), (1920, 1080));

        // 59.94 Hz variant: 148.352 MHz.
        let ntsc = Mode {
            clock: 148_352,
            vrefresh: 60,
            ..mode.clone()
        };
        assert_eq!(ntsc.refresh_mhz(), 59_940);

        // 1080i60: the field rate is twice the frame rate.
        let interlaced = Mode {
            clock: 74_250,
            flags: MODE_FLAG_INTERLACE,
            ..mode.clone()
        };
        assert_eq!(interlaced.refresh_mhz(), 60_000);

        // No timings: fall back to the rounded field.
        let bare = Mode { clock: 0, ..mode };
        assert_eq!(bare.refresh_mhz(), 60_000);
    }

    #[test]
    fn synthetic_modes_have_the_requested_refresh() {
        let mode = Mode::synthetic(1024, 768, 60, true);
        assert!((mode.refresh_mhz() as i64 - 60_000).abs() < 100);
    }

    #[test]
    fn atomic_request_reports_the_last_value_set() {
        let mut request = AtomicRequest::default();
        request.set(1, 10, 5);
        request.set(1, 10, 7);
        assert_eq!(request.value(1, 10), Some(7));
        assert_eq!(request.value(1, 11), None);
    }
}
