/// A 2D point with f32 coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const ZERO: Self = Self { x: 0.0, y: 0.0 };
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

/// A 2D size (width × height) in f32 units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Size {
    pub width: f32,
    pub height: f32,
}

impl Size {
    pub const ZERO: Self = Self {
        width: 0.0,
        height: 0.0,
    };
    pub const fn new(width: f32, height: f32) -> Self {
        Self { width, height }
    }
    pub fn area(self) -> f32 {
        self.width * self.height
    }
}

/// Axis-aligned rectangle defined by origin and size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub origin: Point,
    pub size: Size,
}

impl Rect {
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            origin: Point { x, y },
            size: Size { width, height },
        }
    }
    pub fn x(self) -> f32 {
        self.origin.x
    }
    pub fn y(self) -> f32 {
        self.origin.y
    }
    pub fn width(self) -> f32 {
        self.size.width
    }
    pub fn height(self) -> f32 {
        self.size.height
    }
    pub fn right(self) -> f32 {
        self.origin.x + self.size.width
    }
    pub fn bottom(self) -> f32 {
        self.origin.y + self.size.height
    }
    pub fn center(self) -> Point {
        Point::new(
            self.origin.x + self.size.width / 2.0,
            self.origin.y + self.size.height / 2.0,
        )
    }
    pub fn contains(self, p: Point) -> bool {
        p.x >= self.x() && p.x <= self.right() && p.y >= self.y() && p.y <= self.bottom()
    }
    pub fn split_horizontal(self, n: usize) -> Vec<Rect> {
        if n == 0 {
            return vec![];
        }
        let tile_w = self.size.width / n as f32;
        (0..n)
            .map(|i| {
                Rect::new(
                    self.origin.x + i as f32 * tile_w,
                    self.origin.y,
                    tile_w,
                    self.size.height,
                )
            })
            .collect()
    }
    pub fn split_vertical(self, n: usize) -> Vec<Rect> {
        if n == 0 {
            return vec![];
        }
        let tile_h = self.size.height / n as f32;
        (0..n)
            .map(|i| {
                Rect::new(
                    self.origin.x,
                    self.origin.y + i as f32 * tile_h,
                    self.size.width,
                    tile_h,
                )
            })
            .collect()
    }
    pub fn grid(self, cols: usize, rows: usize) -> Vec<Rect> {
        if cols == 0 || rows == 0 {
            return vec![];
        }
        let tw = self.size.width / cols as f32;
        let th = self.size.height / rows as f32;
        (0..rows)
            .flat_map(|r| {
                (0..cols).map(move |c| {
                    Rect::new(
                        self.origin.x + c as f32 * tw,
                        self.origin.y + r as f32 * th,
                        tw,
                        th,
                    )
                })
            })
            .collect()
    }
    pub fn to_ndc(self, viewport: Size) -> [f32; 4] {
        let x0 = (self.x() / viewport.width) * 2.0 - 1.0;
        let y0 = 1.0 - (self.y() / viewport.height) * 2.0;
        let x1 = (self.right() / viewport.width) * 2.0 - 1.0;
        let y1 = 1.0 - (self.bottom() / viewport.height) * 2.0;
        [x0, y0, x1, y1]
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- Point ---

    #[test]
    fn point_new() {
        let p = Point::new(3.0, 4.0);
        assert_eq!(p.x, 3.0);
        assert_eq!(p.y, 4.0);
    }

    #[test]
    fn point_zero() {
        assert_eq!(Point::ZERO.x, 0.0);
        assert_eq!(Point::ZERO.y, 0.0);
    }

    // --- Size ---

    #[test]
    fn size_new() {
        let s = Size::new(10.0, 20.0);
        assert_eq!(s.width, 10.0);
        assert_eq!(s.height, 20.0);
    }

    #[test]
    fn size_zero() {
        assert_eq!(Size::ZERO.area(), 0.0);
    }

    #[test]
    fn size_area() {
        assert_eq!(Size::new(5.0, 4.0).area(), 20.0);
    }

    #[test]
    fn size_area_zero_width() {
        assert_eq!(Size::new(0.0, 100.0).area(), 0.0);
    }

    // --- Rect construction and accessors ---

    #[test]
    fn rect_new_accessors() {
        let r = Rect::new(1.0, 2.0, 30.0, 40.0);
        assert_eq!(r.x(), 1.0);
        assert_eq!(r.y(), 2.0);
        assert_eq!(r.width(), 30.0);
        assert_eq!(r.height(), 40.0);
    }

    #[test]
    fn rect_right_and_bottom() {
        let r = Rect::new(10.0, 20.0, 100.0, 200.0);
        assert_eq!(r.right(), 110.0);
        assert_eq!(r.bottom(), 220.0);
    }

    #[test]
    fn rect_center() {
        let r = Rect::new(0.0, 0.0, 100.0, 200.0);
        let c = r.center();
        assert_eq!(c.x, 50.0);
        assert_eq!(c.y, 100.0);
    }

    #[test]
    fn rect_center_offset_origin() {
        let r = Rect::new(10.0, 20.0, 80.0, 60.0);
        let c = r.center();
        assert_eq!(c.x, 50.0);
        assert_eq!(c.y, 50.0);
    }

    // --- Rect::contains ---

    #[test]
    fn contains_interior_point() {
        let r = Rect::new(0.0, 0.0, 100.0, 100.0);
        assert!(r.contains(Point::new(50.0, 50.0)));
    }

    #[test]
    fn contains_origin_corner() {
        let r = Rect::new(0.0, 0.0, 100.0, 100.0);
        assert!(r.contains(Point::new(0.0, 0.0)));
    }

    #[test]
    fn contains_far_corner() {
        let r = Rect::new(0.0, 0.0, 100.0, 100.0);
        assert!(r.contains(Point::new(100.0, 100.0)));
    }

    #[test]
    fn contains_outside_right() {
        let r = Rect::new(0.0, 0.0, 100.0, 100.0);
        assert!(!r.contains(Point::new(100.1, 50.0)));
    }

    #[test]
    fn contains_outside_left() {
        let r = Rect::new(10.0, 10.0, 50.0, 50.0);
        assert!(!r.contains(Point::new(9.9, 20.0)));
    }

    #[test]
    fn contains_outside_top() {
        let r = Rect::new(0.0, 10.0, 100.0, 100.0);
        assert!(!r.contains(Point::new(50.0, 9.9)));
    }

    #[test]
    fn contains_outside_bottom() {
        let r = Rect::new(0.0, 0.0, 100.0, 100.0);
        assert!(!r.contains(Point::new(50.0, 100.1)));
    }

    #[test]
    fn contains_zero_size_rect_at_origin() {
        let r = Rect::new(5.0, 5.0, 0.0, 0.0);
        // The degenerate rect's only "contained" point is its own origin.
        assert!(r.contains(Point::new(5.0, 5.0)));
        assert!(!r.contains(Point::new(5.1, 5.0)));
    }

    // --- Rect::split_horizontal ---

    #[test]
    fn split_horizontal_zero_returns_empty() {
        let r = Rect::new(0.0, 0.0, 100.0, 50.0);
        assert!(r.split_horizontal(0).is_empty());
    }

    #[test]
    fn split_horizontal_one_returns_self() {
        let r = Rect::new(0.0, 0.0, 100.0, 50.0);
        let tiles = r.split_horizontal(1);
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0], r);
    }

    #[test]
    fn split_horizontal_two_equal_halves() {
        let r = Rect::new(0.0, 0.0, 100.0, 50.0);
        let tiles = r.split_horizontal(2);
        assert_eq!(tiles.len(), 2);
        assert_eq!(tiles[0], Rect::new(0.0, 0.0, 50.0, 50.0));
        assert_eq!(tiles[1], Rect::new(50.0, 0.0, 50.0, 50.0));
    }

    #[test]
    fn split_horizontal_origin_preserved() {
        let r = Rect::new(10.0, 20.0, 90.0, 30.0);
        let tiles = r.split_horizontal(3);
        assert_eq!(tiles.len(), 3);
        // All tiles should share the original y and height.
        for tile in &tiles {
            assert_eq!(tile.y(), 20.0);
            assert_eq!(tile.height(), 30.0);
        }
        // x origins should advance by tile_w = 30.
        assert!((tiles[0].x() - 10.0).abs() < 1e-5);
        assert!((tiles[1].x() - 40.0).abs() < 1e-5);
        assert!((tiles[2].x() - 70.0).abs() < 1e-5);
    }

    #[test]
    fn split_horizontal_total_width_preserved() {
        let r = Rect::new(0.0, 0.0, 100.0, 50.0);
        let tiles = r.split_horizontal(4);
        let total: f32 = tiles.iter().map(|t| t.width()).sum();
        assert!((total - 100.0).abs() < 1e-4);
    }

    // --- Rect::split_vertical ---

    #[test]
    fn split_vertical_zero_returns_empty() {
        let r = Rect::new(0.0, 0.0, 100.0, 50.0);
        assert!(r.split_vertical(0).is_empty());
    }

    #[test]
    fn split_vertical_one_returns_self() {
        let r = Rect::new(0.0, 0.0, 100.0, 50.0);
        let tiles = r.split_vertical(1);
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0], r);
    }

    #[test]
    fn split_vertical_two_equal_halves() {
        let r = Rect::new(0.0, 0.0, 100.0, 80.0);
        let tiles = r.split_vertical(2);
        assert_eq!(tiles.len(), 2);
        assert_eq!(tiles[0], Rect::new(0.0, 0.0, 100.0, 40.0));
        assert_eq!(tiles[1], Rect::new(0.0, 40.0, 100.0, 40.0));
    }

    #[test]
    fn split_vertical_total_height_preserved() {
        let r = Rect::new(0.0, 0.0, 100.0, 90.0);
        let tiles = r.split_vertical(3);
        let total: f32 = tiles.iter().map(|t| t.height()).sum();
        assert!((total - 90.0).abs() < 1e-4);
    }

    // --- Rect::grid ---

    #[test]
    fn grid_zero_cols_returns_empty() {
        let r = Rect::new(0.0, 0.0, 100.0, 100.0);
        assert!(r.grid(0, 2).is_empty());
    }

    #[test]
    fn grid_zero_rows_returns_empty() {
        let r = Rect::new(0.0, 0.0, 100.0, 100.0);
        assert!(r.grid(2, 0).is_empty());
    }

    #[test]
    fn grid_1x1_returns_self() {
        let r = Rect::new(0.0, 0.0, 100.0, 100.0);
        let cells = r.grid(1, 1);
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0], r);
    }

    #[test]
    fn grid_2x2_cell_count() {
        let r = Rect::new(0.0, 0.0, 100.0, 100.0);
        assert_eq!(r.grid(2, 2).len(), 4);
    }

    #[test]
    fn grid_3x4_cell_count() {
        let r = Rect::new(0.0, 0.0, 300.0, 400.0);
        assert_eq!(r.grid(3, 4).len(), 12);
    }

    #[test]
    fn grid_cell_sizes() {
        let r = Rect::new(0.0, 0.0, 90.0, 60.0);
        let cells = r.grid(3, 2);
        for cell in &cells {
            assert!((cell.width() - 30.0).abs() < 1e-4);
            assert!((cell.height() - 30.0).abs() < 1e-4);
        }
    }

    #[test]
    fn grid_row_major_ordering() {
        // grid() iterates rows first, then columns.
        let r = Rect::new(0.0, 0.0, 100.0, 100.0);
        let cells = r.grid(2, 2);
        // Row 0: cells[0] top-left, cells[1] top-right
        assert_eq!(cells[0].origin, Point::new(0.0, 0.0));
        assert_eq!(cells[1].origin, Point::new(50.0, 0.0));
        // Row 1: cells[2] bottom-left, cells[3] bottom-right
        assert_eq!(cells[2].origin, Point::new(0.0, 50.0));
        assert_eq!(cells[3].origin, Point::new(50.0, 50.0));
    }

    #[test]
    fn grid_offset_origin() {
        let r = Rect::new(20.0, 10.0, 60.0, 40.0);
        let cells = r.grid(2, 2);
        assert_eq!(cells[0].origin, Point::new(20.0, 10.0));
        assert_eq!(cells[1].origin, Point::new(50.0, 10.0));
        assert_eq!(cells[2].origin, Point::new(20.0, 30.0));
        assert_eq!(cells[3].origin, Point::new(50.0, 30.0));
    }

    // --- Rect::to_ndc ---

    #[test]
    fn to_ndc_full_viewport() {
        // A rect covering the full viewport should map to NDC [-1,1] x [-1,1].
        let vp = Size::new(800.0, 600.0);
        let r = Rect::new(0.0, 0.0, 800.0, 600.0);
        let ndc = r.to_ndc(vp);
        assert!((ndc[0] - (-1.0)).abs() < 1e-5); // x0 = -1
        assert!((ndc[1] - 1.0).abs() < 1e-5); // y0 = +1 (top in NDC)
        assert!((ndc[2] - 1.0).abs() < 1e-5); // x1 = +1
        assert!((ndc[3] - (-1.0)).abs() < 1e-5); // y1 = -1 (bottom in NDC)
    }

    #[test]
    fn to_ndc_center_rect() {
        // A rect at the center of the viewport should map to NDC [0,0] x [0,0]...
        // Actually a single-pixel center point: x=400, y=300, w=0, h=0
        let vp = Size::new(800.0, 600.0);
        let r = Rect::new(400.0, 300.0, 0.0, 0.0);
        let ndc = r.to_ndc(vp);
        assert!((ndc[0] - 0.0).abs() < 1e-5);
        assert!((ndc[1] - 0.0).abs() < 1e-5);
    }
}
