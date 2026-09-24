//! Shared document and layer ownership for live Python layer objects.
//!
//! A Python layer object is a [`LayerHandle`] naming one of three places:
//!
//! - **Attached**: a layer id inside a shared document. Ids are stable (the
//!   document's slot arena never reuses them), so moving or removing *other*
//!   layers never retargets a handle.
//! - **Detached**: an owned [`LayerTree`] (a new layer, or one removed from a
//!   document). Detached groups can collect children before being added,
//!   which upstream's shared-pointer layers allow too. A detached tree
//!   remembers its *home* document — where the smart-object link records it
//!   needs live — so smart objects can be edited before they are added.
//! - **Nested**: a child inside some detached handle's tree, addressed by its
//!   path. When that tree is added to a document, every nested handle it
//!   registered is switched to its new attached id.
//!
//! Locks are always taken handle → root handle → document.

use std::sync::{Arc, Mutex, RwLock, Weak};

use psd::core::PsdError;
use psd::{BitDepth, Layer, LayerId, LayerTree, LayeredFile};
use pyo3::exceptions::{PyOSError, PyRuntimeError, PyValueError};
use pyo3::{PyErr, PyResult};

pub type Document<T> = Arc<RwLock<LayeredFile<T>>>;

pub fn psd_error(error: PsdError) -> PyErr {
    let message = error.to_string();
    match error {
        PsdError::Io(_) => PyOSError::new_err(message),
        PsdError::InvalidData { .. }
        | PsdError::UnsupportedBitDepth(_)
        | PsdError::UnsupportedColorMode(_)
        | PsdError::UnsupportedCompression(_)
        | PsdError::UnsupportedVersion(_) => PyValueError::new_err(message),
        _ => PyRuntimeError::new_err(message),
    }
}

fn lock_error() -> PyErr {
    PyRuntimeError::new_err("a Photoshop document lock was poisoned")
}

fn stale_error() -> PyErr {
    PyRuntimeError::new_err("the layer no longer exists (it was removed from its document)")
}

fn no_home_error() -> PyErr {
    PyValueError::new_err(
        "this smart object is not linked to a document; construct it with its LayeredFile",
    )
}

pub fn read_document<T: BitDepth, R>(
    document: &Document<T>,
    f: impl FnOnce(&LayeredFile<T>) -> PyResult<R>,
) -> PyResult<R> {
    let guard = document.read().map_err(|_| lock_error())?;
    f(&guard)
}

pub fn write_document<T: BitDepth, R>(
    document: &Document<T>,
    f: impl FnOnce(&mut LayeredFile<T>) -> PyResult<R>,
) -> PyResult<R> {
    let mut guard = document.write().map_err(|_| lock_error())?;
    f(&mut guard)
}

type Target<T> = Arc<Mutex<LayerTarget<T>>>;

/// A handle registered on a detached root so it can be re-pointed later.
struct Member<T: BitDepth> {
    path: Vec<usize>,
    target: Weak<Mutex<LayerTarget<T>>>,
}

enum LayerTarget<T: BitDepth> {
    Detached {
        tree: Box<LayerTree<T>>,
        members: Vec<Member<T>>,
        home: Option<Document<T>>,
    },
    Nested {
        root: Target<T>,
        path: Vec<usize>,
    },
    Attached {
        document: Document<T>,
        id: LayerId,
    },
    /// Transient state while a tree is being moved.
    Moving,
}

pub struct LayerHandle<T: BitDepth> {
    target: Target<T>,
}

impl<T: BitDepth> Clone for LayerHandle<T> {
    fn clone(&self) -> Self {
        Self {
            target: Arc::clone(&self.target),
        }
    }
}

enum Node<'a, T: BitDepth> {
    Tree(&'a LayerTree<T>),
    Document(&'a LayeredFile<T>, LayerId),
}

enum NodeMut<'a, T: BitDepth> {
    Tree(&'a mut LayerTree<T>),
    Document(&'a mut LayeredFile<T>, LayerId),
}

/// Where a handle currently lives.
pub enum Location<T: BitDepth> {
    Attached(Document<T>, LayerId),
    Detached,
    Nested,
}

fn visit_tree<T: BitDepth>(tree: &LayerTree<T>, visit: &mut impl FnMut(&Layer<T>)) {
    visit(&tree.layer);
    for child in &tree.children {
        visit_tree(child, visit);
    }
}

/// Copy the smart-object links a tree needs from `home` into `target`.
fn import_links<T: BitDepth>(
    target: &mut LayeredFile<T>,
    home: &Document<T>,
    tree: &LayerTree<T>,
) -> PyResult<()> {
    let home = home.read().map_err(|_| lock_error())?;
    let mut result = Ok(());
    visit_tree(tree, &mut |layer| {
        if result.is_ok() {
            result = target.import_smart_object_links(&home, layer);
        }
    });
    result.map_err(psd_error)
}

impl<T: BitDepth> LayerHandle<T> {
    pub fn detached(layer: Layer<T>, home: Option<Document<T>>) -> Self {
        Self::from_tree(LayerTree::new(layer), home)
    }

    pub fn from_tree(tree: LayerTree<T>, home: Option<Document<T>>) -> Self {
        Self {
            target: Arc::new(Mutex::new(LayerTarget::Detached {
                tree: Box::new(tree),
                members: Vec::new(),
                home,
            })),
        }
    }

    pub fn attached(document: Document<T>, id: LayerId) -> Self {
        Self {
            target: Arc::new(Mutex::new(LayerTarget::Attached { document, id })),
        }
    }

    pub fn place(&self) -> PyResult<Location<T>> {
        let target = self.target.lock().map_err(|_| lock_error())?;
        Ok(match &*target {
            LayerTarget::Attached { document, id } => Location::Attached(Arc::clone(document), *id),
            LayerTarget::Detached { .. } => Location::Detached,
            LayerTarget::Nested { .. } => Location::Nested,
            LayerTarget::Moving => return Err(stale_error()),
        })
    }

    /// What this handle names, for equality: the document and id when
    /// attached, the handle's own state otherwise.
    /// A nested child is identified by its detached root and path, so every
    /// wrapper of the same child compares equal.
    pub fn identity(&self) -> PyResult<(usize, Vec<usize>)> {
        let address = |pointer: *const ()| pointer as usize;
        let target = self.target.lock().map_err(|_| lock_error())?;
        Ok(match &*target {
            LayerTarget::Attached { document, id } => {
                (address(Arc::as_ptr(document).cast()), vec![*id])
            }
            LayerTarget::Nested { root, path } => (address(Arc::as_ptr(root).cast()), path.clone()),
            LayerTarget::Detached { .. } | LayerTarget::Moving => {
                (address(Arc::as_ptr(&self.target).cast()), Vec::new())
            }
        })
    }

    pub fn with_layer<R>(&self, f: impl FnOnce(&Layer<T>) -> PyResult<R>) -> PyResult<R> {
        self.visit(|node| match node {
            Node::Tree(tree) => f(&tree.layer),
            Node::Document(file, id) => f(file.layer(id).ok_or_else(stale_error)?),
        })
    }

    pub fn with_layer_mut<R>(&self, f: impl FnOnce(&mut Layer<T>) -> PyResult<R>) -> PyResult<R> {
        self.visit_mut(|node| match node {
            NodeMut::Tree(tree) => f(&mut tree.layer),
            NodeMut::Document(file, id) => f(file.layer_mut(id).ok_or_else(stale_error)?),
        })
    }

    /// Run `detached` on the handle's own tree node, or `attached` with the
    /// document and id.
    pub fn with_tree<R>(
        &self,
        detached: impl FnOnce(&LayerTree<T>) -> PyResult<R>,
        attached: impl FnOnce(&LayeredFile<T>, LayerId) -> PyResult<R>,
    ) -> PyResult<R> {
        self.visit(|node| match node {
            Node::Tree(tree) => detached(tree),
            Node::Document(file, id) => attached(file, id),
        })
    }

    fn visit<R>(&self, f: impl FnOnce(Node<'_, T>) -> PyResult<R>) -> PyResult<R> {
        let target = self.target.lock().map_err(|_| lock_error())?;
        match &*target {
            LayerTarget::Detached { tree, .. } => f(Node::Tree(tree)),
            LayerTarget::Attached { document, id } => {
                read_document(document, |file| f(Node::Document(file, *id)))
            }
            LayerTarget::Nested { root, path } => {
                let root = root.lock().map_err(|_| lock_error())?;
                match &*root {
                    LayerTarget::Detached { tree, .. } => {
                        f(Node::Tree(tree.get(path).ok_or_else(stale_error)?))
                    }
                    _ => Err(stale_error()),
                }
            }
            LayerTarget::Moving => Err(stale_error()),
        }
    }

    fn visit_mut<R>(&self, f: impl FnOnce(NodeMut<'_, T>) -> PyResult<R>) -> PyResult<R> {
        let mut target = self.target.lock().map_err(|_| lock_error())?;
        match &mut *target {
            LayerTarget::Detached { tree, .. } => f(NodeMut::Tree(tree)),
            LayerTarget::Attached { document, id } => {
                let id = *id;
                write_document(document, |file| f(NodeMut::Document(file, id)))
            }
            LayerTarget::Nested { root, path } => {
                let mut root = root.lock().map_err(|_| lock_error())?;
                match &mut *root {
                    LayerTarget::Detached { tree, .. } => {
                        f(NodeMut::Tree(tree.get_mut(path).ok_or_else(stale_error)?))
                    }
                    _ => Err(stale_error()),
                }
            }
            LayerTarget::Moving => Err(stale_error()),
        }
    }

    /// Smart-object operations need the document holding the link records:
    /// `attached` gets the document and id, `detached` the home document and
    /// the layer itself.
    pub fn with_smart_object<R>(
        &self,
        attached: impl FnOnce(&mut LayeredFile<T>, LayerId) -> PyResult<R>,
        detached: impl FnOnce(&mut LayeredFile<T>, &mut Layer<T>) -> PyResult<R>,
    ) -> PyResult<R> {
        let mut target = self.target.lock().map_err(|_| lock_error())?;
        match &mut *target {
            LayerTarget::Attached { document, id } => {
                let id = *id;
                write_document(document, |file| attached(file, id))
            }
            LayerTarget::Detached { tree, home, .. } => {
                let home = home.clone().ok_or_else(no_home_error)?;
                write_document(&home, |file| detached(file, &mut tree.layer))
            }
            LayerTarget::Nested { root, path } => {
                let mut root = root.lock().map_err(|_| lock_error())?;
                match &mut *root {
                    LayerTarget::Detached { tree, home, .. } => {
                        let home = home.clone().ok_or_else(no_home_error)?;
                        let node = tree.get_mut(path).ok_or_else(stale_error)?;
                        write_document(&home, |file| detached(file, &mut node.layer))
                    }
                    _ => Err(stale_error()),
                }
            }
            LayerTarget::Moving => Err(stale_error()),
        }
    }

    /// The document and id of an attached layer.
    pub fn location(&self) -> PyResult<(Document<T>, LayerId)> {
        match self.place()? {
            Location::Attached(document, id) => Ok((document, id)),
            _ => Err(PyValueError::new_err(
                "this layer has not been added to a document",
            )),
        }
    }

    /// Insert a detached handle's tree into `document` under `parent` at
    /// `index` (real layers, bottom-to-top; `None` = top), re-pointing every
    /// nested handle registered on it. Smart-object links the tree needs are
    /// copied from its home document when that is a different document.
    pub fn attach_to(
        &self,
        document: &Document<T>,
        parent: Option<LayerId>,
        index: Option<usize>,
    ) -> PyResult<LayerId> {
        let mut target = self.target.lock().map_err(|_| lock_error())?;
        let (tree, members, home) = match std::mem::replace(&mut *target, LayerTarget::Moving) {
            LayerTarget::Detached {
                tree,
                members,
                home,
            } => (tree, members, home),
            other => {
                let message = match other {
                    LayerTarget::Nested { .. } => {
                        "this layer belongs to a group that is not in a document yet; add the group instead"
                    }
                    _ => "this layer already belongs to a document; use move_layer to move it",
                };
                *target = other;
                return Err(PyValueError::new_err(message));
            }
        };
        let paths: Vec<Vec<usize>> = members.iter().map(|member| member.path.clone()).collect();
        // The tree only moves into the document once every fallible step has
        // passed, so a failed call hands it back untouched.
        let mut pending = Some(tree);
        let result = write_document(document, |file| {
            if file.children(parent).is_none() {
                return Err(PyValueError::new_err("the target layer is not a group"));
            }
            let tree = pending.as_ref().expect("still pending");
            if let Some(home) = home.as_ref().filter(|home| !Arc::ptr_eq(home, document)) {
                import_links(file, home, tree)?;
            }
            let tree = pending.take().expect("taken once");
            let id = file
                .insert_layer_tree(parent, index, *tree)
                .map_err(psd_error)?;
            let ids = paths
                .iter()
                .map(|path| resolve_path(file, id, path))
                .collect::<Vec<_>>();
            Ok((id, ids))
        });
        match result {
            Ok((id, ids)) => {
                *target = LayerTarget::Attached {
                    document: Arc::clone(document),
                    id,
                };
                drop(target);
                for (member, child) in members.into_iter().zip(ids) {
                    if let (Some(member_target), Some(child)) = (member.target.upgrade(), child) {
                        if let Ok(mut state) = member_target.lock() {
                            *state = LayerTarget::Attached {
                                document: Arc::clone(document),
                                id: child,
                            };
                        }
                    }
                }
                Ok(id)
            }
            Err(error) => {
                *target = match pending {
                    Some(tree) => LayerTarget::Detached {
                        tree,
                        members,
                        home,
                    },
                    // Unreachable: insertion cannot fail after the move.
                    None => LayerTarget::Moving,
                };
                Err(error)
            }
        }
    }

    /// Replace an attached handle's target with a detached tree (after the
    /// caller removed the layer from `home`).
    pub fn detach_into(&self, tree: LayerTree<T>, home: Document<T>) -> PyResult<()> {
        let mut target = self.target.lock().map_err(|_| lock_error())?;
        *target = LayerTarget::Detached {
            tree: Box::new(tree),
            members: Vec::new(),
            home: Some(home),
        };
        Ok(())
    }

    /// Move a detached `child` handle into this detached (or nested) group
    /// at `index` among its children (bottom-to-top; `None` = top).
    pub fn adopt(&self, child: &LayerHandle<T>, index: Option<usize>) -> PyResult<()> {
        let (root, parent_path) = self.root_and_path()?;
        if Arc::ptr_eq(&root, &child.target) {
            return Err(PyValueError::new_err(
                "a group cannot be added to its own subtree",
            ));
        }
        let mut child_state = child.target.lock().map_err(|_| lock_error())?;
        let (child_tree, child_members, child_home) =
            match std::mem::replace(&mut *child_state, LayerTarget::Moving) {
                LayerTarget::Detached {
                    tree,
                    members,
                    home,
                } => (tree, members, home),
                other => {
                    *child_state = other;
                    return Err(PyValueError::new_err(
                        "only a layer that is not in a document or group can be added",
                    ));
                }
            };
        let restore = |state: &mut LayerTarget<T>, tree, members, home| {
            *state = LayerTarget::Detached {
                tree,
                members,
                home,
            };
        };
        let mut root_state = root.lock().map_err(|_| lock_error())?;
        let LayerTarget::Detached {
            tree,
            members,
            home,
        } = &mut *root_state
        else {
            restore(&mut child_state, child_tree, child_members, child_home);
            return Err(stale_error());
        };
        match tree.get_mut(&parent_path) {
            Some(parent) if parent.layer.group().is_some() => {}
            Some(_) => {
                restore(&mut child_state, child_tree, child_members, child_home);
                return Err(PyValueError::new_err("layers can only be added to groups"));
            }
            None => {
                restore(&mut child_state, child_tree, child_members, child_home);
                return Err(stale_error());
            }
        }
        // Keep the child's smart-object links reachable from the new root.
        match (home.as_ref(), child_home.as_ref()) {
            (None, Some(source)) => *home = Some(Arc::clone(source)),
            (Some(root_home), Some(source)) if !Arc::ptr_eq(root_home, source) => {
                let imported =
                    write_document(root_home, |file| import_links(file, source, &child_tree));
                if let Err(error) = imported {
                    restore(&mut child_state, child_tree, child_members, child_home);
                    return Err(error);
                }
            }
            _ => {}
        }
        let parent = tree.get_mut(&parent_path).expect("checked above");
        let position = index
            .unwrap_or(parent.children.len())
            .min(parent.children.len());
        parent.children.insert(position, *child_tree);
        // Registered handles of later siblings shift by one.
        members.retain(|member| member.target.strong_count() > 0);
        let depth = parent_path.len();
        for member in members.iter_mut() {
            if member.path.len() > depth
                && member.path[..depth] == parent_path[..]
                && member.path[depth] >= position
            {
                member.path[depth] += 1;
            }
        }
        let mut child_path = parent_path.clone();
        child_path.push(position);
        for member in child_members {
            let mut path = child_path.clone();
            path.extend(member.path);
            if let Some(member_target) = member.target.upgrade() {
                if let Ok(mut state) = member_target.lock() {
                    *state = LayerTarget::Nested {
                        root: Arc::clone(&root),
                        path: path.clone(),
                    };
                }
            }
            members.push(Member {
                path,
                target: member.target,
            });
        }
        members.push(Member {
            path: child_path.clone(),
            target: Arc::downgrade(&child.target),
        });
        drop(root_state);
        *child_state = LayerTarget::Nested {
            root,
            path: child_path,
        };
        Ok(())
    }

    /// A handle for the `index`-th child of this detached (or nested) group.
    pub fn nested_child(&self, index: usize) -> PyResult<LayerHandle<T>> {
        let (root, mut path) = self.root_and_path()?;
        path.push(index);
        let handle = LayerHandle {
            target: Arc::new(Mutex::new(LayerTarget::Nested {
                root: Arc::clone(&root),
                path: path.clone(),
            })),
        };
        let mut root_state = root.lock().map_err(|_| lock_error())?;
        let LayerTarget::Detached { tree, members, .. } = &mut *root_state else {
            return Err(stale_error());
        };
        if tree.get(&path).is_none() {
            return Err(stale_error());
        }
        members.retain(|member| member.target.strong_count() > 0);
        members.push(Member {
            path,
            target: Arc::downgrade(&handle.target),
        });
        Ok(handle)
    }

    /// Remove the `index`-th child of a detached (or nested) group and return
    /// a handle owning it; handles into the removed subtree follow it.
    pub fn take_nested_child(&self, index: usize) -> PyResult<LayerHandle<T>> {
        let (root, parent_path) = self.root_and_path()?;
        let mut root_state = root.lock().map_err(|_| lock_error())?;
        let LayerTarget::Detached {
            tree,
            members,
            home,
        } = &mut *root_state
        else {
            return Err(stale_error());
        };
        let home = home.clone();
        let parent = tree.get_mut(&parent_path).ok_or_else(stale_error)?;
        if index >= parent.children.len() {
            return Err(PyValueError::new_err("child index is out of range"));
        }
        let child = parent.children.remove(index);
        let depth = parent_path.len();
        let mut moved = Vec::new();
        let mut kept = Vec::new();
        for mut member in members.drain(..) {
            let below_parent = member.path.len() > depth && member.path[..depth] == parent_path[..];
            if below_parent && member.path[depth] == index {
                member.path.drain(..=depth);
                moved.push(member);
            } else {
                if below_parent && member.path[depth] > index {
                    member.path[depth] -= 1;
                }
                kept.push(member);
            }
        }
        *members = kept;
        drop(root_state);
        // A live Python object for the removed child becomes the new root, so
        // it can be added to a document again.
        let new_root: Target<T> = match moved
            .iter()
            .position(|member| member.path.is_empty() && member.target.strong_count() > 0)
        {
            Some(position) => moved
                .remove(position)
                .target
                .upgrade()
                .expect("checked strong count"),
            None => Arc::new(Mutex::new(LayerTarget::Moving)),
        };
        for member in &moved {
            if let Some(member_target) = member.target.upgrade() {
                if let Ok(mut state) = member_target.lock() {
                    *state = LayerTarget::Nested {
                        root: Arc::clone(&new_root),
                        path: member.path.clone(),
                    };
                }
            }
        }
        *new_root.lock().map_err(|_| lock_error())? = LayerTarget::Detached {
            tree: Box::new(child),
            members: moved,
            home,
        };
        Ok(LayerHandle { target: new_root })
    }

    /// The detached root target and this handle's path below it.
    fn root_and_path(&self) -> PyResult<(Target<T>, Vec<usize>)> {
        let target = self.target.lock().map_err(|_| lock_error())?;
        match &*target {
            LayerTarget::Detached { .. } => Ok((Arc::clone(&self.target), Vec::new())),
            LayerTarget::Nested { root, path } => Ok((Arc::clone(root), path.clone())),
            _ => Err(PyValueError::new_err(
                "this group belongs to a document; use its document operations",
            )),
        }
    }
}

/// The id reached from `root` by following child indices as
/// `insert_layer_tree` allocated them (bottom-to-top, no dividers).
fn resolve_path<T: BitDepth>(
    file: &LayeredFile<T>,
    root: LayerId,
    path: &[usize],
) -> Option<LayerId> {
    path.iter().try_fold(root, |id, &index| {
        file.children(Some(id))?.get(index).copied()
    })
}
