//! Photoshop custom shapes (`.csh`): named vector paths.
//!
//! The file is a `cush` signature, a version (2) and a shape count, then one
//! record per shape: a Unicode name padded to four bytes, a shape version (1),
//! a length, a Pascal-string id, the shape's bounding rectangle, and the same
//! 26-byte vector-path records a layer's vector mask carries — subpath length
//! records, Bézier knots, the clipboard record and the initial fill rule.
//!
//! Knot coordinates are stored in the file as fixed-point fractions of the
//! bounding rectangle, exactly as a vector mask stores them; convert them with
//! [`psd_core::vector::PathPoint::to_pixels`] and the shape's width and height.
//! The path itself is parsed by [`psd_core::vector::VectorPath`], so this
//! reader adds only the container.

use psd_core::io::BeReader;
use psd_core::strings::{PascalString, UnicodeString};
use psd_core::vector::VectorPath;

use crate::error::{invalid, Error, Result};

/// One custom shape: its name, id, bounding box and vector path.
#[derive(Debug, Clone)]
pub struct CshShape {
    pub name: String,
    pub id: String,
    /// The bounding rectangle's width in pixels; knot fractions scale by it.
    pub width: u32,
    /// The bounding rectangle's height in pixels.
    pub height: u32,
    /// The shape's outline, in the file's own fixed-point form.
    pub path: VectorPath,
}

/// A parsed `.csh` file: its shapes in file order.
#[derive(Debug, Clone, Default)]
pub struct Csh {
    pub shapes: Vec<CshShape>,
}

/// Parses a `.csh` custom-shape file.
///
/// # Errors
///
/// [`Error::Invalid`] for a wrong signature or version, a truncated or
/// inverted shape record, or a malformed vector path. [`Error::Unsupported`]
/// for a file version other than 2.
pub fn read_csh(bytes: &[u8]) -> Result<Csh> {
    let reader = &mut BeReader::new(bytes);
    reader.signature("cush")?;
    let version = reader.u32()?;
    if version != 2 {
        return Err(Error::Unsupported(format!(
            "CSH version {version}; this reader knows 2"
        )));
    }
    let count = reader.u32()?;

    let mut csh = Csh::default();
    for _ in 0..count {
        let name = UnicodeString::read(reader, 2)?.value().to_owned();
        // The name is padded to two bytes; the record that follows sits on a
        // four-byte boundary.
        while !reader.position().is_multiple_of(4) {
            reader.skip(1)?;
        }
        let shape_version = reader.u32()?;
        if shape_version != 1 {
            return Err(invalid(
                reader,
                format!("invalid shape version {shape_version}"),
            ));
        }
        let size = reader.u32()? as usize;
        let end = reader
            .position()
            .checked_add(size)
            .filter(|&end| end <= bytes.len())
            .ok_or_else(|| invalid(reader, "shape length runs past the end of the file"))?;

        let id = PascalString::read(reader, 1)?.value().to_owned();
        // The rectangle is stored y1, x1, y2, x2 and is unsigned; an inverted
        // corner would wrap a subtraction, so it is rejected instead.
        let (y1, x1, y2, x2) = (reader.u32()?, reader.u32()?, reader.u32()?, reader.u32()?);
        if y2 < y1 || x2 < x1 {
            return Err(invalid(reader, "inverted shape rectangle"));
        }
        let (width, height) = (x2 - x1, y2 - y1);

        let path_data = reader.take(end - reader.position())?;
        let path = VectorPath::read(path_data)?;

        csh.shapes.push(CshShape {
            name,
            id,
            width,
            height,
            path,
        });
        reader.seek(end)?;
    }

    Ok(csh)
}

#[cfg(test)]
mod tests {
    use super::*;
    use psd_core::io::BeWriter;
    use psd_core::vector::{BezierKnot, PathPoint, PathRecord, SubpathRecord};

    /// Writes one shape body: id + rectangle + path records.
    fn write_body(
        writer: &mut BeWriter,
        id: &str,
        width: u32,
        height: u32,
        records: &[PathRecord],
    ) {
        PascalString::new(id, 1).write(writer).expect("write id");
        writer.u32(0); // y1
        writer.u32(0); // x1
        writer.u32(height); // y2
        writer.u32(width); // x2

        // The path records, 26 bytes each, as the file stores them.
        for record in records {
            match *record {
                PathRecord::Subpath(sub) => {
                    writer.u16(if sub.closed { 0 } else { 3 });
                    writer.u16(sub.knot_count);
                    writer.i16(sub.operation);
                    writer.u16(sub.flags);
                    writer.bytes(&sub.reserved);
                }
                PathRecord::Knot(knot) => {
                    writer.u16(if knot.closed && knot.linked {
                        1
                    } else if knot.closed {
                        2
                    } else if knot.linked {
                        4
                    } else {
                        5
                    });
                    for point in [&knot.preceding, &knot.anchor, &knot.leaving] {
                        writer.i32(point.vertical);
                        writer.i32(point.horizontal);
                    }
                }
                _ => unreachable!("test fixtures build only subpaths and knots"),
            }
        }
    }

    fn fixture(records: &[PathRecord]) -> Vec<u8> {
        let mut writer = BeWriter::new();
        writer.bytes(b"cush");
        writer.u32(2);
        writer.u32(1);
        UnicodeString::new("Square", 2)
            .expect("name")
            .write(&mut writer)
            .expect("write name");
        while !writer.position().is_multiple_of(4) {
            writer.u8(0);
        }
        writer.u32(1); // shape version

        let mut body = BeWriter::new();
        write_body(&mut body, "abc-123", 100, 80, records);
        writer.u32(body.position() as u32);
        writer.bytes(&body.into_inner());
        writer.into_inner()
    }

    fn knot(closed: bool, linked: bool, x: f64, y: f64) -> PathRecord {
        PathRecord::Knot(BezierKnot {
            closed,
            linked,
            preceding: PathPoint {
                vertical: 0,
                horizontal: 0,
            },
            anchor: PathPoint {
                vertical: (y * 16_777_216.0) as i32,
                horizontal: (x * 16_777_216.0) as i32,
            },
            leaving: PathPoint {
                vertical: 0,
                horizontal: 0,
            },
        })
    }

    #[test]
    fn decodes_a_shape_with_two_knots() {
        let bytes = fixture(&[
            PathRecord::Subpath(SubpathRecord {
                closed: true,
                knot_count: 2,
                operation: 1,
                flags: 2,
                reserved: [0; 18],
            }),
            knot(true, true, 0.25, 0.5),
            knot(true, false, 0.75, 0.5),
        ]);

        let csh = read_csh(&bytes).expect("decode");
        assert_eq!(csh.shapes.len(), 1);
        let shape = &csh.shapes[0];
        assert_eq!(shape.name, "Square");
        assert_eq!(shape.id, "abc-123");
        assert_eq!((shape.width, shape.height), (100, 80));

        let subpaths = shape.path.subpaths().expect("subpaths");
        assert_eq!(subpaths.len(), 1);
        assert_eq!(subpaths[0].knots.len(), 2);
        // The anchor is stored as a fraction of the rectangle; converting it
        // recovers the pixel position.
        let anchor = subpaths[0].knots[0].anchor.to_pixels(100, 80);
        assert!((anchor.0 - 25.0).abs() < 1e-6, "x0 {}", anchor.0);
        assert!((anchor.1 - 40.0).abs() < 1e-6, "y0 {}", anchor.1);
        assert!(subpaths[0].knots[0].linked);
        assert!(!subpaths[0].knots[1].linked);
    }

    #[test]
    fn rejects_bad_signature_version_and_inverted_rectangle() {
        let mut bytes = b"XUSH".to_vec();
        bytes.extend_from_slice(&[0, 0, 0, 2, 0, 0, 0, 0]);
        assert!(matches!(read_csh(&bytes), Err(Error::Format(_))));

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"cush");
        bytes.extend_from_slice(&3u32.to_be_bytes());
        assert!(matches!(read_csh(&bytes), Err(Error::Unsupported(_))));

        // A rectangle with x2 < x1 must be rejected, not wrapped.
        let mut writer = BeWriter::new();
        writer.bytes(b"cush");
        writer.u32(2);
        writer.u32(1);
        UnicodeString::new("x", 2)
            .expect("name")
            .write(&mut writer)
            .expect("write name");
        while !writer.position().is_multiple_of(4) {
            writer.u8(0);
        }
        writer.u32(1);
        writer.u32(20);
        let mut body = BeWriter::new();
        body.u8(1); // pascal id, empty
        body.u32(0); // y1
        body.u32(10); // x1
        body.u32(0); // y2
        body.u32(5); // x2 < x1
        writer.u32(body.position() as u32);
        writer.bytes(&body.into_inner());
        let bytes = writer.into_inner();
        assert!(matches!(read_csh(&bytes), Err(Error::Invalid { .. })));
    }
}
