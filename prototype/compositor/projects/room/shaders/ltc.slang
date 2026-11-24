// Area lights by linearly transformed cosines: `import ltc;`.
//
// A quad light (a monitor showing a window) lights a surface point. The
// GGX specular lobe at the point is approximated by a clamped cosine whose
// directions are transformed by a matrix that depends on the roughness and
// the view angle (Heitz, Dupuy, Hill and Neubelt, "Real-Time Polygonal-Light
// Shading with Linearly Transformed Cosines", 2016). In the space where the
// lobe is the plain cosine, the light's integral is the form factor of a
// polygon, which has a closed form. The matrices come from two tables
// (`assets/ltc_mat.f32`, `assets/ltc_amp.f32`; the tool `ltc-fit` makes them).
//
// The light's colour comes from its texture: one read per lobe, where the
// mean direction of the lobe's weight over the light meets the light, from
// the mip level whose blur matches the lobe's footprint there.
//
// Everything is in the working colour space, linear; a light's value of 1 is
// reference white. Positions and directions are in one space of the
// caller's choice (world space, say).
//
// Per shaded point:   LtcSurface s = ltc_surface(tables, position, normal, view, roughness);
// Per light:          LtcLight l = ltc_quad_textured(tables, s, quad, light, samp, LTC_SIGMA_MIPMAPS);
//                     colour += albedo * l.diffuse + ltc_specular_colour(s, f0) * l.specular;
module ltc;

static const float LTC_PI = 3.14159265358979;
// The tables are 64 × 64 texels.
static const float LTC_TABLE_SCALE = 63.0 / 64.0;
static const float LTC_TABLE_BIAS = 0.5 / 64.0;

// The blur of mip level k of a light's texture, as the standard deviation of
// a Gaussian in texels of that level, for the two ways the levels are made:
// by halving with a box filter (each level is then read with linear
// filtering), and by the prefilter of the `ltc` script module.
public static const float LTC_SIGMA_MIPMAPS = 0.5;
public static const float LTC_SIGMA_PREFILTER = 1.25;

// The blur a lobe reads the light with, over the distance to the point it
// reads. A clamped cosine falls on a plane facing it with the weight
// 1 / (1 + r²/d²)²; a Gaussian of 0.55 d matches it at half its height, and
// the weight's tail is heavier.
static const float LTC_BLUR_PER_DISTANCE = 0.6;

// The two tables and the sampler they are read with (linear, clamped).
public struct LtcTables {
    public Texture2D<float4> mat;
    public Texture2D<float4> amp;
    public SamplerState samp;

    public __init(Texture2D<float4> mat, Texture2D<float4> amp, SamplerState samp) {
        this.mat = mat;
        this.amp = amp;
        this.samp = samp;
    }

    float4 read_mat(float2 at) {
        return mat.SampleLevel(samp, at * LTC_TABLE_SCALE + LTC_TABLE_BIAS, 0.0);
    }

    float4 read_amp(float2 at) {
        return amp.SampleLevel(samp, at * LTC_TABLE_SCALE + LTC_TABLE_BIAS, 0.0);
    }
};

// What depends on the shaded point only. Two table reads.
public struct LtcSurface {
    public float3 position;
    // The frame: the view direction projected onto the surface, the
    // direction across it, the normal.
    public float3 t1;
    public float3 t2;
    public float3 normal;
    // The inverse matrix of the lobe: rows (x, 0, y), (0, 1, 0), (z, 0, w).
    public float4 inv;
    // The integral of the BRDF with F = 1, and its part weighted by
    // Schlick's (1 − v·h)^5.
    public float norm;
    public float fresnel;
};

// `view` points from the surface to the eye. `roughness` is the perceptual
// one (α is its square); below about 0.05 the lobe is too sharp for 32-bit
// arithmetic and for the light's texture, so clamp it there.
public LtcSurface ltc_surface(LtcTables tables, float3 position, float3 normal, float3 view, float roughness) {
    LtcSurface s;
    s.position = position;
    s.normal = normal;
    float cos_theta = saturate(dot(normal, view));
    float3 tangent = view - normal * dot(normal, view);
    float len2 = dot(tangent, tangent);
    if (len2 > 1e-10) {
        s.t1 = tangent * rsqrt(len2);
    } else {
        // The view is along the normal: any direction on the surface.
        float3 other = float3(1.0, 0.0, 0.0);
        if (abs(normal.x) >= 0.9) {
            other = float3(0.0, 1.0, 0.0);
        }
        s.t1 = normalize(cross(other, normal));
    }
    s.t2 = cross(normal, s.t1);
    float2 at = float2(saturate(roughness), sqrt(1.0 - cos_theta));
    s.inv = tables.read_mat(at);
    float4 amp = tables.read_amp(at);
    s.norm = amp.x;
    s.fresnel = amp.y;
    return s;
}

// What the specular term of a light is multiplied with: the BRDF's integral
// with Schlick's Fresnel term for the reflectance `f0` at normal incidence.
public float3 ltc_specular_colour(LtcSurface s, float3 f0) {
    return f0 * (s.norm - s.fresnel) + s.fresnel;
}

// A quad light: a parallelogram. The corners go around it: p0 is the
// texture's (0, 0), p1 its (1, 0), p2 its (1, 1), p3 its (0, 1). Its front
// is the side that sees them clockwise (the texture unmirrored, v down).
public struct LtcQuad {
    public float3 p0;
    public float3 p1;
    public float3 p2;
    public float3 p3;
    // Whether the back lights as well.
    public bool two_sided;

    public __init(float3 p0, float3 p1, float3 p2, float3 p3, bool two_sided) {
        this.p0 = p0;
        this.p1 = p1;
        this.p2 = p2;
        this.p3 = p3;
        this.two_sided = two_sided;
    }
};

// One lobe's share of a light.
public struct LtcLobe {
    // The integral of the lobe over the light: 0 to 1.
    public float form_factor;
    // Where the light's texture is read.
    public float2 uv;
    // The standard deviation of the blur it is read with, in units of the
    // square root of the light's area.
    public float blur;
    // The same along the texture's u and v, in texture coordinates: the
    // lobe's weight over the light is taken for a Gaussian of this size
    // about `uv`.
    public float2 spread;
};

public struct LtcLobes {
    public LtcLobe diffuse;
    public LtcLobe specular;
};

// What a point the light does not light gets.
LtcLobe ltc_no_lobe() {
    LtcLobe lobe;
    lobe.form_factor = 0.0;
    lobe.uv = float2(0.5, 0.5);
    lobe.blur = 1.0;
    lobe.spread = float2(1.0, 1.0);
    return lobe;
}

// θ / sin θ for the angle whose cosine is x. The integral along the edge
// between the unit vectors a and b is this times a × b.
float ltc_theta_over_sin(float x) {
    float e = 1.0 - x;
    if (e < 0.02) {
        // The series about θ = 0, where acos loses its digits.
        return 1.0 + e * (1.0 / 3.0 + e * (2.0 / 15.0 + e * (2.0 / 35.0)));
    }
    x = max(x, -0.9999);
    return acos(x) / sqrt(1.0 - x * x);
}

float3 ltc_edge(float3 a, float3 b) {
    return cross(a, b) * ltc_theta_over_sin(dot(a, b));
}

float3 ltc_direction(float3 v) {
    return v * rsqrt(max(dot(v, v), 1e-30));
}

// The corners of a light seen from the shaded point, in a space where the
// lobe is the clamped cosine about +z.
struct LtcCorners {
    float3 c0;
    float3 c1;
    float3 c2;
    float3 c3;
};

LtcCorners ltc_corners_diffuse(LtcSurface s, LtcQuad q) {
    float3 d0 = q.p0 - s.position;
    float3 d1 = q.p1 - s.position;
    float3 d2 = q.p2 - s.position;
    float3 d3 = q.p3 - s.position;
    LtcCorners c;
    c.c0 = float3(dot(s.t1, d0), dot(s.t2, d0), dot(s.normal, d0));
    c.c1 = float3(dot(s.t1, d1), dot(s.t2, d1), dot(s.normal, d1));
    c.c2 = float3(dot(s.t1, d2), dot(s.t2, d2), dot(s.normal, d2));
    c.c3 = float3(dot(s.t1, d3), dot(s.t2, d3), dot(s.normal, d3));
    return c;
}

float3 ltc_transform(float4 inv, float3 v) {
    return float3(inv.x * v.x + inv.y * v.z, v.y, inv.z * v.x + inv.w * v.z);
}

LtcCorners ltc_corners_specular(LtcSurface s, LtcCorners diffuse) {
    LtcCorners c;
    c.c0 = ltc_transform(s.inv, diffuse.c0);
    c.c1 = ltc_transform(s.inv, diffuse.c1);
    c.c2 = ltc_transform(s.inv, diffuse.c2);
    c.c3 = ltc_transform(s.inv, diffuse.c3);
    return c;
}

// The light's vector form factor: the integral of the directions to the
// light over π. It points at the light; its length is the light's form
// factor for a surface facing it, and its z is the form factor for the
// lobe when no part of the light is below the horizon. `facing`: 1 when the
// point sees the corners clockwise, −1 otherwise.
float3 ltc_vector_form_factor(LtcCorners c, float facing) {
    float3 a = ltc_direction(c.c0);
    float3 b = ltc_direction(c.c1);
    float3 d = ltc_direction(c.c2);
    float3 e = ltc_direction(c.c3);
    return (ltc_edge(a, b) + ltc_edge(b, d) + ltc_edge(d, e) + ltc_edge(e, a)) * (facing / (2.0 * LTC_PI));
}

// Where the lobe reads the light, and how blurred: where the mean direction
// of the lobe's weight over the light meets the light. The vector form
// factor `f` has the mean direction of the light's solid angle; the lobe
// weighs the directions by their cosine to +z, which moves the mean towards
// +z, to first order by the variance of the light's directions (about a
// quarter of the vector form factor's length, per axis) over the mean's
// cosine. Near the horizon the first order overshoots: the cosine is not
// taken below 0.6.
void ltc_read_point(LtcCorners c, float3 f, inout LtcLobe lobe) {
    float3 e1 = c.c1 - c.c0;
    float3 e2 = c.c3 - c.c0;
    float3 raw = cross(e1, e2);
    float area2 = max(dot(raw, raw), 1e-30);
    float len = length(f);
    float3 mean = f / max(len, 1e-30);
    float3 across = float3(0.0, 0.0, 1.0) - mean * mean.z;
    float3 towards = mean + across * (len / (4.0 * max(mean.z, 0.6)));
    float along = dot(raw, towards);
    float t = 0.0;
    if (abs(along) > 1e-30) {
        t = dot(raw, c.c0) / along;
    }
    if (t <= 0.0) {
        // No such point: the light as a whole.
        return;
    }
    float3 hit = towards * t;
    float3 w = hit - c.c0;
    float sigma = LTC_BLUR_PER_DISTANCE * length(hit);
    lobe.uv = float2(dot(cross(w, e2), raw), dot(cross(e1, w), raw)) / area2;
    lobe.blur = sigma * rsqrt(sqrt(area2));
    lobe.spread = sigma * float2(rsqrt(max(dot(e1, e1), 1e-30)), rsqrt(max(dot(e2, e2), 1e-30)));
}

// The lobe's integral over the light, the part below the horizon left out
// by the table's horizon term: the light's vector form factor `f` is taken
// for a sphere cap's, and the cap's clipped form factor is read. One table
// read.
float ltc_form_factor(LtcTables tables, float3 f) {
    float len = length(f);
    float z = f.z / max(len, 1e-7);
    return len * tables.read_amp(float2(z * 0.5 + 0.5, len)).w;
}

// The z of the integral along the part of the edge a → b that is above the
// horizon. Where the edge leaves or enters the upper half space is kept, to
// close the clipped polygon along the horizon afterwards.
float ltc_clipped_edge(float3 a, float3 b, inout float3 leaves, inout float3 enters, inout bool crosses) {
    bool a_up = a.z >= 0.0;
    bool b_up = b.z >= 0.0;
    if (!a_up && !b_up) {
        return 0.0;
    }
    float3 first = a;
    float3 last = b;
    if (a_up != b_up) {
        float3 at = a + (b - a) * (a.z / (a.z - b.z));
        at.z = 0.0;
        crosses = true;
        if (a_up) {
            last = at;
            leaves = at;
        } else {
            first = at;
            enters = at;
        }
    }
    return ltc_edge(ltc_direction(first), ltc_direction(last)).z;
}

// The lobe's integral over the light, the light clipped at the horizon. No
// table read; more arithmetic and more branches than `ltc_form_factor`.
float ltc_form_factor_clipped(LtcCorners c, float facing) {
    float3 leaves = float3(0.0, 0.0, 0.0);
    float3 enters = float3(0.0, 0.0, 0.0);
    bool crosses = false;
    float sum = ltc_clipped_edge(c.c0, c.c1, leaves, enters, crosses);
    sum += ltc_clipped_edge(c.c1, c.c2, leaves, enters, crosses);
    sum += ltc_clipped_edge(c.c2, c.c3, leaves, enters, crosses);
    sum += ltc_clipped_edge(c.c3, c.c0, leaves, enters, crosses);
    if (crosses) {
        sum += ltc_edge(ltc_direction(leaves), ltc_direction(enters)).z;
    }
    return max(sum * facing / (2.0 * LTC_PI), 0.0);
}

// 1 when the point is in front of the light, −1 behind it, 0 when the light
// does not light it.
float ltc_facing(LtcSurface s, LtcQuad q) {
    float side = dot(cross(q.p3 - q.p0, q.p1 - q.p0), s.position - q.p0);
    if (side > 0.0) {
        return 1.0;
    }
    return q.two_sided ? -1.0 : 0.0;
}

// The diffuse and the specular lobe's share of a quad light. Two table
// reads.
public LtcLobes ltc_quad(LtcTables tables, LtcSurface s, LtcQuad q) {
    LtcLobes lobes;
    float facing = ltc_facing(s, q);
    lobes.diffuse = ltc_no_lobe();
    lobes.specular = ltc_no_lobe();
    if (facing != 0.0) {
        LtcCorners diffuse = ltc_corners_diffuse(s, q);
        LtcCorners specular = ltc_corners_specular(s, diffuse);
        float3 diffuse_f = ltc_vector_form_factor(diffuse, facing);
        float3 specular_f = ltc_vector_form_factor(specular, facing);
        ltc_read_point(diffuse, diffuse_f, lobes.diffuse);
        ltc_read_point(specular, specular_f, lobes.specular);
        lobes.diffuse.form_factor = ltc_form_factor(tables, diffuse_f);
        lobes.specular.form_factor = ltc_form_factor(tables, specular_f);
    }
    return lobes;
}

// The same with the light clipped at the horizon instead of the table's
// horizon term: exact where the horizon cuts the light, and costlier.
public LtcLobes ltc_quad_clipped(LtcSurface s, LtcQuad q) {
    LtcLobes lobes;
    float facing = ltc_facing(s, q);
    lobes.diffuse = ltc_no_lobe();
    lobes.specular = ltc_no_lobe();
    if (facing != 0.0) {
        LtcCorners diffuse = ltc_corners_diffuse(s, q);
        LtcCorners specular = ltc_corners_specular(s, diffuse);
        ltc_read_point(diffuse, ltc_vector_form_factor(diffuse, facing), lobes.diffuse);
        ltc_read_point(specular, ltc_vector_form_factor(specular, facing), lobes.specular);
        lobes.diffuse.form_factor = ltc_form_factor_clipped(diffuse, facing);
        lobes.specular.form_factor = ltc_form_factor_clipped(specular, facing);
    }
    return lobes;
}

// The light's colour for a lobe: its texture at the lobe's point, from the
// mip level whose blur matches. `samp`: linear, also between the levels,
// clamped (outside the light, the colour of its edge goes on).
// `level_sigma`: the blur of the levels (`LTC_SIGMA_MIPMAPS` or
// `LTC_SIGMA_PREFILTER`).
public float3 ltc_light_colour(Texture2D<float4> light, SamplerState samp, LtcLobe lobe, float level_sigma) {
    uint width;
    uint height;
    uint levels;
    light.GetDimensions(0, width, height, levels);
    float texels = lobe.blur * sqrt(float(width) * float(height));
    float lod = clamp(log2(max(texels / level_sigma, 1e-6)), 0.0, float(levels) - 1.0);
    return light.SampleLevel(samp, saturate(lobe.uv), lod).rgb;
}

// ---- shadows -----------------------------------------------------------------
//
// An approximation, for the shadows of a few boxes (what stands on a desk):
// a box hides a rectangle of the light from the point, and each lobe loses
// the part of its weight that lies in the rectangle, the weight taken for a
// Gaussian about the point where the lobe reads the light.
//
//   LtcLobes lobes = ltc_quad(tables, s, quad);
//   float4 cover = ltc_box_cover(s, quad, box);
//   lobes.diffuse.form_factor *= ltc_uncovered(lobes.diffuse, cover);
//   lobes.specular.form_factor *= ltc_uncovered(lobes.specular, cover);
//
// With several boxes, multiply what each leaves. A convex box does not
// shadow its own faces: only the part of a box above the shaded surface
// counts.

// A box: its centre and its three half extents (vectors at right angles).
public struct LtcBox {
    public float3 center;
    public float3 x;
    public float3 y;
    public float3 z;

    public __init(float3 center, float3 x, float3 y, float3 z) {
        this.center = center;
        this.x = x;
        this.y = y;
        this.z = z;
    }
};

// The light's plane seen from the shaded point, for `ltc_box_cover`.
struct LtcPlane {
    // A point w of the plane (from the shaded point) has the texture
    // coordinates (dot(w, u_axis), dot(w, v_axis)) − origin.
    float3 u_axis;
    float3 v_axis;
    float2 origin;
    // The unit normal towards the plane, and the distance to it.
    float3 towards;
    float dist;
    // The surface's normal, and what counts as above the surface.
    float3 normal;
    float above;
};

// Adds to `cover` where a corner of a box (`rel`: from the shaded point)
// hides the light's plane.
void ltc_cover_corner(LtcPlane p, float3 rel, inout float4 cover, inout bool above, inout bool front, inout bool beyond) {
    float height = dot(p.normal, rel);
    if (height > p.above) {
        above = true;
    }
    // What is below the surface is taken up to it.
    rel -= p.normal * min(height, 0.0);
    float depth = dot(p.towards, rel);
    if (depth > 1e-3 * p.dist) {
        front = true;
    }
    if (depth < p.dist) {
        beyond = false;
    }
    // A corner that is not towards the light hides nothing itself; taken
    // far out along its direction, it stands for the box's reach that way.
    float3 w = rel * (p.dist / max(depth, 1e-3 * p.dist));
    float2 uv = float2(dot(w, p.u_axis), dot(w, p.v_axis)) - p.origin;
    cover.xy = min(cover.xy, uv);
    cover.zw = max(cover.zw, uv);
}

// The rectangle of the light that a box hides from the shaded point, in
// texture coordinates: (least u, least v, greatest u, greatest v). It bounds
// the box's outline, and can reach beyond the light. Nothing is hidden when
// the least is above the greatest.
public float4 ltc_box_cover(LtcSurface s, LtcQuad q, LtcBox box) {
    LtcPlane p;
    float3 origin = q.p0 - s.position;
    float3 e1 = q.p1 - q.p0;
    float3 e2 = q.p3 - q.p0;
    float3 raw = cross(e1, e2);
    float area2 = max(dot(raw, raw), 1e-30);
    p.u_axis = cross(e2, raw) / area2;
    p.v_axis = cross(raw, e1) / area2;
    p.origin = float2(dot(origin, p.u_axis), dot(origin, p.v_axis));
    p.towards = raw * rsqrt(area2);
    p.dist = dot(p.towards, origin);
    if (p.dist < 0.0) {
        p.towards = -p.towards;
        p.dist = -p.dist;
    }
    p.normal = s.normal;
    p.above = 1e-3 * (length(box.x) + length(box.y) + length(box.z));
    float3 c = box.center - s.position;
    float4 none = float4(1.0, 1.0, 0.0, 0.0);

    // First, cheaply, with a sphere about the box (and about where its
    // corners below the surface are taken up to): most points are nowhere
    // near a box's shadow.
    float radius = sqrt(dot(box.x, box.x) + dot(box.y, box.y) + dot(box.z, box.z));
    float height = dot(p.normal, c);
    float depth = dot(p.towards, c);
    float reach = radius + max(radius - height, 0.0);
    if (height + radius <= 0.0 || depth + reach <= 0.0 || depth - reach >= p.dist) {
        return none;
    }
    if (depth > reach) {
        // The sphere is wholly towards the light: what it hides is within
        // its centre's coordinates, give or take its size, at the least and
        // at the greatest scale of its depths.
        float2 scales = p.dist / float2(depth + reach, depth - reach);
        float2 centre = float2(dot(c, p.u_axis), dot(c, p.v_axis));
        float2 extent = reach * float2(length(p.u_axis), length(p.v_axis));
        float2 lo = centre - extent;
        float2 hi = centre + extent;
        float2 least = min(lo * scales.x, lo * scales.y) - p.origin;
        float2 greatest = max(hi * scales.x, hi * scales.y) - p.origin;
        if (any(least >= float2(1.0, 1.0)) || any(greatest <= float2(0.0, 0.0))) {
            return none;
        }
    }

    float4 cover = float4(1e30, 1e30, -1e30, -1e30);
    bool above = false;
    bool front = false;
    bool beyond = true;
    ltc_cover_corner(p, c - box.x - box.y - box.z, cover, above, front, beyond);
    ltc_cover_corner(p, c + box.x - box.y - box.z, cover, above, front, beyond);
    ltc_cover_corner(p, c - box.x + box.y - box.z, cover, above, front, beyond);
    ltc_cover_corner(p, c + box.x + box.y - box.z, cover, above, front, beyond);
    ltc_cover_corner(p, c - box.x - box.y + box.z, cover, above, front, beyond);
    ltc_cover_corner(p, c + box.x - box.y + box.z, cover, above, front, beyond);
    ltc_cover_corner(p, c - box.x + box.y + box.z, cover, above, front, beyond);
    ltc_cover_corner(p, c + box.x + box.y + box.z, cover, above, front, beyond);
    if (!above || !front || beyond) {
        // The box is below the surface, behind the point, or beyond the light.
        return none;
    }
    return cover;
}

// The normal distribution's cumulative function, by a cubic step that
// reaches 0 and 1 at two standard deviations: no exponential, and within
// 0.03 of it.
float2 ltc_cumulative(float2 x) {
    float2 t = saturate(x * 0.25 + 0.5);
    return t * t * (3.0 - 2.0 * t);
}

// The part of a lobe's weight on the light that a cover (`ltc_box_cover`)
// leaves: 1 when it hides nothing, 0 when it hides all of the lobe's part
// of the light.
public float ltc_uncovered(LtcLobe lobe, float4 cover) {
    float2 least = saturate(cover.xy);
    float2 greatest = saturate(cover.zw);
    if (any(least >= greatest)) {
        return 1.0;
    }
    float2 centre = saturate(lobe.uv);
    float2 sigma = max(lobe.spread, float2(1e-3, 1e-3));
    float2 hidden = ltc_cumulative((greatest - centre) / sigma) - ltc_cumulative((least - centre) / sigma);
    float2 whole = ltc_cumulative((1.0 - centre) / sigma) - ltc_cumulative(-centre / sigma);
    float2 part = hidden / max(whole, float2(1e-4, 1e-4));
    return 1.0 - saturate(part.x * part.y);
}

// What a light adds to a point, before the material: multiply `diffuse` by
// the albedo and `specular` by `ltc_specular_colour`.
public struct LtcLight {
    public float3 diffuse;
    public float3 specular;
};

// A quad light of one colour. Two table reads.
public LtcLight ltc_quad_uniform(LtcTables tables, LtcSurface s, LtcQuad q, float3 colour) {
    LtcLobes lobes = ltc_quad(tables, s, q);
    LtcLight result;
    result.diffuse = colour * lobes.diffuse.form_factor;
    result.specular = colour * lobes.specular.form_factor;
    return result;
}

// A quad light that shows a texture. Two table reads and two reads of the
// texture.
public LtcLight ltc_quad_textured(LtcTables tables, LtcSurface s, LtcQuad q, Texture2D<float4> light, SamplerState samp, float level_sigma) {
    LtcLobes lobes = ltc_quad(tables, s, q);
    LtcLight result;
    result.diffuse = float3(0.0, 0.0, 0.0);
    result.specular = float3(0.0, 0.0, 0.0);
    if (lobes.diffuse.form_factor > 0.0) {
        result.diffuse = ltc_light_colour(light, samp, lobes.diffuse, level_sigma) * lobes.diffuse.form_factor;
    }
    if (lobes.specular.form_factor > 0.0) {
        result.specular = ltc_light_colour(light, samp, lobes.specular, level_sigma) * lobes.specular.form_factor;
    }
    return result;
}
