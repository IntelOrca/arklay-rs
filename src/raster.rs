//! The reference's 3D triangle pipeline, reproduced step for step.
//!
//! The reference draws every model triangle through one GL 3.3 shader pair
//! (`MarniDX_GL.cpp`: `DrawTriangles3D`) and the oracle runs it on Mesa's
//! llvmpipe. Which pixels a triangle covers, which texel each pixel samples
//! and how its colour rounds all fall out of llvmpipe's arithmetic, so this
//! module follows it operation by operation in 32-bit floats:
//!
//! * the vertex shader's ortho matrix and its perspective premultiply by `w`,
//!   then the draw module's `1/w` divide and viewport transform;
//! * triangle setup: positions snapped to 1/256 pixel, fixed-point edge
//!   functions with GL's bottom-left fill rule on a Y-down framebuffer, and
//!   plane coefficients for every attribute at the framebuffer origin;
//! * per-pixel plane evaluation with fused multiply-adds and the perspective
//!   divide by the interpolated `1/w`.
//!
//! Sampling, the depth test and blending stay with the caller, which gets one
//! [`Fragment`] per covered pixel in the triangle.

/// One vertex as the reference hands it to `DrawTriangles3D`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlVertex {
    /// Game-space screen position (320x240, Y down).
    pub x: f32,
    pub y: f32,
    /// Normalised depth in `[0, 1]` (`TmdDepthNdc`).
    pub depth: f32,
    /// View-space Z, the perspective divisor.
    pub w: f32,
    /// Normalised texture coordinates.
    pub u: f32,
    pub v: f32,
    /// Vertex colour, multiplied into the texel.
    pub color: [f32; 4],
}

/// One covered pixel with its interpolated attributes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fragment {
    pub x: usize,
    pub y: usize,
    /// Window depth converted to the 24-bit depth buffer's integer scale.
    pub depth: u32,
    pub u: f32,
    pub v: f32,
    pub color: [f32; 4],
}

/// The largest 24-bit depth value, the cleared depth buffer.
pub const DEPTH_CLEAR: u32 = 0x00FF_FFFF;

/// Subpixel precision of llvmpipe's rasteriser (`FIXED_ORDER`).
const FIXED_ORDER: u32 = 8;
const FIXED_ONE: f32 = (1 << FIXED_ORDER) as f32;

/// A post-viewport vertex: window position, window depth, `1/w` and the
/// perspective-premultiplied attributes (`u, v, r, g, b, a`, each times `1/w`).
#[derive(Clone, Copy)]
struct SetupVertex {
    position: [f32; 4],
    attributes: [f32; 6],
}

/// The vertex shader and the draw module's viewport stage for a
/// `width` x `height` target.
fn transform(vertex: &GlVertex, width: u32, height: u32) -> SetupVertex {
    // `uMVP`: a Y-down ortho mapping game pixels onto clip space.
    let scale_x = 2.0f32 / width as f32;
    let scale_y = -2.0f32 / height as f32;
    let clip_x = vertex.x * scale_x + -1.0;
    let clip_y = vertex.y * scale_y + 1.0;
    let clip_z = vertex.depth * 2.0 - 1.0;
    // `uPersp`: premultiply by w (clamped like the shader) so the divide
    // restores the same position while the attributes interpolate in 1/w.
    let w = vertex.w.max(1e-4);
    let (clip_x, clip_y, clip_z) = (clip_x * w, clip_y * w, clip_z * w);
    let one_over_w = 1.0 / w;
    let half_width = width as f32 * 0.5;
    let half_height = height as f32 * 0.5;
    // The window-system framebuffer is Y_0_TOP, so the state tracker flips
    // the viewport's Y scale.
    let position = [
        (clip_x * one_over_w).mul_add(half_width, half_width),
        (clip_y * one_over_w).mul_add(-half_height, half_height),
        (clip_z * one_over_w).mul_add(0.5, 0.5),
        one_over_w,
    ];
    let source = [
        vertex.u,
        vertex.v,
        vertex.color[0],
        vertex.color[1],
        vertex.color[2],
        vertex.color[3],
    ];
    SetupVertex {
        position,
        attributes: source.map(|value| value * one_over_w),
    }
}

/// `subpixel_snap` after the half-pixel offset: SSE's `cvtps2dq` rounds to
/// nearest even.
fn snap(coordinate: f32) -> i32 {
    ((coordinate - 0.5) * FIXED_ONE).round_ties_even() as i32
}

/// The plane coefficients `(a0, dadx, dady)` of one attribute, computed like
/// `calc_coef4` against the triangle's origin terms.
#[derive(Clone, Copy)]
struct Plane {
    a0: f32,
    dadx: f32,
    dady: f32,
}

impl Plane {
    /// The attribute at pixel `(x, y)` (`interp_attrib_linear`: two fused
    /// multiply-adds from the origin value).
    fn at(self, x: f32, y: f32) -> f32 {
        self.dady.mul_add(y, self.dadx.mul_add(x, self.a0))
    }
}

struct Gradients {
    x0_center: f32,
    y0_center: f32,
    dx01_ooa: f32,
    dy01_ooa: f32,
    dx20_ooa: f32,
    dy20_ooa: f32,
}

impl Gradients {
    fn new(v0: [f32; 4], v1: [f32; 4], v2: [f32; 4]) -> Self {
        let dx01 = v0[0] - v1[0];
        let dy01 = v0[1] - v1[1];
        let dx20 = v2[0] - v0[0];
        let dy20 = v2[1] - v0[1];
        let e = dx01 * dy20;
        let f = dy01 * dx20;
        let ooa = 1.0 / (e - f);
        Self {
            x0_center: v0[0] - 0.5,
            y0_center: v0[1] - 0.5,
            dx01_ooa: dx01 * ooa,
            dy01_ooa: dy01 * ooa,
            dx20_ooa: dx20 * ooa,
            dy20_ooa: dy20 * ooa,
        }
    }

    fn plane(&self, a0: f32, a1: f32, a2: f32) -> Plane {
        let da01 = a0 - a1;
        let da20 = a2 - a0;
        let dadx = da01 * self.dy20_ooa - da20 * self.dy01_ooa;
        let dady = da20 * self.dx01_ooa - da01 * self.dx20_ooa;
        let origin = dadx * self.x0_center + dady * self.y0_center;
        Plane {
            a0: a0 - origin,
            dadx,
            dady,
        }
    }
}

/// One fixed-point half-edge function, `c - dcdx * X + dcdy * Y > 0` inside.
struct Edge {
    c: i64,
    dcdx: i64,
    dcdy: i64,
}

impl Edge {
    fn new(from: [i32; 2], to: [i32; 2]) -> Self {
        let dcdx = i64::from(from[1]) - i64::from(to[1]);
        let dcdy = i64::from(from[0]) - i64::from(to[0]);
        let mut c = dcdx * i64::from(from[0]) - dcdy * i64::from(from[1]);
        // The bottom-left fill convention (`bottom_edge_rule`): left edges and
        // horizontal edges running right to left own their samples.
        if dcdx < 0 || (dcdx == 0 && dcdy < 0) {
            c += 1;
        }
        Self { c, dcdx, dcdy }
    }

    fn inside(&self, x: i64, y: i64) -> bool {
        self.c - self.dcdx * (x << FIXED_ORDER) + self.dcdy * (y << FIXED_ORDER) > 0
    }
}

/// Rasterize one triangle into a `width` x `height` target, calling
/// `fragment` once per covered pixel.
pub fn draw_triangle(
    vertices: &[GlVertex; 3],
    width: u32,
    height: u32,
    mut fragment: impl FnMut(Fragment),
) {
    let mut setup = vertices
        .each_ref()
        .map(|vertex| transform(vertex, width, height));
    if setup
        .iter()
        .any(|vertex| vertex.position.iter().any(|value| !value.is_finite()))
    {
        return;
    }
    let mut fixed = setup.map(|vertex| [snap(vertex.position[0]), snap(vertex.position[1])]);
    let area = (i64::from(fixed[0][0]) - i64::from(fixed[1][0]))
        * (i64::from(fixed[2][1]) - i64::from(fixed[0][1]))
        - (i64::from(fixed[2][0]) - i64::from(fixed[0][0]))
            * (i64::from(fixed[0][1]) - i64::from(fixed[1][1]));
    if area == 0 {
        return;
    }
    if area < 0 {
        // `triangle_both` rotates a clockwise triangle to counter-clockwise by
        // swapping its first two vertices (the provoking vertex is the last).
        setup.swap(0, 1);
        fixed.swap(0, 1);
    }

    let gradients = Gradients::new(setup[0].position, setup[1].position, setup[2].position);
    let position_plane = |index: usize| {
        gradients.plane(
            setup[0].position[index],
            setup[1].position[index],
            setup[2].position[index],
        )
    };
    let depth_plane = position_plane(2);
    let w_plane = position_plane(3);
    let attribute_planes: [Plane; 6] = std::array::from_fn(|index| {
        gradients.plane(
            setup[0].attributes[index],
            setup[1].attributes[index],
            setup[2].attributes[index],
        )
    });

    let edges = [
        Edge::new(fixed[0], fixed[1]),
        Edge::new(fixed[1], fixed[2]),
        Edge::new(fixed[2], fixed[0]),
    ];

    // The bounding box in whole pixels (`do_triangle_ccw`, bottom-left rule).
    let min_x = fixed.iter().map(|p| p[0]).min().unwrap_or(0) >> FIXED_ORDER;
    let max_x = (fixed.iter().map(|p| p[0]).max().unwrap_or(0) - 1) >> FIXED_ORDER;
    let min_y = (fixed.iter().map(|p| p[1]).min().unwrap_or(0) + 1) >> FIXED_ORDER;
    let max_y = fixed.iter().map(|p| p[1]).max().unwrap_or(0) >> FIXED_ORDER;
    let min_x = i64::from(min_x).max(0);
    let min_y = i64::from(min_y).max(0);
    let max_x = i64::from(max_x).min(i64::from(width) - 1);
    let max_y = i64::from(max_y).min(i64::from(height) - 1);

    for y in min_y..=max_y {
        for x in min_x..=max_x {
            if !edges.iter().all(|edge| edge.inside(x, y)) {
                continue;
            }
            let (fx, fy) = (x as f32, y as f32);
            let depth = depth_plane.at(fx, fy).clamp(0.0, 1.0);
            let w = w_plane.at(fx, fy);
            let recip = 1.0 / w;
            let value = |index: usize| attribute_planes[index].at(fx, fy) * recip;
            fragment(Fragment {
                x: x as usize,
                y: y as usize,
                depth: (depth * DEPTH_CLEAR as f32).round_ties_even() as u32,
                u: value(0),
                v: value(1),
                color: [value(2), value(3), value(4), value(5)],
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vertex(x: f32, y: f32) -> GlVertex {
        GlVertex {
            x,
            y,
            depth: 0.5,
            w: 1000.0,
            u: x / 320.0,
            v: y / 240.0,
            color: [1.0; 4],
        }
    }

    fn covered(vertices: [GlVertex; 3]) -> Vec<(usize, usize)> {
        let mut pixels = Vec::new();
        draw_triangle(&vertices, 320, 240, |fragment| {
            pixels.push((fragment.x, fragment.y))
        });
        pixels.sort_unstable();
        pixels
    }

    #[test]
    fn covers_pixel_centres_inside_the_triangle_either_winding() {
        let clockwise = covered([vertex(0.0, 0.0), vertex(4.0, 0.0), vertex(0.0, 4.0)]);
        let counter = covered([vertex(0.0, 0.0), vertex(0.0, 4.0), vertex(4.0, 0.0)]);
        assert_eq!(clockwise, counter);
        // Centres strictly inside x + y < 4, plus the owned diagonal samples.
        assert!(clockwise.contains(&(0, 0)));
        assert!(clockwise.contains(&(2, 0)));
        assert!(!clockwise.contains(&(3, 3)));
        assert!(!clockwise.is_empty());
    }

    #[test]
    fn a_shared_edge_assigns_each_sample_to_exactly_one_triangle() {
        // A quad split along its diagonal through pixel centres.
        let a = covered([vertex(0.5, 0.5), vertex(8.5, 0.5), vertex(8.5, 8.5)]);
        let b = covered([vertex(0.5, 0.5), vertex(8.5, 8.5), vertex(0.5, 8.5)]);
        for pixel in &a {
            assert!(!b.contains(pixel), "{pixel:?} drawn twice");
        }
        assert_eq!(a.len() + b.len(), 64);
    }

    #[test]
    fn attributes_interpolate_to_their_vertex_values() {
        let mut seen = None;
        let vertices = [vertex(0.0, 0.0), vertex(16.0, 0.0), vertex(0.0, 16.0)];
        draw_triangle(&vertices, 320, 240, |fragment| {
            if fragment.x == 2 && fragment.y == 3 {
                seen = Some(fragment);
            }
        });
        let fragment = seen.expect("pixel (2, 3) is inside");
        assert!((fragment.u - 2.5 / 320.0).abs() < 1e-6);
        assert!((fragment.v - 3.5 / 240.0).abs() < 1e-6);
        assert!((fragment.color[0] - 1.0).abs() < 1e-6);
    }
}
