//! The script-side spelling of what 3D drawing needs: image kinds, mip
//! levels and samples, views of a texture, the attachments of a pass,
//! sampler controls, format queries and partial buffer writes. The checks
//! themselves are tested in `proto-gfx-core`; this is about the binding.
#![cfg(feature = "luau")]

mod common;

use common::*;

const SCRIPT: &str = r#"
local function refused(what, f, ...)
    local ok, err = pcall(f, ...)
    assert(not ok, what .. ": expected an error")
    return tostring(err)
end

function run()
    -- Format queries: one capability or a list of them.
    assert(gfx.format_supports("rgba16float", "color_target"))
    assert(gfx.format_supports("rgba8unorm", { "sampled", "filter_linear", "blend", "mipmaps" }))
    assert(gfx.format_supports("d32float", "depth_target"))
    assert(not gfx.format_supports("d32float", "color_target"))
    assert(not gfx.format_supports("d32float", { "sampled", "storage" }))
    refused("unknown format", gfx.format_supports, "rgba9000", "sampled")
    refused("unknown capability", gfx.format_supports, "rgba8unorm", "teleport")
    refused("capability of the wrong type", gfx.format_supports, "rgba8unorm", 3)

    -- Image kinds, mip levels, samples.
    local usage = { "sampled", "color_target", "copy_src", "copy_dst" }
    local mipped = gfx.create_image({ format = "rgba8unorm", width = 8, height = 4, mip_levels = 4, usage = usage })
    local w, h, format, mips, depth, kind, samples = gfx.image_info(mipped)
    assert(w == 8 and h == 4 and format == "rgba8unorm" and mips == 4 and depth == 1 and kind == "2d" and samples == 1)
    local volume = gfx.create_image({ format = "rgba8unorm", kind = "3d", width = 2, height = 2, depth = 3, usage = { "sampled", "copy_dst" } })
    assert(select(5, gfx.image_info(volume)) == 3 and select(6, gfx.image_info(volume)) == "3d")
    local cube = gfx.create_image({ format = "rgba8unorm", kind = "cube", width = 4, height = 4, usage = usage })
    local multisampled = gfx.create_image({ format = "rgba8unorm", width = 4, height = 4, samples = 4, usage = { "color_target" } })
    local resolved = gfx.create_image({ format = "rgba8unorm", width = 4, height = 4, usage = usage })
    local depth_image = gfx.create_image({ format = "d32float", width = 4, height = 4, usage = { "depth_target", "sampled" } })
    refused("unknown kind", gfx.create_image, { format = "rgba8unorm", kind = "4d", width = 2, height = 2, usage = usage })
    refused("too many mip levels", gfx.create_image, { format = "rgba8unorm", width = 8, height = 4, mip_levels = 5, usage = usage })
    refused("3 samples", gfx.create_image, { format = "rgba8unorm", width = 4, height = 4, samples = 3, usage = { "color_target" } })

    -- Uploads to a level and a layer; the size is that of the level.
    gfx.upload_image(mipped, buffer.create(8 * 4 * 4))
    gfx.upload_image(mipped, buffer.create(4 * 2 * 4), { mip = 1 })
    gfx.upload_image(cube, buffer.create(4 * 4 * 4), { layer = 5 })
    gfx.upload_image(volume, buffer.create(2 * 2 * 3 * 4))
    refused("size of another level", gfx.upload_image, mipped, buffer.create(8 * 4 * 4), { mip = 1 })
    refused("a level too far", gfx.upload_image, mipped, buffer.create(4), { mip = 4 })
    gfx.generate_mipmaps(mipped)
    refused("mip levels of a volume", gfx.generate_mipmaps, volume)

    -- Views.
    gfx.create_texture_slot(mipped)
    gfx.create_texture_slot(mipped, { base_mip = 1, mip_count = 2 })
    gfx.create_texture_slot(cube, { layer = 3 })
    gfx.create_texture_slot(depth_image)
    refused("a level too far", gfx.create_texture_slot, mipped, { base_mip = 4 })
    refused("a layer too far", gfx.create_texture_slot, cube, { layer = 6 })
    refused("a multisampled texture", gfx.create_texture_slot, multisampled)

    -- Samplers.
    gfx.create_sampler({ mip = "linear", address_u = "border", address_w = "mirror", border = "white", lod_min = 1, lod_max = 3, lod_bias = 0.5, anisotropy = 4 })
    gfx.create_sampler({ mag = "linear", min = "linear", compare = "less-equal" })
    refused("unknown comparison", gfx.create_sampler, { compare = "roughly" })
    refused("unknown border", gfx.create_sampler, { border = "pink" })
    refused("unknown mip filter", gfx.create_sampler, { mip = "cubic" })
    refused("an empty range", gfx.create_sampler, { lod_min = 3, lod_max = 1 })

    -- Passes: a level, a cube face, a resolve target, a depth attachment.
    gfx.begin_pass({ color = { { image = mipped, mip = 2, load = "dont-care" } } })
    gfx.set_viewport(0, 0, 2, 1, 0.25, 0.75)
    refused("a depth range outside 0..1", gfx.set_viewport, 0, 0, 2, 1, 0, 2)
    gfx.end_pass()
    gfx.begin_pass({ color = { { image = cube, layer = 4 } }, depth = { image = depth_image, clear = 0.5 } })
    gfx.end_pass()
    gfx.begin_pass({ color = { { image = multisampled, store = false, resolve = resolved } } })
    gfx.end_pass()
    gfx.begin_pass({ color = { { image = multisampled, resolve = { image = resolved, mip = 0 } } } })
    gfx.end_pass()
    gfx.begin_pass({ depth = { image = depth_image, load = "load", store = false } })
    gfx.end_pass()
    refused("no attachment", gfx.begin_pass, {})
    refused("no attachment", gfx.begin_pass, { color = {} })
    refused("a level too far", gfx.begin_pass, { color = { { image = mipped, mip = 9 } } })
    refused("a resolve target of the wrong type", gfx.begin_pass, { color = { { image = multisampled, resolve = "x" } } })
    refused("an unknown load op", gfx.begin_pass, { depth = { image = depth_image, load = "maybe" } })
    refused("a clear depth out of range", gfx.begin_pass, { depth = { image = depth_image, clear = 7 } })
    refused("depth as colour", gfx.begin_pass, { color = { { image = depth_image } } })

    -- A part of a buffer.
    local target = gfx.create_buffer({ size = 64, usage = { "vertex" } })
    local data = buffer.create(1024)
    gfx.write_buffer(target, 0, data, 512, 64)
    gfx.write_buffer(target, 32, data, 1000)
    gfx.write_buffer(target, 0, "abcdef", 2, 3)
    refused("more than the target holds", gfx.write_buffer, target, 0, data)
    refused("a range outside the data", gfx.write_buffer, target, 0, data, 1000, 64)
    refused("a start outside the data", gfx.write_buffer, target, 0, data, 2000)
    return true
end
"#;

#[test]
fn the_3d_calls_from_a_script() {
    let host = host();
    host.load("gfx3d.luau", SCRIPT).unwrap();
    host.call_in_frame("run", ()).unwrap();
    // The device still records and submits frames.
    host.call_in_frame("run", ()).unwrap();
    assert_no_validation_errors();
}
