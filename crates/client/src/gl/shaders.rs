//! GLSL ES 3.00 shader sources.
//!
//! All shaders share one vertex layout: position (layer/canvas pixels),
//! texture coordinates and a premultiplied RGBA color.

/// Shared vertex shader. `u_proj` maps pixel coordinates to clip space.
pub const VERTEX: &str = r#"#version 300 es
layout(location = 0) in vec2 a_pos;
layout(location = 1) in vec2 a_uv;
layout(location = 2) in vec4 a_color;
uniform mat3 u_proj;
out vec2 v_uv;
out vec4 v_color;
void main() {
    vec3 p = u_proj * vec3(a_pos, 1.0);
    gl_Position = vec4(p.xy, 0.0, 1.0);
    v_uv = a_uv;
    v_color = a_color;
}
"#;

/// Textured (or, with a 1x1 white texture, solid) quads. Used for fast-path
/// fills, image draws, copies and the final composite.
pub const BASIC_FRAGMENT: &str = r#"#version 300 es
precision highp float;
in vec2 v_uv;
in vec4 v_color;
uniform sampler2D u_tex;
out vec4 o_color;
void main() {
    o_color = texture(u_tex, v_uv) * v_color;
}
"#;

/// General drawing through a coverage mask, used for arbitrary paths, clip
/// masks and the "unbounded" composite modes.
///
/// `u_pass == 0` paints inside the shape; `u_pass == 1` clears everything
/// outside the shape (but inside the clip), which is how the HTML canvas
/// implements `copy`, `source-in`, `source-out`, `destination-in` and
/// `destination-atop`.
pub const MASKED_FRAGMENT: &str = r#"#version 300 es
precision highp float;
precision highp int;
uniform int u_pass;
uniform int u_paint;          // 0 = solid, 1 = texture, 2 = repeating pattern
uniform vec4 u_color;         // premultiplied; alpha also scales textures
uniform sampler2D u_tex;
uniform mat3 u_map;           // target pixel -> source pixel
uniform vec2 u_tex_size;      // allocated size of u_tex
uniform vec4 u_src_rect;      // valid source region (x0, y0, x1, y1) or pattern size in zw
uniform bool u_use_mask;
uniform sampler2D u_mask;
uniform bool u_use_clip;
uniform sampler2D u_clip;
out vec4 o_color;

void main() {
    ivec2 p = ivec2(gl_FragCoord.xy);
    if (u_use_clip && texelFetch(u_clip, p, 0).r < 0.5) discard;
    float inside = u_use_mask ? texelFetch(u_mask, p, 0).r : 1.0;
    if (u_pass == 1) {
        if (inside >= 0.5) discard;
        o_color = vec4(0.0);
        return;
    }
    if (inside < 0.5) discard;
    if (u_paint == 0) {
        o_color = u_color;
        return;
    }
    vec2 sp = (u_map * vec3(gl_FragCoord.xy, 1.0)).xy;
    if (u_paint == 2) {
        sp = mod(sp, u_src_rect.zw);
    } else if (sp.x < u_src_rect.x || sp.y < u_src_rect.y || sp.x >= u_src_rect.z || sp.y >= u_src_rect.w) {
        o_color = vec4(0.0);
        return;
    }
    o_color = texture(u_tex, sp / u_tex_size) * u_color.a;
}
"#;

/// Raster operations of the `transfer` instruction, evaluated on
/// un-premultiplied 8-bit channels exactly like the reference client's
/// `getImageData()` implementation.
pub const TRANSFER_FRAGMENT: &str = r#"#version 300 es
precision highp float;
precision highp int;
uniform sampler2D u_src;
uniform sampler2D u_dst;
uniform ivec2 u_src_origin;   // source rect origin in u_src
uniform ivec2 u_dst_origin;   // destination copy origin in u_dst
uniform ivec2 u_target_origin;
uniform int u_function;
out vec4 o_color;

uvec3 unpremultiply(vec4 c) {
    if (c.a <= 0.0) return uvec3(0u);
    return uvec3(clamp(round(c.rgb / c.a * 255.0), 0.0, 255.0));
}

void main() {
    ivec2 rel = ivec2(gl_FragCoord.xy) - u_target_origin;
    vec4 s = texelFetch(u_src, u_src_origin + rel, 0);
    vec4 d = texelFetch(u_dst, u_dst_origin + rel, 0);
    uvec3 su = unpremultiply(s);
    uvec3 du = unpremultiply(d);
    uvec3 r = uvec3(0u);
    if ((u_function & 8) != 0) r |= ~su & ~du;
    if ((u_function & 4) != 0) r |= ~su & du;
    if ((u_function & 2) != 0) r |= su & ~du;
    if ((u_function & 1) != 0) r |= su & du;
    r &= uvec3(255u);
    float a = (u_function == 3 || u_function == 12) ? s.a : d.a;
    o_color = vec4(vec3(r) / 255.0 * a, a);
}
"#;
