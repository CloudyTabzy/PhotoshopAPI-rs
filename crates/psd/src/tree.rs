//! Structural edits of the layer tree: detaching, moving, and inserting
//! layers (upstream `LayeredFile::remove_layer`/`move_layer`/
//! `is_layer_in_file` and `GroupLayer::remove_layer`).
//!
//! Children lists are in on-disk (bottom-to-top) order. A group read from a
//! file is preceded in its parent's list by the `</Layer group>` section
//! divider that closes it; every edit keeps that pair together, and indices
//! passed to these methods count only real layers (dividers are skipped).
//! Groups without a divider get one synthesized on write.

use psd_core::{PsdError, Result};

use crate::bitdepth::BitDepth;
use crate::layer::{Layer, LayerId, LayerKind};
use crate::layered_file::LayeredFile;

/// A layer detached from a document, together with its descendants.
///
/// For groups, `layer`'s own child-id list is empty: the children live in
/// [`children`](Self::children), bottom-to-top, until the tree is inserted
/// again with [`LayeredFile::insert_layer_tree`].
#[derive(Debug, Clone, PartialEq)]
pub struct LayerTree<T: BitDepth> {
    pub layer: Layer<T>,
    pub children: Vec<LayerTree<T>>,
}

impl<T: BitDepth> LayerTree<T> {
    /// A tree holding one layer. A group layer's existing child ids are
    /// dropped, since they only mean something inside a document.
    pub fn new(mut layer: Layer<T>) -> Self {
        if let Some(group) = layer.group_mut() {
            group.children.clear();
        }
        Self {
            layer,
            children: Vec::new(),
        }
    }

    /// Append `child` as the topmost child. Fails unless this tree's layer
    /// is a group.
    pub fn push_child(&mut self, child: LayerTree<T>) -> Result<()> {
        if self.layer.group().is_none() {
            return Err(not_a_group());
        }
        self.children.push(child);
        Ok(())
    }

    /// Number of layers in the tree, including its root.
    pub fn len(&self) -> usize {
        1 + self.children.iter().map(LayerTree::len).sum::<usize>()
    }

    /// Always `false`: a tree holds at least its root layer.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// The descendant at `path` (child indices from the root).
    pub fn get(&self, path: &[usize]) -> Option<&LayerTree<T>> {
        path.iter()
            .try_fold(self, |tree, &index| tree.children.get(index))
    }

    /// Mutable form of [`get`](Self::get).
    pub fn get_mut(&mut self, path: &[usize]) -> Option<&mut LayerTree<T>> {
        path.iter()
            .try_fold(self, |tree, &index| tree.children.get_mut(index))
    }
}

fn validate_detached_tree<T: BitDepth>(
    tree: &LayerTree<T>,
    parent_is_artboard: bool,
) -> Result<usize> {
    let mut pending = vec![(tree, parent_is_artboard)];
    let mut artboards = 0usize;
    while let Some((tree, parent_is_artboard)) = pending.pop() {
        if tree.layer.group().is_none() && !tree.children.is_empty() {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "only group layers can contain children",
            });
        }
        let is_artboard = tree.layer.is_artboard();
        if parent_is_artboard && is_artboard {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "artboards cannot be nested inside other artboards",
            });
        }
        artboards =
            artboards
                .checked_add(usize::from(is_artboard))
                .ok_or(PsdError::InvalidData {
                    offset: 0,
                    message: "artboard count exceeds the descriptor integer range",
                })?;
        pending.extend(
            tree.children
                .iter()
                .map(|child| (child, parent_is_artboard || is_artboard)),
        );
    }
    Ok(artboards)
}

impl<T: BitDepth> From<Layer<T>> for LayerTree<T> {
    fn from(layer: Layer<T>) -> Self {
        Self::new(layer)
    }
}

fn not_a_group() -> PsdError {
    PsdError::InvalidData {
        offset: 0,
        message: "target layer is not a group",
    }
}

fn unknown_layer() -> PsdError {
    PsdError::InvalidData {
        offset: 0,
        message: "layer id does not name a layer in the document",
    }
}

impl<T: BitDepth> LayeredFile<T> {
    /// The group directly containing `id`; `None` for root-level layers and
    /// unknown ids.
    pub fn parent(&self, id: LayerId) -> Option<LayerId> {
        self.layers_with_ids().find_map(|(group_id, layer)| {
            layer
                .group()
                .filter(|group| group.children.contains(&id))
                .map(|_| group_id)
        })
    }

    /// Whether `id` names a layer of this document (upstream
    /// `is_layer_in_file`). Every live layer is reachable from the root.
    pub fn contains_layer(&self, id: LayerId) -> bool {
        self.layer(id).is_some()
    }

    /// The child ids of `parent` (the root when `None`), bottom-to-top,
    /// section dividers included.
    pub fn children(&self, parent: Option<LayerId>) -> Option<&[LayerId]> {
        match parent {
            None => Some(self.root_children()),
            Some(id) => self
                .layer(id)?
                .group()
                .map(|group| group.children.as_slice()),
        }
    }

    /// Whether `descendant` is `ancestor` or lies below it.
    pub fn is_descendant(&self, descendant: LayerId, ancestor: LayerId) -> bool {
        let mut current = Some(descendant);
        while let Some(id) = current {
            if id == ancestor {
                return true;
            }
            current = self.parent(id);
        }
        false
    }

    pub(crate) fn parent_is_within_artboard(&self, parent: Option<LayerId>) -> bool {
        parent.is_some_and(|parent| {
            self.artboards()
                .into_iter()
                .any(|artboard| parent == artboard || self.is_descendant(parent, artboard))
        })
    }

    /// Remove a layer (a group with everything below it) and return it as a
    /// detached tree. The removed ids are never reused; other ids stay valid.
    /// A group's `</Layer group>` divider is dropped with it.
    pub fn remove_layer(&mut self, id: LayerId) -> Result<LayerTree<T>> {
        if self
            .layer(id)
            .is_some_and(|layer| matches!(layer.kind, LayerKind::SectionDivider(_)))
        {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "section dividers are removed together with their group",
            });
        }
        let removed_artboards = self
            .artboards()
            .into_iter()
            .filter(|&artboard| artboard == id || self.is_descendant(artboard, id))
            .count();
        let settings_update = if removed_artboards > 0 {
            self.artboard_settings()?
                .map(|mut settings| {
                    let remaining = self.artboards().len().saturating_sub(removed_artboards);
                    let count = i32::try_from(remaining).map_err(|_| PsdError::InvalidData {
                        offset: 0,
                        message: "artboard count exceeds the descriptor integer range",
                    })?;
                    settings.set_count(count);
                    settings.to_tagged_block(self.version)
                })
                .transpose()?
        } else {
            None
        };
        let parent = self.parent(id);
        let list = self.children_mut(parent).ok_or_else(unknown_layer)?;
        let position = list
            .iter()
            .position(|&child| child == id)
            .ok_or_else(unknown_layer)?;
        let unit = self.unit_range(parent, position);
        let removed: Vec<LayerId> = self
            .children_mut(parent)
            .expect("checked above")
            .drain(unit)
            .collect();
        for divider in removed.iter().copied().filter(|&other| other != id) {
            self.slots_mut()[divider] = None;
        }
        let removed = self.take_tree(id);
        if let Some(block) = settings_update {
            if let Some(document_blocks) = &mut self.document_blocks {
                if let Some(existing) = document_blocks
                    .blocks
                    .iter_mut()
                    .find(|existing| existing.key == block.key)
                {
                    *existing = block;
                }
            }
        }
        Ok(removed)
    }

    /// Insert a detached tree under `parent` (the root when `None`) and return
    /// the id of its root layer. `index` counts the real layers of the new
    /// siblings bottom-to-top (`Some(0)` is the bottom); `None` places the
    /// tree on top.
    pub fn insert_layer_tree(
        &mut self,
        parent: Option<LayerId>,
        index: Option<usize>,
        tree: LayerTree<T>,
    ) -> Result<LayerId> {
        self.children(parent).ok_or_else(not_a_group)?;
        let added = validate_detached_tree(&tree, self.parent_is_within_artboard(parent))?;
        let settings_update = {
            if added == 0 {
                None
            } else {
                let count =
                    self.artboards()
                        .len()
                        .checked_add(added)
                        .ok_or(PsdError::InvalidData {
                            offset: 0,
                            message: "artboard count exceeds the descriptor integer range",
                        })?;
                Some(self.artboard_settings_block_with_count(count)?)
            }
        };
        let id = self.allocate_tree(tree);
        let position = self.physical_index(parent, index);
        self.children_mut(parent)
            .expect("checked above")
            .insert(position, id);
        if let Some(block) = settings_update {
            self.upsert_document_block(block);
        }
        Ok(id)
    }

    /// Copy a layer — with its subtree, group divider, mask, effects, text and
    /// every preserved block — directly above the original, returning the
    /// copy's id.
    ///
    /// The copy gets a fresh `lyid` when the original had one, since Photoshop
    /// expects layer ids to be unique. Smart-object and linked-file *references*
    /// come along as they are; the document-level blocks they point at are not
    /// copied, exactly as in Photoshop's own "Duplicate Layer".
    pub fn duplicate_layer(&mut self, id: LayerId) -> Result<LayerId> {
        let parent = self.parent(id);
        let tree = self.clone_tree(id)?;
        let children = self.children(parent).ok_or_else(unknown_layer)?;
        let position = children
            .iter()
            .position(|&child| child == id)
            .ok_or_else(unknown_layer)?;
        // The real-layer index of the original; the copy goes above it.
        let above = children[..=position]
            .iter()
            .filter(|&&child| !self.is_divider(child))
            .count();
        self.insert_layer_tree(parent, Some(above), tree)
    }

    /// Copy a layer (with its subtree) from another document into this one, on
    /// top, returning the new id.
    ///
    /// The copy owns its data, so the source document does not have to outlive
    /// it. Layer ids are made unique against this document. Document-level data
    /// a layer only *references* — a smart object's linked file, a pattern — is
    /// not copied; bring it over with the same layer from a document that has
    /// it, or relink afterwards.
    pub fn copy_layer_from(&mut self, source: &LayeredFile<T>, id: LayerId) -> Result<LayerId> {
        let tree = source.clone_tree(id)?;
        self.insert_layer_tree(None, None, tree)
    }

    /// Insert a single layer; see [`insert_layer_tree`](Self::insert_layer_tree).
    pub fn insert_layer(
        &mut self,
        parent: Option<LayerId>,
        index: Option<usize>,
        layer: Layer<T>,
    ) -> Result<LayerId> {
        self.insert_layer_tree(parent, index, LayerTree::new(layer))
    }

    /// Move a layer (with its subtree and group divider) under `parent` (the
    /// root when `None`). `index` is interpreted after the layer left its old
    /// place; see [`insert_layer_tree`](Self::insert_layer_tree). Moving a
    /// layer into itself or its own subtree is rejected, as upstream does.
    pub fn move_layer(
        &mut self,
        id: LayerId,
        parent: Option<LayerId>,
        index: Option<usize>,
    ) -> Result<()> {
        if self
            .layer(id)
            .is_none_or(|layer| matches!(layer.kind, LayerKind::SectionDivider(_)))
        {
            return Err(unknown_layer());
        }
        self.children(parent).ok_or_else(not_a_group)?;
        if self.parent_is_within_artboard(parent)
            && self
                .artboards()
                .into_iter()
                .any(|artboard| artboard == id || self.is_descendant(artboard, id))
        {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "artboards cannot be nested inside other artboards",
            });
        }
        if parent.is_some_and(|target| self.is_descendant(target, id)) {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "cannot move a layer into itself or its own subtree",
            });
        }
        let old_parent = self.parent(id);
        let position = self
            .children(old_parent)
            .and_then(|list| list.iter().position(|&child| child == id))
            .ok_or_else(unknown_layer)?;
        let unit = self.unit_range(old_parent, position);
        let moved: Vec<LayerId> = self
            .children_mut(old_parent)
            .expect("checked above")
            .drain(unit)
            .collect();
        let position = self.physical_index(parent, index);
        self.children_mut(parent)
            .expect("checked above")
            .splice(position..position, moved);
        Ok(())
    }

    /// Physical range of the child at `position`: a group together with the
    /// section divider right before it.
    fn unit_range(&self, parent: Option<LayerId>, position: usize) -> std::ops::Range<usize> {
        let list = self.children(parent).expect("caller validated the parent");
        let is_group = self
            .layer(list[position])
            .is_some_and(|layer| layer.group().is_some());
        let divider_before = position > 0
            && self
                .layer(list[position - 1])
                .is_some_and(|layer| matches!(layer.kind, LayerKind::SectionDivider(_)));
        if is_group && divider_before {
            position - 1..position + 1
        } else {
            position..position + 1
        }
    }

    /// Map a real-layer index to a position in the physical children list,
    /// never splitting a group from its divider.
    fn physical_index(&self, parent: Option<LayerId>, index: Option<usize>) -> usize {
        let list = self.children(parent).expect("caller validated the parent");
        let Some(index) = index else {
            return list.len();
        };
        let mut seen = 0;
        for (position, &child) in list.iter().enumerate() {
            if self
                .layer(child)
                .is_some_and(|layer| matches!(layer.kind, LayerKind::SectionDivider(_)))
            {
                continue;
            }
            if seen == index {
                return self.unit_range(parent, position).start;
            }
            seen += 1;
        }
        list.len()
    }

    fn children_mut(&mut self, parent: Option<LayerId>) -> Option<&mut Vec<LayerId>> {
        match parent {
            None => Some(self.root_children_mut()),
            Some(id) => self
                .layer_mut(id)?
                .group_mut()
                .map(|group| &mut group.children),
        }
    }

    /// Empty the slots of `id`'s subtree and rebuild it as a detached tree.
    fn take_tree(&mut self, id: LayerId) -> LayerTree<T> {
        let mut layer = self.slots_mut()[id]
            .take()
            .expect("tree links name live layers");
        let children = layer
            .group_mut()
            .map(|group| std::mem::take(&mut group.children))
            .unwrap_or_default();
        let children = children
            .into_iter()
            .filter_map(|child| {
                // Dividers inside the group were only needed on disk.
                let is_divider = self.slots()[child]
                    .as_ref()
                    .is_some_and(|layer| matches!(layer.kind, LayerKind::SectionDivider(_)));
                if is_divider {
                    self.slots_mut()[child] = None;
                    None
                } else {
                    Some(self.take_tree(child))
                }
            })
            .collect();
        LayerTree { layer, children }
    }

    /// A detached copy of `id`'s subtree, shaped like [`take_tree`]'s result:
    /// the group's child ids are dropped (they only mean something inside a
    /// document) and the dividers between them are left behind, since a group
    /// gets one synthesized on write.
    fn clone_tree(&self, id: LayerId) -> Result<LayerTree<T>> {
        let layer = self.layer(id).ok_or_else(unknown_layer)?;
        let children: Vec<LayerId> = layer
            .group()
            .map(|group| group.children.clone())
            .unwrap_or_default();
        let mut tree = LayerTree::new(layer.clone());
        for child in children {
            if self.is_divider(child) {
                continue;
            }
            tree.children.push(self.clone_tree(child)?);
        }
        Ok(tree)
    }

    /// Whether `id` names a section divider.
    fn is_divider(&self, id: LayerId) -> bool {
        self.layer(id)
            .is_some_and(|layer| matches!(layer.kind, LayerKind::SectionDivider(_)))
    }

    /// Push a detached tree into fresh slots, returning its root id.
    ///
    /// This is the one place every detached-tree insertion passes through, so
    /// the layer-id guard lives here: a duplicate, a cross-document copy, or a
    /// re-inserted tree whose id was taken again while it was detached all get
    /// fresh ids where they collide.
    fn allocate_tree(&mut self, tree: LayerTree<T>) -> LayerId {
        let LayerTree {
            mut layer,
            children,
        } = tree;
        if let Some(group) = layer.group_mut() {
            group.children.clear();
        }
        self.ensure_unique_layer_id(&mut layer);
        self.slots_mut().push(Some(layer));
        let id = self.slots().len() - 1;
        let child_ids: Vec<LayerId> = children
            .into_iter()
            .map(|child| self.allocate_tree(child))
            .collect();
        if let Some(group) = self.layer_mut(id).and_then(Layer::group_mut) {
            group.children = child_ids;
        }
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::ChannelKey;
    use crate::layer::Rect;
    use psd_core::ColorMode;

    fn image(name: &str) -> Layer<u8> {
        let mut layer = Layer::new_image(name, Rect::new(0, 0, 1, 1));
        let pixels = layer.image_mut().unwrap();
        for channel in 0..3 {
            pixels.set_channel(ChannelKey::color(channel), vec![channel]);
        }
        layer
    }

    fn names(document: &LayeredFile<u8>, parent: Option<LayerId>) -> Vec<String> {
        document
            .children(parent)
            .unwrap()
            .iter()
            .map(|&id| document.layer(id).unwrap().name.clone())
            .collect()
    }

    #[test]
    fn remove_detaches_subtrees_and_keeps_other_ids() {
        let mut document = LayeredFile::<u8>::new(ColorMode::Rgb, 1, 1).unwrap();
        let bottom = document.add_layer(image("Bottom"));
        let group = document.add_layer(Layer::new_group("Group"));
        let child = document.add_layer_to_group(group, image("Child")).unwrap();
        let top = document.add_layer(image("Top"));

        let tree = document.remove_layer(group).unwrap();
        assert_eq!(tree.len(), 2);
        assert_eq!(tree.children[0].layer.name, "Child");
        assert!(document.layer(group).is_none() && document.layer(child).is_none());
        assert_eq!(document.layer(top).unwrap().name, "Top");
        assert_eq!(document.layer_count(), 2);
        assert!(!document.contains_layer(child));

        // Re-inserting at the bottom restores the subtree under fresh ids.
        let restored = document.insert_layer_tree(None, Some(0), tree).unwrap();
        assert_ne!(restored, group);
        assert_eq!(names(&document, None), ["Group", "Bottom", "Top"]);
        assert_eq!(names(&document, Some(restored)), ["Child"]);
        assert!(document.find_layer("Group/Child").is_some());
        assert!(document.remove_layer(bottom).is_ok());

        let reread = LayeredFile::<u8>::from_bytes(&document.to_bytes().unwrap()).unwrap();
        assert!(reread.find_layer("Group/Child").is_some());
        assert!(reread.find_layer("Bottom").is_none());
    }

    /// A section divider is half of a pair: removing it on its own would leave
    /// the group without its closing record, which is the malformed shape a
    /// group linker has to survive. The API refuses the removal instead, and
    /// removing the group takes both records with it.
    #[test]
    fn a_section_divider_cannot_be_removed_on_its_own() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/documents/Groups/Groups_8bit.psd"
        );
        let mut document = LayeredFile::<u8>::read(path).unwrap();
        let group = document.find_layer("Group").unwrap();
        let divider = document
            .flatten()
            .into_iter()
            .find(|&id| {
                matches!(
                    document.layer(id).unwrap().kind,
                    LayerKind::SectionDivider(_)
                )
            })
            .unwrap();

        let error = document.remove_layer(divider).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("removed together with their group"),
            "unexpected error: {error}"
        );
        assert!(document.layer(divider).is_some(), "the divider stays");

        // Removing the group takes its divider record with it: the document
        // still round-trips, the group is gone, and the other groups keep their
        // nesting (a dangling divider would mis-parent them on the next read).
        document.remove_layer(group).unwrap();
        let reread = LayeredFile::<u8>::from_bytes(&document.to_bytes().unwrap()).unwrap();
        assert!(reread.find_layer("Group").is_none());
        assert!(reread.find_layer("Group/GroupedLayer").is_none());
        assert!(reread.find_layer("GroupTopLevel/CollapsedGroup").is_some());
        assert!(reread
            .find_layer("GroupTopLevel/GroupNested/NestedGroupedLayer")
            .is_some());
    }

    #[test]
    fn move_keeps_read_dividers_paired_and_rejects_cycles() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/documents/Groups/Groups_8bit.psd"
        );
        let mut document = LayeredFile::<u8>::read(path).unwrap();
        let nested = document.find_layer("GroupTopLevel/GroupNested").unwrap();
        let group = document.find_layer("GroupTopLevel").unwrap();
        assert!(document.move_layer(group, Some(nested), None).is_err());
        assert!(document.move_layer(group, Some(group), None).is_err());

        document.move_layer(nested, None, Some(0)).unwrap();
        assert_eq!(document.parent(nested), None);
        let reread = LayeredFile::<u8>::from_bytes(&document.to_bytes().unwrap()).unwrap();
        assert!(reread
            .find_layer("GroupNested/NestedGroupedLayer")
            .is_some());
        assert!(reread.find_layer("GroupTopLevel/GroupNested").is_none());
        assert!(reread
            .find_layer("GroupTopLevel/CollapsedGroup/BlackLayer")
            .is_some());
        assert!(reread.find_layer("Group/GroupedLayer").is_some());
        // The moved group is still preceded by exactly one divider.
        let root = reread.children(None).unwrap();
        let moved = reread.find_layer("GroupNested").unwrap();
        let at = root.iter().position(|&id| id == moved).unwrap();
        assert!(matches!(
            reread.layer(root[at - 1]).unwrap().kind,
            LayerKind::SectionDivider(_)
        ));
    }

    #[test]
    fn insert_index_counts_real_layers_only() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/documents/Groups/Groups_8bit.psd"
        );
        let mut document = LayeredFile::<u8>::read(path).unwrap();
        let before: Vec<String> = document
            .children(None)
            .unwrap()
            .iter()
            .map(|&id| document.layer(id).unwrap().name.clone())
            .filter(|name| name != "</Layer group>")
            .collect();
        document
            .insert_layer(None, Some(1), image("Inserted"))
            .unwrap();
        let after: Vec<String> = document
            .children(None)
            .unwrap()
            .iter()
            .map(|&id| document.layer(id).unwrap().name.clone())
            .filter(|name| name != "</Layer group>")
            .collect();
        let mut expected = before.clone();
        expected.insert(1, "Inserted".to_owned());
        assert_eq!(after, expected);
        let reread = LayeredFile::<u8>::from_bytes(&document.to_bytes().unwrap()).unwrap();
        assert_eq!(reread.layer_count(), document.layer_count());
        assert!(reread.find_layer("Inserted").is_some());
    }

    #[test]
    fn malformed_detached_trees_are_rejected_before_allocating_layers() {
        let mut document = LayeredFile::<u8>::new(ColorMode::Rgb, 1, 1).unwrap();
        document.add_layer(image("Existing"));
        let before = document.to_bytes().unwrap();
        let tree = LayerTree {
            layer: image("Invalid parent"),
            children: vec![LayerTree::new(Layer::new_group("Child"))],
        };
        assert!(document.insert_layer_tree(None, None, tree).is_err());
        assert_eq!(document.layer_count(), 1);
        assert_eq!(document.to_bytes().unwrap(), before);
    }

    #[test]
    fn detached_trees_build_before_insertion() {
        let mut tree = LayerTree::new(Layer::<u8>::new_group("Outer"));
        let mut inner = LayerTree::new(Layer::new_group("Inner"));
        inner.push_child(image("Leaf").into()).unwrap();
        tree.push_child(inner).unwrap();
        assert!(LayerTree::new(image("Leaf"))
            .push_child(image("x").into())
            .is_err());
        assert_eq!(tree.get(&[0, 0]).unwrap().layer.name, "Leaf");

        let mut document = LayeredFile::<u8>::new(ColorMode::Rgb, 1, 1).unwrap();
        document.insert_layer_tree(None, None, tree).unwrap();
        let reread = LayeredFile::<u8>::from_bytes(&document.to_bytes().unwrap()).unwrap();
        assert!(reread.find_layer("Outer/Inner/Leaf").is_some());
    }

    /// A divider record whose `lsct` value this build does not know still pairs
    /// with its group, so the tree keeps its shape and a save cannot add a
    /// second divider.
    #[test]
    fn an_unknown_section_divider_type_keeps_its_pairing() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/documents/Groups/Groups_8bit.psd"
        );
        let original = LayeredFile::<u8>::read(path).unwrap();
        let expected = original.flatten().len();

        // Patch every bounding-section divider (`lsct` type 3) to a value the
        // enum does not define, the way a newer Photoshop or a patched file
        // could.
        let mut bytes = std::fs::read(path).unwrap();
        let mut patched = 0;
        let mut at = 0;
        while let Some(found) = bytes[at..]
            .windows(16)
            .position(|window| window.starts_with(b"8BIMlsct") && window[12..16] == [0, 0, 0, 3])
        {
            let start = at + found + 12;
            bytes[start..start + 4].copy_from_slice(&0xcafe_babeu32.to_be_bytes());
            patched += 1;
            at = start + 4;
        }
        assert!(patched > 0, "the fixture must carry bounding dividers");

        let document = LayeredFile::<u8>::from_bytes(&bytes).unwrap();
        assert_eq!(
            document.flatten().len(),
            expected,
            "the divider records keep their place in the tree"
        );
        assert!(
            document
                .flatten()
                .into_iter()
                .any(|id| document.layer(id).is_some_and(|layer| matches!(
                    layer.kind,
                    LayerKind::SectionDivider(kind) if !kind.is_known()
                ))),
            "the unknown divider is still a divider layer"
        );
        // The written document keeps the same records (no second divider for
        // the group) and the unknown value itself, byte for byte. The whole
        // file is not compared: the writer's transparency-channel rule is a
        // separate, deliberate difference.
        let written = document.to_bytes().unwrap();
        assert!(
            written
                .windows(4)
                .any(|window| window == 0xcafe_babeu32.to_be_bytes()),
            "the unknown divider value survives the write"
        );
        let back = LayeredFile::<u8>::from_bytes(&written).unwrap();
        assert_eq!(
            back.flatten().len(),
            expected,
            "the re-read document has the same records"
        );
    }

    /// A duplicate lands directly above its original, carries the pixels and
    /// blocks, and gets a fresh id; the document still round-trips.
    #[test]
    fn duplicating_a_layer_places_a_copy_above_it() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/documents/Groups/Groups_8bit.psd"
        );
        let mut document = LayeredFile::<u8>::read(path).unwrap();
        let id = document.find_layer("Group/GroupedLayer").unwrap();
        let original_id = document.layer(id).unwrap().layer_id();
        let pixels = document.layer(id).unwrap().channels().cloned();

        let copy = document.duplicate_layer(id).unwrap();
        assert_ne!(copy, id);
        let parent = document.parent(id);
        assert_eq!(
            document.parent(copy),
            parent,
            "the copy stays in the same parent"
        );
        assert_eq!(document.layer(copy).unwrap().name, "GroupedLayer");
        assert_eq!(
            document.layer(copy).unwrap().channels().cloned(),
            pixels,
            "the copy carries the pixels"
        );
        assert_ne!(
            document.layer(copy).unwrap().layer_id(),
            original_id,
            "the copy gets a fresh id"
        );
        assert_eq!(document.layer(id).unwrap().layer_id(), original_id);

        // The copy sits directly above the original in file order.
        let children = document.children(parent).unwrap().to_vec();
        let physical = |id: LayerId| children.iter().position(|&child| child == id).unwrap();
        assert_eq!(physical(copy), physical(id) + 1, "directly above");

        // Both copies survive a write; ids change on re-read, so match by name.
        let back = LayeredFile::<u8>::from_bytes(&document.to_bytes().unwrap()).unwrap();
        let grouped: Vec<LayerId> = back
            .flatten()
            .into_iter()
            .filter(|&id| back.layer(id).unwrap().name == "GroupedLayer")
            .collect();
        assert_eq!(grouped.len(), 2, "the original and its copy");
        assert_eq!(
            back.parent(grouped[0]),
            back.parent(grouped[1]),
            "both sit in the same group"
        );
    }

    /// Duplicating a group brings its whole subtree, and the copy's divider is
    /// synthesized on write, so both groups survive a round trip.
    #[test]
    fn duplicating_a_group_copies_its_subtree() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/documents/Groups/Groups_8bit.psd"
        );
        let mut document = LayeredFile::<u8>::read(path).unwrap();
        let group = document.find_layer("GroupTopLevel/GroupNested").unwrap();
        let children_before = names(&document, Some(group));

        let copy = document.duplicate_layer(group).unwrap();
        assert_eq!(document.layer(copy).unwrap().name, "GroupNested");
        assert_eq!(names(&document, Some(copy)), children_before);
        assert_eq!(document.parent(copy), document.parent(group));

        let back = LayeredFile::<u8>::from_bytes(&document.to_bytes().unwrap()).unwrap();
        let copies: Vec<&str> = back
            .flatten()
            .into_iter()
            .filter(|&id| back.layer(id).unwrap().name == "GroupNested")
            .map(|_| "GroupNested")
            .collect();
        assert_eq!(copies.len(), 2, "both groups survive the write");
        // The copied group's children came along.
        let copied = back
            .flatten()
            .into_iter()
            .filter(|&id| back.layer(id).unwrap().name == "GroupNested")
            .map(|id| names(&back, Some(id)))
            .find(|names| names != &children_before);
        assert!(
            copied.is_none(),
            "both copies carry the same children: {copied:?}"
        );
    }

    /// A layer copied from another document arrives on top, owns its data, and
    /// leaves the source untouched.
    #[test]
    fn copying_a_layer_between_documents() {
        let groups = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/documents/Groups/Groups_8bit.psd"
        );
        let source = LayeredFile::<u8>::read(groups).unwrap();
        let id = source.find_layer("Group/GroupedLayer").unwrap();
        let source_ids: Vec<u32> = source
            .flatten()
            .into_iter()
            .filter_map(|id| source.layer(id).unwrap().layer_id())
            .collect();

        let mut target = LayeredFile::<u8>::new(ColorMode::Rgb, 8, 8).unwrap();
        let copy = target.copy_layer_from(&source, id).unwrap();
        assert_eq!(target.layer(copy).unwrap().name, "GroupedLayer");
        assert_eq!(target.children(None).unwrap().len(), 1, "arrives on top");
        assert_eq!(
            target.layer(copy).unwrap().channels(),
            source.layer(id).unwrap().channels()
        );

        // A group comes over with its subtree.
        let group = source.find_layer("Group").unwrap();
        let copied_group = target.copy_layer_from(&source, group).unwrap();
        assert_eq!(names(&target, Some(copied_group)), ["GroupedLayer"]);

        // The source is untouched, and its ids are still unique there.
        assert_eq!(
            source
                .flatten()
                .into_iter()
                .filter_map(|id| source.layer(id).unwrap().layer_id())
                .collect::<Vec<_>>(),
            source_ids
        );

        // Copying the same layer twice gives two unique ids in the target.
        let second = target.copy_layer_from(&source, id).unwrap();
        let ids: Vec<u32> = target
            .flatten()
            .into_iter()
            .filter_map(|id| target.layer(id).unwrap().layer_id())
            .collect();
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(ids.len(), unique.len(), "ids are unique: {ids:?}");
        assert_ne!(copy, second);

        // Everything arrives after a write (a copied group gains its divider
        // record there, so counts differ by design).
        let back = LayeredFile::<u8>::from_bytes(&target.to_bytes().unwrap()).unwrap();
        let copied_names: Vec<String> = back
            .flatten()
            .into_iter()
            .map(|id| back.layer(id).unwrap().name.clone())
            .filter(|name| name != "</Layer group>")
            .collect();
        assert_eq!(
            copied_names
                .iter()
                .filter(|name| *name == "GroupedLayer")
                .count(),
            3,
            "two copies plus the one inside the copied group: {copied_names:?}"
        );
        assert!(back.find_layer("Group/GroupedLayer").is_some());
    }
}
