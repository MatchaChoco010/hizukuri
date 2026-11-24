//! Linearly transformed cosines for area lights: fitting the tables, and
//! the CPU side of checking them.
//!
//! - [`fit`]: fits the distribution to the GGX BRDF ([`ggx`]) over a grid
//!   of roughness and view angle.
//! - [`table`]: the two tables a shader looks up, and their files.
//! - [`shade`]: shading with the tables on the CPU, as the shader does.
//! - [`brute`]: the same light integrated by brute force with the BRDF.
//! - [`reads`]: where on a light a lobe's weight is, measured.
//! - [`cases`]: the points, views and lights the checks compare on.

pub mod brute;
pub mod cases;
pub mod fit;
pub mod ggx;
pub mod ltc;
pub mod reads;
pub mod shade;
pub mod table;
pub mod vec;
