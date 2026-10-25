use crate::{Axis, Direction, Rect, WindowId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Layout {
    #[default]
    Tiles,
    Accordion,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum Node {
    Window(WindowId),
    Container {
        axis: Axis,
        layout: Layout,
        children: Vec<Node>,
        weights: Vec<f64>,
    },
}

impl Node {
    pub fn container(axis: Axis, layout: Layout) -> Self {
        Self::Container {
            axis,
            layout,
            children: Vec::new(),
            weights: Vec::new(),
        }
    }
    pub fn contains(&self, id: WindowId) -> bool {
        match self {
            Self::Window(w) => *w == id,
            Self::Container { children, .. } => children.iter().any(|c| c.contains(id)),
        }
    }
    pub fn ids(&self, out: &mut Vec<WindowId>) {
        match self {
            Self::Window(w) => out.push(*w),
            Self::Container { children, .. } => children.iter().for_each(|c| c.ids(out)),
        }
    }
    pub fn first(&self) -> Option<WindowId> {
        match self {
            Self::Window(w) => Some(*w),
            Self::Container { children, .. } => children.iter().find_map(Self::first),
        }
    }
    pub fn insert(&mut self, id: WindowId, after: Option<WindowId>) {
        if let Self::Container {
            children, weights, ..
        } = self
        {
            if let Some(after) = after {
                if let Some(index) = children.iter().position(|c| c.contains(after)) {
                    if matches!(children[index], Self::Container { .. }) {
                        children[index].insert(id, Some(after));
                    } else {
                        children.insert(index + 1, Self::Window(id));
                        weights.insert(index + 1, 1.);
                    }
                    return;
                }
            }
            children.push(Self::Window(id));
            weights.push(1.);
        }
    }
    pub fn remove(&mut self, id: WindowId) -> bool {
        let removed = self.remove_raw(id);
        if removed {
            self.normalize();
        }
        removed
    }

    fn remove_raw(&mut self, id: WindowId) -> bool {
        let Self::Container {
            children, weights, ..
        } = self
        else {
            return false;
        };
        if let Some(index) = children
            .iter()
            .position(|c| matches!(c, Self::Window(w) if *w == id))
        {
            children.remove(index);
            weights.remove(index);
            return true;
        }
        children.iter_mut().any(|c| c.remove_raw(id))
    }
    pub fn normalize(&mut self) {
        if let Self::Container {
            children, weights, ..
        } = self
        {
            for c in children.iter_mut() {
                c.normalize();
            }
            let mut index = 0;
            while index < children.len() {
                if let Self::Container {
                    children: nested, ..
                } = &mut children[index]
                {
                    if nested.is_empty() {
                        children.remove(index);
                        weights.remove(index);
                        continue;
                    }
                    if nested.len() == 1 {
                        children[index] = nested.remove(0);
                    }
                }
                index += 1;
            }
        }
    }
    pub fn parent_mut(&mut self, id: WindowId) -> Option<&mut Self> {
        let Self::Container { children, .. } = self else {
            return None;
        };
        if children
            .iter()
            .any(|c| matches!(c, Self::Window(w) if *w == id))
        {
            return Some(self);
        }
        let Self::Container { children, .. } = self else {
            return None;
        };
        children.iter_mut().find_map(|c| c.parent_mut(id))
    }
    pub fn set_layout(
        &mut self,
        id: Option<WindowId>,
        axis: Option<Axis>,
        layout: Option<Layout>,
        root: bool,
    ) {
        let target = if root {
            Some(self)
        } else {
            id.and_then(|id| self.parent_mut(id))
        };
        if let Some(Self::Container {
            axis: a, layout: l, ..
        }) = target
        {
            if let Some(axis) = axis {
                *a = axis;
            }
            if let Some(layout) = layout {
                *l = layout;
            }
        }
    }
    pub fn split(&mut self, id: WindowId, axis: Axis) {
        match self {
            Self::Window(w) if *w == id => {
                *self = Self::Container {
                    axis,
                    layout: Layout::Tiles,
                    children: vec![Self::Window(id)],
                    weights: vec![1.],
                };
            }
            Self::Container {
                axis: current,
                children,
                ..
            } => {
                // A pending split already surrounds this leaf. Replacing its
                // orientation avoids unbounded chains from repeated split keys.
                if children.len() == 1 && matches!(children[0], Self::Window(w) if w == id) {
                    *current = axis;
                    return;
                }
                for child in children {
                    if child.contains(id) {
                        child.split(id, axis);
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    fn path(&self, id: WindowId) -> Option<Vec<usize>> {
        match self {
            Self::Window(w) => (*w == id).then(Vec::new),
            Self::Container { children, .. } => {
                children.iter().enumerate().find_map(|(index, child)| {
                    child.path(id).map(|mut path| {
                        path.insert(0, index);
                        path
                    })
                })
            }
        }
    }

    fn at_mut(&mut self, path: &[usize]) -> Option<&mut Self> {
        if path.is_empty() {
            return Some(self);
        }
        match self {
            Self::Container { children, .. } => children.get_mut(path[0])?.at_mut(&path[1..]),
            _ => None,
        }
    }

    pub fn join_direction(&mut self, id: WindowId, direction: Direction) {
        let Some(path) = self.path(id) else {
            return;
        };
        for depth in (0..path.len()).rev() {
            let own = path[depth];
            let Some(Self::Container { axis, children, .. }) = self.at_mut(&path[..depth]) else {
                continue;
            };
            if *axis != direction.axis() {
                continue;
            }
            let neighbor = if direction.positive() {
                own.checked_add(1)
            } else {
                own.checked_sub(1)
            };
            let Some(neighbor) = neighbor.filter(|index| *index < children.len()) else {
                continue;
            };
            self.remove_raw(id);
            let neighbor = if depth + 1 == path.len() && own < neighbor {
                neighbor - 1
            } else {
                neighbor
            };
            if let Some(Self::Container {
                children, weights, ..
            }) = self.at_mut(&path[..depth])
            {
                let target = children.remove(neighbor);
                let joined = if direction.positive() {
                    vec![Self::Window(id), target]
                } else {
                    vec![target, Self::Window(id)]
                };
                children.insert(
                    neighbor,
                    Self::Container {
                        axis: direction.axis().other(),
                        layout: Layout::Tiles,
                        children: joined,
                        weights: vec![1., 1.],
                    },
                );
                // The target's outer weight is preserved.
                debug_assert_eq!(children.len(), weights.len());
            }
            self.normalize();
            return;
        }
    }

    /// Move into a sibling container, or out to the nearest matching
    /// ancestor. A swap deliberately keeps the original tree shape.
    pub fn move_window(&mut self, id: WindowId, direction: Direction) {
        let Some(path) = self.path(id) else {
            return;
        };
        for depth in (0..path.len()).rev() {
            let own = path[depth];
            let Some(Self::Container { axis, children, .. }) = self.at_mut(&path[..depth]) else {
                continue;
            };
            if *axis != direction.axis() {
                continue;
            }
            let neighbor = if direction.positive() {
                own.checked_add(1)
            } else {
                own.checked_sub(1)
            };
            if depth + 1 == path.len() {
                let Some(neighbor) = neighbor.filter(|index| *index < children.len()) else {
                    if depth == 0 {
                        return;
                    }
                    continue;
                };
                self.remove_raw(id);
                let adjusted = if own < neighbor {
                    neighbor - 1
                } else {
                    neighbor
                };
                if let Some(Self::Container {
                    children, weights, ..
                }) = self.at_mut(&path[..depth])
                {
                    if matches!(children[adjusted], Self::Container { .. }) {
                        children[adjusted].insert_edge(id, direction);
                    } else {
                        let insertion = if direction.positive() {
                            adjusted + 1
                        } else {
                            adjusted
                        };
                        children.insert(insertion, Self::Window(id));
                        weights.insert(insertion, 1.);
                    }
                }
            } else {
                self.remove_raw(id);
                if let Some(Self::Container {
                    children, weights, ..
                }) = self.at_mut(&path[..depth])
                {
                    let index = own + usize::from(direction.positive());
                    children.insert(index, Self::Window(id));
                    weights.insert(index, 1.);
                }
            }
            self.normalize();
            return;
        }
        let mut ids = Vec::new();
        self.ids(&mut ids);
        if ids.len() < 2 {
            return;
        }
        self.remove_raw(id);
        self.normalize();
        let old = std::mem::replace(self, Self::container(direction.axis(), Layout::Tiles));
        if let Self::Container {
            children, weights, ..
        } = self
        {
            if direction.positive() {
                children.extend([old, Self::Window(id)]);
            } else {
                children.extend([Self::Window(id), old]);
            }
            weights.extend([1., 1.]);
        }
        self.normalize();
    }

    fn insert_edge(&mut self, id: WindowId, direction: Direction) {
        if let Self::Container {
            axis,
            children,
            weights,
            ..
        } = self
        {
            if *axis == direction.axis() || children.is_empty() {
                let index = if direction.positive() {
                    0
                } else {
                    children.len()
                };
                children.insert(index, Self::Window(id));
                weights.insert(index, 1.);
            } else {
                children[0].insert_beside(id, direction);
            }
        }
    }

    fn insert_beside(&mut self, id: WindowId, direction: Direction) {
        match self {
            Self::Container { .. } => self.insert_edge(id, direction),
            Self::Window(existing) => {
                let ids = if direction.positive() {
                    [id, *existing]
                } else {
                    [*existing, id]
                };
                *self = Self::Container {
                    axis: direction.axis(),
                    layout: Layout::Tiles,
                    children: ids.into_iter().map(Self::Window).collect(),
                    weights: vec![1., 1.],
                };
            }
        }
    }
    pub fn swap(&mut self, a: WindowId, b: WindowId) {
        match self {
            Self::Window(id) => {
                if *id == a {
                    *id = b;
                } else if *id == b {
                    *id = a;
                }
            }
            Self::Container { children, .. } => children.iter_mut().for_each(|c| c.swap(a, b)),
        }
    }
    pub fn insert_relative(
        &mut self,
        id: WindowId,
        target: WindowId,
        direction: Direction,
    ) -> bool {
        match self {
            Self::Window(window) if *window == target => {
                let children = if direction.positive() {
                    vec![Self::Window(target), Self::Window(id)]
                } else {
                    vec![Self::Window(id), Self::Window(target)]
                };
                *self = Self::Container {
                    axis: direction.axis(),
                    layout: Layout::Tiles,
                    children,
                    weights: vec![1., 1.],
                };
                true
            }
            Self::Container { children, .. } => children
                .iter_mut()
                .any(|child| child.insert_relative(id, target, direction)),
            _ => false,
        }
    }
    pub fn balance(&mut self) {
        if let Self::Container {
            children, weights, ..
        } = self
        {
            weights.fill(1.);
            children.iter_mut().for_each(Self::balance);
        }
    }
    pub fn resize(
        &mut self,
        id: WindowId,
        axis: Axis,
        delta: f64,
        rect: Rect,
        gap: f64,
        padding: f64,
    ) -> bool {
        let Self::Container {
            axis: current,
            layout,
            children,
            weights,
        } = self
        else {
            return false;
        };
        let Some(index) = children.iter().position(|c| c.contains(id)) else {
            return false;
        };
        // Descendants resize in their own allocated space, excluding gaps.
        // Accordion children overlap, but all have the same length.
        let length = current.length(rect);
        let gap = gap.max(0.).min(length / (children.len() as f64 * 2.));
        let total: f64 = weights.iter().sum();
        let usable = (length - gap * children.len().saturating_sub(1) as f64).max(1.);
        let child_length = if *layout == Layout::Accordion {
            let pad = padding.max(0.).min(length / (children.len() as f64 + 1.));
            (length - children.len().saturating_sub(1) as f64 * pad).max(1.)
        } else {
            (usable * weights[index] / total.max(f64::EPSILON)).max(1.)
        };
        let mut child_rect = rect;
        match current {
            Axis::Horizontal => child_rect.width = child_length,
            Axis::Vertical => child_rect.height = child_length,
        }
        // Prefer the nearest matching tile container, then an ancestor.
        if children[index].resize(id, axis, delta, child_rect, gap, padding) {
            return true;
        }
        if *current != axis || children.len() < 2 || *layout == Layout::Accordion {
            return false;
        }
        let other = if index + 1 < children.len() {
            index + 1
        } else {
            index - 1
        };
        let change = delta / usable * total;
        let minimum = (total * 0.02)
            .min(0.2)
            .min(weights[index])
            .min(weights[other]);
        let change = change.clamp(minimum - weights[index], weights[other] - minimum);
        weights[index] += change;
        weights[other] -= change;
        true
    }
    pub fn frames(
        &self,
        rect: Rect,
        gap: f64,
        padding: f64,
        focused: Option<WindowId>,
        out: &mut Vec<(WindowId, Rect)>,
    ) {
        match self {
            Self::Window(id) => out.push((*id, rect)),
            Self::Container {
                axis,
                layout,
                children,
                weights,
            } => {
                let n = children.len();
                if n == 0 {
                    return;
                }
                let length = axis.length(rect);
                let mut cursor = 0.;
                let total: f64 = weights.iter().sum();
                let focused_index = focused
                    .and_then(|id| children.iter().position(|c| c.contains(id)))
                    .unwrap_or(0);
                let pad = padding.max(0.).min(length / (n as f64 + 1.));
                let gap = gap.max(0.).min(length / (n as f64 * 2.));
                for (index, child) in children.iter().enumerate() {
                    let (offset, size) = if *layout == Layout::Accordion {
                        let offset = if index <= focused_index {
                            index as f64 * pad
                        } else {
                            length - (n - index) as f64 * pad
                        };
                        (offset, (length - (n - 1) as f64 * pad).max(1.))
                    } else {
                        let usable = (length - gap * n.saturating_sub(1) as f64).max(1.);
                        let size = if index + 1 == n {
                            (length - cursor).max(1.)
                        } else {
                            (usable * weights[index] / total.max(f64::EPSILON)).max(1.)
                        };
                        (cursor, size)
                    };
                    let r = match axis {
                        Axis::Horizontal => Rect::new(rect.x + offset, rect.y, size, rect.height),
                        Axis::Vertical => Rect::new(rect.x, rect.y + offset, rect.width, size),
                    };
                    child.frames(r, gap, padding, focused, out);
                    cursor += size + gap;
                }
            }
        }
    }
}
