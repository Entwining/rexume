//! The saved form of a session's layout, built from `session.view` and keyed by lineage instead of
//! the IDs of one server instance, and the `session` table that holds it. Only this file and
//! `restore/create.rs` know the layout's form.

mod persist;
#[cfg(test)]
mod tests;
mod view;

use serde::{Deserialize, Serialize};

pub use persist::{
    bind_restored, forget_block, forget_session, forget_sessions_except, forget_window, save_view,
};
pub use view::{from_view, view_blocks};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionDoc {
    pub label: String,
    /// Index into `windows` of the window the session shows.
    pub active: Option<usize>,
    pub windows: Vec<WindowDoc>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowDoc {
    pub lineage: String,
    pub label: Option<String>,
    pub focused: Option<String>,
    pub zoomed: Option<String>,
    pub tiled: Option<Node>,
    pub floating: Vec<Floating>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Floating {
    pub bounds: [f64; 4],
    pub layout: Node,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Node {
    Split {
        direction: String,
        ratio: f64,
        before: Box<Node>,
        after: Box<Node>,
    },
    Block {
        lineage: String,
        label: Option<String>,
    },
}

impl Node {
    /// The blocks' lineages in the order Rex returns new block IDs: each split's before side ahead
    /// of its after.
    pub fn leaves(&self) -> Vec<&str> {
        match self {
            Self::Split { before, after, .. } => {
                let mut leaves = before.leaves();
                leaves.extend(after.leaves());
                leaves
            }
            Self::Block { lineage, .. } => vec![lineage],
        }
    }

    fn rename(&mut self, from: &str, to: &str) -> bool {
        match self {
            Self::Split { before, after, .. } => before.rename(from, to) | after.rename(from, to),
            Self::Block { lineage, .. } if lineage == from => {
                to.clone_into(lineage);
                true
            }
            Self::Block { .. } => false,
        }
    }

    /// The tree without `lineage`, its sibling taking the split's place; `None` when nothing is left.
    fn without(self, lineage: &str) -> Option<Self> {
        match self {
            Self::Block {
                lineage: ref own, ..
            } if own == lineage => None,
            Self::Block { .. } => Some(self),
            Self::Split {
                direction,
                ratio,
                before,
                after,
            } => match (before.without(lineage), after.without(lineage)) {
                (Some(before), Some(after)) => Some(Self::Split {
                    direction,
                    ratio,
                    before: Box::new(before),
                    after: Box::new(after),
                }),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            },
        }
    }
}

impl WindowDoc {
    pub fn blocks(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self.tiled.iter().flat_map(Node::leaves).collect();
        for layer in &self.floating {
            out.extend(layer.layout.leaves());
        }
        out
    }

    fn is_empty(&self) -> bool {
        self.tiled.is_none() && self.floating.is_empty()
    }
}

impl SessionDoc {
    pub fn blocks(&self) -> Vec<&str> {
        self.windows.iter().flat_map(WindowDoc::blocks).collect()
    }

    /// Removes a closed block, and its window once the window has no blocks left, as Rex does.
    pub fn remove_block(&mut self, lineage: &str) -> bool {
        let before = self.blocks().len();
        for window in &mut self.windows {
            window.tiled = window.tiled.take().and_then(|node| node.without(lineage));
            window.floating = std::mem::take(&mut window.floating)
                .into_iter()
                .filter_map(|layer| {
                    layer.layout.without(lineage).map(|layout| Floating {
                        bounds: layer.bounds,
                        layout,
                    })
                })
                .collect();
            if window.focused.as_deref() == Some(lineage) {
                window.focused = None;
            }
            if window.zoomed.as_deref() == Some(lineage) {
                window.zoomed = None;
            }
        }
        self.drop_empty_windows();
        self.blocks().len() != before
    }

    /// Renames a window or block lineage wherever the layout names it.
    fn rename(&mut self, from: &str, to: &str) -> bool {
        let mut changed = false;
        for window in &mut self.windows {
            if window.lineage == from {
                to.clone_into(&mut window.lineage);
                changed = true;
            }
            let layouts = window
                .tiled
                .iter_mut()
                .chain(window.floating.iter_mut().map(|layer| &mut layer.layout));
            for node in layouts {
                changed |= node.rename(from, to);
            }
            for named in [&mut window.focused, &mut window.zoomed] {
                if named.as_deref() == Some(from) {
                    *named = Some(to.to_owned());
                    changed = true;
                }
            }
        }
        changed
    }

    pub fn remove_window(&mut self, lineage: &str) -> bool {
        let before = self.windows.len();
        self.retain_windows(|window| window.lineage != lineage);
        self.windows.len() != before
    }

    fn drop_empty_windows(&mut self) {
        self.retain_windows(|window| !window.is_empty());
    }

    /// Keeps the active window active by lineage, since its index can move.
    fn retain_windows(&mut self, keep: impl Fn(&WindowDoc) -> bool) {
        let active = self
            .active
            .and_then(|i| self.windows.get(i))
            .map(|w| w.lineage.clone());
        self.windows.retain(keep);
        self.active = active.and_then(|a| self.windows.iter().position(|w| w.lineage == a));
    }
}
