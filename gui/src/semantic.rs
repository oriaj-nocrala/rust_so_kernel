//! The semantic tree (backbone B5 of `docs/ux/backbones.md`, principle P7): what a window shows, as nodes a program other than its owner
//! can read — a test (`scripts/gui-e2e.sh`), later `agentd` and a screen reader.
//!
//! The schema is AccessKit's (roles, actions and the per-node properties used here keep its names), **mirrored, not depended on**: the
//! tree crosses the wire to a `no_std` compositor, and `accesskit::Node` has no encoding without `serde`; a host adapter can map these
//! one to one. A node's `id` is the client's, non-zero, stable across frames and unique in its surface; `parent` 0 marks a root. A
//! virtualized list sends only the rows it shows, each with `pos`/`set_size` (AccessKit's `position_in_set`/`size_of_set`, 1-based; 0
//! means not in a set).
//!
//! On the wire (see `protocol`): `surface.semantics_node(...)` once per node, parents before their children, applied at the surface's
//! next `commit` (like its buffer, so the tree matches the pixels); a commit after no `semantics_node` keeps the old tree.
//! `compositor.get_semantics(callback)` answers on that callback with `semantics_window` per window and its nodes, then `done`.

use alloc::string::String;

use crate::region::Rect;

/// Most nodes one surface may send per commit.
pub const MAX_NODES: usize = 4096;

/// AccessKit's roles that the widgets use; the wire carries the number. Unknown numbers decode as `Unknown`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Role {
    Unknown = 0,
    Window = 1,
    Pane = 2,
    Group = 3,
    Label = 4,
    Button = 5,
    TextInput = 6,
    ListBox = 7,
    ListBoxOption = 8,
    ScrollView = 9,
    Splitter = 10,
    Navigation = 11,
    Image = 12,
    Toolbar = 13,
    ColumnHeader = 14,
}

const ROLES: [(Role, &str); 15] = [
    (Role::Unknown, "Unknown"),
    (Role::Window, "Window"),
    (Role::Pane, "Pane"),
    (Role::Group, "Group"),
    (Role::Label, "Label"),
    (Role::Button, "Button"),
    (Role::TextInput, "TextInput"),
    (Role::ListBox, "ListBox"),
    (Role::ListBoxOption, "ListBoxOption"),
    (Role::ScrollView, "ScrollView"),
    (Role::Splitter, "Splitter"),
    (Role::Navigation, "Navigation"),
    (Role::Image, "Image"),
    (Role::Toolbar, "Toolbar"),
    (Role::ColumnHeader, "ColumnHeader"),
];

impl Role {
    pub fn from_u32(v: u32) -> Role {
        ROLES.get(v as usize).map_or(Role::Unknown, |r| r.0)
    }

    /// AccessKit's name for it.
    pub fn name(self) -> &'static str {
        ROLES[self as usize].1
    }
}

/// State bits (AccessKit's flags of the same names).
pub mod flag {
    pub const FOCUSED: u32 = 1;
    pub const SELECTED: u32 = 2;
    pub const DISABLED: u32 = 4;
    /// AccessKit's `is_hovered` is not used; `Expanded` is.
    pub const EXPANDED: u32 = 8;
    pub const READ_ONLY: u32 = 16;
    pub const NAMES: [(u32, &str); 5] =
        [(FOCUSED, "focused"), (SELECTED, "selected"), (DISABLED, "disabled"), (EXPANDED, "expanded"), (READ_ONLY, "read-only")];
}

/// What a node can be asked to do (AccessKit's `Action` names).
pub mod action {
    pub const CLICK: u32 = 1;
    pub const FOCUS: u32 = 2;
    pub const SET_VALUE: u32 = 4;
    pub const SCROLL_INTO_VIEW: u32 = 8;
    pub const SCROLL_DOWN: u32 = 16;
    pub const SCROLL_UP: u32 = 32;
    pub const NAMES: [(u32, &str); 6] = [
        (CLICK, "Click"),
        (FOCUS, "Focus"),
        (SET_VALUE, "SetValue"),
        (SCROLL_INTO_VIEW, "ScrollIntoView"),
        (SCROLL_DOWN, "ScrollDown"),
        (SCROLL_UP, "ScrollUp"),
    ];
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub id: u32,
    pub parent: u32,
    pub role: Role,
    /// [`flag`] bits.
    pub flags: u32,
    /// [`action`] bits.
    pub actions: u32,
    /// In the surface's pixels.
    pub bounds: Rect,
    pub pos: u32,
    pub set_size: u32,
    pub name: String,
    pub value: String,
}

impl Node {
    pub fn new(id: u32, parent: u32, role: Role, bounds: Rect) -> Node {
        Node { id, parent, role, flags: 0, actions: 0, bounds, pos: 0, set_size: 0, name: String::new(), value: String::new() }
    }

    pub fn has(&self, f: u32) -> bool {
        self.flags & f != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_number_and_name_one_to_one() {
        for (i, &(r, name)) in ROLES.iter().enumerate() {
            assert_eq!(r as usize, i, "{name}");
            assert_eq!(Role::from_u32(i as u32), r);
            assert_eq!(r.name(), name);
        }
        assert_eq!(Role::from_u32(ROLES.len() as u32), Role::Unknown);
        assert_eq!(Role::from_u32(u32::MAX), Role::Unknown);
    }

    /// The mirror keeps AccessKit's names (the dev-dependency is only for this).
    #[test]
    fn names_are_accesskits() {
        extern crate std;
        use accesskit::{Action as A, Role as K};
        use std::format;
        let theirs = [
            K::Unknown,
            K::Window,
            K::Pane,
            K::Group,
            K::Label,
            K::Button,
            K::TextInput,
            K::ListBox,
            K::ListBoxOption,
            K::ScrollView,
            K::Splitter,
            K::Navigation,
            K::Image,
            K::Toolbar,
            K::ColumnHeader,
        ];
        assert_eq!(theirs.len(), ROLES.len());
        for (k, (_, name)) in theirs.iter().zip(ROLES) {
            assert_eq!(&format!("{:?}", k), name);
        }
        let theirs = [A::Click, A::Focus, A::SetValue, A::ScrollIntoView, A::ScrollDown, A::ScrollUp];
        assert_eq!(theirs.len(), action::NAMES.len());
        for (k, (_, name)) in theirs.iter().zip(action::NAMES) {
            assert_eq!(&format!("{:?}", k), name);
        }
    }
}
