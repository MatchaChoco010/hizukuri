/// A TrueType font with one glyph: `A` is a square from (100, 0) to
/// (700, 600) in an em of 1000 units, 800 units wide. Everything else is
/// the empty glyph.
pub fn square_font() -> Vec<u8> {
    fn table(out: &mut Vec<u8>, values: &[i32]) {
        for v in values {
            out.extend_from_slice(&(*v as u16).to_be_bytes());
        }
    }
    let mut head = Vec::new();
    table(&mut head, &[1, 0, 1, 0]); // version, revision
    head.extend_from_slice(&0u32.to_be_bytes()); // checksum adjustment
    head.extend_from_slice(&0x5f0f_3cf5u32.to_be_bytes()); // magic
    table(&mut head, &[0, 1000]); // flags, units per em
    head.extend_from_slice(&[0; 16]); // created, modified
    table(&mut head, &[0, 0, 800, 800, 0, 8, 2, 0, 0]); // box, style, ..., short loca

    let mut hhea = Vec::new();
    // version, ascender, descender, line gap, widest advance, bearings,
    // extent, caret, reserved, metric format, number of metrics.
    table(
        &mut hhea,
        &[
            1, 0, 800, -200, 0, 800, 0, 0, 800, 1, 0, 0, 0, 0, 0, 0, 0, 2,
        ],
    );

    let mut maxp = Vec::new();
    // version 1.0, 2 glyphs, 4 points, 1 contour, then the limits of
    // composites and instructions.
    table(&mut maxp, &[1, 0, 2, 4, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0]);

    let mut hmtx = Vec::new();
    table(&mut hmtx, &[500, 0, 800, 100]);

    let mut glyf = Vec::new();
    // One contour, its box, the last point, no instructions.
    table(&mut glyf, &[1, 100, 0, 700, 600, 3, 0]);
    glyf.extend_from_slice(&[1, 1, 1, 1]); // four points on the curve
    table(&mut glyf, &[100, 0, 600, 0]); // x, as steps
    table(&mut glyf, &[0, 600, 0, -600]); // y, as steps
    if glyf.len() % 2 == 1 {
        glyf.push(0);
    }

    let mut loca = Vec::new();
    // Glyph 0 is empty; glyph 1 is all of glyf. Offsets are halved.
    table(&mut loca, &[0, 0, glyf.len() as i32 / 2]);

    let mut cmap = Vec::new();
    // One subtable: Windows, Unicode BMP, format 4, two segments.
    table(&mut cmap, &[0, 1, 3, 1, 0, 12]);
    table(
        &mut cmap,
        &[
            4,
            32,
            0,
            4,
            4,
            1,
            0, // format, length, language, segments, search
            0x41,
            0xffff,
            0, // ends, padding
            0x41,
            0xffff, // starts
            1 - 0x41,
            1, // deltas: 'A' is glyph 1
            0,
            0, // no glyph id array
        ],
    );

    let tables: [(&[u8; 4], &Vec<u8>); 7] = [
        (b"cmap", &cmap),
        (b"glyf", &glyf),
        (b"head", &head),
        (b"hhea", &hhea),
        (b"hmtx", &hmtx),
        (b"loca", &loca),
        (b"maxp", &maxp),
    ];
    let mut font = Vec::new();
    table(&mut font, &[1, 0, tables.len() as i32, 64, 2, 48]);
    let mut offset = 12 + 16 * tables.len();
    for (tag, data) in &tables {
        font.extend_from_slice(*tag);
        font.extend_from_slice(&0u32.to_be_bytes());
        font.extend_from_slice(&(offset as u32).to_be_bytes());
        font.extend_from_slice(&(data.len() as u32).to_be_bytes());
        offset += data.len().next_multiple_of(4);
    }
    for (_, data) in &tables {
        font.extend_from_slice(data);
        font.resize(font.len().next_multiple_of(4), 0);
    }
    font
}
