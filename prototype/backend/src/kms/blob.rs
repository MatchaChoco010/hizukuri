//! Parsing of the property blobs the backend looks into: the format and
//! modifier list of a plane (`IN_FORMATS`), and the EDID of a display.
//! Pure functions over bytes.

use super::model::{FormatModifier, Fourcc};

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn u64_at(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

/// Parses an `IN_FORMATS` blob (`struct drm_format_modifier_blob`) into the
/// list of format and modifier pairs the plane takes. Returns `None` for a
/// blob that is truncated or of an unknown version.
pub fn parse_in_formats(blob: &[u8]) -> Option<Vec<FormatModifier>> {
    // struct drm_format_modifier_blob {
    //     __u32 version; __u32 flags;
    //     __u32 count_formats; __u32 formats_offset;
    //     __u32 count_modifiers; __u32 modifiers_offset;
    // };
    if u32_at(blob, 0)? != 1 {
        return None;
    }
    let count_formats = u32_at(blob, 8)? as usize;
    let formats_offset = u32_at(blob, 12)? as usize;
    let count_modifiers = u32_at(blob, 16)? as usize;
    let modifiers_offset = u32_at(blob, 20)? as usize;

    let mut formats = Vec::with_capacity(count_formats);
    for index in 0..count_formats {
        formats.push(Fourcc(u32_at(blob, formats_offset + index * 4)?));
    }

    // struct drm_format_modifier {
    //     __u64 formats;   // bitmask of formats, starting at `offset`
    //     __u32 offset; __u32 pad;
    //     __u64 modifier;
    // };
    let mut pairs = Vec::new();
    for index in 0..count_modifiers {
        let base = modifiers_offset + index * 24;
        let mask = u64_at(blob, base)?;
        let offset = u32_at(blob, base + 8)? as usize;
        let modifier = u64_at(blob, base + 16)?;
        for bit in 0..64 {
            if mask & (1 << bit) != 0
                && let Some(format) = formats.get(offset + bit)
            {
                pairs.push(FormatModifier {
                    format: *format,
                    modifier,
                });
            }
        }
    }
    Some(pairs)
}

/// What the base block of an EDID says about the display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdidIdentity {
    /// Three letter PNP id of the manufacturer, e.g. `GSM`.
    pub manufacturer: String,
    pub product_code: u16,
    /// The monitor name descriptor, when present.
    pub name: Option<String>,
}

const EDID_HEADER: [u8; 8] = [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];

fn descriptor_text(descriptor: &[u8]) -> String {
    String::from_utf8_lossy(&descriptor[5..18])
        .trim_end_matches(['\n', ' '])
        .to_string()
}

/// Reads the manufacturer, product code and name out of an EDID. Returns
/// `None` when the data is not an EDID base block.
pub fn edid_identity(edid: &[u8]) -> Option<EdidIdentity> {
    if edid.len() < 128 || edid[..8] != EDID_HEADER {
        return None;
    }
    let id = u16::from_be_bytes([edid[8], edid[9]]);
    let letter = |shift: u16| (b'A' - 1 + ((id >> shift) & 0x1f) as u8) as char;
    let manufacturer: String = [letter(10), letter(5), letter(0)].iter().collect();
    let product_code = u16::from_le_bytes([edid[10], edid[11]]);
    // Four 18 byte descriptors start at offset 54. A display descriptor
    // starts with 00 00 00 <tag>; tag 0xfc is the monitor name.
    let name = (0..4)
        .map(|i| &edid[54 + i * 18..54 + (i + 1) * 18])
        .find(|d| d[..3] == [0, 0, 0] && d[3] == 0xfc)
        .map(descriptor_text);
    Some(EdidIdentity {
        manufacturer,
        product_code,
        name,
    })
}

/// The PNP id an anonymized EDID carries: `XXX`, five bits per letter.
const ANONYMOUS_MANUFACTURER: u16 = (24 << 10) | (24 << 5) | 24;
/// The name an anonymized EDID carries.
const ANONYMOUS_NAME: &[u8; 13] = b"Display\n     ";

/// Blanks an 18 byte display descriptor when it holds text that identifies
/// the display: its name, its serial number, or free text.
fn anonymize_descriptor(descriptor: &mut [u8]) {
    if descriptor[..3] != [0, 0, 0] {
        return;
    }
    match descriptor[3] {
        // Display product name.
        0xfc => descriptor[5..18].copy_from_slice(ANONYMOUS_NAME),
        // Display product serial number.
        0xff => descriptor[5..18].copy_from_slice(b"0\n           "),
        // Alphanumeric data string: free text of the manufacturer.
        0xfe => descriptor[5..18].copy_from_slice(b"\n            "),
        _ => {}
    }
}

/// Makes the last byte of a 128 byte EDID block the checksum of the rest.
fn fix_block_checksum(block: &mut [u8]) {
    let sum = block[..127].iter().fold(0u8, |s, b| s.wrapping_add(*b));
    block[127] = 0u8.wrapping_sub(sum);
}

/// The descriptors after the data blocks of a CTA-861 extension block.
fn anonymize_cta_block(block: &mut [u8]) {
    let mut offset = block[2] as usize;
    if offset < 4 {
        return;
    }
    while offset + 18 <= 127 {
        anonymize_descriptor(&mut block[offset..offset + 18]);
        offset += 18;
    }
}

/// The product identification data block of a DisplayID extension block.
fn anonymize_displayid_block(block: &mut [u8]) {
    // Byte 0 is the EDID extension tag. The DisplayID section follows:
    // version, length of the data, product type, extension count, the data
    // blocks, and a checksum over the section.
    let version = block[1];
    let end = 5 + block[2] as usize;
    if end >= 127 {
        return;
    }
    let mut offset = 5;
    while offset + 3 <= end {
        let tag = block[offset];
        let next = offset + 3 + block[offset + 2] as usize;
        if next > end {
            break;
        }
        // Tag 0x00 in DisplayID 1.x (an empty one is padding), 0x20 in 2.x.
        let identification = if version < 0x20 {
            tag == 0x00 && next > offset + 3
        } else {
            tag == 0x20
        };
        if identification {
            block[offset + 3..next].fill(0);
        }
        offset = next;
    }
    let sum = block[1..end].iter().fold(0u8, |s, b| s.wrapping_add(*b));
    block[end] = 0u8.wrapping_sub(sum);
}

/// Removes what identifies the display from an EDID, in place: the
/// manufacturer, the product code, the serial number, the date of
/// manufacture, and the name, serial number and free text descriptors. The
/// same is done to the descriptors of CTA-861 extension blocks and to the
/// product identification of DisplayID extension blocks. Checksums are
/// corrected.
///
/// What a compositor acts on (modes, color characteristics, HDR metadata,
/// the range of refresh rates) is left as it is.
///
/// Used before a captured EDID is stored in the repository as a test
/// fixture. Applying it twice changes nothing more.
pub fn anonymize_edid(edid: &mut [u8]) {
    if edid.len() < 128 || edid[..8] != EDID_HEADER {
        return;
    }
    let (base, extensions) = edid.split_at_mut(128);
    base[8..10].copy_from_slice(&ANONYMOUS_MANUFACTURER.to_be_bytes());
    // Product code, serial number, week and year of manufacture.
    base[10..18].fill(0);
    for index in 0..4 {
        anonymize_descriptor(&mut base[54 + index * 18..54 + (index + 1) * 18]);
    }
    fix_block_checksum(base);
    for block in extensions.as_chunks_mut::<128>().0 {
        match block[0] {
            0x02 => anonymize_cta_block(block),
            0x70 => anonymize_displayid_block(block),
            _ => continue,
        }
        fix_block_checksum(block);
    }
}

/// Hex text of a blob, for JSON.
pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Bytes of hex text. `None` when the text is not hex.
pub fn from_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len() / 2)
        .map(|i| u8::from_str_radix(text.get(i * 2..i * 2 + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kms::model::{FOURCC_ARGB8888, FOURCC_XRGB8888};

    /// Builds an IN_FORMATS blob the way the kernel lays it out.
    fn in_formats(formats: &[Fourcc], modifiers: &[(u64, u32, u64)]) -> Vec<u8> {
        let formats_offset = 24u32;
        let modifiers_offset = formats_offset + formats.len() as u32 * 4;
        // Modifiers are 8 byte aligned.
        let modifiers_offset = modifiers_offset.next_multiple_of(8);
        let mut blob = Vec::new();
        for value in [
            1,
            0,
            formats.len() as u32,
            formats_offset,
            modifiers.len() as u32,
            modifiers_offset,
        ] {
            blob.extend_from_slice(&value.to_le_bytes());
        }
        for format in formats {
            blob.extend_from_slice(&format.0.to_le_bytes());
        }
        blob.resize(modifiers_offset as usize, 0);
        for (mask, offset, modifier) in modifiers {
            blob.extend_from_slice(&mask.to_le_bytes());
            blob.extend_from_slice(&offset.to_le_bytes());
            blob.extend_from_slice(&0u32.to_le_bytes());
            blob.extend_from_slice(&modifier.to_le_bytes());
        }
        blob
    }

    #[test]
    fn in_formats_pairs_formats_with_modifiers() {
        let other = Fourcc::from_bytes(*b"AB30");
        let blob = in_formats(
            &[FOURCC_XRGB8888, FOURCC_ARGB8888, other],
            &[
                // LINEAR for all three.
                (0b111, 0, 0),
                // A vendor modifier for the first two only.
                (0b011, 0, 0x0200_0000_0000_0001),
                // A mask relative to an offset: bit 0 is format index 2.
                (0b001, 2, 0x0200_0000_0000_0002),
            ],
        );
        let pairs = parse_in_formats(&blob).unwrap();
        let has = |format, modifier| pairs.contains(&FormatModifier { format, modifier });
        assert_eq!(pairs.len(), 6);
        assert!(has(FOURCC_XRGB8888, 0));
        assert!(has(other, 0));
        assert!(has(FOURCC_ARGB8888, 0x0200_0000_0000_0001));
        assert!(!has(other, 0x0200_0000_0000_0001));
        assert!(has(other, 0x0200_0000_0000_0002));
        assert!(!has(FOURCC_XRGB8888, 0x0200_0000_0000_0002));
    }

    #[test]
    fn broken_in_formats_blobs_are_rejected() {
        let blob = in_formats(&[FOURCC_XRGB8888], &[(1, 0, 0)]);
        assert!(parse_in_formats(&blob[..blob.len() - 1]).is_none());
        assert!(parse_in_formats(&blob[..10]).is_none());
        assert!(parse_in_formats(&[]).is_none());
        let mut wrong_version = blob.clone();
        wrong_version[0] = 2;
        assert!(parse_in_formats(&wrong_version).is_none());
        // A mask bit beyond the format list is ignored, not an error.
        let blob = in_formats(&[FOURCC_XRGB8888], &[(0b11, 0, 0)]);
        assert_eq!(parse_in_formats(&blob).unwrap().len(), 1);
    }

    /// A made-up EDID base block: manufacturer "HZK", product 0x1234,
    /// serial 0xdeadbeef, a serial text descriptor and a name descriptor.
    fn edid() -> Vec<u8> {
        let mut edid = vec![0u8; 128];
        edid[..8].copy_from_slice(&EDID_HEADER);
        // "HZK": H = 8, Z = 26, K = 11, five bits each, big endian.
        let id: u16 = (8 << 10) | (26 << 5) | 11;
        edid[8..10].copy_from_slice(&id.to_be_bytes());
        edid[10..12].copy_from_slice(&0x1234u16.to_le_bytes());
        edid[12..16].copy_from_slice(&0xdead_beefu32.to_le_bytes());
        edid[18] = 1;
        edid[19] = 4;
        // Descriptor 1: a detailed timing (non-zero pixel clock).
        edid[54] = 0x02;
        edid[55] = 0x3a;
        // Descriptor 2: serial number text.
        edid[72..77].copy_from_slice(&[0, 0, 0, 0xff, 0]);
        edid[77..90].copy_from_slice(b"SN123456789\n ");
        // Descriptor 3: monitor name.
        edid[90..95].copy_from_slice(&[0, 0, 0, 0xfc, 0]);
        edid[95..108].copy_from_slice(b"Test Panel\n  ");
        let sum: u8 = edid[..127].iter().fold(0u8, |s, b| s.wrapping_add(*b));
        edid[127] = 0u8.wrapping_sub(sum);
        edid
    }

    fn checksum_ok(edid: &[u8]) -> bool {
        edid[..128].iter().fold(0u8, |s, b| s.wrapping_add(*b)) == 0
    }

    #[test]
    fn edid_identity_is_read_from_the_base_block() {
        let identity = edid_identity(&edid()).unwrap();
        assert_eq!(identity.manufacturer, "HZK");
        assert_eq!(identity.product_code, 0x1234);
        assert_eq!(identity.name.as_deref(), Some("Test Panel"));
        assert!(edid_identity(&[0; 128]).is_none());
        assert!(edid_identity(&edid()[..100]).is_none());
    }

    #[test]
    fn anonymizing_removes_the_identity_and_keeps_the_rest_valid() {
        let original = edid();
        assert!(checksum_ok(&original));
        let mut cleaned = original.clone();
        anonymize_edid(&mut cleaned);
        assert!(checksum_ok(&cleaned));
        // Product code, serial number and date of manufacture.
        assert_eq!(cleaned[10..18], [0; 8]);
        assert!(!cleaned.windows(5).any(|w| w == b"SN123"));
        assert!(!cleaned.windows(4).any(|w| w == b"Test"));
        assert_eq!(
            edid_identity(&cleaned).unwrap(),
            EdidIdentity {
                manufacturer: "XXX".to_string(),
                product_code: 0,
                name: Some("Display".to_string()),
            }
        );
        // The timing descriptor and everything after the date are untouched.
        assert_eq!(cleaned[54..72], original[54..72]);
        assert_eq!(cleaned[18..54], original[18..54]);
        // Doing it again changes nothing.
        let mut again = cleaned.clone();
        anonymize_edid(&mut again);
        assert_eq!(again, cleaned);
        // Things that are not an EDID are left alone.
        let mut junk = vec![7u8; 200];
        anonymize_edid(&mut junk);
        assert_eq!(junk, vec![7u8; 200]);
    }

    #[test]
    fn anonymizing_covers_the_extension_blocks() {
        let mut edid = edid();
        edid[126] = 2;
        fix_block_checksum(&mut edid[..128]);

        // A CTA-861 block: one data block of three bytes, then a detailed
        // timing and a name descriptor.
        let mut cta = vec![0u8; 128];
        cta[..4].copy_from_slice(&[0x02, 3, 8, 0]);
        cta[4..8].copy_from_slice(&[0x23, 0x09, 0x07, 0x07]);
        cta[8] = 0x02;
        cta[9] = 0x3a;
        cta[26..31].copy_from_slice(&[0, 0, 0, 0xfc, 0]);
        cta[31..44].copy_from_slice(b"Other Name\n  ");
        fix_block_checksum(&mut cta);

        // A DisplayID 1.2 block: a product identification block, then
        // another data block that must survive.
        let mut displayid = vec![0u8; 128];
        displayid[..5].copy_from_slice(&[0x70, 0x12, 23, 0, 0]);
        displayid[5..8].copy_from_slice(&[0x00, 0, 15]);
        displayid[8..23].copy_from_slice(b"VNDpc\x01\x02\x03\x04wy\x03ABC");
        displayid[23..26].copy_from_slice(&[0x0f, 0, 2]);
        displayid[26..28].copy_from_slice(&[0xaa, 0xbb]);
        let sum = displayid[1..28].iter().fold(0u8, |s, b| s.wrapping_add(*b));
        displayid[28] = 0u8.wrapping_sub(sum);
        fix_block_checksum(&mut displayid);

        edid.extend_from_slice(&cta);
        edid.extend_from_slice(&displayid);
        let original = edid.clone();
        anonymize_edid(&mut edid);

        for block in edid.as_chunks::<128>().0 {
            assert!(checksum_ok(block));
        }
        let (cta, displayid) = (&edid[128..256], &edid[256..384]);
        assert!(!cta.windows(5).any(|w| w == b"Other"));
        assert_eq!(cta[..26], original[128..154]);
        assert_eq!(displayid[8..23], [0; 15]);
        assert_eq!(displayid[23..28], original[256 + 23..256 + 28]);
        let section = displayid[1..29].iter().fold(0u8, |s, b| s.wrapping_add(*b));
        assert_eq!(section, 0);
    }

    #[test]
    fn hex_round_trips() {
        let bytes = [0x00, 0xff, 0x10, 0xab];
        assert_eq!(to_hex(&bytes), "00ff10ab");
        assert_eq!(from_hex("00ff10ab").unwrap(), bytes);
        assert!(from_hex("0").is_none());
        assert!(from_hex("zz").is_none());
    }
}
