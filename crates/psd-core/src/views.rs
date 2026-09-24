//! Parsing helpers shared by the read-only tagged-block views
//! (adjustments, vector data).

use crate::descriptor::{Descriptor, DescriptorKey, DescriptorValue};
use crate::error::{PsdError, Result};
use crate::io::BeReader;

/// Read the `u32` descriptor version (16) and the descriptor that follows.
pub(crate) fn read_versioned_descriptor(reader: &mut BeReader) -> Result<Descriptor> {
    if reader.u32()? != 16 {
        return Err(unsupported(reader));
    }
    Descriptor::read(reader)
}

pub(crate) fn trailing(reader: &mut BeReader) -> Result<Vec<u8>> {
    Ok(reader.take(reader.remaining())?.to_vec())
}

pub(crate) fn next_is(reader: &BeReader, marker: &[u8; 4]) -> bool {
    let mut probe = reader.clone();
    probe.take(4).is_ok_and(|bytes| bytes == marker)
}

pub(crate) fn four_cc(reader: &mut BeReader) -> Result<[u8; 4]> {
    let mut code = [0; 4];
    code.copy_from_slice(reader.take(4)?);
    Ok(code)
}

/// A numeric descriptor item, whether stored as `long`, `doub`, or `UntF`.
pub(crate) fn number(descriptor: &Descriptor, key: &str) -> Option<f64> {
    match descriptor.get(key)? {
        DescriptorValue::Integer(value) => Some(f64::from(*value)),
        DescriptorValue::LargeInteger(value) => Some(*value as f64),
        DescriptorValue::Double(value) => Some(*value),
        DescriptorValue::UnitFloat { value, .. } => Some(*value),
        _ => None,
    }
}

pub(crate) fn enumerator<'a>(descriptor: &'a Descriptor, key: &str) -> Option<&'a DescriptorKey> {
    descriptor.get(key)?.as_enum().map(|(_, value)| value)
}

pub(crate) fn raw_data<'a>(descriptor: &'a Descriptor, key: &str) -> Option<&'a [u8]> {
    match descriptor.get(key)? {
        DescriptorValue::RawData { data, .. } => Some(data),
        _ => None,
    }
}

pub(crate) fn unsupported(reader: &BeReader) -> PsdError {
    invalid(reader, "unsupported tagged-block payload version")
}

pub(crate) fn invalid(reader: &BeReader, message: &'static str) -> PsdError {
    PsdError::InvalidData {
        offset: reader.position() as u64,
        message,
    }
}
