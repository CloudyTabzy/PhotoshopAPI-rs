//! Layer-tree helpers shared by the document and group classes.
//!
//! The Python API follows upstream's ordering: `layers` lists children top
//! to bottom, `flat_layers` walks the tree top to bottom with each group
//! before its children, and `add_layer` inserts at the *bottom* of its
//! parent (upstream appends to a top-to-bottom list). The `</Layer group>`
//! section dividers the Rust tree keeps for byte-exact files are hidden.

use std::sync::Arc;

use psd::{BitDepth, LayerId, LayerKind, LayeredFile};
use pyo3::exceptions::{PyKeyError, PyTypeError, PyValueError};
use pyo3::prelude::*;

use crate::state::{psd_error, read_document, write_document, Document, LayerHandle, Location};

/// Real children of `parent` (the root when `None`), top to bottom.
pub fn visible_children<T: BitDepth>(
    file: &LayeredFile<T>,
    parent: Option<LayerId>,
) -> Vec<LayerId> {
    file.children(parent)
        .unwrap_or_default()
        .iter()
        .rev()
        .copied()
        .filter(|&id| {
            file.layer(id)
                .is_some_and(|layer| !matches!(layer.kind, LayerKind::SectionDivider(_)))
        })
        .collect()
}

/// Every real layer, top to bottom, groups before their children.
pub fn flat_ids<T: BitDepth>(file: &LayeredFile<T>) -> Vec<LayerId> {
    fn walk<T: BitDepth>(file: &LayeredFile<T>, parent: Option<LayerId>, out: &mut Vec<LayerId>) {
        for id in visible_children(file, parent) {
            out.push(id);
            if file.layer(id).is_some_and(|layer| layer.group().is_some()) {
                walk(file, Some(id), out);
            }
        }
    }
    let mut out = Vec::new();
    walk(file, None, &mut out);
    out
}

pub fn handles<T: BitDepth>(document: &Document<T>, ids: Vec<LayerId>) -> Vec<LayerHandle<T>> {
    ids.into_iter()
        .map(|id| LayerHandle::attached(Arc::clone(document), id))
        .collect()
}

/// Child handles of a group handle, top to bottom, whether it is in a
/// document or still detached.
pub fn group_children<T: BitDepth>(group: &LayerHandle<T>) -> PyResult<Vec<LayerHandle<T>>> {
    match group.place()? {
        Location::Attached(document, id) => {
            let ids = read_document(&document, |file| Ok(visible_children(file, Some(id))))?;
            Ok(handles(&document, ids))
        }
        Location::Detached | Location::Nested => {
            let count = group.with_tree(|tree| Ok(tree.children.len()), |_, _| unreachable!())?;
            (0..count)
                .rev()
                .map(|index| group.nested_child(index))
                .collect()
        }
    }
}

/// The first child named `name` of a group or the root.
pub fn child_named<T: BitDepth>(
    document: &Document<T>,
    parent: Option<LayerId>,
    name: &str,
) -> PyResult<LayerId> {
    read_document(document, |file| {
        visible_children(file, parent)
            .into_iter()
            .find(|&id| file.layer(id).is_some_and(|layer| layer.name == name))
            .ok_or_else(|| PyKeyError::new_err(format!("no layer named '{name}'")))
    })
}

/// Resolve a layer argument (a layer object of this document or a path).
pub fn resolve<T: BitDepth>(
    document: &Document<T>,
    value: &Bound<'_, PyAny>,
    handle_of: impl Fn(&Bound<'_, PyAny>) -> Option<LayerHandle<T>>,
) -> PyResult<LayerId> {
    if let Ok(path) = value.extract::<String>() {
        return read_document(document, |file| {
            file.find_layer(&path).ok_or_else(|| {
                PyValueError::new_err(format!("'{path}' is not a layer path in this document"))
            })
        });
    }
    let handle =
        handle_of(value).ok_or_else(|| PyTypeError::new_err("expected a layer or a layer path"))?;
    match handle.place()? {
        Location::Attached(owner, id) if Arc::ptr_eq(&owner, document) => Ok(id),
        _ => Err(PyValueError::new_err(
            "the layer is not part of this document",
        )),
    }
}

/// Add a detached layer to a document group (or the root) at the bottom,
/// or into a detached group.
pub fn add_child<T: BitDepth>(
    parent: Option<&LayerHandle<T>>,
    document: Option<&Document<T>>,
    child: &LayerHandle<T>,
) -> PyResult<()> {
    match (parent, document) {
        (None, Some(document)) => child.attach_to(document, None, Some(0)).map(|_| ()),
        (Some(parent), _) => match parent.place()? {
            Location::Attached(document, id) => {
                let is_group = read_document(&document, |file| {
                    Ok(file.layer(id).is_some_and(|layer| layer.group().is_some()))
                })?;
                if !is_group {
                    return Err(PyValueError::new_err("layers can only be added to groups"));
                }
                child.attach_to(&document, Some(id), Some(0)).map(|_| ())
            }
            Location::Detached | Location::Nested => parent.adopt(child, Some(0)),
        },
        (None, None) => Err(PyValueError::new_err("no parent to add the layer to")),
    }
}

/// Remove a layer from its document, detaching the given handle (if any)
/// into the removed subtree.
pub fn remove_id<T: BitDepth>(
    document: &Document<T>,
    id: LayerId,
    handle: Option<&LayerHandle<T>>,
) -> PyResult<()> {
    let tree = write_document(document, |file| file.remove_layer(id).map_err(psd_error))?;
    match handle {
        Some(handle) => handle.detach_into(tree, Arc::clone(document)),
        None => Ok(()),
    }
}

/// Move `id` under `parent` (the root when `None`), placing it at the
/// bottom like upstream's remove-then-append.
pub fn move_id<T: BitDepth>(
    document: &Document<T>,
    id: LayerId,
    parent: Option<LayerId>,
) -> PyResult<()> {
    write_document(document, |file| {
        if let Some(parent) = parent {
            if file
                .layer(parent)
                .is_none_or(|layer| layer.group().is_none())
            {
                return Err(PyValueError::new_err(
                    "layers can only be moved under group layers",
                ));
            }
        }
        file.move_layer(id, parent, Some(0)).map_err(psd_error)
    })
}
