//! Decisions of the KMS backend, as pure functions of a
//! [`DeviceSnapshot`]: which connector, CRTC, plane and mode to drive, how
//! to assemble the atomic commits, and which way frames get to the plane.
//!
//! Nothing here touches a device, so all of it is unit tested, including
//! against snapshots of real machines.

use super::model::{
    AtomicRequest, Connector, DeviceSnapshot, FOURCC_XRGB8888, MODIFIER_LINEAR, Mode, PlaneKind,
    property,
};

/// What the user asked to be driven.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OutputRequest {
    /// Connector name (`DP-1`). `None` takes the first connected one.
    pub connector: Option<String>,
    /// `None` takes the preferred mode.
    pub mode: Option<ModeRequest>,
}

/// A mode asked for by size, optionally by refresh rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeRequest {
    pub width: u32,
    pub height: u32,
    /// In mHz. `None` takes the highest refresh rate of that size.
    pub refresh_mhz: Option<u32>,
}

/// Why no pipeline could be planned.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    #[error("no connector is connected (connectors: {available:?})")]
    NothingConnected { available: Vec<String> },
    #[error("there is no connector named {name:?} (connectors: {available:?})")]
    UnknownConnector {
        name: String,
        available: Vec<String>,
    },
    #[error("connector {name} is not connected")]
    Disconnected { name: String },
    #[error("connector {name} is not a display (it is a writeback connector)")]
    NotADisplay { name: String },
    #[error("connector {connector} has no modes")]
    NoModes { connector: String },
    #[error("connector {connector} has no {width}x{height} mode (modes: {available:?})")]
    NoSuchMode {
        connector: String,
        width: u32,
        height: u32,
        available: Vec<String>,
    },
    #[error("no free CRTC can drive connector {connector}")]
    NoCrtc { connector: String },
    #[error("CRTC {crtc} has no primary plane that takes XRGB8888")]
    NoPrimaryPlane { crtc: u32 },
    #[error("{object} {id} has no {name} property")]
    MissingProperty {
        object: &'static str,
        id: u32,
        name: &'static str,
    },
}

/// Ids of the properties an atomic commit sets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineProperties {
    pub connector_crtc_id: u32,
    pub crtc_active: u32,
    pub crtc_mode_id: u32,
    pub plane_fb_id: u32,
    pub plane_crtc_id: u32,
    pub plane_src_x: u32,
    pub plane_src_y: u32,
    pub plane_src_w: u32,
    pub plane_src_h: u32,
    pub plane_crtc_x: u32,
    pub plane_crtc_y: u32,
    pub plane_crtc_w: u32,
    pub plane_crtc_h: u32,
}

/// One connector with the CRTC and primary plane that drive it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pipeline {
    pub connector: u32,
    pub connector_name: String,
    pub physical_size_mm: (u32, u32),
    pub crtc: u32,
    pub primary_plane: u32,
    pub mode: Mode,
    pub properties: PipelineProperties,
    /// The modifiers the primary plane takes for XRGB8888. `None` when the
    /// driver does not report modifiers.
    pub plane_modifiers: Option<Vec<u64>>,
    /// Other planes that are on this CRTC right now, left there by whoever
    /// drove the display before (a cursor, an overlay). The modeset takes
    /// them off, or they would stay on screen above our frames.
    pub stale_planes: Vec<StalePlane>,
}

/// A plane to detach, with the ids of the properties that does it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StalePlane {
    pub plane: u32,
    pub fb_id: u32,
    pub crtc_id: u32,
}

/// The planes other than `primary` that are attached to `crtc`.
pub fn stale_planes(snapshot: &DeviceSnapshot, crtc: u32, primary: u32) -> Vec<StalePlane> {
    snapshot
        .planes
        .iter()
        .filter(|plane| plane.id != primary)
        .filter_map(|plane| {
            let crtc_id = property(&plane.properties, "CRTC_ID")?;
            let fb_id = property(&plane.properties, "FB_ID")?;
            (crtc_id.value == crtc as u64).then_some(StalePlane {
                plane: plane.id,
                fb_id: fb_id.id,
                crtc_id: crtc_id.id,
            })
        })
        .collect()
}

fn connector_names(snapshot: &DeviceSnapshot) -> Vec<String> {
    snapshot.connectors.iter().map(|c| c.name.clone()).collect()
}

/// Chooses the connector: the named one, or the first connected one.
pub fn choose_connector<'a>(
    snapshot: &'a DeviceSnapshot,
    name: Option<&str>,
) -> Result<&'a Connector, PlanError> {
    match name {
        Some(name) => {
            let connector =
                snapshot
                    .connector_by_name(name)
                    .ok_or_else(|| PlanError::UnknownConnector {
                        name: name.to_string(),
                        available: connector_names(snapshot),
                    })?;
            if connector.is_writeback() {
                return Err(PlanError::NotADisplay {
                    name: name.to_string(),
                });
            }
            if !connector.connected {
                return Err(PlanError::Disconnected {
                    name: name.to_string(),
                });
            }
            Ok(connector)
        }
        // Writeback connectors can report themselves connected (vkms on
        // kernel 6.19 does), and things like VR headsets are connected
        // without being a desktop display. Neither is picked unasked.
        None => snapshot
            .connectors
            .iter()
            .find(|c| c.connected && !c.is_writeback() && !c.is_non_desktop())
            .ok_or_else(|| PlanError::NothingConnected {
                available: connector_names(snapshot),
            }),
    }
}

/// Chooses the mode.
///
/// Without a request: the mode the display marks as preferred, else the one
/// with the most pixels, and among those the highest refresh rate. With a
/// request: a mode of that size; with a refresh rate the closest one,
/// without the highest.
pub fn choose_mode(connector: &Connector, request: Option<ModeRequest>) -> Result<Mode, PlanError> {
    if connector.modes.is_empty() {
        return Err(PlanError::NoModes {
            connector: connector.name.clone(),
        });
    }
    let area = |m: &Mode| m.hdisplay as u64 * m.vdisplay as u64;
    match request {
        None => Ok(connector
            .modes
            .iter()
            .find(|m| m.is_preferred())
            .or_else(|| {
                connector
                    .modes
                    .iter()
                    .max_by_key(|m| (area(m), m.refresh_mhz()))
            })
            .expect("modes is not empty")
            .clone()),
        Some(request) => {
            let candidates = connector
                .modes
                .iter()
                .filter(|m| m.size() == (request.width, request.height));
            let chosen = match request.refresh_mhz {
                Some(wanted) => {
                    candidates.min_by_key(|m| (m.refresh_mhz() as i64 - wanted as i64).abs())
                }
                None => candidates.max_by_key(|m| m.refresh_mhz()),
            };
            chosen.cloned().ok_or_else(|| PlanError::NoSuchMode {
                connector: connector.name.clone(),
                width: request.width,
                height: request.height,
                available: connector
                    .modes
                    .iter()
                    .map(|m| format!("{}x{}@{}", m.hdisplay, m.vdisplay, m.refresh_mhz()))
                    .collect(),
            })
        }
    }
}

/// Chooses a CRTC for a connector.
///
/// Other connectors are left alone: a CRTC that another connector is
/// routed to is never taken. The CRTC the connector is already routed to is
/// preferred, so taking over from whoever drove the display before changes
/// as little as possible.
pub fn choose_crtc(snapshot: &DeviceSnapshot, connector: &Connector) -> Result<u32, PlanError> {
    let routed = |c: &Connector| {
        property(&c.properties, "CRTC_ID")
            .map(|p| p.value as u32)
            .filter(|id| *id != 0)
    };
    let busy: Vec<u32> = snapshot
        .connectors
        .iter()
        .filter(|c| c.id != connector.id)
        .filter_map(routed)
        .collect();

    let mut possible: Vec<u32> = Vec::new();
    for encoder_id in &connector.encoders {
        if let Some(encoder) = snapshot.encoders.iter().find(|e| e.id == *encoder_id) {
            for crtc in snapshot.crtcs_in_mask(encoder.possible_crtcs) {
                if !possible.contains(&crtc) {
                    possible.push(crtc);
                }
            }
        }
    }
    possible.retain(|crtc| !busy.contains(crtc));

    if let Some(current) = routed(connector)
        && possible.contains(&current)
    {
        return Ok(current);
    }
    possible.first().copied().ok_or_else(|| PlanError::NoCrtc {
        connector: connector.name.clone(),
    })
}

/// Chooses the primary plane of a CRTC: a plane of type `Primary` that can
/// be on that CRTC and takes XRGB8888.
pub fn choose_primary_plane(snapshot: &DeviceSnapshot, crtc: u32) -> Result<u32, PlanError> {
    snapshot
        .planes
        .iter()
        .find(|plane| {
            plane.kind() == Some(PlaneKind::Primary)
                && snapshot.crtcs_in_mask(plane.possible_crtcs).contains(&crtc)
                && plane.formats.contains(&FOURCC_XRGB8888)
        })
        .map(|plane| plane.id)
        .ok_or(PlanError::NoPrimaryPlane { crtc })
}

/// Plans the pipeline for one output.
pub fn plan_output(
    snapshot: &DeviceSnapshot,
    request: &OutputRequest,
) -> Result<Pipeline, PlanError> {
    let connector = choose_connector(snapshot, request.connector.as_deref())?;
    let mode = choose_mode(connector, request.mode)?;
    let crtc = choose_crtc(snapshot, connector)?;
    let plane_id = choose_primary_plane(snapshot, crtc)?;
    let plane = snapshot.plane(plane_id).expect("just chosen");
    let crtc_object = snapshot.crtc(crtc).expect("just chosen");

    let find = |properties, object, id, name: &'static str| {
        property(properties, name)
            .map(|p| p.id)
            .ok_or(PlanError::MissingProperty { object, id, name })
    };
    let plane_property = |name| find(&plane.properties, "plane", plane.id, name);
    let properties = PipelineProperties {
        connector_crtc_id: find(&connector.properties, "connector", connector.id, "CRTC_ID")?,
        crtc_active: find(&crtc_object.properties, "CRTC", crtc, "ACTIVE")?,
        crtc_mode_id: find(&crtc_object.properties, "CRTC", crtc, "MODE_ID")?,
        plane_fb_id: plane_property("FB_ID")?,
        plane_crtc_id: plane_property("CRTC_ID")?,
        plane_src_x: plane_property("SRC_X")?,
        plane_src_y: plane_property("SRC_Y")?,
        plane_src_w: plane_property("SRC_W")?,
        plane_src_h: plane_property("SRC_H")?,
        plane_crtc_x: plane_property("CRTC_X")?,
        plane_crtc_y: plane_property("CRTC_Y")?,
        plane_crtc_w: plane_property("CRTC_W")?,
        plane_crtc_h: plane_property("CRTC_H")?,
    };

    Ok(Pipeline {
        connector: connector.id,
        connector_name: connector.name.clone(),
        physical_size_mm: connector.physical_size_mm,
        crtc,
        primary_plane: plane.id,
        mode,
        properties,
        plane_modifiers: plane.modifiers_for(FOURCC_XRGB8888),
        stale_planes: stale_planes(snapshot, crtc, plane.id),
    })
}

fn plane_changes(pipeline: &Pipeline, request: &mut AtomicRequest, fb: u32) {
    let p = &pipeline.properties;
    let plane = pipeline.primary_plane;
    let (width, height) = pipeline.mode.size();
    request.set(plane, p.plane_fb_id, fb as u64);
    request.set(plane, p.plane_crtc_id, pipeline.crtc as u64);
    // The source rectangle is in 16.16 fixed point.
    request.set(plane, p.plane_src_x, 0);
    request.set(plane, p.plane_src_y, 0);
    request.set(plane, p.plane_src_w, (width as u64) << 16);
    request.set(plane, p.plane_src_h, (height as u64) << 16);
    request.set(plane, p.plane_crtc_x, 0);
    request.set(plane, p.plane_crtc_y, 0);
    request.set(plane, p.plane_crtc_w, width as u64);
    request.set(plane, p.plane_crtc_h, height as u64);
}

/// The commit that turns the output on with a mode and a first frame.
/// Needs `ALLOW_MODESET`.
pub fn modeset_request(pipeline: &Pipeline, fb: u32, mode_blob: u32) -> AtomicRequest {
    let mut request = AtomicRequest::default();
    let p = &pipeline.properties;
    request.set(
        pipeline.connector,
        p.connector_crtc_id,
        pipeline.crtc as u64,
    );
    request.set(pipeline.crtc, p.crtc_mode_id, mode_blob as u64);
    request.set(pipeline.crtc, p.crtc_active, 1);
    plane_changes(pipeline, &mut request, fb);
    for stale in &pipeline.stale_planes {
        request.set(stale.plane, stale.fb_id, 0);
        request.set(stale.plane, stale.crtc_id, 0);
    }
    request
}

/// The commit that shows the next frame: only the framebuffer changes.
pub fn flip_request(pipeline: &Pipeline, fb: u32) -> AtomicRequest {
    let mut request = AtomicRequest::default();
    request.set(
        pipeline.primary_plane,
        pipeline.properties.plane_fb_id,
        fb as u64,
    );
    request
}

/// The commit that turns the output off and detaches everything. Needs
/// `ALLOW_MODESET`.
pub fn disable_request(pipeline: &Pipeline) -> AtomicRequest {
    let mut request = AtomicRequest::default();
    let p = &pipeline.properties;
    request.set(pipeline.primary_plane, p.plane_fb_id, 0);
    request.set(pipeline.primary_plane, p.plane_crtc_id, 0);
    request.set(pipeline.crtc, p.crtc_active, 0);
    request.set(pipeline.crtc, p.crtc_mode_id, 0);
    request.set(pipeline.connector, p.connector_crtc_id, 0);
    request
}

/// How rendered frames get to the primary plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanoutPath {
    /// Whatever draws allocates the scanout images and exports them as
    /// dma-bufs; KMS scans them out directly. The modifiers are the ones
    /// both sides accept.
    Dmabuf { modifiers: Vec<u64> },
    /// Whatever draws draws offscreen; each frame is read back and copied
    /// into a dumb buffer on the CPU.
    CpuCopy,
}

/// What the user wants for the scanout path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScanoutPreference {
    /// dma-buf when it can work, CPU copy otherwise.
    #[default]
    Auto,
    Dmabuf,
    CpuCopy,
}

/// The facts the choice of scanout path depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanoutFacts {
    pub preference: ScanoutPreference,
    /// The single-plane modifiers whatever draws can export an XRGB8888
    /// render target with. Empty when it cannot export dma-bufs (or there
    /// is nothing that draws).
    pub export_modifiers: Vec<u64>,
    /// Whether what draws runs on the device that owns the KMS card.
    /// A dma-buf from another device (or from a software rasterizer)
    /// generally cannot be scanned out.
    pub draws_on_kms_device: bool,
    /// The modifiers the primary plane takes for XRGB8888; `None` when the
    /// driver reports no modifiers.
    pub plane_modifiers: Option<Vec<u64>>,
    pub dumb_buffers: bool,
}

/// Why no scanout path is possible.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScanoutError {
    #[error("the dma-buf path was requested but cannot work: {0}")]
    DmabufImpossible(String),
    #[error("the CPU copy path needs dumb buffers, which the driver does not have")]
    NoDumbBuffers,
    #[error("no scanout path works: {dmabuf}; and the driver has no dumb buffers")]
    Nothing { dmabuf: String },
}

/// The modifiers a dma-buf scanout image may have, or the reason the
/// dma-buf path cannot work.
fn dmabuf_modifiers(facts: &ScanoutFacts) -> Result<Vec<u64>, String> {
    if facts.export_modifiers.is_empty() {
        return Err("what draws cannot export render targets as dma-bufs".to_string());
    }
    if !facts.draws_on_kms_device {
        return Err("what draws does not run on the device that drives the display".to_string());
    }
    // A driver without modifier support scans out what its own allocator
    // would produce without modifiers, which is linear memory.
    let accepted: Vec<u64> = match &facts.plane_modifiers {
        Some(list) => list.clone(),
        None => vec![MODIFIER_LINEAR],
    };
    let common: Vec<u64> = facts
        .export_modifiers
        .iter()
        .copied()
        .filter(|m| accepted.contains(m))
        .collect();
    if common.is_empty() {
        return Err(format!(
            "what draws and the plane share no format modifier (export {:x?}, plane {:x?})",
            facts.export_modifiers, accepted
        ));
    }
    Ok(common)
}

/// Chooses the scanout path.
pub fn choose_scanout_path(facts: &ScanoutFacts) -> Result<ScanoutPath, ScanoutError> {
    let dmabuf = dmabuf_modifiers(facts);
    match facts.preference {
        ScanoutPreference::Dmabuf => dmabuf
            .map(|modifiers| ScanoutPath::Dmabuf { modifiers })
            .map_err(ScanoutError::DmabufImpossible),
        ScanoutPreference::CpuCopy => {
            if facts.dumb_buffers {
                Ok(ScanoutPath::CpuCopy)
            } else {
                Err(ScanoutError::NoDumbBuffers)
            }
        }
        ScanoutPreference::Auto => match dmabuf {
            Ok(modifiers) => Ok(ScanoutPath::Dmabuf { modifiers }),
            Err(_) if facts.dumb_buffers => Ok(ScanoutPath::CpuCopy),
            Err(dmabuf) => Err(ScanoutError::Nothing { dmabuf }),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::super::mock::SnapshotBuilder;
    use super::super::model::{CommitFlags, MODIFIER_INVALID, Property, PropertyKind};
    use super::*;

    fn two_heads() -> DeviceSnapshot {
        SnapshotBuilder::new("test")
            .crtcs(2)
            .connector(
                "DP-1",
                true,
                &[(1920, 1080, 60, true), (1280, 720, 60, false)],
            )
            .connector("HDMI-A-1", true, &[(1024, 768, 60, true)])
            .build()
    }

    #[test]
    fn first_connected_connector_is_chosen_by_default() {
        let snapshot = SnapshotBuilder::new("test")
            .crtcs(2)
            .connector("DP-1", false, &[])
            .connector("HDMI-A-1", true, &[(1024, 768, 60, true)])
            .build();
        assert_eq!(choose_connector(&snapshot, None).unwrap().name, "HDMI-A-1");
    }

    #[test]
    fn writeback_and_non_desktop_connectors_are_not_picked_unasked() {
        let mut snapshot = SnapshotBuilder::new("test")
            .crtcs(3)
            .connector("Writeback-1", true, &[(1024, 768, 60, true)])
            .connector("DP-1", true, &[(2160, 2160, 90, true)])
            .connector("HDMI-A-1", true, &[(1024, 768, 60, true)])
            .build();
        // DP-1 is a headset.
        snapshot.connectors[1].properties.push(Property {
            id: 9000,
            name: "non-desktop".into(),
            value: 1,
            immutable: true,
            kind: PropertyKind::Range { min: 0, max: 1 },
        });
        assert_eq!(choose_connector(&snapshot, None).unwrap().name, "HDMI-A-1");
        // Asked for by name, the headset is used; writeback never is.
        assert_eq!(
            choose_connector(&snapshot, Some("DP-1")).unwrap().name,
            "DP-1"
        );
        assert_eq!(
            choose_connector(&snapshot, Some("Writeback-1")),
            Err(PlanError::NotADisplay {
                name: "Writeback-1".into()
            })
        );
    }

    #[test]
    fn a_named_connector_must_exist_and_be_connected() {
        let snapshot = SnapshotBuilder::new("test")
            .crtcs(1)
            .connector("DP-1", false, &[])
            .connector("DP-2", true, &[(800, 600, 60, true)])
            .build();
        assert_eq!(
            choose_connector(&snapshot, Some("DP-2")).unwrap().name,
            "DP-2"
        );
        assert_eq!(
            choose_connector(&snapshot, Some("DP-1")),
            Err(PlanError::Disconnected {
                name: "DP-1".into()
            })
        );
        assert!(matches!(
            choose_connector(&snapshot, Some("VGA-1")),
            Err(PlanError::UnknownConnector { available, .. }) if available == ["DP-1", "DP-2"]
        ));
        let none = SnapshotBuilder::new("test")
            .crtcs(1)
            .connector("DP-1", false, &[])
            .build();
        assert!(matches!(
            choose_connector(&none, None),
            Err(PlanError::NothingConnected { .. })
        ));
    }

    #[test]
    fn preferred_mode_wins_then_the_largest_and_fastest() {
        let snapshot = SnapshotBuilder::new("test")
            .crtcs(1)
            .connector(
                "DP-1",
                true,
                &[
                    (3840, 2160, 60, false),
                    (1920, 1080, 144, true),
                    (3840, 2160, 144, false),
                ],
            )
            .build();
        let connector = &snapshot.connectors[0];
        let mode = choose_mode(connector, None).unwrap();
        assert_eq!((mode.size(), mode.vrefresh), ((1920, 1080), 144));

        let mut no_preferred = connector.clone();
        for mode in &mut no_preferred.modes {
            mode.kind = 0;
        }
        let mode = choose_mode(&no_preferred, None).unwrap();
        assert_eq!((mode.size(), mode.vrefresh), ((3840, 2160), 144));

        let mut empty = connector.clone();
        empty.modes.clear();
        assert!(matches!(
            choose_mode(&empty, None),
            Err(PlanError::NoModes { .. })
        ));
    }

    #[test]
    fn a_requested_mode_is_matched_by_size_and_nearest_refresh() {
        let snapshot = SnapshotBuilder::new("test")
            .crtcs(1)
            .connector(
                "DP-1",
                true,
                &[
                    (1920, 1080, 60, true),
                    (1920, 1080, 120, false),
                    (1280, 720, 60, false),
                ],
            )
            .build();
        let connector = &snapshot.connectors[0];
        let request = |refresh_mhz| ModeRequest {
            width: 1920,
            height: 1080,
            refresh_mhz,
        };
        assert_eq!(
            choose_mode(connector, Some(request(None)))
                .unwrap()
                .vrefresh,
            120
        );
        assert_eq!(
            choose_mode(connector, Some(request(Some(59_940))))
                .unwrap()
                .vrefresh,
            60
        );
        assert_eq!(
            choose_mode(connector, Some(request(Some(100_000))))
                .unwrap()
                .vrefresh,
            120
        );
        assert!(matches!(
            choose_mode(
                connector,
                Some(ModeRequest {
                    width: 640,
                    height: 480,
                    refresh_mhz: None
                })
            ),
            Err(PlanError::NoSuchMode { available, .. }) if available.len() == 3
        ));
    }

    #[test]
    fn crtc_already_routed_to_the_connector_is_kept() {
        let mut snapshot = two_heads();
        let (crtc0, crtc1) = (snapshot.crtcs[0].id, snapshot.crtcs[1].id);
        // Nothing routed: the first possible CRTC.
        assert_eq!(
            choose_crtc(&snapshot, &snapshot.connectors[0]).unwrap(),
            crtc0
        );
        // DP-1 already on the second CRTC: keep it.
        SnapshotBuilder::route(&mut snapshot, "DP-1", crtc1);
        assert_eq!(
            choose_crtc(&snapshot, &snapshot.connectors[0]).unwrap(),
            crtc1
        );
    }

    #[test]
    fn crtcs_of_other_connectors_are_not_taken() {
        let mut snapshot = two_heads();
        let (crtc0, crtc1) = (snapshot.crtcs[0].id, snapshot.crtcs[1].id);
        // The other connector occupies the first CRTC.
        SnapshotBuilder::route(&mut snapshot, "HDMI-A-1", crtc0);
        assert_eq!(
            choose_crtc(&snapshot, &snapshot.connectors[0]).unwrap(),
            crtc1
        );
        // Both CRTCs belong to others: nothing is stolen.
        let mut snapshot = SnapshotBuilder::new("test")
            .crtcs(1)
            .connector("DP-1", true, &[(800, 600, 60, true)])
            .connector("DP-2", true, &[(800, 600, 60, true)])
            .build();
        let crtc = snapshot.crtcs[0].id;
        SnapshotBuilder::route(&mut snapshot, "DP-2", crtc);
        assert!(matches!(
            choose_crtc(&snapshot, &snapshot.connectors[0]),
            Err(PlanError::NoCrtc { .. })
        ));
    }

    #[test]
    fn primary_plane_belongs_to_the_crtc_and_takes_xrgb() {
        let snapshot = two_heads();
        let (crtc0, crtc1) = (snapshot.crtcs[0].id, snapshot.crtcs[1].id);
        let plane0 = choose_primary_plane(&snapshot, crtc0).unwrap();
        let plane1 = choose_primary_plane(&snapshot, crtc1).unwrap();
        assert_ne!(plane0, plane1);
        for (plane, crtc) in [(plane0, crtc0), (plane1, crtc1)] {
            let plane = snapshot.plane(plane).unwrap();
            assert_eq!(plane.kind(), Some(PlaneKind::Primary));
            assert!(snapshot.crtcs_in_mask(plane.possible_crtcs).contains(&crtc));
        }
        // A cursor plane is never chosen, even when it is listed first.
        let mut reordered = snapshot.clone();
        reordered.planes.reverse();
        assert_eq!(choose_primary_plane(&reordered, crtc0).unwrap(), plane0);
        // No XRGB8888: no plane.
        let mut odd = snapshot.clone();
        for plane in &mut odd.planes {
            plane.formats.retain(|f| *f != FOURCC_XRGB8888);
        }
        assert_eq!(
            choose_primary_plane(&odd, crtc0),
            Err(PlanError::NoPrimaryPlane { crtc: crtc0 })
        );
    }

    #[test]
    fn plan_reports_missing_properties() {
        let mut snapshot = two_heads();
        snapshot.crtcs[0].properties.retain(|p| p.name != "MODE_ID");
        assert_eq!(
            plan_output(&snapshot, &OutputRequest::default()),
            Err(PlanError::MissingProperty {
                object: "CRTC",
                id: snapshot.crtcs[0].id,
                name: "MODE_ID"
            })
        );
    }

    #[test]
    fn modeset_request_routes_and_sizes_everything() {
        let snapshot = two_heads();
        let pipeline = plan_output(
            &snapshot,
            &OutputRequest {
                connector: Some("DP-1".into()),
                mode: None,
            },
        )
        .unwrap();
        assert_eq!(pipeline.connector_name, "DP-1");
        assert_eq!(pipeline.mode.size(), (1920, 1080));
        let p = &pipeline.properties;

        let request = modeset_request(&pipeline, 77, 88);
        assert_eq!(
            request.value(pipeline.connector, p.connector_crtc_id),
            Some(pipeline.crtc as u64)
        );
        assert_eq!(request.value(pipeline.crtc, p.crtc_mode_id), Some(88));
        assert_eq!(request.value(pipeline.crtc, p.crtc_active), Some(1));
        let plane = pipeline.primary_plane;
        assert_eq!(request.value(plane, p.plane_fb_id), Some(77));
        assert_eq!(
            request.value(plane, p.plane_crtc_id),
            Some(pipeline.crtc as u64)
        );
        assert_eq!(request.value(plane, p.plane_src_w), Some(1920 << 16));
        assert_eq!(request.value(plane, p.plane_src_h), Some(1080 << 16));
        assert_eq!(request.value(plane, p.plane_crtc_w), Some(1920));
        assert_eq!(request.value(plane, p.plane_crtc_h), Some(1080));
        assert_eq!(request.value(plane, p.plane_crtc_x), Some(0));
        // Only objects of this pipeline are touched.
        assert!(pipeline.stale_planes.is_empty());
        assert!(
            request
                .changes
                .iter()
                .all(|c| [pipeline.connector, pipeline.crtc, plane].contains(&c.object))
        );

        let flip = flip_request(&pipeline, 99);
        assert_eq!(flip.changes.len(), 1);
        assert_eq!(flip.value(plane, p.plane_fb_id), Some(99));

        let disable = disable_request(&pipeline);
        assert_eq!(disable.value(pipeline.crtc, p.crtc_active), Some(0));
        assert_eq!(disable.value(plane, p.plane_fb_id), Some(0));
        assert_eq!(
            disable.value(pipeline.connector, p.connector_crtc_id),
            Some(0)
        );

        // The flags that go with them.
        let (modeset, flip) = (CommitFlags::MODESET, CommitFlags::FLIP);
        assert!(modeset.allow_modeset && !modeset.nonblock && modeset.page_flip_event);
        assert!(flip.nonblock && flip.page_flip_event && !flip.allow_modeset);
    }

    fn facts() -> ScanoutFacts {
        ScanoutFacts {
            preference: ScanoutPreference::Auto,
            export_modifiers: vec![0x0200_0000_0008_2305, MODIFIER_LINEAR],
            draws_on_kms_device: true,
            plane_modifiers: Some(vec![MODIFIER_LINEAR, 0x0200_0000_0008_2305]),
            dumb_buffers: true,
        }
    }

    #[test]
    fn dmabuf_path_uses_the_modifiers_both_sides_accept() {
        assert_eq!(
            choose_scanout_path(&facts()),
            Ok(ScanoutPath::Dmabuf {
                modifiers: vec![0x0200_0000_0008_2305, MODIFIER_LINEAR]
            })
        );
        // A driver without modifiers (virtio-gpu): linear only.
        assert_eq!(
            choose_scanout_path(&ScanoutFacts {
                plane_modifiers: None,
                ..facts()
            }),
            Ok(ScanoutPath::Dmabuf {
                modifiers: vec![MODIFIER_LINEAR]
            })
        );
    }

    #[test]
    fn cpu_copy_is_the_fallback() {
        // A software rasterizer exports dma-bufs, but not ones KMS can use.
        assert_eq!(
            choose_scanout_path(&ScanoutFacts {
                draws_on_kms_device: false,
                ..facts()
            }),
            Ok(ScanoutPath::CpuCopy)
        );
        // Nothing draws, or it cannot export dma-bufs.
        assert_eq!(
            choose_scanout_path(&ScanoutFacts {
                export_modifiers: vec![],
                ..facts()
            }),
            Ok(ScanoutPath::CpuCopy)
        );
        // No modifier in common.
        assert_eq!(
            choose_scanout_path(&ScanoutFacts {
                plane_modifiers: Some(vec![MODIFIER_INVALID]),
                ..facts()
            }),
            Ok(ScanoutPath::CpuCopy)
        );
    }

    #[test]
    fn explicit_preferences_are_honored_or_refused() {
        assert_eq!(
            choose_scanout_path(&ScanoutFacts {
                preference: ScanoutPreference::CpuCopy,
                ..facts()
            }),
            Ok(ScanoutPath::CpuCopy)
        );
        assert_eq!(
            choose_scanout_path(&ScanoutFacts {
                preference: ScanoutPreference::CpuCopy,
                dumb_buffers: false,
                ..facts()
            }),
            Err(ScanoutError::NoDumbBuffers)
        );
        assert!(matches!(
            choose_scanout_path(&ScanoutFacts {
                preference: ScanoutPreference::Dmabuf,
                draws_on_kms_device: false,
                ..facts()
            }),
            Err(ScanoutError::DmabufImpossible(_))
        ));
        assert!(matches!(
            choose_scanout_path(&ScanoutFacts {
                export_modifiers: vec![],
                dumb_buffers: false,
                ..facts()
            }),
            Err(ScanoutError::Nothing { .. })
        ));
    }
}
