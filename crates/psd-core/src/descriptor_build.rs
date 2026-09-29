//! Building and editing descriptors the way Photoshop writes them.
//!
//! Reading a descriptor records how each key was encoded, and writing reproduces it, so a
//! descriptor read from disk round-trips byte for byte. Items created here have no such
//! record, so they follow the rule real Photoshop files show: a four-byte ID is written as a
//! zero-length "character ID" and anything longer as an explicit string ID (`present`,
//! `showInDialog`, `masterFXSwitch`, `dropShadowMulti`, long enumerators like `multiply`).
//! Measured over about 600 effects descriptors in Photoshop-authored files, no four-byte key
//! was ever explicit and every longer one was.
//!
//! The editing methods never drop what they do not touch: [`Descriptor::set`] replaces a
//! value in place, keeping the key encoding and position it had, and appends only when the key
//! is new. That is what lets a typed model patch a descriptor it only partly understands.

use crate::descriptor::{Descriptor, DescriptorItem, DescriptorKey, DescriptorValue};
use crate::strings::UnicodeString;

/// Four-byte IDs that Photoshop nevertheless writes as explicit strings.
const EXPLICIT_FOUR_BYTE_IDS: [[u8; 4]; 4] = [*b"warp", *b"time", *b"hold", *b"list"];

/// Unit key of a percentage (`#Prc`).
pub const UNIT_PERCENT: [u8; 4] = *b"#Prc";
/// Unit key of a length in pixels (`#Pxl`).
pub const UNIT_PIXELS: [u8; 4] = *b"#Pxl";
/// Unit key of an angle in degrees (`#Ang`).
pub const UNIT_ANGLE: [u8; 4] = *b"#Ang";

impl DescriptorKey {
    /// A key spelled the way Photoshop spells it: a four-byte ID as a zero-length character
    /// ID, anything else (and the few four-byte IDs Photoshop writes explicitly) as an
    /// explicit string ID.
    pub fn id(text: &str) -> Self {
        match <[u8; 4]>::try_from(text.as_bytes()) {
            Ok(code) if !EXPLICIT_FOUR_BYTE_IDS.contains(&code) => Self::char_id(code),
            _ => Self::new(text),
        }
    }
}

impl DescriptorValue {
    /// `bool`.
    pub fn boolean(value: bool) -> Self {
        Self::Boolean(value)
    }

    /// `long`.
    pub fn long(value: i32) -> Self {
        Self::Integer(value)
    }

    /// `doub`.
    pub fn double(value: f64) -> Self {
        Self::Double(value)
    }

    /// `UntF` in the given unit.
    pub fn unit(unit: [u8; 4], value: f64) -> Self {
        Self::UnitFloat { unit, value }
    }

    /// `UntF#Prc`: a percentage, normally 0-100.
    pub fn percent(value: f64) -> Self {
        Self::unit(UNIT_PERCENT, value)
    }

    /// `UntF#Pxl`: a length in pixels.
    pub fn pixels(value: f64) -> Self {
        Self::unit(UNIT_PIXELS, value)
    }

    /// `UntF#Ang`: an angle in degrees.
    pub fn angle(value: f64) -> Self {
        Self::unit(UNIT_ANGLE, value)
    }

    /// `TEXT`, aligned the way descriptor strings are.
    pub fn text(value: &str) -> Self {
        Self::String(descriptor_string(value))
    }

    /// `enum` with both IDs spelled by [`DescriptorKey::id`].
    pub fn enumerated(type_id: &str, value: &str) -> Self {
        Self::Enumerated {
            type_id: DescriptorKey::id(type_id),
            value: DescriptorKey::id(value),
        }
    }
}

/// A Unicode string as descriptors carry it: no alignment padding, and a terminating null
/// code unit counted in the marker, which every name and `TEXT` value in a Photoshop-authored
/// descriptor has (an empty descriptor name is one null unit, not zero).
pub(crate) fn descriptor_string(value: &str) -> UnicodeString {
    UnicodeString::terminated(value, 1).expect("a short string fits the UnicodeString limit")
}

/// Typed reads of a descriptor's fields. Each returns `None` when the key is absent or holds
/// another type, which the models treat as "not modelled" and leave alone.
impl Descriptor {
    /// A `bool` field.
    pub(crate) fn get_bool(&self, key: &str) -> Option<bool> {
        self.get(key)?.as_bool()
    }

    /// A `long` field.
    pub(crate) fn get_long(&self, key: &str) -> Option<i32> {
        self.get(key)?.as_integer()
    }

    /// A `TEXT` field, without its terminating null.
    pub(crate) fn get_text(&self, key: &str) -> Option<String> {
        Some(self.get(key)?.as_str()?.trim_end_matches('\0').to_owned())
    }

    /// A unit float's value in the expected unit; `None` for another unit.
    pub(crate) fn get_unit(&self, key: &str, unit: [u8; 4]) -> Option<f64> {
        let (found, value) = self.get(key)?.as_unit_float()?;
        (*found == unit).then_some(value)
    }

    /// The enumerator of an `enum` field, as its ID bytes.
    pub(crate) fn get_enum(&self, key: &str) -> Option<&[u8]> {
        Some(self.get(key)?.as_enum()?.1.as_bytes())
    }
}

impl Descriptor {
    /// An empty descriptor of the given class, with no name and no items.
    pub fn with_class(class_id: &str) -> Self {
        Self {
            name: descriptor_string(""),
            class_id: DescriptorKey::id(class_id),
            items: Vec::new(),
        }
    }

    /// Set `key` to `value`.
    ///
    /// An existing item keeps its position and key encoding and only its value changes; a new
    /// item is appended with the encoding [`DescriptorKey::id`] gives it.
    pub fn set(&mut self, key: &str, value: DescriptorValue) {
        if let Some(existing) = self.get_mut(key) {
            *existing = value;
        } else {
            self.items.push(DescriptorItem {
                key: DescriptorKey::id(key),
                value,
            });
        }
    }

    /// Set a `TEXT` field, leaving it untouched when it already holds this text, so a string
    /// Photoshop wrote keeps its exact bytes, terminator convention included.
    pub fn set_text(&mut self, key: &str, text: &str) {
        if self.get_text(key).as_deref() != Some(text) {
            self.set(key, DescriptorValue::text(text));
        }
    }

    /// [`set_text`](Self::set_text) that places a new item by `order`, as
    /// [`set_ordered`](Self::set_ordered) does.
    pub fn set_text_ordered(&mut self, key: &str, text: &str, order: &[&str]) {
        if self.get_text(key).as_deref() != Some(text) {
            self.set_ordered(key, DescriptorValue::text(text), order);
        }
    }

    /// Set the descriptor's own name, leaving it untouched when it is unchanged.
    pub fn set_name(&mut self, name: &str) {
        if self.name.value().trim_end_matches('\0') != name {
            self.name = descriptor_string(name);
        }
    }

    /// Set `key` to `value`, placing a new item where `order` says it belongs.
    ///
    /// `order` lists keys in the order Photoshop writes them. A new item goes before the first
    /// existing item that `order` ranks after it, so a descriptor patched key by key stays in
    /// Photoshop's layout, unknown items included: they keep their places and the new item
    /// lands beside its neighbours. A key `order` does not mention is appended.
    pub fn set_ordered(&mut self, key: &str, value: DescriptorValue, order: &[&str]) {
        if let Some(existing) = self.get_mut(key) {
            *existing = value;
            return;
        }
        let rank = |name: &[u8]| order.iter().position(|k| k.as_bytes() == name);
        let at = rank(key.as_bytes()).and_then(|mine| {
            self.items
                .iter()
                .position(|item| rank(item.key.as_bytes()).is_some_and(|theirs| theirs > mine))
        });
        let item = DescriptorItem {
            key: DescriptorKey::id(key),
            value,
        };
        match at {
            Some(at) => self.items.insert(at, item),
            None => self.items.push(item),
        }
    }

    /// Remove `key`, returning its value if it was present.
    pub fn remove(&mut self, key: &str) -> Option<DescriptorValue> {
        let at = self
            .items
            .iter()
            .position(|item| item.key.as_bytes() == key.as_bytes())?;
        Some(self.items.remove(at).value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::{BeReader, BeWriter};

    #[test]
    fn four_byte_ids_are_character_ids_and_longer_ones_are_explicit() {
        for id in ["enab", "Clr ", "Md  ", "Opct", "Mltp"] {
            assert!(DescriptorKey::id(id).uses_implicit_length(), "{id}");
        }
        for id in [
            "present",
            "showInDialog",
            "masterFXSwitch",
            "multiply",
            "phase",
        ] {
            assert!(!DescriptorKey::id(id).uses_implicit_length(), "{id}");
        }
        // Photoshop writes these four-byte IDs explicitly.
        for id in ["warp", "time", "hold", "list"] {
            assert!(!DescriptorKey::id(id).uses_implicit_length(), "{id}");
        }
    }

    #[test]
    fn set_replaces_in_place_and_keeps_the_key_encoding() {
        let mut d = Descriptor::with_class("null");
        d.items.push(DescriptorItem {
            key: DescriptorKey::char_id(*b"enab"),
            value: DescriptorValue::boolean(false),
        });
        d.items.push(DescriptorItem {
            key: DescriptorKey::new("odd"),
            value: DescriptorValue::long(1),
        });
        d.set("enab", DescriptorValue::boolean(true));
        assert_eq!(d.items.len(), 2);
        assert!(d.items[0].key.uses_implicit_length());
        assert_eq!(d.get("enab").unwrap().as_bool(), Some(true));

        d.set("Opct", DescriptorValue::percent(50.0));
        assert_eq!(d.items[2].key.as_bytes(), b"Opct");
        assert!(d.items[2].key.uses_implicit_length());
    }

    #[test]
    fn set_ordered_lands_beside_its_neighbours_and_keeps_unknown_items() {
        const ORDER: [&str; 5] = ["enab", "present", "Md  ", "Clr ", "Opct"];
        let mut d = Descriptor::with_class("null");
        d.set("enab", DescriptorValue::boolean(true));
        d.set("mystery", DescriptorValue::long(7)); // unknown to `ORDER`
        d.set("Opct", DescriptorValue::percent(75.0));
        d.set_ordered("Md  ", DescriptorValue::enumerated("BlnM", "Nrml"), &ORDER);
        d.set_ordered("present", DescriptorValue::boolean(true), &ORDER);
        d.set_ordered("Clr ", DescriptorValue::text("stand-in"), &ORDER);

        let keys: Vec<String> = d
            .items
            .iter()
            .map(|i| i.key.as_str().into_owned())
            .collect();
        // `Md  ` and `present` slot in before `Opct`, `Clr ` before `Opct` too; the unknown
        // item stays where it was, so the new ones cannot precede it.
        assert_eq!(keys, ["enab", "mystery", "present", "Md  ", "Clr ", "Opct"]);

        // A key outside `ORDER` is appended.
        d.set_ordered("later", DescriptorValue::long(1), &ORDER);
        assert_eq!(d.items.last().unwrap().key.as_bytes(), b"later");
    }

    #[test]
    fn remove_takes_the_item_out() {
        let mut d = Descriptor::with_class("null");
        d.set("enab", DescriptorValue::boolean(true));
        d.set("Opct", DescriptorValue::percent(1.0));
        assert!(d.remove("enab").is_some());
        assert!(d.remove("enab").is_none());
        assert_eq!(d.items.len(), 1);
    }

    #[test]
    fn a_built_descriptor_survives_a_write_and_read() {
        let mut d = Descriptor::with_class("null");
        d.set("enab", DescriptorValue::boolean(true));
        d.set("present", DescriptorValue::boolean(true));
        d.set("Md  ", DescriptorValue::enumerated("BlnM", "multiply"));
        d.set("Opct", DescriptorValue::percent(75.0));
        d.set("lagl", DescriptorValue::angle(120.0));
        d.set("blur", DescriptorValue::pixels(5.0));
        let mut writer = BeWriter::new();
        d.write(&mut writer).unwrap();
        let bytes = writer.into_inner();
        let read = Descriptor::read(&mut BeReader::new(&bytes)).unwrap();
        assert_eq!(read, d);
        // Reading and writing again reproduces the bytes: the encodings were recorded.
        let mut again = BeWriter::new();
        read.write(&mut again).unwrap();
        assert_eq!(again.into_inner(), bytes);
    }
}
