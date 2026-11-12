# Test fixtures: snapshots of DRM devices

Each `*.kms.json` file is a `DeviceSnapshot`: what a DRM device reports
(connectors with their modes and EDID, CRTCs, planes, properties and their
current values). The tests build a mock device from them, so planning and
the KMS backend are exercised against what real devices look like.

They are captured with

```sh
hizukuri kms-info --device /dev/dri/cardN --anonymize
```

which opens the device read-only, does not take DRM master and changes
nothing. `--anonymize` removes what identifies the displays from the EDIDs:
manufacturer, product code, serial number, date of manufacture, and the
name, serial number and free text descriptors. Modes, color characteristics
and HDR metadata stay. The tests check that every EDID stored here is
anonymized. The files are stored as compact JSON to keep them small.

| File | Device |
| --- | --- |
| `amdgpu.kms.json` | A device of the `amdgpu` driver in use. What the tests rely on: three connected connectors (DP-1 with a 144 Hz mode and the HDR and VRR properties, DP-2, HDMI-A-1), each routed to a CRTC, primary planes with framebuffers, a cursor plane in use, and `IN_FORMATS` with vendor modifiers. |
| `virtio-gpu-7.3.3.kms.json` | The verification VM's virtio-gpu (`virtio-vga-gl`), kernel 7.3.3. One connector (`Virtual-1`), one CRTC, the primary plane only: the kernel hides this driver's cursor plane from clients that do not declare `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT`. |
| `vkms-7.3.3.kms.json` | vkms in the same guest: `Virtual-2` (connector numbers are global to the kernel), one CRTC, a primary plane, eight overlays and a cursor plane. No writeback connector, because hizukuri does not ask for them. |

## Records made with drm_info

The tests also read the `drm_info -j` records of the verification VM's
devices (virtio-gpu and vkms, on two kernels) in `tools/vm/reference/`,
through `hizukuri_backend::kms::drm_info`. A record made with `drm_info -j`
on any machine can be used the same way.
