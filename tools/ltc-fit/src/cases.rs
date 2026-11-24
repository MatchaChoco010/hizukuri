//! The configurations the checks compare on: a point of a floor (the
//! normal is +z), a view direction, a roughness and a quad light.

use crate::shade::{Box3, Quad};
use crate::vec::{V3, normalize};

#[derive(Clone, Debug)]
pub struct Case {
    pub name: String,
    pub position: V3,
    pub normal: V3,
    pub view: V3,
    pub roughness: f64,
    pub quad: Quad,
}

/// A quad from its (0, 0) corner and its two edges.
pub fn quad(origin: V3, u: V3, v: V3, two_sided: bool) -> Quad {
    let at = |s: f64, t: f64| -> V3 { std::array::from_fn(|c| origin[c] + s * u[c] + t * v[c]) };
    Quad {
        p: [at(0.0, 0.0), at(1.0, 0.0), at(1.0, 1.0), at(0.0, 1.0)],
        two_sided,
    }
}

/// The lights of the cases, by name.
pub fn lights() -> Vec<(&'static str, Quad)> {
    vec![
        // A 1 × 1 panel one unit above the floor, facing down.
        (
            "overhead",
            quad([-0.5, -0.5, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0], false),
        ),
        // A 1.6 × 0.9 screen standing on the floor's far side, facing the
        // viewer (−y), its lower edge 0.1 above the floor.
        (
            "screen",
            quad([-0.8, 1.0, 1.0], [1.6, 0.0, 0.0], [0.0, 0.0, -0.9], false),
        ),
        // The same screen leaning back by 30°, sunk so that its lower part
        // is below the floor: the horizon cuts it.
        (
            "sunk screen",
            quad(
                [-0.8, 1.0 + 0.45, 0.5],
                [1.6, 0.0, 0.0],
                [0.0, -0.45, -0.779_422_9],
                false,
            ),
        ),
    ]
}

/// A box standing on the floor in front of the screen of [`lights`]: 0.4
/// wide, 0.24 deep, 0.2 high.
pub const OCCLUDER: Box3 = Box3 {
    center: [0.2, 0.55, 0.1],
    half: [[0.2, 0.0, 0.0], [0.0, 0.12, 0.0], [0.0, 0.0, 0.1]],
};

/// How the names of the cases on [`OCCLUDER`]'s own faces begin.
pub const OWN_FACE: &str = "the box's own face";

/// Floor points around [`OCCLUDER`], lit by the screen: in the box's shadow,
/// at its edge and beside it. Then points of the box's own faces, which it
/// does not shadow: on its top, on the side towards the screen, on a side
/// across, and (unlit) on the side away from the screen.
pub fn shadowed() -> Vec<Case> {
    let screen = lights()[1].1;
    let mut out = Vec::new();
    let own: [(&str, V3, V3); 6] = [
        ("top, middle", [0.2, 0.55, 0.2], [0.0, 0.0, 1.0]),
        ("top, near an edge", [0.01, 0.44, 0.2], [0.0, 0.0, 1.0]),
        ("top, a corner", [0.4, 0.67, 0.2], [0.0, 0.0, 1.0]),
        ("towards the screen", [0.2, 0.67, 0.1], [0.0, 1.0, 0.0]),
        ("across", [0.4, 0.55, 0.1], [1.0, 0.0, 0.0]),
        ("away from the screen", [0.2, 0.43, 0.1], [0.0, -1.0, 0.0]),
    ];
    for (name, position, normal) in own {
        out.push(Case {
            name: format!("{OWN_FACE}: {name}"),
            position,
            normal,
            view: normalize([normal[0], normal[1] - 0.5, normal[2] + 0.5]),
            roughness: 0.4,
            quad: screen,
        });
    }
    for x in [0.2, 0.0, -0.15, 0.45] {
        for y in [0.42, 0.3, 0.1, -0.3] {
            for roughness in [0.3, 0.6] {
                out.push(Case {
                    name: format!("floor ({x}, {y}); roughness {roughness}"),
                    position: [x, y, 0.0],
                    normal: [0.0, 0.0, 1.0],
                    view: normalize([0.0, -1.0, 1.0]),
                    roughness,
                    quad: screen,
                });
            }
        }
    }
    out
}

/// Every case: each light, seen from a few points and directions, for a
/// few roughnesses.
pub fn all() -> Vec<Case> {
    let mut out = Vec::new();
    // (name, position, view direction towards the eye)
    let views: [(&str, V3, V3); 4] = [
        ("below, view 0°", [0.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
        ("below, view 45°", [0.0, 0.0, 0.0], [0.0, -1.0, 1.0]),
        ("below, view 75°", [0.0, 0.0, 0.0], [0.0, -3.732, 1.0]),
        ("aside, view 60°", [0.9, 0.3, 0.0], [0.5, -1.65, 1.0]),
    ];
    for (light_name, light) in lights() {
        for (view_name, position, view) in views {
            for roughness in [0.1, 0.25, 0.5, 1.0] {
                out.push(Case {
                    name: format!("{light_name}; {view_name}; roughness {roughness}"),
                    position,
                    normal: [0.0, 0.0, 1.0],
                    view: normalize(view),
                    roughness,
                    quad: light,
                });
            }
        }
    }
    out
}
