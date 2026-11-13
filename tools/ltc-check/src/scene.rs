//! The scenes ltc-check draws: a floor (z = 0, +z up) under quad lights,
//! and the picture the textured lights show.

use proto_ltc_fit::cases::Case;
use proto_ltc_fit::shade::{Box3, Quad};
use proto_ltc_fit::vec::{V3, cross, normalize, sub};

use crate::{SIGMA_MIPMAPS, SIGMA_PREFILTER};

pub struct Light {
    pub quad: Quad,
    pub intensity: f32,
    pub tint: [f32; 3],
}

pub struct Scene {
    pub eye: V3,
    pub target: V3,
    pub fov_degrees: f64,
    /// The floor's five bands along x go from the first roughness to the
    /// second.
    pub roughness: (f32, f32),
    pub albedo: [f32; 3],
    pub f0: [f32; 3],
    /// The blur of the mip levels of the lights' textures (the shader's
    /// `LTC_SIGMA_MIPMAPS` or `LTC_SIGMA_PREFILTER`).
    pub level_sigma: f32,
    pub lights: Vec<Light>,
    /// Boxes standing on the floor (up to four), and their albedo.
    pub boxes: Vec<Box3>,
    pub box_albedo: [f32; 3],
}

impl Scene {
    /// The shader's `Scene`: 624 bytes.
    pub fn bytes(&self, aspect: f32) -> Vec<u8> {
        let forward = normalize(sub(self.target, self.eye));
        let right = normalize(cross(forward, [0.0, 0.0, 1.0]));
        let up = cross(right, forward);
        let mut out = Vec::with_capacity(624);
        let mut row = |v: [f32; 3], w: f32| {
            for value in v.into_iter().chain([w]) {
                out.extend_from_slice(&value.to_ne_bytes());
            }
        };
        let f = |v: V3| v.map(|c| c as f32);
        row(
            f(self.eye),
            (self.fov_degrees.to_radians() * 0.5).tan() as f32,
        );
        row(f(forward), aspect);
        row(f(right), self.roughness.0);
        row(f(up), self.roughness.1);
        row(self.albedo, self.lights.len().min(4) as f32);
        row(self.f0, self.level_sigma);
        for index in 0..4 {
            match self.lights.get(index) {
                Some(light) => {
                    let quad = &light.quad;
                    row(f(quad.p[0]), light.intensity);
                    row(
                        f(sub(quad.p[1], quad.p[0])),
                        if quad.two_sided { 1.0 } else { 0.0 },
                    );
                    row(f(sub(quad.p[3], quad.p[0])), 0.0);
                    row(light.tint, 0.0);
                }
                None => (0..4).for_each(|_| row([0.0; 3], 0.0)),
            }
        }
        row(self.box_albedo, self.boxes.len().min(4) as f32);
        for index in 0..4 {
            match self.boxes.get(index) {
                Some(b) => {
                    row(f(b.center), 0.0);
                    b.half.iter().for_each(|half| row(f(*half), 0.0));
                }
                // A box of no size would divide by zero in the shader.
                None => (0..4).for_each(|_| row([1.0; 3], 0.0)),
            }
        }
        out
    }
}

/// A screen standing on the floor: `width` × `height`, its lower edge
/// `lift` above the floor and centred at (`x`, `y`), facing −y, then leaning
/// back by `lean` and turned about the vertical by `yaw` (degrees).
pub fn screen(x: f64, y: f64, width: f64, height: f64, lift: f64, lean: f64, yaw: f64) -> Quad {
    let (sin_lean, cos_lean) = lean.to_radians().sin_cos();
    let (sin_yaw, cos_yaw) = yaw.to_radians().sin_cos();
    let turn = |v: V3| -> V3 {
        [
            v[0] * cos_yaw - v[1] * sin_yaw,
            v[0] * sin_yaw + v[1] * cos_yaw,
            v[2],
        ]
    };
    // From the lower edge's centre: to the top left corner, along the top
    // edge, and down the left edge.
    let corner = turn([-width / 2.0, height * sin_lean, height * cos_lean]);
    let u = turn([width, 0.0, 0.0]);
    let v = turn([0.0, -height * sin_lean, -height * cos_lean]);
    let origin = [x + corner[0], y + corner[1], lift + corner[2]];
    let at = |s: f64, t: f64| -> V3 { std::array::from_fn(|c| origin[c] + s * u[c] + t * v[c]) };
    Quad {
        p: [at(0.0, 0.0), at(1.0, 0.0), at(1.0, 1.0), at(0.0, 1.0)],
        two_sided: false,
    }
}

/// The colour of the picture at a texture coordinate: a desktop with three
/// windows of different colours over a dark gradient.
pub fn desktop_at(u: f64, v: f64) -> [f64; 3] {
    let inside = |u0: f64, v0: f64, u1: f64, v1: f64| u >= u0 && u < u1 && v >= v0 && v < v1;
    if v >= 0.94 {
        return [0.1, 0.5, 0.9];
    }
    if inside(0.06, 0.12, 0.46, 0.8) {
        return if v < 0.18 {
            [0.25, 0.25, 0.3]
        } else {
            [0.9, 0.9, 0.9]
        };
    }
    if inside(0.54, 0.1, 0.94, 0.5) {
        return [1.0, 0.25, 0.05];
    }
    if inside(0.54, 0.58, 0.94, 0.9) {
        return [0.05, 0.8, 0.2];
    }
    [0.02 + 0.08 * v, 0.05 - 0.03 * v, 0.2 - 0.05 * v]
}

/// The picture as `width` × `height` RGBA texels.
pub fn desktop(width: u32, height: u32) -> Vec<[f32; 4]> {
    desktop_scrolled(width, height, 0)
}

/// The picture moved to the right by `shift` texels, what leaves on the
/// right coming back on the left.
pub fn desktop_scrolled(width: u32, height: u32, shift: u32) -> Vec<[f32; 4]> {
    let mut out = Vec::with_capacity((width * height) as usize);
    for y in 0..height {
        for x in 0..width {
            let c = desktop_at(
                (((x + width - shift % width) % width) as f64 + 0.5) / width as f64,
                (y as f64 + 0.5) / height as f64,
            );
            out.push([c[0] as f32, c[1] as f32, c[2] as f32, 1.0]);
        }
    }
    out
}

fn base(lights: Vec<Light>) -> Scene {
    Scene {
        eye: [0.0, -3.4, 1.3],
        target: [0.0, 0.6, 0.3],
        fov_degrees: 50.0,
        roughness: (0.1, 0.9),
        albedo: [0.5, 0.5, 0.5],
        f0: [0.04, 0.04, 0.04],
        level_sigma: SIGMA_MIPMAPS,
        lights,
        boxes: Vec::new(),
        box_albedo: [0.7, 0.6, 0.45],
    }
}

fn light(quad: Quad) -> Light {
    Light {
        quad,
        intensity: 1.0,
        tint: [1.0; 3],
    }
}

/// The ids of the lights' textures: a white one, and the desktop picture
/// with its levels made by `update_light`, by halving, and without levels.
pub struct Pictures {
    pub white: i64,
    pub prefiltered: i64,
    pub halved: i64,
    pub sharp: i64,
}

/// The pictures to look at: `(name, scene, the lights' textures)`.
pub fn shots(ids: &Pictures) -> Vec<(&'static str, Scene, Vec<i64>)> {
    let upright = || screen(0.0, 1.0, 1.6, 0.9, 0.05, 0.0, 0.0);
    let textured = |roughness: (f32, f32), level_sigma: f32| {
        let mut scene = base(vec![light(upright())]);
        scene.roughness = roughness;
        scene.level_sigma = level_sigma;
        scene
    };
    let mut four = bench(4);
    (four.eye, four.target) = ([0.0, -3.4, 1.3], [0.0, 0.6, 0.3]);
    vec![
        // A white screen over a floor whose roughness goes from 0.1 (left
        // band) to 0.9 (right band).
        ("roughness", base(vec![light(upright())]), vec![ids.white]),
        // The screen leaning back and turned.
        (
            "tilted",
            base(vec![light(screen(0.0, 1.0, 1.6, 0.9, 0.05, 35.0, 25.0))]),
            vec![ids.white],
        ),
        // The screen showing the desktop picture, over the bands...
        (
            "textured",
            textured((0.1, 0.9), SIGMA_PREFILTER),
            vec![ids.prefiltered],
        ),
        // ...and over one roughness, with the three ways of making the
        // levels of the light's texture.
        (
            "textured-0.3-prefilter",
            textured((0.3, 0.3), SIGMA_PREFILTER),
            vec![ids.prefiltered],
        ),
        (
            "textured-0.3-mipmaps",
            textured((0.3, 0.3), SIGMA_MIPMAPS),
            vec![ids.halved],
        ),
        (
            "textured-0.3-no-levels",
            textured((0.3, 0.3), SIGMA_MIPMAPS),
            vec![ids.sharp],
        ),
        ("four-lights", four, vec![ids.prefiltered; 4]),
        // What the frame time is measured on: the floor fills the frame.
        ("four-lights-from-above", bench(4), vec![ids.prefiltered; 4]),
        // Boxes on the floor keep the screens' light from it.
        ("shadows", shadows(1), vec![ids.prefiltered]),
        ("shadows-two-lights", shadows(2), vec![ids.prefiltered; 2]),
    ]
}

/// A screen showing a picture, seen from a few points of the floor in front
/// of it, for a few roughnesses.
pub fn textured_cases() -> Vec<Case> {
    let quad = screen(0.0, 1.0, 1.6, 0.9, 0.05, 0.0, 0.0);
    let mut out = Vec::new();
    for (x, y) in [
        (-0.6, 0.5),
        (0.0, 0.5),
        (0.6, 0.5),
        (0.0, -0.5),
        (-1.2, 0.2),
    ] {
        for roughness in [0.15, 0.3, 0.6] {
            out.push(Case {
                name: format!("floor ({x}, {y}); roughness {roughness}"),
                position: [x, y, 0.0],
                normal: [0.0, 0.0, 1.0],
                view: normalize([0.0, -1.0, 1.0]),
                roughness,
                quad,
            });
        }
    }
    out
}

/// One or two screens and three boxes standing in front of them.
fn shadows(screens: usize) -> Scene {
    let lights = if screens == 1 {
        vec![light(screen(0.0, 1.0, 1.6, 0.9, 0.05, 0.0, 0.0))]
    } else {
        vec![
            light(screen(-0.85, 1.1, 1.6, 0.9, 0.05, 5.0, -12.0)),
            light(screen(0.85, 1.1, 1.6, 0.9, 0.05, 5.0, 12.0)),
        ]
    };
    let mut scene = base(lights);
    scene.roughness = (0.35, 0.35);
    scene.level_sigma = SIGMA_PREFILTER;
    (scene.eye, scene.target) = ([0.4, -2.6, 1.7], [0.0, 0.4, 0.1]);
    scene.boxes = boxes();
    scene
}

/// Three boxes standing on the floor in front of the screens: a wide low
/// one, a tall one, and one turned about the vertical.
pub fn boxes() -> Vec<Box3> {
    let (sin, cos) = 30.0_f64.to_radians().sin_cos();
    vec![
        Box3 {
            center: [-0.45, 0.3, 0.04],
            half: [[0.3, 0.0, 0.0], [0.0, 0.1, 0.0], [0.0, 0.0, 0.04]],
        },
        Box3 {
            center: [0.3, 0.45, 0.2],
            half: [[0.08, 0.0, 0.0], [0.0, 0.08, 0.0], [0.0, 0.0, 0.2]],
        },
        Box3 {
            center: [0.75, 0.1, 0.07],
            half: [
                [0.16 * cos, 0.16 * sin, 0.0],
                [-0.11 * sin, 0.11 * cos, 0.0],
                [0.0, 0.0, 0.07],
            ],
        },
    ]
}

/// The scene the frame time is measured on: up to four screens in an arc,
/// seen from above, so that the lit floor fills the frame.
pub fn bench(count: usize) -> Scene {
    let screens = [
        screen(-0.85, 1.3, 1.6, 0.9, 0.05, 5.0, -8.0),
        screen(0.85, 1.3, 1.6, 0.9, 0.05, 5.0, 8.0),
        screen(-2.45, 0.9, 1.6, 0.9, 0.05, 5.0, -30.0),
        screen(2.45, 0.9, 1.6, 0.9, 0.05, 5.0, 30.0),
    ];
    let mut scene = base(screens.into_iter().take(count).map(light).collect());
    scene.roughness = (0.25, 0.25);
    scene.level_sigma = SIGMA_PREFILTER;
    (scene.eye, scene.target) = ([0.0, -2.2, 3.2], [0.0, 0.2, 0.0]);
    scene
}
