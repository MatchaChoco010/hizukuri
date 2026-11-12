//! Images read back from what draws, and their file formats.
//!
//! - [`EncodedImage`]: the output after encoding, written as PNG (8 or 16
//!   bit RGBA).
//! - [`CompositeImage`]: the compositing buffer (half float RGBA, linear,
//!   premultiplied alpha), written as an uncompressed scanline OpenEXR file.
//!
//! The OpenEXR support is deliberately minimal: it writes, and reads back,
//! only the subset this crate produces (uncompressed scanlines, HALF
//! channels A, B, G, R).

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;

use proto_color::{CompositeSpace, ImageDescription, Primaries};

use proto_backend::OutputFormat;

/// Errors of reading and writing image files.
#[derive(Debug, thiserror::Error)]
pub enum ImageIoError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("PNG encoding error: {0}")]
    PngEncode(#[from] png::EncodingError),
    #[error("PNG decoding error: {0}")]
    PngDecode(#[from] png::DecodingError),
    #[error("unsupported file: {0}")]
    Unsupported(String),
}

/// An output image after encoding: code values as the output would see them.
#[derive(Debug, Clone, PartialEq)]
pub struct EncodedImage {
    pub width: u32,
    pub height: u32,
    pub format: OutputFormat,
    /// What the code values mean.
    pub image_description: ImageDescription,
    /// Tightly packed RGBA rows. 16 bit channels are native endian.
    pub data: Vec<u8>,
}

impl EncodedImage {
    /// Raw code values of one pixel as `[r, g, b, a]`, whatever the byte
    /// order of the format. 8 bit formats return values in `0..=255`, 16 bit
    /// formats in `0..=65535`.
    pub fn pixel(&self, x: u32, y: u32) -> [u16; 4] {
        assert!(x < self.width && y < self.height, "pixel out of bounds");
        let index = (y as usize * self.width as usize + x as usize) * self.format.bytes_per_pixel();
        match self.format {
            OutputFormat::Rgba8Unorm => {
                let p = &self.data[index..index + 4];
                [p[0] as u16, p[1] as u16, p[2] as u16, p[3] as u16]
            }
            OutputFormat::Bgra8Unorm => {
                let p = &self.data[index..index + 4];
                [p[2] as u16, p[1] as u16, p[0] as u16, p[3] as u16]
            }
            OutputFormat::Rgba16Unorm => {
                let p = &self.data[index..index + 8];
                std::array::from_fn(|i| u16::from_ne_bytes([p[i * 2], p[i * 2 + 1]]))
            }
        }
    }

    /// One pixel with channels normalized to `[0, 1]`.
    pub fn pixel_normalized(&self, x: u32, y: u32) -> [f32; 4] {
        let max = self.format.max_code() as f32;
        self.pixel(x, y).map(|v| v as f32 / max)
    }

    /// Writes an 8 or 16 bit RGBA PNG. A `bgra8` image is written as RGBA
    /// and reads back as `rgba8`.
    ///
    /// The PNG carries no color metadata; the code values are those of
    /// [`Self::image_description`], whatever that is.
    pub fn write_png(&self, path: &Path) -> Result<(), ImageIoError> {
        let file = BufWriter::new(File::create(path)?);
        let mut encoder = png::Encoder::new(file, self.width, self.height);
        encoder.set_color(png::ColorType::Rgba);
        match self.format {
            OutputFormat::Rgba8Unorm => {
                encoder.set_depth(png::BitDepth::Eight);
                let mut writer = encoder.write_header()?;
                writer.write_image_data(&self.data)?;
                writer.finish()?;
            }
            OutputFormat::Bgra8Unorm => {
                encoder.set_depth(png::BitDepth::Eight);
                let mut writer = encoder.write_header()?;
                // PNG wants R, G, B, A. The fourth byte of a scanout format
                // is padding as far as the display is concerned; the
                // drawing may write the compositing alpha there.
                let rgba: Vec<u8> = self
                    .data
                    .chunks_exact(4)
                    .flat_map(|p| [p[2], p[1], p[0], p[3]])
                    .collect();
                writer.write_image_data(&rgba)?;
                writer.finish()?;
            }
            OutputFormat::Rgba16Unorm => {
                encoder.set_depth(png::BitDepth::Sixteen);
                let mut writer = encoder.write_header()?;
                // PNG stores 16 bit samples big endian.
                let big_endian: Vec<u8> = self
                    .data
                    .chunks_exact(2)
                    .flat_map(|c| u16::from_ne_bytes([c[0], c[1]]).to_be_bytes())
                    .collect();
                writer.write_image_data(&big_endian)?;
                writer.finish()?;
            }
        }
        Ok(())
    }

    /// Reads a PNG written by [`Self::write_png`]. The image description is
    /// not stored in the file, so the caller supplies it.
    pub fn read_png(
        path: &Path,
        image_description: ImageDescription,
    ) -> Result<Self, ImageIoError> {
        let decoder = png::Decoder::new(BufReader::new(File::open(path)?));
        let mut reader = decoder.read_info()?;
        let size = reader
            .output_buffer_size()
            .ok_or_else(|| ImageIoError::Unsupported("PNG is too large".to_string()))?;
        let mut buffer = vec![0; size];
        let info = reader.next_frame(&mut buffer)?;
        buffer.truncate(info.buffer_size());
        if info.color_type != png::ColorType::Rgba {
            return Err(ImageIoError::Unsupported(format!(
                "PNG color type {:?}, expected RGBA",
                info.color_type
            )));
        }
        let (format, data) = match info.bit_depth {
            png::BitDepth::Eight => (OutputFormat::Rgba8Unorm, buffer),
            png::BitDepth::Sixteen => (
                OutputFormat::Rgba16Unorm,
                buffer
                    .chunks_exact(2)
                    .flat_map(|c| u16::from_be_bytes([c[0], c[1]]).to_ne_bytes())
                    .collect(),
            ),
            other => {
                return Err(ImageIoError::Unsupported(format!(
                    "PNG bit depth {other:?}"
                )));
            }
        };
        Ok(Self {
            width: info.width,
            height: info.height,
            format,
            image_description,
            data,
        })
    }
}

/// The compositing buffer: linear light, premultiplied alpha, reference
/// white at 1.0, in the primaries of [`Self::space`].
#[derive(Debug, Clone, PartialEq)]
pub struct CompositeImage {
    pub width: u32,
    pub height: u32,
    pub space: CompositeSpace,
    /// Tightly packed RGBA rows of IEEE 754 half floats (bit patterns).
    pub data: Vec<u16>,
}

impl CompositeImage {
    /// One pixel as `f32` RGBA.
    pub fn pixel(&self, x: u32, y: u32) -> [f32; 4] {
        assert!(x < self.width && y < self.height, "pixel out of bounds");
        let index = (y as usize * self.width as usize + x as usize) * 4;
        std::array::from_fn(|i| half_to_f32(self.data[index + i]))
    }

    /// Writes an uncompressed scanline OpenEXR file with HALF channels and a
    /// `chromaticities` attribute describing [`Self::space`].
    pub fn write_exr(&self, path: &Path) -> Result<(), ImageIoError> {
        let mut out = BufWriter::new(File::create(path)?);
        let (w, h) = (self.width as usize, self.height as usize);

        let mut header = Vec::new();
        // Channels must be listed in alphabetical order.
        let mut channels = Vec::new();
        for name in ["A", "B", "G", "R"] {
            channels.extend_from_slice(name.as_bytes());
            channels.push(0);
            channels.extend_from_slice(&1i32.to_le_bytes()); // HALF
            channels.extend_from_slice(&[0, 0, 0, 0]); // pLinear + reserved
            channels.extend_from_slice(&1i32.to_le_bytes()); // xSampling
            channels.extend_from_slice(&1i32.to_le_bytes()); // ySampling
        }
        channels.push(0);
        exr_attribute(&mut header, "channels", "chlist", &channels);

        let p = &self.space.primaries;
        let chromaticities: Vec<u8> = [
            p.red.x, p.red.y, p.green.x, p.green.y, p.blue.x, p.blue.y, p.white.x, p.white.y,
        ]
        .iter()
        .flat_map(|v| (*v as f32).to_le_bytes())
        .collect();
        exr_attribute(
            &mut header,
            "chromaticities",
            "chromaticities",
            &chromaticities,
        );

        exr_attribute(&mut header, "compression", "compression", &[0]);
        let window: Vec<u8> = [0i32, 0, self.width as i32 - 1, self.height as i32 - 1]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        exr_attribute(&mut header, "dataWindow", "box2i", &window);
        exr_attribute(&mut header, "displayWindow", "box2i", &window);
        exr_attribute(&mut header, "lineOrder", "lineOrder", &[0]);
        exr_attribute(
            &mut header,
            "pixelAspectRatio",
            "float",
            &1.0f32.to_le_bytes(),
        );
        exr_attribute(&mut header, "screenWindowCenter", "v2f", &[0; 8]);
        exr_attribute(
            &mut header,
            "screenWindowWidth",
            "float",
            &1.0f32.to_le_bytes(),
        );
        header.push(0);

        out.write_all(&EXR_MAGIC)?;
        out.write_all(&EXR_VERSION)?;
        out.write_all(&header)?;

        let line_bytes = w * 4 * 2;
        let chunk_bytes = 8 + line_bytes;
        let first_chunk = 8 + header.len() + h * 8;
        for y in 0..h {
            out.write_all(&((first_chunk + y * chunk_bytes) as u64).to_le_bytes())?;
        }
        let mut line = Vec::with_capacity(line_bytes);
        for y in 0..h {
            out.write_all(&(y as i32).to_le_bytes())?;
            out.write_all(&(line_bytes as i32).to_le_bytes())?;
            line.clear();
            let row = &self.data[y * w * 4..(y + 1) * w * 4];
            // Stored channel order is A, B, G, R; ours is R, G, B, A.
            for channel in [3, 2, 1, 0] {
                for x in 0..w {
                    line.extend_from_slice(&row[x * 4 + channel].to_le_bytes());
                }
            }
            out.write_all(&line)?;
        }
        out.flush()?;
        Ok(())
    }

    /// Reads a file written by [`Self::write_exr`].
    pub fn read_exr(path: &Path) -> Result<Self, ImageIoError> {
        let mut bytes = Vec::new();
        File::open(path)?.read_to_end(&mut bytes)?;
        let unsupported = |what: &str| ImageIoError::Unsupported(format!("OpenEXR: {what}"));

        if bytes.len() < 8 || bytes[0..4] != EXR_MAGIC {
            return Err(unsupported("bad magic number"));
        }
        if bytes[4..8] != EXR_VERSION {
            return Err(unsupported("only version 2 single-part scanline files"));
        }

        let mut pos = 8;
        let mut window = None;
        let mut primaries = None;
        let mut channels_ok = false;
        loop {
            let name = exr_read_str(&bytes, &mut pos).ok_or_else(|| unsupported("truncated"))?;
            if name.is_empty() {
                break;
            }
            let kind = exr_read_str(&bytes, &mut pos).ok_or_else(|| unsupported("truncated"))?;
            let size = exr_read_i32(&bytes, &mut pos).ok_or_else(|| unsupported("truncated"))?;
            let value = bytes
                .get(pos..pos + size as usize)
                .ok_or_else(|| unsupported("truncated"))?;
            pos += size as usize;
            match (name.as_str(), kind.as_str()) {
                ("channels", "chlist") => {
                    let mut p = 0;
                    let mut names = Vec::new();
                    while let Some(channel) = exr_read_str(value, &mut p) {
                        if channel.is_empty() {
                            break;
                        }
                        let pixel_type = exr_read_i32(value, &mut p);
                        if pixel_type != Some(1) {
                            return Err(unsupported("only HALF channels"));
                        }
                        p += 12;
                        names.push(channel);
                    }
                    if names != ["A", "B", "G", "R"] {
                        return Err(unsupported("channels must be A, B, G, R"));
                    }
                    channels_ok = true;
                }
                ("compression", _) => {
                    if value != [0] {
                        return Err(unsupported("only uncompressed files"));
                    }
                }
                ("dataWindow", "box2i") if value.len() == 16 => {
                    let v: [i32; 4] = std::array::from_fn(|i| {
                        i32::from_le_bytes(value[i * 4..i * 4 + 4].try_into().expect("4 bytes"))
                    });
                    window = Some(v);
                }
                ("chromaticities", "chromaticities") if value.len() == 32 => {
                    let v: [f64; 8] = std::array::from_fn(|i| {
                        f32::from_le_bytes(value[i * 4..i * 4 + 4].try_into().expect("4 bytes"))
                            as f64
                    });
                    primaries = Some(v);
                }
                _ => {}
            }
        }
        if !channels_ok {
            return Err(unsupported("missing channel list"));
        }
        let [x_min, y_min, x_max, y_max] = window.ok_or_else(|| unsupported("no data window"))?;
        if x_min != 0 || y_min != 0 || x_max < 0 || y_max < 0 {
            return Err(unsupported("data window must start at the origin"));
        }
        let (w, h) = (x_max as usize + 1, y_max as usize + 1);

        pos += h * 8; // offset table
        let mut data = vec![0u16; w * h * 4];
        for _ in 0..h {
            let y = exr_read_i32(&bytes, &mut pos).ok_or_else(|| unsupported("truncated"))?;
            let size = exr_read_i32(&bytes, &mut pos).ok_or_else(|| unsupported("truncated"))?;
            if y < 0 || y as usize >= h || size as usize != w * 8 {
                return Err(unsupported("unexpected scanline chunk"));
            }
            let line = bytes
                .get(pos..pos + size as usize)
                .ok_or_else(|| unsupported("truncated"))?;
            pos += size as usize;
            let row = &mut data[y as usize * w * 4..(y as usize + 1) * w * 4];
            for (plane, channel) in [3, 2, 1, 0].into_iter().enumerate() {
                for x in 0..w {
                    let i = (plane * w + x) * 2;
                    row[x * 4 + channel] = u16::from_le_bytes([line[i], line[i + 1]]);
                }
            }
        }

        // The chromaticities attribute stores f32; snap back to a named set
        // of primaries when it matches one, so round trips compare equal.
        let space = match primaries {
            Some(v) => CompositeSpace {
                primaries: snap_primaries(v),
            },
            None => return Err(unsupported("missing chromaticities attribute")),
        };

        Ok(Self {
            width: w as u32,
            height: h as u32,
            space,
            data,
        })
    }
}

const EXR_MAGIC: [u8; 4] = [0x76, 0x2f, 0x31, 0x01];
const EXR_VERSION: [u8; 4] = [2, 0, 0, 0];

fn exr_attribute(out: &mut Vec<u8>, name: &str, kind: &str, value: &[u8]) {
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    out.extend_from_slice(kind.as_bytes());
    out.push(0);
    out.extend_from_slice(&(value.len() as i32).to_le_bytes());
    out.extend_from_slice(value);
}

fn exr_read_str(bytes: &[u8], pos: &mut usize) -> Option<String> {
    let rest = bytes.get(*pos..)?;
    let len = rest.iter().position(|b| *b == 0)?;
    let s = String::from_utf8_lossy(&rest[..len]).into_owned();
    *pos += len + 1;
    Some(s)
}

fn exr_read_i32(bytes: &[u8], pos: &mut usize) -> Option<i32> {
    let v = i32::from_le_bytes(bytes.get(*pos..*pos + 4)?.try_into().ok()?);
    *pos += 4;
    Some(v)
}

fn snap_primaries(v: [f64; 8]) -> Primaries {
    use proto_color::Chromaticity;
    let read = Primaries {
        red: Chromaticity::new(v[0], v[1]),
        green: Chromaticity::new(v[2], v[3]),
        blue: Chromaticity::new(v[4], v[5]),
        white: Chromaticity::new(v[6], v[7]),
    };
    let close =
        |a: Chromaticity, b: Chromaticity| (a.x - b.x).abs() < 1e-6 && (a.y - b.y).abs() < 1e-6;
    Primaries::NAMED
        .iter()
        .map(|(_, p)| *p)
        .find(|p| {
            close(p.red, read.red)
                && close(p.green, read.green)
                && close(p.blue, read.blue)
                && close(p.white, read.white)
        })
        .unwrap_or(read)
}

/// Converts an IEEE 754 half float bit pattern to `f32`.
pub fn half_to_f32(bits: u16) -> f32 {
    let sign = ((bits >> 15) & 1) as u32;
    let exponent = ((bits >> 10) & 0x1f) as u32;
    let mantissa = (bits & 0x3ff) as u32;
    let out = match exponent {
        0 => {
            if mantissa == 0 {
                sign << 31
            } else {
                // Subnormal: renormalize.
                let shift = mantissa.leading_zeros() - 21;
                let mantissa = (mantissa << shift) & 0x3ff;
                let exponent = 127 - 15 + 1 - shift;
                (sign << 31) | (exponent << 23) | (mantissa << 13)
            }
        }
        0x1f => (sign << 31) | (0xff << 23) | (mantissa << 13),
        _ => (sign << 31) | ((exponent + 127 - 15) << 23) | (mantissa << 13),
    };
    f32::from_bits(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("proto-image-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn half_conversion_known_values() {
        assert_eq!(half_to_f32(0x0000), 0.0);
        assert_eq!(half_to_f32(0x3c00), 1.0);
        assert_eq!(half_to_f32(0xbc00), -1.0);
        assert_eq!(half_to_f32(0x3800), 0.5);
        assert_eq!(half_to_f32(0x4000), 2.0);
        assert_eq!(half_to_f32(0x7bff), 65504.0);
        assert_eq!(half_to_f32(0x0001), 2.0f32.powi(-24));
        assert_eq!(half_to_f32(0x0400), 2.0f32.powi(-14));
        assert_eq!(half_to_f32(0x7c00), f32::INFINITY);
        assert!(half_to_f32(0x7e00).is_nan());
        // 1/3 rounded to half precision.
        assert!((half_to_f32(0x3555) - 1.0 / 3.0).abs() < 1e-4);
    }

    #[test]
    fn png_round_trips_8_bit() {
        let image = EncodedImage {
            width: 3,
            height: 2,
            format: OutputFormat::Rgba8Unorm,
            image_description: ImageDescription::SRGB_GAMMA22,
            data: (0..24).map(|v| v * 10).collect(),
        };
        let path = temp_path("rt8.png");
        image.write_png(&path).unwrap();
        let read = EncodedImage::read_png(&path, ImageDescription::SRGB_GAMMA22).unwrap();
        assert_eq!(read, image);
        assert_eq!(read.pixel(1, 0), [40, 50, 60, 70]);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn png_round_trips_16_bit() {
        let values: Vec<u16> = (0..16).map(|v| v * 4000 + 123).collect();
        let image = EncodedImage {
            width: 2,
            height: 2,
            format: OutputFormat::Rgba16Unorm,
            image_description: ImageDescription::BT2020_PQ,
            data: values.iter().flat_map(|v| v.to_ne_bytes()).collect(),
        };
        let path = temp_path("rt16.png");
        image.write_png(&path).unwrap();
        let read = EncodedImage::read_png(&path, ImageDescription::BT2020_PQ).unwrap();
        assert_eq!(read, image);
        assert_eq!(read.pixel(1, 1), [48123, 52123, 56123, 60123]);
        assert!((read.pixel_normalized(0, 0)[0] - 123.0 / 65535.0).abs() < 1e-7);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn exr_round_trips() {
        let image = CompositeImage {
            width: 3,
            height: 2,
            space: CompositeSpace::BT2020_LINEAR,
            data: (0..24).map(|v| 0x3c00 + v * 37).collect(),
        };
        let path = temp_path("rt.exr");
        image.write_exr(&path).unwrap();
        let read = CompositeImage::read_exr(&path).unwrap();
        assert_eq!(read, image);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn exr_rejects_other_files() {
        let path = temp_path("not.exr");
        std::fs::write(&path, b"definitely not an exr file").unwrap();
        assert!(matches!(
            CompositeImage::read_exr(&path),
            Err(ImageIoError::Unsupported(_))
        ));
        std::fs::remove_file(path).unwrap();
    }
}
