//! Adobe Swatch Exchange (`.ase`): swatch palettes.
//!
//! The layout is a four-byte `ASEF` signature, a 1.0 version pair and a block
//! count, then length-prefixed blocks: a colour entry (`0x0001`), a group start
//! (`0xC001`) or a group end (`0xC002`). Every block begins with a UTF-16BE
//! name whose length field counts the terminating nul. A colour carries a
//! four-byte model tag — `RGB `, `CMYK`, `Gray` or `LAB ` — one to four
//! big-endian `f32` components in the file's own scale (0–1), and a two-byte
//! colour type.
//!
//! There is no official Adobe specification; the layout here follows the
//! workspace's ag-psd port, whose reading matches independent community
//! documentation of real Photoshop output.

use psd_core::io::BeReader;

use crate::error::{invalid, Error, Result};

/// Where a swatch sits in the palette: a library colour, a spot colour, or a
/// plain document colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AseColorType {
    /// `0` — a colour saved in the user's swatch library.
    Global,
    /// `1` — a spot colour.
    Spot,
    /// `2` — an ordinary colour.
    Normal,
}

impl AseColorType {
    fn from_index(index: u16) -> Option<Self> {
        match index {
            0 => Some(Self::Global),
            1 => Some(Self::Spot),
            2 => Some(Self::Normal),
            _ => None,
        }
    }
}

/// A colour's components, in the file's own model and scale.
#[derive(Debug, Clone, PartialEq)]
pub enum AseColorValue {
    /// `RGB ` — red, green, blue.
    Rgb {
        r: f32,
        g: f32,
        b: f32,
        color_type: AseColorType,
    },
    /// `CMYK` — cyan, magenta, yellow, key.
    Cmyk {
        c: f32,
        m: f32,
        y: f32,
        k: f32,
        color_type: AseColorType,
    },
    /// `Gray` — one component.
    Gray { k: f32, color_type: AseColorType },
    /// `LAB ` — lightness and the two opponent axes.
    Lab {
        l: f32,
        a: f32,
        b: f32,
        color_type: AseColorType,
    },
}

impl AseColorValue {
    /// The colour's library role.
    #[must_use]
    pub fn color_type(&self) -> AseColorType {
        match self {
            Self::Rgb { color_type, .. }
            | Self::Cmyk { color_type, .. }
            | Self::Gray { color_type, .. }
            | Self::Lab { color_type, .. } => *color_type,
        }
    }
}

/// One named swatch.
#[derive(Debug, Clone, PartialEq)]
pub struct AseColor {
    pub name: String,
    pub color: AseColorValue,
}

/// A named group of swatches.
#[derive(Debug, Clone, PartialEq)]
pub struct AseGroup {
    pub name: String,
    pub colors: Vec<AseColor>,
}

/// A top-level palette entry: a loose colour or a group.
#[derive(Debug, Clone, PartialEq)]
pub enum AseEntry {
    Color(AseColor),
    Group(AseGroup),
}

/// A parsed `.ase` palette: top-level colours and groups in file order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Ase {
    pub entries: Vec<AseEntry>,
}

/// Parses an `.ase` palette.
///
/// # Errors
///
/// [`Error::Invalid`] for a wrong signature or version, an unknown block type
/// or colour model, a colour-type index outside `global`/`spot`/`normal`, or a
/// truncated block. A colour inside a group that is not open — a group end
/// without a start — is one of these.
pub fn read_ase(bytes: &[u8]) -> Result<Ase> {
    let reader = &mut BeReader::new(bytes);
    reader.signature("ASEF")?;
    let (major, minor) = (reader.u16()?, reader.u16()?);
    if (major, minor) != (1, 0) {
        return Err(Error::Unsupported(format!(
            "ASEF version {major}.{minor}; this reader knows 1.0"
        )));
    }
    let block_count = reader.u32()?;

    let mut ase = Ase::default();
    // The index of the group currently open in `ase.entries`; colours outside
    // any group land at the top level.
    let mut open_group: Option<usize> = None;

    for _ in 0..block_count {
        let block_type = reader.u16()?;
        let length = reader.u32()? as usize;
        let end = reader
            .position()
            .checked_add(length)
            .filter(|&end| end <= bytes.len())
            .ok_or_else(|| invalid(reader, "block length runs past the end of the file"))?;

        match block_type {
            0x0001 => {
                let name = read_name(reader)?;
                let color = read_color(reader)?;
                let entry = AseColor { name, color };
                match open_group {
                    Some(index) => match &mut ase.entries[index] {
                        AseEntry::Group(group) => group.colors.push(entry),
                        AseEntry::Color(_) => {
                            return Err(invalid(reader, "a group index pointed at a colour"))
                        }
                    },
                    None => ase.entries.push(AseEntry::Color(entry)),
                }
            }
            0xC001 => {
                let name = read_name(reader)?;
                ase.entries.push(AseEntry::Group(AseGroup {
                    name,
                    colors: Vec::new(),
                }));
                open_group = Some(ase.entries.len() - 1);
            }
            0xC002 => open_group = None,
            _ => {
                return Err(invalid(
                    reader,
                    format!("unknown block type {block_type:#06x}"),
                ))
            }
        }

        reader.seek(end)?;
    }

    Ok(ase)
}

/// A block name: a `u16` code-unit count that includes the terminator, then
/// that many UTF-16BE units. Interior nuls are kept; only a trailing one is
/// dropped, as the reference reads it.
fn read_name(reader: &mut BeReader<'_>) -> Result<String> {
    let units = reader.u16()? as usize;
    let mut text = String::new();
    for index in 0..units {
        let unit = reader.u16()?;
        if unit != 0 || index + 1 < units {
            text.push(char::from_u32(u32::from(unit)).unwrap_or('\u{FFFD}'));
        }
    }
    Ok(text)
}

fn read_color_type(reader: &mut BeReader<'_>) -> Result<AseColorType> {
    AseColorType::from_index(reader.u16()?)
        .ok_or_else(|| invalid(reader, "colour type outside global/spot/normal"))
}

fn read_color(reader: &mut BeReader<'_>) -> Result<AseColorValue> {
    let mut model = [0u8; 4];
    model.copy_from_slice(reader.take(4)?);
    // The components come first; the two-byte colour type closes the block.
    match &model {
        b"RGB " => {
            let (r, g, b) = (reader.f32()?, reader.f32()?, reader.f32()?);
            Ok(AseColorValue::Rgb {
                r,
                g,
                b,
                color_type: read_color_type(reader)?,
            })
        }
        b"CMYK" => {
            let (c, m, y, k) = (reader.f32()?, reader.f32()?, reader.f32()?, reader.f32()?);
            Ok(AseColorValue::Cmyk {
                c,
                m,
                y,
                k,
                color_type: read_color_type(reader)?,
            })
        }
        b"Gray" => {
            let k = reader.f32()?;
            Ok(AseColorValue::Gray {
                k,
                color_type: read_color_type(reader)?,
            })
        }
        b"LAB " => {
            let (l, a, b) = (reader.f32()?, reader.f32()?, reader.f32()?);
            Ok(AseColorValue::Lab {
                l,
                a,
                b,
                color_type: read_color_type(reader)?,
            })
        }
        other => Err(invalid(
            reader,
            format!("unknown colour model {}", String::from_utf8_lossy(other)),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use psd_core::io::BeWriter;

    /// Writes a block name the way the format stores it: a u16 unit count that
    /// includes the terminator, the UTF-16BE units, then the terminator.
    fn write_name(writer: &mut BeWriter, name: &str) {
        let units: Vec<u16> = name.encode_utf16().collect();
        writer.u16(units.len() as u16 + 1);
        for unit in &units {
            writer.u16(*unit);
        }
        writer.u16(0);
    }

    fn write_block(writer: &mut BeWriter, block_type: u16, body: &[u8]) {
        writer.u16(block_type);
        writer.u32(body.len() as u32);
        writer.bytes(body);
    }

    fn write_color_body(
        writer: &mut BeWriter,
        name: &str,
        model: &[u8; 4],
        components: &[f32],
        color_type: u16,
    ) {
        write_name(writer, name);
        writer.bytes(model);
        for &component in components {
            writer.f32(component);
        }
        writer.u16(color_type);
    }

    fn header(block_count: u32) -> BeWriter {
        let mut writer = BeWriter::new();
        writer.bytes(b"ASEF");
        writer.u16(1);
        writer.u16(0);
        writer.u32(block_count);
        writer
    }

    #[test]
    fn decodes_every_colour_model_and_groups() {
        let mut writer = header(6);
        write_block(&mut writer, 0x0001, &{
            let mut body = BeWriter::new();
            write_color_body(&mut body, "Red", b"RGB ", &[1.0, 0.0, 0.0], 0);
            body.into_inner()
        });
        write_block(&mut writer, 0xC001, &{
            let mut body = BeWriter::new();
            write_name(&mut body, "Grays");
            body.into_inner()
        });
        write_block(&mut writer, 0x0001, &{
            let mut body = BeWriter::new();
            write_color_body(&mut body, "Mid", b"Gray", &[0.5], 2);
            body.into_inner()
        });
        write_block(&mut writer, 0x0001, &{
            let mut body = BeWriter::new();
            write_color_body(&mut body, "Ink", b"CMYK", &[1.0, 0.0, 0.0, 0.4], 1);
            body.into_inner()
        });
        write_block(&mut writer, 0xC002, &[]);
        write_block(&mut writer, 0x0001, &{
            let mut body = BeWriter::new();
            write_color_body(&mut body, "Lab", b"LAB ", &[50.0, 10.0, -20.0], 2);
            body.into_inner()
        });

        let ase = read_ase(&writer.into_inner()).expect("decode");
        assert_eq!(
            ase.entries,
            vec![
                AseEntry::Color(AseColor {
                    name: "Red".into(),
                    color: AseColorValue::Rgb {
                        r: 1.0,
                        g: 0.0,
                        b: 0.0,
                        color_type: AseColorType::Global
                    },
                }),
                AseEntry::Group(AseGroup {
                    name: "Grays".into(),
                    colors: vec![
                        AseColor {
                            name: "Mid".into(),
                            color: AseColorValue::Gray {
                                k: 0.5,
                                color_type: AseColorType::Normal
                            },
                        },
                        AseColor {
                            name: "Ink".into(),
                            color: AseColorValue::Cmyk {
                                c: 1.0,
                                m: 0.0,
                                y: 0.0,
                                k: 0.4,
                                color_type: AseColorType::Spot,
                            },
                        },
                    ],
                }),
                AseEntry::Color(AseColor {
                    name: "Lab".into(),
                    color: AseColorValue::Lab {
                        l: 50.0,
                        a: 10.0,
                        b: -20.0,
                        color_type: AseColorType::Normal
                    },
                }),
            ]
        );
    }

    #[test]
    fn keeps_interior_nuls_and_drops_the_trailing_one() {
        let mut writer = header(1);
        write_block(&mut writer, 0x0001, &{
            let mut body = BeWriter::new();
            // "a\0b" — the interior nul survives, the trailing one does not.
            body.u16(4);
            body.u16(u16::from(b'a'));
            body.u16(0);
            body.u16(u16::from(b'b'));
            body.u16(0);
            write_color_body_tail(&mut body, b"Gray", &[0.5], 2);
            body.into_inner()
        });
        let ase = read_ase(&writer.into_inner()).expect("decode");
        match &ase.entries[0] {
            AseEntry::Color(color) => assert_eq!(color.name, "a\u{0}b"),
            other => panic!("expected a colour, got {other:?}"),
        }
    }

    fn write_color_body_tail(
        writer: &mut BeWriter,
        model: &[u8; 4],
        components: &[f32],
        color_type: u16,
    ) {
        writer.bytes(model);
        for &component in components {
            writer.f32(component);
        }
        writer.u16(color_type);
    }

    #[test]
    fn rejects_bad_signature_version_block_type_and_colour_type() {
        let mut bytes = b"XSEF".to_vec();
        bytes.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]);
        assert!(matches!(read_ase(&bytes), Err(Error::Format(_))));

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"ASEF");
        bytes.extend_from_slice(&[0, 2, 0, 0, 0, 0, 0, 0]);
        assert!(matches!(read_ase(&bytes), Err(Error::Unsupported(_))));

        let mut writer = header(1);
        write_block(&mut writer, 0x0003, &[]);
        assert!(matches!(
            read_ase(&writer.into_inner()),
            Err(Error::Invalid { .. })
        ));

        let mut writer = header(1);
        write_block(&mut writer, 0x0001, &{
            let mut body = BeWriter::new();
            write_name(&mut body, "x");
            write_color_body_tail(&mut body, b"Gray", &[0.5], 7);
            body.into_inner()
        });
        assert!(matches!(
            read_ase(&writer.into_inner()),
            Err(Error::Invalid { .. })
        ));
    }

    #[test]
    fn rejects_a_block_that_runs_past_the_file() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"ASEF");
        bytes.extend_from_slice(&[0, 1, 0, 0]);
        bytes.extend_from_slice(&1u32.to_be_bytes());
        bytes.extend_from_slice(&0x0001u16.to_be_bytes());
        bytes.extend_from_slice(&9999u32.to_be_bytes());
        assert!(matches!(read_ase(&bytes), Err(Error::Invalid { .. })));
    }

    #[test]
    fn a_colour_after_a_group_end_lands_at_the_top_level() {
        let mut writer = header(4);
        write_block(&mut writer, 0xC001, &{
            let mut body = BeWriter::new();
            write_name(&mut body, "G");
            body.into_inner()
        });
        write_block(&mut writer, 0xC002, &[]);
        write_block(&mut writer, 0x0001, &{
            let mut body = BeWriter::new();
            write_color_body(&mut body, "Loose", b"Gray", &[0.5], 2);
            body.into_inner()
        });
        // An empty group end with no start is tolerated as the reference does.
        write_block(&mut writer, 0xC002, &[]);

        let ase = read_ase(&writer.into_inner()).expect("decode");
        assert_eq!(ase.entries.len(), 2);
        assert!(matches!(ase.entries[0], AseEntry::Group(_)));
        assert!(matches!(ase.entries[1], AseEntry::Color(_)));
    }
}
