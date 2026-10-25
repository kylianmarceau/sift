use serde::{Deserialize, Serialize};

/// AX/Quartz coordinates: global points, origin at the main display's top left.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn valid(self) -> bool {
        [self.x, self.y, self.width, self.height]
            .iter()
            .all(|n| n.is_finite())
            && self.width > 0.
            && self.height > 0.
    }

    pub fn right(self) -> f64 {
        self.x + self.width
    }
    pub fn bottom(self) -> f64 {
        self.y + self.height
    }
    pub fn center(self) -> (f64, f64) {
        (self.x + self.width / 2., self.y + self.height / 2.)
    }

    pub fn inset(self, left: f64, top: f64, right: f64, bottom: f64) -> Self {
        let left = left.max(0.).min((self.width - 1.).max(0.));
        let top = top.max(0.).min((self.height - 1.).max(0.));
        Self::new(
            self.x + left,
            self.y + top,
            (self.width - left - right.max(0.)).max(1.),
            (self.height - top - bottom.max(0.)).max(1.),
        )
    }

    pub fn nearly(self, other: Self) -> bool {
        (self.x - other.x).abs() < 1.
            && (self.y - other.y).abs() < 1.
            && (self.width - other.width).abs() < 1.
            && (self.height - other.height).abs() < 1.
    }

    pub fn contains(self, x: f64, y: f64) -> bool {
        x >= self.x && y >= self.y && x < self.right() && y < self.bottom()
    }

    pub fn constrained(self, bounds: Self) -> Self {
        let width = self.width.min(bounds.width).max(1.);
        let height = self.height.min(bounds.height).max(1.);
        Self::new(
            self.x.clamp(bounds.x, bounds.right() - width),
            self.y.clamp(bounds.y, bounds.bottom() - height),
            width,
            height,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Axis {
    Horizontal,
    Vertical,
}

impl Axis {
    pub fn other(self) -> Self {
        match self {
            Self::Horizontal => Self::Vertical,
            Self::Vertical => Self::Horizontal,
        }
    }
    pub fn length(self, r: Rect) -> f64 {
        match self {
            Self::Horizontal => r.width,
            Self::Vertical => r.height,
        }
    }
}
