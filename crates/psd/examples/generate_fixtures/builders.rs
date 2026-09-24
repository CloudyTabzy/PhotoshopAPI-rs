//! Byte and descriptor helpers shared by the fixture families.

use psd::core::{
    BeWriter, Descriptor, DescriptorItem, DescriptorKey, DescriptorValue, UnicodeString,
};

/// A descriptor key encoded the way Photoshop writes it: four-character IDs
/// with a zero length field, longer string IDs with an explicit length.
pub fn key(text: &str) -> DescriptorKey {
    match <[u8; 4]>::try_from(text.as_bytes()) {
        Ok(code) => DescriptorKey::char_id(code),
        Err(_) => DescriptorKey::new(text),
    }
}

pub fn descriptor(class: &str, items: Vec<(&str, DescriptorValue)>) -> Descriptor {
    Descriptor {
        name: UnicodeString::new("", 1).expect("empty name"),
        class_id: key(class),
        items: items
            .into_iter()
            .map(|(item_key, value)| DescriptorItem {
                key: key(item_key),
                value,
            })
            .collect(),
    }
}

pub fn object(class: &str, items: Vec<(&str, DescriptorValue)>) -> DescriptorValue {
    DescriptorValue::Descriptor(descriptor(class, items))
}

pub fn text(value: &str) -> DescriptorValue {
    DescriptorValue::String(UnicodeString::new(value, 1).expect("short text"))
}

pub fn enumerated(type_id: &str, value: &str) -> DescriptorValue {
    DescriptorValue::Enumerated {
        type_id: key(type_id),
        value: key(value),
    }
}

pub fn unit(unit: &[u8; 4], value: f64) -> DescriptorValue {
    DescriptorValue::UnitFloat { unit: *unit, value }
}

pub fn rgb(red: f64, green: f64, blue: f64) -> DescriptorValue {
    object(
        "RGBC",
        vec![
            ("Rd  ", DescriptorValue::Double(red)),
            ("Grn ", DescriptorValue::Double(green)),
            ("Bl  ", DescriptorValue::Double(blue)),
        ],
    )
}

/// `u32` version 16 followed by the descriptor, the framing of every
/// descriptor-based adjustment block.
pub fn versioned_descriptor(writer: &mut BeWriter, descriptor: &Descriptor) {
    writer.u32(16);
    descriptor.write(writer).expect("descriptor fits");
}

/// Photoshop pads adjustment payloads to a multiple of four bytes and counts
/// the padding in the block length.
pub fn padded(writer: BeWriter) -> Vec<u8> {
    let mut bytes = writer.into_inner();
    bytes.resize(bytes.len().next_multiple_of(4), 0);
    bytes
}
