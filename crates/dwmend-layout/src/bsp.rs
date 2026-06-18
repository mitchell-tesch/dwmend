//! Binary space partition (BSP) layout — Hyprland's "dwindle" style.
//!
//! ## Model
//!
//! The tree is held in a single `Vec<Node>` arena. Each node is either:
//! * **Leaf**: a single window (`T`), keeping its parent index.
//! * **Stack**: a stable rectangle shared by N windows; only the focused
//!   member is visible. New windows opened while the focus is on a stack
//!   join the stack instead of splitting it. Toggling stack mode off
//!   either reverts to a Leaf (size 1) or expands the children into a
//!   right-leaning chain of vertical splits (size > 1).
//! * **Split**: two children, an axis (`Vertical` or `Horizontal`), and a
//!   ratio in `[0.0, 1.0]` controlling where the split sits. Children may be
//!   leaves, stacks, or further splits.
//!
//! ## Invariants
//! * Exactly one `root_idx` if the tree is non-empty.
//! * Every non-root node has a `parent` field that points back at its
//!   `Split` parent.
//! * Every `Split` has exactly two children; we collapse splits with a
//!   missing child during removal so the invariant is never observably violated.
//! * Every `Stack` has at least one child while it exists; removing the
//!   last child collapses the stack the same way removing a leaf does.
//!
//! ## Insertion policy (the "dwindle" feel)
//!
//! New windows split the currently-focused leaf, alternating axes by depth
//! so wide windows tend to get vertical splits and tall ones horizontal —
//! this is what makes BSP look reasonable without any hand-tuning. The
//! exception: if the focused node is a `Stack`, new windows are appended
//! to the stack instead.

use crate::rect::{Axis, Direction, Rect};
use std::collections::HashMap;
use std::hash::Hash;

/// Index into the arena. We use `u32` to keep `Node` compact; in practice you
/// won't have more than 2^32 windows on a workspace.
type NodeIdx = u32;

const NONE: NodeIdx = u32::MAX;

/// How `insert` chooses the split axis when a new window enters the tree.
///
/// * **Dwindle** \u2014 default i3 / Hyprland behaviour. The split axis is
///   driven by the focused leaf's aspect ratio: wider rects split
///   vertically, taller rects split horizontally. Visually this produces
///   a chain of progressively smaller halves down the right (or bottom)
///   edge, hence "dwindle".
/// * **Spiral** \u2014 fixed alternation: each new split flips the axis of
///   the parent split. From a single window the first split is vertical;
///   the next split (added on either side) is horizontal; and so on.
///   This yields a spiral pattern that winds inward and is the canonical
///   alternative offered by every other BSP tiler.
///
/// Both modes share the same insertion mechanics \u2014 only the `axis`
/// decision differs \u2014 so users can flip per-workspace at runtime
/// without a tree rebuild. The new policy applies to subsequent inserts
/// only; existing splits keep their axes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LayoutMode {
    #[default]
    Dwindle,
    Spiral,
}

#[derive(Debug, Clone)]
enum NodeKind<T> {
    Leaf {
        value: T,
    },
    Stack {
        children: Vec<T>,
        focused: usize,
    },
    Split {
        axis: Axis,
        ratio: f32,
        left: NodeIdx,
        right: NodeIdx,
    },
}

#[derive(Debug, Clone)]
struct Node<T> {
    parent: NodeIdx,
    kind: NodeKind<T>,
}

/// BSP tree. `T` is the per-leaf payload (typically a window identifier).
#[derive(Debug, Clone)]
pub struct BspTree<T: Copy + Eq + Hash> {
    nodes: Vec<Node<T>>,
    free: Vec<NodeIdx>,
    root: NodeIdx,
    /// Index of the leaf that should be split next (the "focused" leaf).
    focus: NodeIdx,
    /// `leaf` → idx lookup for O(1) finds.
    index_of: HashMap<T, NodeIdx>,
    /// How `insert` chooses the split axis. See [`LayoutMode`] for the
    /// semantics; runtime swaps via [`Self::set_layout_mode`].
    mode: LayoutMode,
}

impl<T: Copy + Eq + Hash> Default for BspTree<T> {
    fn default() -> Self {
        Self {
            nodes: Vec::new(),
            free: Vec::new(),
            root: NONE,
            focus: NONE,
            index_of: HashMap::new(),
            mode: LayoutMode::default(),
        }
    }
}

impl<T: Copy + Eq + Hash> BspTree<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read the current layout mode \u2014 see [`LayoutMode`].
    pub fn layout_mode(&self) -> LayoutMode {
        self.mode
    }

    /// Replace the layout mode. Affects future inserts only; existing
    /// splits retain their axes so toggling never reflows the workspace
    /// (matching i3's `layout` semantics: it's a "what comes next?"
    /// switch, not a destructive remap).
    pub fn set_layout_mode(&mut self, mode: LayoutMode) {
        self.mode = mode;
    }

    pub fn is_empty(&self) -> bool {
        self.root == NONE
    }

    pub fn len(&self) -> usize {
        self.index_of.len()
    }

    /// Currently-focused leaf (or the focused member of the focused stack).
    pub fn focused(&self) -> Option<T> {
        if self.focus == NONE {
            return None;
        }
        match &self.nodes[self.focus as usize].kind {
            NodeKind::Leaf { value } => Some(*value),
            NodeKind::Stack { children, focused } => children.get(*focused).copied(),
            NodeKind::Split { .. } => None,
        }
    }

    /// Set focus to a particular window. If `t` is a stack member, also
    /// updates the stack's internal focused index so the next render shows
    /// `t`. No-op if `t` is not in the tree.
    pub fn focus(&mut self, t: T) {
        let Some(&idx) = self.index_of.get(&t) else {
            return;
        };
        self.focus = idx;
        if let NodeKind::Stack { children, focused } = &mut self.nodes[idx as usize].kind
            && let Some(pos) = children.iter().position(|c| *c == t)
        {
            *focused = pos;
        }
    }

    /// Whether the tree currently contains `t`.
    pub fn contains(&self, t: &T) -> bool {
        self.index_of.contains_key(t)
    }

    /// If `t` is part of a stack, return `(its 0-based position, stack size)`.
    /// Returns `None` for plain leaves.
    pub fn stack_position(&self, t: T) -> Option<(usize, usize)> {
        let &idx = self.index_of.get(&t)?;
        if let NodeKind::Stack { children, .. } = &self.nodes[idx as usize].kind {
            children
                .iter()
                .position(|c| *c == t)
                .map(|p| (p, children.len()))
        } else {
            None
        }
    }

    /// If the currently-focused node is a stack, return
    /// `(focused_index, stack_size)`. Returns `None` if no focus or focus
    /// is a plain leaf. Used by the bar to render a "2/4" indicator.
    pub fn focused_stack_info(&self) -> Option<(usize, usize)> {
        if self.focus == NONE {
            return None;
        }
        if let NodeKind::Stack { children, focused } = &self.nodes[self.focus as usize].kind {
            Some((*focused, children.len()))
        } else {
            None
        }
    }

    /// Every window in the tree (visible + hidden stack members).
    pub fn windows(&self) -> Vec<T> {
        let mut out = Vec::with_capacity(self.index_of.len());
        if self.root != NONE {
            self.collect_all(self.root, &mut out);
        }
        out
    }

    /// Members of any stack that are NOT the currently-focused stack member,
    /// i.e. the windows that should be hidden by the caller.
    pub fn hidden_in_stacks(&self) -> Vec<T> {
        let mut out = Vec::new();
        if self.root != NONE {
            self.collect_hidden(self.root, &mut out);
        }
        out
    }

    fn collect_all(&self, idx: NodeIdx, out: &mut Vec<T>) {
        match &self.nodes[idx as usize].kind {
            NodeKind::Leaf { value } => out.push(*value),
            NodeKind::Stack { children, .. } => out.extend(children.iter().copied()),
            NodeKind::Split { left, right, .. } => {
                self.collect_all(*left, out);
                self.collect_all(*right, out);
            }
        }
    }

    fn collect_hidden(&self, idx: NodeIdx, out: &mut Vec<T>) {
        match &self.nodes[idx as usize].kind {
            NodeKind::Stack { children, focused } => {
                for (i, c) in children.iter().enumerate() {
                    if i != *focused {
                        out.push(*c);
                    }
                }
            }
            NodeKind::Split { left, right, .. } => {
                self.collect_hidden(*left, out);
                self.collect_hidden(*right, out);
            }
            NodeKind::Leaf { .. } => {}
        }
    }

    // ---- arena helpers ---------------------------------------------------

    fn alloc(&mut self, n: Node<T>) -> NodeIdx {
        if let Some(idx) = self.free.pop() {
            self.nodes[idx as usize] = n;
            idx
        } else {
            self.nodes.push(n);
            (self.nodes.len() - 1) as NodeIdx
        }
    }

    fn free(&mut self, idx: NodeIdx) {
        // Mark the slot as free. We don't overwrite the old contents — they
        // remain inert (no `index_of` entry points here anymore) until
        // `alloc` reuses the slot and stomps the value with the next `Node`.
        // This avoids needing `T: Default` or unsafe placeholder values.
        self.free.push(idx);
    }

    // ---- structural ops --------------------------------------------------

    /// Insert `t` as a new leaf.
    ///
    /// If the tree is empty, `t` becomes the root.
    /// If the currently-focused node is a `Stack`, `t` is appended to the
    /// stack and becomes its focused member — no split.
    /// Otherwise, split the currently-focused leaf, choosing the split axis
    /// based on the rectangle's aspect ratio so wider leaves are split
    /// vertically and taller ones horizontally.
    pub fn insert(&mut self, t: T, work_area: Rect) {
        if self.index_of.contains_key(&t) {
            return;
        }

        if self.root == NONE {
            let idx = self.alloc(Node {
                parent: NONE,
                kind: NodeKind::Leaf { value: t },
            });
            self.root = idx;
            self.focus = idx;
            self.index_of.insert(t, idx);
            return;
        }

        // If the focused node is a Stack, append rather than split.
        let focus_idx = if self.focus == NONE {
            self.first_leaf()
        } else {
            self.focus
        };
        if matches!(self.nodes[focus_idx as usize].kind, NodeKind::Stack { .. }) {
            if let NodeKind::Stack { children, focused } = &mut self.nodes[focus_idx as usize].kind
            {
                children.push(t);
                *focused = children.len() - 1;
            }
            self.index_of.insert(t, focus_idx);
            return;
        }

        // We need to split the focused leaf. Compute its rect to pick a
        // sensible default axis.
        let focused = focus_idx;
        let focused_rect = self.leaf_rect(focused, work_area).unwrap_or(work_area);
        let axis = match self.mode {
            LayoutMode::Dwindle => {
                // Dwindle: aspect-ratio driven. Wide rects split into
                // left|right, tall rects into top/bottom.
                if focused_rect.w >= focused_rect.h {
                    Axis::Vertical
                } else {
                    Axis::Horizontal
                }
            }
            LayoutMode::Spiral => {
                // Spiral: alternate from the parent split's axis. The
                // first split (focused leaf is the root) has no parent,
                // so we fall back to aspect-ratio so the very first
                // window pair still looks reasonable.
                let parent_idx = self.nodes[focused as usize].parent;
                if parent_idx == NONE {
                    if focused_rect.w >= focused_rect.h {
                        Axis::Vertical
                    } else {
                        Axis::Horizontal
                    }
                } else if let NodeKind::Split { axis, .. } = self.nodes[parent_idx as usize].kind {
                    axis.flip()
                } else {
                    // Parent is a Stack \u2014 we already early-returned for
                    // stack focuses above; this branch is unreachable in
                    // practice but a safe fallback keeps the match total.
                    if focused_rect.w >= focused_rect.h {
                        Axis::Vertical
                    } else {
                        Axis::Horizontal
                    }
                }
            }
        };

        // Materialise the new leaf.
        let new_leaf = self.alloc(Node {
            parent: NONE, // patched below
            kind: NodeKind::Leaf { value: t },
        });

        // Materialise the new Split node that replaces `focused` in place.
        // We do this by allocating a *new* split, re-pointing the focused
        // node's parent to it, and re-parenting both leaves underneath.
        let old_parent = self.nodes[focused as usize].parent;
        let split_idx = self.alloc(Node {
            parent: old_parent,
            kind: NodeKind::Split {
                axis,
                ratio: 0.5,
                left: focused,
                right: new_leaf,
            },
        });

        // Re-parent the existing leaf and the new leaf.
        self.nodes[focused as usize].parent = split_idx;
        self.nodes[new_leaf as usize].parent = split_idx;

        // Hook the split into its former parent (or set as root).
        if old_parent == NONE {
            self.root = split_idx;
        } else {
            self.replace_child(old_parent, focused, split_idx);
        }

        self.index_of.insert(t, new_leaf);
        self.focus = new_leaf;
    }

    /// Remove `t`, collapsing its parent split if it was the last sibling.
    /// Returns `true` if removed.
    ///
    /// If `t` is a member of a stack, only that one member is removed; the
    /// stack node persists as long as it still has at least one child.
    pub fn remove(&mut self, t: T) -> bool {
        let Some(idx) = self.index_of.remove(&t) else {
            return false;
        };

        // Stack-member fast path: remove from children. If at least one
        // child remains the stack node persists; otherwise we fall through
        // to the leaf-removal path which frees the (now-empty) stack slot
        // and promotes its sibling.
        if let NodeKind::Stack { children, focused } = &mut self.nodes[idx as usize].kind
            && let Some(pos) = children.iter().position(|c| *c == t)
        {
            children.remove(pos);
            if !children.is_empty() {
                if *focused >= children.len() {
                    *focused = children.len() - 1;
                }
                // self.focus still points at this stack node, which still
                // has at least one valid child.
                return true;
            }
        }

        // Leaf removal (or now-empty Stack collapses identically).
        let parent = self.nodes[idx as usize].parent;

        if parent == NONE {
            // Removing the root — tree becomes empty.
            self.free(idx);
            self.root = NONE;
            self.focus = NONE;
            return true;
        }

        // Promote the sibling into the parent's slot.
        let sibling = self.sibling_of(parent, idx);
        let grandparent = self.nodes[parent as usize].parent;

        // Re-parent the sibling.
        self.nodes[sibling as usize].parent = grandparent;

        if grandparent == NONE {
            self.root = sibling;
        } else {
            self.replace_child(grandparent, parent, sibling);
        }

        self.free(idx);
        self.free(parent);

        // Move focus to the promoted sibling's leftmost leaf-like node if
        // the removed leaf was focused.
        if self.focus == idx || self.focus == parent {
            self.focus = self.leftmost_leaf(sibling);
        }
        true
    }

    /// Swap two windows without changing the tree shape. Handles plain
    /// leaves and stack members in any combination. When swapping a leaf
    /// with a stack member, the leaf-value joins the stack at the swapped
    /// position and the stack-value pops out as a standalone leaf.
    pub fn swap(&mut self, a: T, b: T) {
        if a == b {
            return;
        }
        let (Some(&ai), Some(&bi)) = (self.index_of.get(&a), self.index_of.get(&b)) else {
            return;
        };
        let Some(a_pos) = self.locate_in_node(ai, a) else {
            return;
        };
        let Some(b_pos) = self.locate_in_node(bi, b) else {
            return;
        };
        // Write each new value into its slot; update the lookup map.
        self.set_value_at(ai, a_pos, b);
        self.set_value_at(bi, b_pos, a);
        self.index_of.insert(a, bi);
        self.index_of.insert(b, ai);
    }

    /// Returns `Some(0)` for a matching Leaf, `Some(pos)` for a Stack member,
    /// `None` for a Split (should never happen since index_of points at
    /// payload-bearing nodes).
    fn locate_in_node(&self, idx: NodeIdx, t: T) -> Option<usize> {
        match &self.nodes[idx as usize].kind {
            NodeKind::Leaf { value } => (*value == t).then_some(0),
            NodeKind::Stack { children, .. } => children.iter().position(|c| *c == t),
            NodeKind::Split { .. } => None,
        }
    }

    fn set_value_at(&mut self, idx: NodeIdx, pos: usize, value: T) {
        match &mut self.nodes[idx as usize].kind {
            NodeKind::Leaf { value: v } => *v = value,
            NodeKind::Stack { children, .. } => {
                if pos < children.len() {
                    children[pos] = value;
                }
            }
            NodeKind::Split { .. } => {}
        }
    }

    /// Toggle stack mode on the currently-focused node.
    ///
    /// This is the "smart" toggle bound to the default `Alt+G` keybinding.
    /// The goal is for the user to see something visibly happen on the
    /// first press, not just a "[1/1]" badge on the title.
    ///
    /// Rules (first match wins):
    ///
    /// * **Focused is a Stack with >1 members** → expand into a chain of
    ///   vertical splits so every member becomes a standalone tile.
    /// * **Focused is a 1-member Stack** → degrade back to a plain Leaf.
    /// * **Focused is a Leaf with a Leaf sibling** → replace the parent
    ///   Split with a single Stack containing both leaves. Focus stays on
    ///   the original window; the sibling joins as the second member.
    /// * **Focused is a Leaf with a Stack sibling** → push focused into
    ///   the sibling stack, then replace the parent Split with the (now
    ///   larger) stack. Focus stays on the original window.
    /// * **Focused is a Leaf with a Split sibling** → the sibling is a
    ///   subtree; fall back to converting the focused leaf alone into a
    ///   1-member Stack. Visually subtle but semantically meaningful:
    ///   subsequent inserts on this workspace pile into the stack.
    /// * **Focused is the root Leaf** (no sibling) → same 1-Stack fallback.
    /// * **Focused is a Split** → no-op (shouldn't normally happen since
    ///   `self.focus` only points at payload-bearing nodes).
    ///
    /// Returns `true` if the tree shape changed.
    pub fn toggle_stack_focused(&mut self) -> bool {
        if self.focus == NONE {
            return false;
        }
        let focus_idx = self.focus;
        let kind = self.nodes[focus_idx as usize].kind.clone();
        match kind {
            NodeKind::Stack { children, focused } => {
                if children.len() == 1 {
                    self.nodes[focus_idx as usize].kind = NodeKind::Leaf { value: children[0] };
                    true
                } else {
                    self.expand_stack(focus_idx, children, focused);
                    true
                }
            }
            NodeKind::Leaf { value } => {
                // Try to merge with the immediate sibling first — that's
                // what makes a single press of `Alt+G` actually produce a
                // visible change in the common 2-tile case.
                let parent = self.nodes[focus_idx as usize].parent;
                if parent != NONE {
                    let sibling_idx = self.sibling_of(parent, focus_idx);
                    let sibling_kind = self.nodes[sibling_idx as usize].kind.clone();
                    match sibling_kind {
                        NodeKind::Leaf {
                            value: sibling_value,
                        } => {
                            self.merge_two_leaves_into_stack(
                                parent,
                                focus_idx,
                                sibling_idx,
                                value,
                                sibling_value,
                            );
                            return true;
                        }
                        NodeKind::Stack {
                            children: sibling_children,
                            focused: _,
                        } => {
                            self.merge_leaf_into_sibling_stack(
                                parent,
                                focus_idx,
                                sibling_idx,
                                value,
                                sibling_children,
                            );
                            return true;
                        }
                        NodeKind::Split { .. } => {
                            // Sibling is a subtree; fall through to the
                            // single-leaf 1-stack toggle.
                        }
                    }
                }
                // Fallback: turn this leaf into a 1-member stack. Visually
                // identical until the user opens another window on the
                // same workspace, at which point it joins the stack.
                self.nodes[focus_idx as usize].kind = NodeKind::Stack {
                    children: vec![value],
                    focused: 0,
                };
                true
            }
            NodeKind::Split { .. } => false,
        }
    }

    /// Pull the neighbor in `dir` (relative to the focused tile) into the
    /// focused window's tile, forming or extending a Stack at the focused
    /// position. The original focused tile becomes a Stack (if it wasn't
    /// already) and the neighbor is appended; focus stays on the original
    /// window.
    ///
    /// Returns `true` if a neighbor was found and merged.
    pub fn stack_swallow_dir(&mut self, dir: Direction, work_area: Rect) -> bool {
        let Some(focused_value) = self.focused() else {
            return false;
        };
        // Find the neighbor leaf using the same geometric search the
        // focus/move commands use, so behavior is consistent across all
        // direction-based ops.
        let positions = self.compute(work_area, 0);
        let Some(src_rect) = positions
            .iter()
            .find_map(|(x, r)| (*x == focused_value).then_some(*r))
        else {
            return false;
        };
        let Some(neighbor) = nearest_in_direction(&positions, focused_value, src_rect, dir) else {
            return false;
        };
        // Pull `neighbor` out of the tree, then append it to the focused
        // node (converting that node into a Stack if necessary).
        if !self.remove(neighbor) {
            return false;
        }
        self.append_to_focused_as_stack(neighbor)
    }

    /// Extract the currently-focused stack member as a standalone Leaf,
    /// re-inserted next to the stack via a new vertical Split. Inverse of
    /// `stack_swallow_dir`. Returns `true` if focus was on a stack with
    /// >1 members.
    ///
    /// If the stack ends up with a single child after the pop, it
    /// auto-degrades back to a plain Leaf — so the user doesn't see a
    /// stack indicator on what is really just one window.
    pub fn stack_pop_focused(&mut self) -> bool {
        if self.focus == NONE {
            return false;
        }
        let stack_idx = self.focus;
        let popped = if let NodeKind::Stack { children, focused } =
            &mut self.nodes[stack_idx as usize].kind
        {
            if children.len() <= 1 {
                return false;
            }
            let v = children.remove(*focused);
            if *focused >= children.len() {
                *focused = children.len() - 1;
            }
            v
        } else {
            return false;
        };

        // If only one child is left, degrade the stack back to a plain Leaf.
        let solo = if let NodeKind::Stack { children, .. } = &self.nodes[stack_idx as usize].kind {
            (children.len() == 1).then_some(children[0])
        } else {
            None
        };
        if let Some(v) = solo {
            self.nodes[stack_idx as usize].kind = NodeKind::Leaf { value: v };
        }

        // Allocate a new Leaf for the popped window and splice a new
        // vertical Split (parent_old_parent → new_split → [stack, new_leaf])
        // into the stack's old slot.
        let stack_parent = self.nodes[stack_idx as usize].parent;
        let new_leaf = self.alloc(Node {
            parent: NONE,
            kind: NodeKind::Leaf { value: popped },
        });
        let new_split = self.alloc(Node {
            parent: stack_parent,
            kind: NodeKind::Split {
                axis: Axis::Vertical,
                ratio: 0.5,
                left: stack_idx,
                right: new_leaf,
            },
        });
        self.nodes[stack_idx as usize].parent = new_split;
        self.nodes[new_leaf as usize].parent = new_split;
        if stack_parent == NONE {
            self.root = new_split;
        } else {
            self.replace_child(stack_parent, stack_idx, new_split);
        }

        self.index_of.insert(popped, new_leaf);
        self.focus = new_leaf;
        true
    }

    /// Replace `parent` Split with a new Stack node holding `[a, b]`.
    /// Focus moves to the new Stack with `a` as the focused member.
    /// Both `focus_idx` and `sibling_idx` are freed.
    fn merge_two_leaves_into_stack(
        &mut self,
        parent: NodeIdx,
        focus_idx: NodeIdx,
        sibling_idx: NodeIdx,
        a: T,
        b: T,
    ) {
        let grandparent = self.nodes[parent as usize].parent;
        let stack = self.alloc(Node {
            parent: grandparent,
            kind: NodeKind::Stack {
                children: vec![a, b],
                focused: 0,
            },
        });
        if grandparent == NONE {
            self.root = stack;
        } else {
            self.replace_child(grandparent, parent, stack);
        }
        self.free(focus_idx);
        self.free(sibling_idx);
        self.free(parent);
        self.index_of.insert(a, stack);
        self.index_of.insert(b, stack);
        self.focus = stack;
    }

    /// Replace `parent` Split with a Stack containing all of the sibling
    /// stack's children followed by `value` as the last (and focused)
    /// member. Then free both focus and sibling slots and the parent
    /// split.
    fn merge_leaf_into_sibling_stack(
        &mut self,
        parent: NodeIdx,
        focus_idx: NodeIdx,
        sibling_idx: NodeIdx,
        value: T,
        mut sibling_children: Vec<T>,
    ) {
        let grandparent = self.nodes[parent as usize].parent;
        sibling_children.push(value);
        let new_focused = sibling_children.len() - 1;
        let stack = self.alloc(Node {
            parent: grandparent,
            kind: NodeKind::Stack {
                children: sibling_children.clone(),
                focused: new_focused,
            },
        });
        if grandparent == NONE {
            self.root = stack;
        } else {
            self.replace_child(grandparent, parent, stack);
        }
        self.free(focus_idx);
        self.free(sibling_idx);
        self.free(parent);
        for &c in &sibling_children {
            self.index_of.insert(c, stack);
        }
        self.focus = stack;
    }

    /// Append `value` to the focused node, converting it from a Leaf into
    /// a 2-Stack if necessary. The newly-appended value becomes the
    /// focused stack member (so the user sees the swallowed window).
    /// Returns `true` on success.
    fn append_to_focused_as_stack(&mut self, value: T) -> bool {
        if self.focus == NONE {
            return false;
        }
        let focus_idx = self.focus;
        let kind = self.nodes[focus_idx as usize].kind.clone();
        match kind {
            NodeKind::Leaf { value: existing } => {
                self.nodes[focus_idx as usize].kind = NodeKind::Stack {
                    children: vec![existing, value],
                    focused: 1,
                };
                self.index_of.insert(value, focus_idx);
                // `existing` is already in index_of pointing at this node.
                true
            }
            NodeKind::Stack { .. } => {
                if let NodeKind::Stack { children, focused } =
                    &mut self.nodes[focus_idx as usize].kind
                {
                    children.push(value);
                    *focused = children.len() - 1;
                }
                self.index_of.insert(value, focus_idx);
                true
            }
            NodeKind::Split { .. } => false,
        }
    }

    /// Cycle focus forward within the currently-focused stack. Returns the
    /// new focused member, or `None` if the focus isn't on a stack.
    pub fn focus_stack_next(&mut self) -> Option<T> {
        if self.focus == NONE {
            return None;
        }
        if let NodeKind::Stack { children, focused } = &mut self.nodes[self.focus as usize].kind {
            if children.is_empty() {
                return None;
            }
            *focused = (*focused + 1) % children.len();
            return children.get(*focused).copied();
        }
        None
    }

    /// Cycle focus backward within the currently-focused stack.
    pub fn focus_stack_prev(&mut self) -> Option<T> {
        if self.focus == NONE {
            return None;
        }
        if let NodeKind::Stack { children, focused } = &mut self.nodes[self.focus as usize].kind {
            if children.is_empty() {
                return None;
            }
            *focused = if *focused == 0 {
                children.len() - 1
            } else {
                *focused - 1
            };
            return children.get(*focused).copied();
        }
        None
    }

    /// Expand a multi-child stack node at `stack_idx` into a right-leaning
    /// chain of vertical splits, one Leaf per former stack member. Focus
    /// lands on the previously-focused stack member's new Leaf.
    fn expand_stack(&mut self, stack_idx: NodeIdx, children: Vec<T>, focused: usize) {
        let n = children.len();
        debug_assert!(n > 1, "expand_stack called with {n} children");

        // Allocate fresh Leaf nodes for every former stack member.
        let mut leaf_idxs: Vec<NodeIdx> = Vec::with_capacity(n);
        for &c in &children {
            let idx = self.alloc(Node {
                parent: NONE, // patched as we build the split chain below
                kind: NodeKind::Leaf { value: c },
            });
            leaf_idxs.push(idx);
        }

        // Build the chain bottom-up: [c0, [c1, [c2, c3]]].
        let mut right = leaf_idxs[n - 1];
        for i in (0..n - 1).rev() {
            let left = leaf_idxs[i];
            let split = self.alloc(Node {
                parent: NONE,
                kind: NodeKind::Split {
                    axis: Axis::Vertical,
                    ratio: 0.5,
                    left,
                    right,
                },
            });
            self.nodes[left as usize].parent = split;
            self.nodes[right as usize].parent = split;
            right = split;
        }

        // Splice the new sub-tree into the slot the stack used to occupy.
        let parent = self.nodes[stack_idx as usize].parent;
        self.nodes[right as usize].parent = parent;
        if parent == NONE {
            self.root = right;
        } else {
            self.replace_child(parent, stack_idx, right);
        }
        self.free(stack_idx);

        // Repoint index_of to the new Leaf for each former stack member.
        for (i, &c) in children.iter().enumerate() {
            self.index_of.insert(c, leaf_idxs[i]);
        }

        // Land focus on the former focused stack member's new Leaf.
        self.focus = leaf_idxs[focused.min(n - 1)];
    }

    /// Move `t` toward `dir`: find the neighbour leaf in that direction and
    /// swap with it. No-op if there is no neighbour.
    pub fn move_in_direction(&mut self, t: T, dir: Direction, work_area: Rect) -> bool {
        let positions = self.compute(work_area, 0);
        let Some(src_rect) = positions.iter().find_map(|(x, r)| (*x == t).then_some(*r)) else {
            return false;
        };
        let Some(target) = nearest_in_direction(&positions, t, src_rect, dir) else {
            return false;
        };
        self.swap(t, target);
        true
    }

    /// Move focus toward `dir`. Returns the new focused leaf.
    pub fn focus_in_direction(&mut self, dir: Direction, work_area: Rect) -> Option<T> {
        let cur = self.focused()?;
        let positions = self.compute(work_area, 0);
        let src_rect = positions
            .iter()
            .find_map(|(x, r)| (*x == cur).then_some(*r))?;
        let target = nearest_in_direction(&positions, cur, src_rect, dir)?;
        self.focus(target);
        Some(target)
    }

    /// Adjust the closest split on `t`'s ancestor chain by `delta_px`.
    ///
    /// Semantic: `dir` is the direction to *push the split*. So:
    /// * `resize right +N` moves the split rightward by N px (ratio increases)
    /// * `resize left  +N` moves the split leftward  by N px (ratio decreases)
    ///
    /// Whether the focused tile grows or shrinks depends on which side of the
    /// split it sits on, which is exactly the i3/Sway/Hyprland convention.
    pub fn resize(&mut self, t: T, dir: Direction, delta_px: i32, work_area: Rect) {
        let Some(&leaf) = self.index_of.get(&t) else {
            return;
        };
        let want_axis = Axis::from_direction(dir);

        // Walk up the ancestor chain until we find a Split of the right axis.
        let mut cur = leaf;
        loop {
            let parent = self.nodes[cur as usize].parent;
            if parent == NONE {
                return; // no matching split on the path to the root
            }
            if let NodeKind::Split { axis, .. } = self.nodes[parent as usize].kind
                && axis == want_axis
            {
                cur = parent;
                break;
            }
            cur = parent;
        }

        // `cur` now indexes a Split of want_axis.
        let total = match want_axis {
            Axis::Vertical => work_area_in_subtree(self, cur, work_area).w,
            Axis::Horizontal => work_area_in_subtree(self, cur, work_area).h,
        };
        if total <= 0 {
            return;
        }
        let delta_ratio = delta_px as f32 / total as f32;

        // `dir` points the direction the split itself moves. Right/Down means
        // bigger ratio (split sits further from origin); Left/Up means smaller.
        let sign = match dir {
            Direction::Right | Direction::Down => 1.0,
            Direction::Left | Direction::Up => -1.0,
        };

        let NodeKind::Split { ratio, .. } = &mut self.nodes[cur as usize].kind else {
            unreachable!()
        };
        *ratio = (*ratio + sign * delta_ratio).clamp(0.1, 0.9);
    }

    /// Compute every leaf's screen-space rect within `work_area`, inset by
    /// `gap` pixels on each side of every leaf for visual padding.
    pub fn compute(&self, work_area: Rect, gap: i32) -> Vec<(T, Rect)> {
        let mut out = Vec::with_capacity(self.len());
        if self.root != NONE {
            self.layout_node(self.root, work_area, gap, &mut out);
        }
        out
    }

    // ---- internal traversal ---------------------------------------------

    fn layout_node(&self, idx: NodeIdx, area: Rect, gap: i32, out: &mut Vec<(T, Rect)>) {
        match &self.nodes[idx as usize].kind {
            NodeKind::Leaf { value } => {
                out.push((*value, area.inset(gap)));
            }
            NodeKind::Stack { children, focused } => {
                // Only the focused stack member is emitted — the others
                // share the same rect but are hidden by the host (see
                // `hidden_in_stacks`). This keeps drag-into-stack swaps
                // working without painting overlapped windows.
                if let Some(v) = children.get(*focused) {
                    out.push((*v, area.inset(gap)));
                }
            }
            NodeKind::Split {
                axis,
                ratio,
                left,
                right,
            } => {
                let (l, r) = match axis {
                    Axis::Vertical => area.split_vertical(*ratio),
                    Axis::Horizontal => area.split_horizontal(*ratio),
                };
                self.layout_node(*left, l, gap, out);
                self.layout_node(*right, r, gap, out);
            }
        }
    }

    fn first_leaf(&self) -> NodeIdx {
        self.leftmost_leaf(self.root)
    }

    fn leftmost_leaf(&self, idx: NodeIdx) -> NodeIdx {
        // Stack nodes are "leaf-like" — they hold values directly, never have
        // child indices. We stop traversal as soon as we hit one.
        let mut cur = idx;
        loop {
            match &self.nodes[cur as usize].kind {
                NodeKind::Leaf { .. } | NodeKind::Stack { .. } => return cur,
                NodeKind::Split { left, .. } => cur = *left,
            }
        }
    }

    fn leaf_rect(&self, idx: NodeIdx, area: Rect) -> Option<Rect> {
        // Build a path from the root to `idx`, then walk it applying splits.
        let mut path = Vec::new();
        let mut cur = idx;
        while cur != NONE {
            path.push(cur);
            let parent = self.nodes[cur as usize].parent;
            if parent == NONE {
                break;
            }
            cur = parent;
        }
        path.reverse();

        let mut area = area;
        for window in path.windows(2) {
            let (parent, child) = (window[0], window[1]);
            let NodeKind::Split {
                axis, ratio, left, ..
            } = &self.nodes[parent as usize].kind
            else {
                return None;
            };
            let (l, r) = match axis {
                Axis::Vertical => area.split_vertical(*ratio),
                Axis::Horizontal => area.split_horizontal(*ratio),
            };
            area = if *left == child { l } else { r };
        }
        Some(area)
    }

    fn replace_child(&mut self, parent: NodeIdx, old: NodeIdx, new: NodeIdx) {
        if let NodeKind::Split { left, right, .. } = &mut self.nodes[parent as usize].kind {
            if *left == old {
                *left = new;
            } else if *right == old {
                *right = new;
            }
        }
    }

    fn sibling_of(&self, parent: NodeIdx, child: NodeIdx) -> NodeIdx {
        if let NodeKind::Split { left, right, .. } = &self.nodes[parent as usize].kind {
            if *left == child { *right } else { *left }
        } else {
            NONE
        }
    }
}

// ---- free helpers -----------------------------------------------------------

fn work_area_in_subtree<T: Copy + Eq + Hash>(
    tree: &BspTree<T>,
    idx: NodeIdx,
    work_area: Rect,
) -> Rect {
    // Find the rect this node occupies by walking from the root.
    if tree.root == NONE {
        return work_area;
    }
    // We re-compute by walking from root down — cheaper than a path build for
    // shallow trees.
    fn walk<T: Copy + Eq + Hash>(
        tree: &BspTree<T>,
        cur: NodeIdx,
        target: NodeIdx,
        area: Rect,
    ) -> Option<Rect> {
        if cur == target {
            return Some(area);
        }
        if let NodeKind::Split {
            axis,
            ratio,
            left,
            right,
        } = &tree.nodes[cur as usize].kind
        {
            let (l, r) = match axis {
                Axis::Vertical => area.split_vertical(*ratio),
                Axis::Horizontal => area.split_horizontal(*ratio),
            };
            walk(tree, *left, target, l).or_else(|| walk(tree, *right, target, r))
        } else {
            None
        }
    }
    walk(tree, tree.root, idx, work_area).unwrap_or(work_area)
}

fn nearest_in_direction<T: Copy + Eq + Hash>(
    positions: &[(T, Rect)],
    src: T,
    src_rect: Rect,
    dir: Direction,
) -> Option<T> {
    let (sx, sy) = (src_rect.center_x(), src_rect.center_y());
    let mut best: Option<(T, i64)> = None;
    for &(t, r) in positions {
        if t == src {
            continue;
        }
        let (cx, cy) = (r.center_x(), r.center_y());
        let in_dir = match dir {
            Direction::Left => cx < sx,
            Direction::Right => cx > sx,
            Direction::Up => cy < sy,
            Direction::Down => cy > sy,
        };
        if !in_dir {
            continue;
        }
        let dx = (cx - sx) as i64;
        let dy = (cy - sy) as i64;
        // Squared distance with a perpendicular-axis penalty so we prefer
        // candidates closer to the source's centerline.
        let perp_penalty = match dir {
            Direction::Left | Direction::Right => dy * dy * 2,
            Direction::Up | Direction::Down => dx * dx * 2,
        };
        let dist = dx * dx + dy * dy + perp_penalty;
        if best.map(|(_, b)| dist < b).unwrap_or(true) {
            best = Some((t, dist));
        }
    }
    best.map(|(t, _)| t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fhd() -> Rect {
        Rect::new(0, 0, 1920, 1080)
    }

    #[test]
    fn single_window_fills_work_area() {
        let mut t = BspTree::<u32>::new();
        t.insert(1, fhd());
        let pos = t.compute(fhd(), 0);
        assert_eq!(pos, vec![(1, fhd())]);
    }

    #[test]
    fn two_windows_split_vertically_for_wide_area() {
        let mut t = BspTree::<u32>::new();
        t.insert(1, fhd());
        t.insert(2, fhd());
        let pos = t.compute(fhd(), 0);
        assert_eq!(pos.len(), 2);
        let r1 = pos.iter().find(|(w, _)| *w == 1).unwrap().1;
        let r2 = pos.iter().find(|(w, _)| *w == 2).unwrap().1;
        // Vertical split → left + right, each 960 wide
        assert_eq!(r1.w, 960);
        assert_eq!(r2.w, 960);
        assert_eq!(r1.h, 1080);
        assert_eq!(r2.h, 1080);
        // They tile exactly with no gap.
        assert_eq!(r1.right(), r2.left());
    }

    #[test]
    fn spiral_alternates_split_axis() {
        // Spiral: each new split flips the parent split's axis. Starting
        // from a wide work area the first split is vertical (parent
        // doesn't exist; aspect-ratio fallback). The second insert
        // splits w2's parent (vertical) into a horizontal child, so
        // w2 and w3 share the right column.
        let mut t = BspTree::<u32>::new();
        t.set_layout_mode(LayoutMode::Spiral);
        for i in 1..=3 {
            t.insert(i, fhd());
        }
        let pos = t.compute(fhd(), 0);
        let r1 = pos.iter().find(|(w, _)| *w == 1).unwrap().1;
        let r2 = pos.iter().find(|(w, _)| *w == 2).unwrap().1;
        let r3 = pos.iter().find(|(w, _)| *w == 3).unwrap().1;
        // w1 occupies the left half (vertical split first).
        assert_eq!(r1.x, 0);
        assert_eq!(r1.w, 960);
        // w2 and w3 split the right half horizontally (axis flipped
        // from the vertical parent), so they share x and width.
        assert_eq!(r2.x, 960);
        assert_eq!(r3.x, 960);
        assert_eq!(r2.w, 960);
        assert_eq!(r3.w, 960);
        // And their heights stack together to fill the right column.
        assert_eq!(r2.h + r3.h, 1080);
    }

    #[test]
    fn spiral_four_windows_no_overlap() {
        // Sanity check: 4 inserts in spiral mode still cover the full
        // work area without overlap (each leaf rect is unique).
        let mut t = BspTree::<u32>::new();
        t.set_layout_mode(LayoutMode::Spiral);
        for i in 1..=4 {
            t.insert(i, fhd());
        }
        let pos = t.compute(fhd(), 0);
        assert_eq!(pos.len(), 4);
        let total: i64 = pos.iter().map(|(_, r)| r.area()).sum();
        assert_eq!(total, fhd().area());
        let unique: std::collections::HashSet<_> = pos.iter().map(|(_, r)| *r).collect();
        assert_eq!(unique.len(), 4);
    }

    #[test]
    fn layout_mode_round_trip() {
        let mut t = BspTree::<u32>::new();
        assert_eq!(t.layout_mode(), LayoutMode::Dwindle); // default
        t.set_layout_mode(LayoutMode::Spiral);
        assert_eq!(t.layout_mode(), LayoutMode::Spiral);
        t.set_layout_mode(LayoutMode::Dwindle);
        assert_eq!(t.layout_mode(), LayoutMode::Dwindle);
    }

    #[test]
    fn four_windows_distinct_rects() {
        let mut t = BspTree::<u32>::new();
        for i in 1..=4 {
            t.insert(i, fhd());
        }
        let pos = t.compute(fhd(), 0);
        assert_eq!(pos.len(), 4);
        // Total area should equal the work area.
        let total: i64 = pos.iter().map(|(_, r)| r.area()).sum();
        assert_eq!(total, fhd().area());
        // All rects should be unique.
        let unique: std::collections::HashSet<_> = pos.iter().map(|(_, r)| *r).collect();
        assert_eq!(unique.len(), 4);
    }

    #[test]
    fn eight_windows_no_overlap_no_gap() {
        let mut t = BspTree::<u32>::new();
        for i in 1..=8 {
            t.insert(i, fhd());
        }
        let pos = t.compute(fhd(), 0);
        assert_eq!(pos.len(), 8);
        let total: i64 = pos.iter().map(|(_, r)| r.area()).sum();
        assert_eq!(total, fhd().area());
    }

    #[test]
    fn remove_collapses_split() {
        let mut t = BspTree::<u32>::new();
        t.insert(1, fhd());
        t.insert(2, fhd());
        assert!(t.remove(2));
        let pos = t.compute(fhd(), 0);
        assert_eq!(pos, vec![(1, fhd())]);
        assert!(!t.is_empty());
    }

    #[test]
    fn remove_last_empties_tree() {
        let mut t = BspTree::<u32>::new();
        t.insert(1, fhd());
        assert!(t.remove(1));
        assert!(t.is_empty());
        assert_eq!(t.len(), 0);
    }

    #[test]
    fn gap_inset_shrinks_each_tile() {
        let mut t = BspTree::<u32>::new();
        t.insert(1, fhd());
        t.insert(2, fhd());
        let pos = t.compute(fhd(), 8);
        // Each tile should be inset by 8 px on all sides.
        let r1 = pos[0].1;
        assert_eq!(r1.w, 960 - 16);
        assert_eq!(r1.h, 1080 - 16);
    }

    #[test]
    fn swap_preserves_geometry_swaps_payload() {
        let mut t = BspTree::<u32>::new();
        t.insert(1, fhd());
        t.insert(2, fhd());
        let before = t.compute(fhd(), 0);
        t.swap(1, 2);
        let after = t.compute(fhd(), 0);
        // The rects in the same positions are the same, but the windows in
        // them are swapped.
        let by_rect_before: HashMap<_, _> = before.iter().map(|(w, r)| (*r, *w)).collect();
        let by_rect_after: HashMap<_, _> = after.iter().map(|(w, r)| (*r, *w)).collect();
        for (r, w_before) in by_rect_before {
            assert_ne!(by_rect_after[&r], w_before);
        }
    }

    #[test]
    fn focus_in_direction_picks_neighbour() {
        let mut t = BspTree::<u32>::new();
        t.insert(1, fhd());
        t.insert(2, fhd()); // right of 1 (vertical split)
        t.focus(1);
        let f = t.focus_in_direction(Direction::Right, fhd());
        assert_eq!(f, Some(2));
    }

    #[test]
    fn focus_in_direction_returns_none_at_edge() {
        let mut t = BspTree::<u32>::new();
        t.insert(1, fhd());
        t.insert(2, fhd());
        t.focus(2); // right tile
        let f = t.focus_in_direction(Direction::Right, fhd());
        assert_eq!(f, None);
    }

    #[test]
    fn move_in_direction_swaps_with_neighbour() {
        let mut t = BspTree::<u32>::new();
        t.insert(1, fhd());
        t.insert(2, fhd());
        let before = t.compute(fhd(), 0);
        assert!(t.move_in_direction(1, Direction::Right, fhd()));
        let after = t.compute(fhd(), 0);
        // window 1 should now occupy the rect window 2 used to.
        let r1_before = before.iter().find(|(w, _)| *w == 1).unwrap().1;
        let r1_after = after.iter().find(|(w, _)| *w == 1).unwrap().1;
        assert_ne!(r1_before, r1_after);
    }

    #[test]
    fn resize_changes_split_ratio_only() {
        let mut t = BspTree::<u32>::new();
        t.insert(1, fhd());
        t.insert(2, fhd());
        let before = t.compute(fhd(), 0);
        t.resize(1, Direction::Right, 100, fhd()); // grow 1 by 100px
        let after = t.compute(fhd(), 0);
        let w1_before = before.iter().find(|(w, _)| *w == 1).unwrap().1.w;
        let w1_after = after.iter().find(|(w, _)| *w == 1).unwrap().1.w;
        assert!(w1_after > w1_before);
    }

    #[test]
    fn resize_clamps_so_no_tile_collapses_to_zero() {
        // Audit-flagged "surviving mutation" test. The split ratio is
        // clamped to [0.1, 0.9]; if a regression weakened that lower
        // bound to 0.0, a single oversized resize call could drive a
        // tile to width 0 (or even negative when factoring in gaps),
        // hiding it entirely. Drive the resize past the bound and assert
        // every tile is left visibly occupiable.
        let mut t = BspTree::<u32>::new();
        t.insert(1, fhd());
        t.insert(2, fhd());

        // Push the split as far left as possible \u2014 enough to overshoot
        // the 0.1 floor by an order of magnitude.
        t.resize(1, Direction::Left, 10_000, fhd());
        let rects = t.compute(fhd(), 0);
        assert_eq!(rects.len(), 2);
        for (id, r) in &rects {
            assert!(
                r.w > 0,
                "tile {id} collapsed to width {}; resize clamp regressed",
                r.w
            );
            assert!(
                r.h > 0,
                "tile {id} collapsed to height {}; resize clamp regressed",
                r.h
            );
        }
        // Specifically: the smaller side must still be at least 10% of
        // the work area's width \u2014 the documented contract of the 0.1
        // clamp. Pinning the lower edge directly catches any "0.05 is
        // close enough" softening of the bound.
        let min_w = rects.iter().map(|(_, r)| r.w).min().unwrap();
        let total = fhd().w;
        assert!(
            min_w >= total / 10,
            "smaller tile width {min_w} below 10% of total {total}; clamp regressed"
        );

        // And the symmetric case in the opposite direction.
        t.resize(1, Direction::Right, 10_000, fhd());
        let rects = t.compute(fhd(), 0);
        let min_w = rects.iter().map(|(_, r)| r.w).min().unwrap();
        assert!(
            min_w >= total / 10,
            "smaller tile width {min_w} below 10% of total after right-saturation"
        );
    }

    // ---- stack containers ------------------------------------------------

    #[test]
    fn toggle_stack_on_single_leaf_then_back() {
        let mut t = BspTree::<u32>::new();
        t.insert(1, fhd());
        assert!(t.toggle_stack_focused());
        assert_eq!(t.focused(), Some(1));
        assert_eq!(t.focused_stack_info(), Some((0, 1)));
        // Compute still emits the single member at the full area.
        assert_eq!(t.compute(fhd(), 0), vec![(1, fhd())]);
        // Toggle off returns to a plain Leaf.
        assert!(t.toggle_stack_focused());
        assert_eq!(t.focused_stack_info(), None);
        assert_eq!(t.compute(fhd(), 0), vec![(1, fhd())]);
    }

    #[test]
    fn insert_into_focused_stack_appends_instead_of_splitting() {
        let mut t = BspTree::<u32>::new();
        t.insert(1, fhd());
        t.toggle_stack_focused();
        t.insert(2, fhd()); // joins the stack
        t.insert(3, fhd()); // also joins
        assert_eq!(t.focused_stack_info(), Some((2, 3)));
        // Visible window is the newest member, occupying the full area.
        assert_eq!(t.compute(fhd(), 0), vec![(3, fhd())]);
        // Stack members 1 and 2 are hidden.
        let hidden: std::collections::HashSet<_> = t.hidden_in_stacks().into_iter().collect();
        assert_eq!(hidden, [1u32, 2].into_iter().collect());
        // windows() reports all three.
        let all: std::collections::HashSet<_> = t.windows().into_iter().collect();
        assert_eq!(all, [1u32, 2, 3].into_iter().collect());
    }

    #[test]
    fn focus_stack_next_prev_cycles() {
        let mut t = BspTree::<u32>::new();
        t.insert(1, fhd());
        t.toggle_stack_focused();
        t.insert(2, fhd());
        t.insert(3, fhd());
        // Currently focused: 3 (idx 2 of 3).
        assert_eq!(t.focused(), Some(3));
        // Next wraps around.
        assert_eq!(t.focus_stack_next(), Some(1));
        assert_eq!(t.focused(), Some(1));
        // Prev goes backwards.
        assert_eq!(t.focus_stack_prev(), Some(3));
    }

    #[test]
    fn focus_member_updates_stack_focused_index() {
        let mut t = BspTree::<u32>::new();
        t.insert(1, fhd());
        t.toggle_stack_focused();
        t.insert(2, fhd());
        t.insert(3, fhd());
        t.focus(1);
        assert_eq!(t.focused(), Some(1));
        assert_eq!(t.compute(fhd(), 0), vec![(1, fhd())]);
    }

    #[test]
    fn remove_from_stack_keeps_node() {
        let mut t = BspTree::<u32>::new();
        t.insert(1, fhd());
        t.toggle_stack_focused();
        t.insert(2, fhd());
        t.insert(3, fhd());
        assert!(t.remove(3));
        assert_eq!(t.focused_stack_info(), Some((1, 2))); // focused = last idx
        assert!(t.remove(1));
        assert_eq!(t.focused_stack_info(), Some((0, 1)));
        assert!(t.remove(2));
        assert!(t.is_empty());
    }

    #[test]
    fn toggle_off_multi_stack_expands_into_splits() {
        let mut t = BspTree::<u32>::new();
        t.insert(1, fhd());
        t.toggle_stack_focused();
        t.insert(2, fhd());
        t.insert(3, fhd());
        // Expand the 3-stack back into separate tiles.
        assert!(t.toggle_stack_focused());
        let pos = t.compute(fhd(), 0);
        // Now every former member has its own rect.
        assert_eq!(pos.len(), 3);
        let total: i64 = pos.iter().map(|(_, r)| r.area()).sum();
        assert_eq!(total, fhd().area());
        // Stack info gone.
        assert_eq!(t.focused_stack_info(), None);
    }

    #[test]
    fn stack_alongside_leaf_only_focused_member_visible() {
        // Three-tile layout so the focused leaf has a Split-shaped sibling,
        // which falls back to the 1-stack toggle. That way the *other* two
        // tiles stay visible after the toggle.
        let mut t = BspTree::<u32>::new();
        t.insert(1, fhd());
        t.insert(2, fhd()); // Split(1, 2)
        t.focus(1);
        t.insert(3, fhd()); // Splits leaf 1 again -> Split(Split(1, 3), 2)
        t.focus(2);
        // Focused leaf 2's sibling under the root Split is a Split(1, 3) —
        // not a Leaf/Stack — so toggle falls back to 1-stack on leaf 2.
        assert!(t.toggle_stack_focused());
        t.insert(4, fhd()); // joins the stack at the focused node
        let pos = t.compute(fhd(), 0);
        // 3 tiles visible: leaves 1 and 3, plus the stack's focused
        // member 4 (the original 2 is hidden behind it).
        assert_eq!(pos.len(), 3);
        let visible: std::collections::HashSet<_> = pos.iter().map(|(w, _)| *w).collect();
        assert_eq!(visible, [1u32, 3, 4].into_iter().collect());
        assert_eq!(t.hidden_in_stacks(), vec![2]);
    }

    // ---- smart toggle merge behaviour ------------------------------------

    #[test]
    fn toggle_two_leaves_merges_into_stack() {
        // The headline UX behaviour: two side-by-side tiles + Alt+G should
        // visibly collapse into a single tile (a 2-stack).
        let mut t = BspTree::<u32>::new();
        t.insert(1, fhd());
        t.insert(2, fhd()); // Split(1, 2), focus on 2
        assert!(t.toggle_stack_focused());
        let pos = t.compute(fhd(), 0);
        // One visible tile now, occupying the whole work area.
        assert_eq!(pos.len(), 1);
        assert_eq!(pos[0].1, fhd());
        // Focused window is still 2 (the original focus); 1 is hidden.
        assert_eq!(t.focused(), Some(2));
        assert_eq!(t.focused_stack_info(), Some((0, 2)));
        assert_eq!(t.hidden_in_stacks(), vec![1]);
    }

    #[test]
    fn toggle_leaf_with_stack_sibling_joins_stack() {
        // Build Split(Leaf 1, Stack[2, 3]) by inserting 1/2/3 then merging
        // the rightmost pair into a stack.
        let mut t = BspTree::<u32>::new();
        t.insert(1, fhd()); // Leaf 1 at root
        t.insert(2, fhd()); // Split(Leaf 1, Leaf 2), focus 2
        t.insert(3, fhd()); // Split(Leaf 1, Split(Leaf 2, Leaf 3)), focus 3
        t.focus(2);
        // 2's sibling is Leaf 3 → merge into Stack[2, 3] at the inner
        // split's slot. Tree becomes Split(Leaf 1, Stack[2, 3]).
        assert!(t.toggle_stack_focused());
        // Now focus Leaf 1; its sibling is the Stack.
        t.focus(1);
        assert!(t.toggle_stack_focused());
        // Result: the outer Split collapses into a single Stack at the
        // root with all three windows; 1 is the visible (just-swallowed)
        // member.
        let pos = t.compute(fhd(), 0);
        assert_eq!(pos.len(), 1);
        assert_eq!(pos[0].0, 1);
        assert_eq!(t.focused(), Some(1));
        // Stack now has three members.
        assert_eq!(t.focused_stack_info().map(|(_, n)| n), Some(3));
    }

    #[test]
    fn toggle_stack_off_two_member_expands() {
        // Smart toggle ON, then immediate toggle OFF, should round-trip
        // back to two separate tiles.
        let mut t = BspTree::<u32>::new();
        t.insert(1, fhd());
        t.insert(2, fhd());
        t.toggle_stack_focused(); // merges into 2-stack
        assert_eq!(t.compute(fhd(), 0).len(), 1);
        t.toggle_stack_focused(); // expands back
        let pos = t.compute(fhd(), 0);
        assert_eq!(pos.len(), 2);
        let visible: std::collections::HashSet<_> = pos.iter().map(|(w, _)| *w).collect();
        assert_eq!(visible, [1u32, 2].into_iter().collect());
    }

    #[test]
    fn stack_swallow_dir_pulls_in_neighbor() {
        let mut t = BspTree::<u32>::new();
        t.insert(1, fhd());
        t.insert(2, fhd()); // Split(1, 2) — 2 on the right, focused
        // Swallow the leftward neighbor (1) into 2's tile.
        assert!(t.stack_swallow_dir(Direction::Left, fhd()));
        let pos = t.compute(fhd(), 0);
        assert_eq!(pos.len(), 1);
        // The swallowed window (1) becomes the visible member.
        assert_eq!(pos[0].0, 1);
        assert_eq!(t.focused(), Some(1));
    }

    #[test]
    fn stack_pop_focused_extracts_member_back_to_leaf() {
        let mut t = BspTree::<u32>::new();
        t.insert(1, fhd());
        t.insert(2, fhd());
        t.toggle_stack_focused(); // 2-stack at root with [2, 1], focused 2
        assert!(t.stack_pop_focused());
        let pos = t.compute(fhd(), 0);
        // After popping the focused member (2) back out, we should be at
        // Split(Stack[1], Leaf 2) — two visible tiles again.
        assert_eq!(pos.len(), 2);
        let visible: std::collections::HashSet<_> = pos.iter().map(|(w, _)| *w).collect();
        assert_eq!(visible, [1u32, 2].into_iter().collect());
    }
}
