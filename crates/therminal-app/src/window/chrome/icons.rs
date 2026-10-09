//! Font-independent control icons, drawn with the existing chrome triangle pipeline.

use crate::grid_renderer::ColorVertex;

#[derive(Clone, Copy)]
pub(super) enum Icon {
    Settings,
    Minimize,
    Maximize,
    Restore,
    Close,
    SplitColumns,
    SplitRows,
}

/// Append an icon centered in a button's pixel bounds. The 12-unit design grid
/// keeps stroke weight and proportions consistent across window and pane headers.
pub(super) fn append_icon(
    vertices: &mut Vec<ColorVertex>,
    icon: Icon,
    bounds: [f32; 4],
    surface: [f32; 2],
    color: [f32; 4],
) {
    let [x, y, width, height] = bounds;
    let size = (height * 0.65).min(14.0).min(width - 4.0);
    if size <= 0.0 || surface.iter().any(|v| *v <= 0.0) {
        return;
    }
    let scale = size / 12.0;
    let left = (x + (width - size) / 2.0).round();
    let top = (y + (height - size) / 2.0).round();
    let mut line = |a: [f32; 2], b: [f32; 2]| {
        let a = [left + a[0] * scale, top + a[1] * scale];
        let b = [left + b[0] * scale, top + b[1] * scale];
        let dx = b[0] - a[0];
        let dy = b[1] - a[1];
        let length = dx.hypot(dy);
        let half_stroke = scale.max(1.0) * 0.5;
        let normal = [-dy / length * half_stroke, dx / length * half_stroke];
        let points = [
            [a[0] + normal[0], a[1] + normal[1]],
            [b[0] + normal[0], b[1] + normal[1]],
            [a[0] - normal[0], a[1] - normal[1]],
            [b[0] - normal[0], b[1] - normal[1]],
        ];
        for i in [0, 1, 2, 1, 3, 2] {
            vertices.push(ColorVertex {
                position: [
                    points[i][0] / surface[0] * 2.0 - 1.0,
                    1.0 - points[i][1] / surface[1] * 2.0,
                ],
                color,
            });
        }
    };
    let outline = |line: &mut dyn FnMut([f32; 2], [f32; 2]), l, t, r, b| {
        line([l, t], [r, t]);
        line([r, t], [r, b]);
        line([r, b], [l, b]);
        line([l, b], [l, t]);
    };
    match icon {
        Icon::Settings => {
            // Three adjustment sliders, with gaps around their handles.
            for (y, handle) in [(2.0, 4.0), (6.0, 8.0), (10.0, 4.0)] {
                line([1.0, y], [handle - 1.0, y]);
                line([handle + 1.0, y], [11.0, y]);
                line([handle, y - 1.5], [handle, y + 1.5]);
            }
        }
        Icon::Minimize => line([1.0, 9.0], [11.0, 9.0]),
        Icon::Close => {
            line([2.0, 2.0], [10.0, 10.0]);
            line([10.0, 2.0], [2.0, 10.0]);
        }
        Icon::Restore => {
            line([4.0, 3.0], [4.0, 1.0]);
            line([4.0, 1.0], [11.0, 1.0]);
            line([11.0, 1.0], [11.0, 8.0]);
            line([11.0, 8.0], [9.0, 8.0]);
            outline(&mut line, 1.0, 4.0, 8.0, 11.0);
        }
        Icon::Maximize | Icon::SplitColumns | Icon::SplitRows => {
            outline(&mut line, 1.0, 1.0, 11.0, 11.0);
            match icon {
                Icon::SplitColumns => line([6.0, 1.0], [6.0, 11.0]),
                Icon::SplitRows => line([1.0, 6.0], [11.0, 6.0]),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_stay_inside_their_hit_targets_without_font_system() {
        for height in [18.0, 28.0, 40.0] {
            for icon in [
                Icon::Settings,
                Icon::Minimize,
                Icon::Maximize,
                Icon::Restore,
                Icon::Close,
                Icon::SplitColumns,
                Icon::SplitRows,
            ] {
                let mut vertices = Vec::new();
                append_icon(
                    &mut vertices,
                    icon,
                    [100.0, 50.0, 24.0, height],
                    [800.0, 600.0],
                    [1.0; 4],
                );
                assert!(!vertices.is_empty());
                for vertex in vertices {
                    let x = (vertex.position[0] + 1.0) * 400.0;
                    let y = (1.0 - vertex.position[1]) * 300.0;
                    assert!((100.0..124.0).contains(&x));
                    assert!((50.0..50.0 + height).contains(&y));
                }
            }
        }
    }
}
