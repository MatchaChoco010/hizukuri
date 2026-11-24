-- The renderer of the room.
--
-- Per frame:
--
--   1. Every monitor draws what it shows into an image of its own, at its
--      own resolution, in linear light of the working space: the surfaces
--      of its window (read through the core's `color` module), or the
--      NO SIGNAL picture, or both half way through a change. The image
--      gets mip levels: the screen is seen smaller than it is, and as a
--      light it is seen blurred.
--   2. The room is drawn from the camera into a linear buffer of the size
--      of the output, with a depth buffer and several samples per pixel:
--      the geometry, lit by the monitors as textured area lights (`ltc`),
--      and the screens themselves, which show their images.
--   3. The cursor is drawn over that, and the core's `color.present`
--      converts the buffer to what the output takes.

local affine = hz.include("affine")
local color = hz.include("color")
local ltc = hz.include("ltc")
local m3 = hz.include("math3d")
local scene = hz.include("scene")
local camera = hz.include("camera")
local wm = hz.include("wm")
local dots = hz.include("dots")
local menu = hz.include("menu")

local R = {}

R.settings = {
    -- Whether the core is told which windows are not seen (`hz.set_visible`
    -- in `main.luau`), so that their clients stop drawing. false keeps
    -- every window drawing, for comparison.
    hide_unseen = true,
    -- Whether the brightest parts of the surfaces of the room bend over
    -- towards the brightest the output can show, instead of being cut off
    -- there. Off, the values go out as they are: for a way out that maps
    -- tones itself.
    shoulder = true,
    -- Samples per pixel of the picture of the room: 1 (off), 2, 4 or 8. On
    -- an output of more than about 2560x1440, fewer are taken.
    samples = 4,
    -- The format of the buffers the room is drawn in. false: the format the
    -- core chooses (`color.format()`, the same as the buffer that goes out),
    -- with which a window seen up close comes out as the client stored it.
    -- "rgba16float" takes half the memory, and moves the most saturated
    -- colors by a code value or two.
    format = false,
    -- Light in the room that comes from nowhere (very little).
    ambient = 0.002,
    -- How strongly the screens light the room: the diffuse light, and the
    -- glossy reflections. 1 is what the screens physically give; a camera
    -- that exposes for a dark room sees more.
    light = 11.0,
    gloss = 3.5,
    -- How strongly the top of the desk throws light back into the room.
    bounce = 10.0,
    -- With dark screens, the room is shown brighter (the eye opens up):
    -- 0 turns this off, 1 keeps the room equally bright whatever the
    -- screens show. `adapt_to` is the mean brightness of the screens at
    -- which nothing is changed.
    adapt = 0.5,
    adapt_to = 0.25,
    -- Brightness of a window's white on a screen (1: reference white).
    screen_brightness = 1.0,
    -- Brightness of the NO SIGNAL picture.
    no_signal_gain = 1.0,
    -- In a view of the room, as through a lens: how much the light of the
    -- bright parts spreads around them (0: off, which also saves two
    -- passes), from which brightness on, and how much darker the corners
    -- of the picture are. Up close to a monitor, none of this is done.
    bloom = 0.5,
    bloom_from = 0.35,
    vignette = 0.35,
    -- How far (meters) the camera moves with the pointer in a view of the
    -- room, as a head does. 0: it stands still.
    parallax = 0.02,
    -- How a monitor is blurred when it is read as a light: "gaussian"
    -- (smooth; a second, smaller image per monitor with mip levels blurred
    -- one by one) or "mips" (the plain mip levels of its picture: cheaper,
    -- and what a software renderer should use).
    light_filter = "gaussian",
    -- The size of that smaller image, as a part of the monitor's resolution.
    light_scale = 0.5,
    -- Whether the things on the desk keep the light of the screens off it
    -- (an approximation of the shadows of area lights; it costs about as
    -- much per thing as a light). Without it the desk is only darkened
    -- close around each thing.
    shadows = true,
}

local MAX_MONITORS = 8
local VERTEX_BYTES = 56
-- The uniform block of `shaders/scene.slang`: the monitors start at
-- MONITORS_AT.
local MONITORS_AT = 176
-- Samples of all pixels of an output together, at most: on a larger
-- output, fewer samples per pixel are taken.
local MAX_SAMPLES_IN_ALL = 17000000
local MAX_BLOCKERS = 8
local BLOCKERS_AT = MONITORS_AT + MAX_MONITORS * 64
local SRGB_AT = BLOCKERS_AT + MAX_BLOCKERS * 32
local FRAME_BYTES = SRGB_AT + 64
-- Offsets in the push data of `shaders/window.slang`.
local WINDOW_AT, TARGET_SIZE_AT, OPACITY_AT = 32, 128, 136
-- Bindings of `shaders/scene.slang`.
local BIND_FRAME, BIND_SCREEN_SAMPLER, BIND_LIGHT_SAMPLER, BIND_SCREENS = 0, 1, 2, 3
local BIND_LTC_MAT, BIND_LTC_AMP, BIND_LTC_SAMPLER, BIND_LIGHTS = 4, 5, 6, 7

local WORDS = "NO SIGNAL"

local format, final_format
local index_count = 0
local vertex_buffer, index_buffer
local frame_buffer, frame_slot
local frame_data = buffer.create(FRAME_BYTES)
local scene_shaders, scene_pipelines
local window_pipeline, cursor_pipeline, nosignal_pipeline
local bright_pipeline, combine_pipeline
local linear_sampler, nearest_sampler, screen_sampler, light_sampler
local text_image, text_slot, text_uploaded, black_image, black_slot
local tables
-- Rows of the matrix from linear sRGB to the working space, and what each
-- channel of the working space adds to the brightness of a color.
local srgb_rows, luma

-- A color written as linear sRGB (a table of three numbers), in the
-- working space.
local function linear_srgb(c)
    local r, g, b = hz.color(c[1], c[2], c[3], { space = "linear-srgb" })
    return r, g, b
end
-- Monitor index -> { image, slot, signature }
local screens = {}
local screen_slots, light_slots
local smooth_lights = true
local copy_pipeline
-- Output id -> { width, height, depth, color, bright, bright_slot, final,
-- final_slot }: the depth buffer and the multisampled color buffer of the
-- picture of the room, its bright parts at half the size (with mip
-- levels), and the picture as seen through the lens.
local targets = {}
local window_push = buffer.create(OPACITY_AT + 8)
local nosignal_push = buffer.create(80)
local screen_push = buffer.create(8)
local post_push = buffer.create(32)
local copy_push = buffer.create(8)

-- What the last frame was drawn with, for hit testing: { pose, aspect,
-- x, y, width, height } (the output in global logical coordinates).
R.last = nil

local function shader(stage, file, entry)
    return gfx.create_shader({ stage = stage, shader = "shaders/" .. file .. ".slang", entry = entry })
end

-- The geometry of the room: four vertices and two triangles per quad.
local function build_geometry()
    local quads = scene.build()
    local vertices = buffer.create(#quads * 4 * VERTEX_BYTES)
    local indices = buffer.create(#quads * 6 * 4)
    for q, quad in quads do
        local material = quad.material
        local first = (q - 1) * 4
        for c, corner in quad.corners do
            local at = (first + c - 1) * VERTEX_BYTES
            buffer.writef32(vertices, at, corner[1])
            buffer.writef32(vertices, at + 4, corner[2])
            buffer.writef32(vertices, at + 8, corner[3])
            buffer.writef32(vertices, at + 12, quad.normal[1])
            buffer.writef32(vertices, at + 16, quad.normal[2])
            buffer.writef32(vertices, at + 20, quad.normal[3])
            local albedo_r, albedo_g, albedo_b = linear_srgb(material.albedo)
            buffer.writef32(vertices, at + 24, albedo_r)
            buffer.writef32(vertices, at + 28, albedo_g)
            buffer.writef32(vertices, at + 32, albedo_b)
            buffer.writef32(vertices, at + 36, material.roughness)
            buffer.writef32(vertices, at + 40, material.f0)
            buffer.writef32(vertices, at + 44, material.kind)
        end
        local at = (q - 1) * 24
        buffer.writeu32(indices, at, first)
        buffer.writeu32(indices, at + 4, first + 1)
        buffer.writeu32(indices, at + 8, first + 2)
        buffer.writeu32(indices, at + 12, first)
        buffer.writeu32(indices, at + 16, first + 2)
        buffer.writeu32(indices, at + 20, first + 3)
    end
    vertex_buffer = gfx.create_buffer({ size = buffer.len(vertices), usage = { "vertex" } })
    gfx.write_buffer(vertex_buffer, 0, vertices)
    index_buffer = gfx.create_buffer({ size = buffer.len(indices), usage = { "index" } })
    gfx.write_buffer(index_buffer, 0, indices)
    index_count = #quads * 6
end

local function text_pixels()
    return dots.pixels(WORDS)
end

local function mip_count(width, height)
    return math.floor(math.log(math.max(width, height), 2)) + 1
end

function R.init()
    -- Colors here are written as linear sRGB; the working space is the
    -- session's.
    local red, green, blue = { linear_srgb({ 1, 0, 0 }) }, { linear_srgb({ 0, 1, 0 }) }, { linear_srgb({ 0, 0, 1 }) }
    srgb_rows = {}
    for row = 1, 3 do
        srgb_rows[row] = { red[row], green[row], blue[row] }
        buffer.writef32(nosignal_push, 32 + (row - 1) * 16, red[row])
        buffer.writef32(nosignal_push, 36 + (row - 1) * 16, green[row])
        buffer.writef32(nosignal_push, 40 + (row - 1) * 16, blue[row])
    end
    if hz.working_luminance then
        luma = { hz.working_luminance() }
    else
        -- Of ACEScg, the default working space.
        luma = { 0.2722, 0.6741, 0.0537 }
    end
    -- The renderer's own buffers, and the buffer the core's way out takes.
    final_format = color.format()
    format = R.settings.format or final_format
    build_geometry()

    scene_shaders = {
        room_vertex = shader("vertex", "scene", "vs_room"),
        room_fragment = shader("fragment", "scene", "fs_room"),
        screen_vertex = shader("vertex", "scene", "vs_screen"),
        screen_fragment = shader("fragment", "scene", "fs_screen"),
    }
    scene_pipelines = {}
    local window_vertex = shader("vertex", "window", "vs_main")
    local window_fragment = shader("fragment", "window", "fs_main")
    window_pipeline = gfx.create_render_pipeline({
        vertex = window_vertex,
        fragment = window_fragment,
        targets = { { format = format, blend = "premultiplied" } },
        topology = "triangle-strip",
    })
    cursor_pipeline = gfx.create_render_pipeline({
        vertex = window_vertex,
        fragment = window_fragment,
        targets = { { format = final_format, blend = "premultiplied" } },
        topology = "triangle-strip",
    })
    nosignal_pipeline = gfx.create_render_pipeline({
        vertex = shader("vertex", "nosignal", "vs_main"),
        fragment = shader("fragment", "nosignal", "fs_main"),
        targets = { { format = format } },
    })
    bright_pipeline = gfx.create_render_pipeline({
        vertex = shader("vertex", "post", "vs_main"),
        fragment = shader("fragment", "post", "fs_bright"),
        targets = { { format = format } },
    })
    combine_pipeline = gfx.create_render_pipeline({
        vertex = shader("vertex", "post", "vs_main"),
        fragment = shader("fragment", "post", "fs_combine"),
        targets = { { format = final_format } },
    })
    linear_sampler = gfx.create_sampler({ mag = "linear", min = "linear" })
    nearest_sampler = gfx.create_sampler({ mag = "nearest", min = "nearest" })
    -- A screen is seen at an angle and smaller than its picture.
    screen_sampler = gfx.create_sampler({ mag = "linear", min = "linear", mip = "linear", anisotropy = 8 })
    -- As a light, a screen is read blurred: between the levels too.
    light_sampler = gfx.create_sampler({ mag = "linear", min = "linear", mip = "linear" })

    frame_buffer = gfx.create_buffer({ size = FRAME_BYTES, usage = { "uniform" } })
    frame_slot = gfx.create_uniform_slot(frame_buffer, 0, FRAME_BYTES)

    local raw = { raw = { width = ltc.TABLE_SIZE, height = ltc.TABLE_SIZE, channels = 4 }, format = "rgba16float" }
    tables = ltc.create_tables({
        mat = hz.load_image("assets/ltc_mat.f32", raw).texture,
        amp = hz.load_image("assets/ltc_amp.f32", raw).texture,
    })

    local text_width = text_pixels()
    text_image = gfx.create_image({ format = "r8unorm", width = text_width, height = 7, usage = { "sampled", "copy_dst" } })
    text_slot = gfx.create_texture_slot(text_image)
    text_uploaded = false

    -- One image per monitor for what it shows. The shader takes an array
    -- of a fixed length: the rest is a black image.
    --
    -- As a light, a monitor is read from a second, smaller image whose
    -- mip levels are blurred smoothly (`ltc.update_light`), or, with
    -- `light_filter = "mips"`, from the same image (cheaper, but what the
    -- desk mirrors then shows the squares of the mip levels).
    smooth_lights = R.settings.light_filter ~= "mips"
    black_image = gfx.create_image({ format = format, width = 1, height = 1, usage = { "sampled", "color_target" } })
    black_slot = gfx.create_texture_slot(black_image)
    screens = {}
    local images, light_images = {}, {}
    for index = 1, MAX_MONITORS do
        local monitor = scene.monitors[index]
        if monitor then
            local image = gfx.create_image({
                format = format,
                width = monitor.pixels_x,
                height = monitor.pixels_y,
                mip_levels = mip_count(monitor.pixels_x, monitor.pixels_y),
                usage = { "color_target", "sampled", "copy_src", "copy_dst" },
            })
            local screen = { image = image }
            if smooth_lights then
                local scale = R.settings.light_scale
                screen.light = ltc.create_light(
                    math.max(math.round(monitor.pixels_x * scale), 1),
                    math.max(math.round(monitor.pixels_y * scale), 1),
                    format
                )
            end
            screens[index] = screen
            images[index] = image
            light_images[index] = if screen.light then screen.light.image else image
        else
            images[index] = black_image
            light_images[index] = black_image
        end
    end
    screen_slots = gfx.create_texture_slots(images)
    light_slots = if smooth_lights then gfx.create_texture_slots(light_images) else screen_slots
    for index, screen in screens do
        screen.slot = screen_slots[index]
    end
    copy_pipeline = gfx.create_render_pipeline({
        vertex = shader("vertex", "copy", "vs_main"),
        fragment = shader("fragment", "copy", "fs_main"),
        targets = { { format = format } },
    })
    -- The launcher, drawn into the images of the monitors.
    menu.init(format, linear_srgb)
    targets = {}
    R.last = nil
    R.goal_key = nil
end

-- The pipelines that draw the room with `count` samples per pixel.
local function scene_pipelines_for(count)
    local pipelines = scene_pipelines[count]
    if not pipelines then
        local depth = { format = "d32float" }
        local multisample = if count > 1 then count else nil
        pipelines = {
            room = gfx.create_render_pipeline({
                vertex = scene_shaders.room_vertex,
                fragment = scene_shaders.room_fragment,
                vertex_buffers = {
                    {
                        stride = VERTEX_BYTES,
                        attributes = {
                            { location = 0, format = "f32x3", offset = 0 },
                            { location = 1, format = "f32x3", offset = 12 },
                            { location = 2, format = "f32x4", offset = 24 },
                            { location = 3, format = "f32x4", offset = 40 },
                        },
                    },
                },
                targets = { { format = format } },
                -- The quads are counter-clockwise seen from the front, and
                -- so they are on the screen: the picture is upright.
                cull = "back",
                front_face = "ccw",
                depth = depth,
                samples = multisample,
            }),
            screen = gfx.create_render_pipeline({
                vertex = scene_shaders.screen_vertex,
                fragment = scene_shaders.screen_fragment,
                targets = { { format = format } },
                topology = "triangle-strip",
                depth = depth,
                samples = multisample,
            }),
        }
        scene_pipelines[count] = pipelines
    end
    return pipelines
end

-- The buffers of the picture of the room for an output: the depth buffer,
-- the multisampled color buffer, the picture itself, and its bright parts
-- at half the size.
local function targets_for(output)
    local target = targets[output.id]
    local stale = target
        and (target.width ~= output.width or target.height ~= output.height or target.wanted ~= R.settings.samples)
    if stale then
        gfx.destroy(target.depth)
        if target.color then
            gfx.destroy(target.color)
        end
        gfx.destroy(target.bright_slot)
        gfx.destroy(target.bright)
        gfx.destroy(target.scene_slot)
        gfx.destroy(target.scene)
        target = nil
    end
    if not target then
        target = { width = output.width, height = output.height, wanted = R.settings.samples }
        -- On a large output, fewer samples: the multisampled buffers are
        -- what takes the most memory.
        local samples = R.settings.samples
        while samples > 1 and output.width * output.height * samples > MAX_SAMPLES_IN_ALL do
            samples //= 2
        end
        -- And not every device has every number of samples at every size:
        -- the next lower one is tried until one works.
        while true do
            local multisample = if samples > 1 then samples else nil
            local made, depth = pcall(gfx.create_image, {
                format = "d32float",
                width = output.width,
                height = output.height,
                samples = multisample,
                usage = { "depth_target" },
            })
            local color_made, color_image = made, nil
            if made and samples > 1 then
                color_made, color_image = pcall(gfx.create_image, {
                    format = format,
                    width = output.width,
                    height = output.height,
                    samples = samples,
                    usage = { "color_target" },
                })
                if not color_made then
                    gfx.destroy(depth)
                end
            end
            if made and color_made then
                target.depth, target.color = depth, color_image
                break
            elseif samples <= 1 then
                error(if made then color_image else depth)
            end
            samples //= 2
        end
        target.samples = samples
        target.scene = gfx.create_image({
            format = format,
            width = output.width,
            height = output.height,
            usage = { "color_target", "sampled" },
        })
        target.scene_slot = gfx.create_texture_slot(target.scene)
        local half_width, half_height = math.max(output.width // 2, 1), math.max(output.height // 2, 1)
        target.bright = gfx.create_image({
            format = format,
            width = half_width,
            height = half_height,
            mip_levels = math.min(mip_count(half_width, half_height), 7),
            usage = { "color_target", "sampled", "copy_src", "copy_dst" },
        })
        target.bright_slot = gfx.create_texture_slot(target.bright)
        targets[output.id] = target
    end
    return target
end

-- Draws one surface as a flat quad. `to_pixels` maps the quad's own
-- coordinates to pixels of the target.
local function draw_surface(to_pixels, width, height, surface, opacity, target_width, target_height, brightness)
    local push = window_push
    buffer.writef32(push, 0, to_pixels[1])
    buffer.writef32(push, 4, to_pixels[2])
    buffer.writef32(push, 8, to_pixels[5])
    buffer.writef32(push, 12, width)
    buffer.writef32(push, 16, to_pixels[3])
    buffer.writef32(push, 20, to_pixels[4])
    buffer.writef32(push, 24, to_pixels[6])
    buffer.writef32(push, 28, height)
    color.write_window_params(push, WINDOW_AT, surface, { brightness = brightness })
    buffer.writef32(push, TARGET_SIZE_AT, target_width)
    buffer.writef32(push, TARGET_SIZE_AT + 4, target_height)
    buffer.writef32(push, OPACITY_AT, opacity)
    gfx.push(0, push)
    gfx.bind(0, surface.texture)
    gfx.draw(4)
end

local function ease(t)
    if t < 0.5 then
        return 4 * t * t * t
    end
    local u = 2 - 2 * t
    return 1 - u * u * u / 2
end

-- What a monitor shows at time t: a list of { window = id or nil, shift },
-- drawn in order. `shift` moves the picture sideways, in pixels. Also
-- returns whether the picture changes by itself (it has to be drawn again
-- for the next frame).
local function sources_of(monitor, slot, t)
    local transition = slot.transition
    if transition then
        local p = (t - transition.start) / wm.settings.switch_duration
        if p < 1 then
            local k = ease(math.max(p, 0))
            local width = monitor.pixels_x
            return {
                { window = transition.from, shift = -transition.direction * k * width },
                { window = slot.window, shift = transition.direction * (1 - k) * width },
            },
                true
        end
    end
    return { { window = slot.window, shift = 0 } }, slot.window == nil
end

-- Draws what a monitor shows into its image, when it changed. Returns
-- whether the picture changes by itself.
local function draw_screen(monitor, t)
    local screen = screens[monitor.index]
    local slot = wm.slot(monitor.index)
    local sources, animated = sources_of(monitor, slot, t)
    local width, height = monitor.pixels_x, monitor.pixels_y

    -- What is about to be drawn, to skip the work when it is what the
    -- image already holds.
    local plan = {}
    local signature = {}
    for _, source in sources do
        if source.window then
            for _, item in wm.items(monitor, source.window) do
                local to_pixels = affine.multiply(affine.translate(source.shift, 0), item.transform)
                for _, surface in hz.surfaces(item.id) do
                    if surface.texture and surface.uv then
                        table.insert(plan, {
                            surface = surface,
                            to_pixels = affine.multiply(to_pixels, affine.translate(surface.x, surface.y)),
                        })
                        table.insert(
                            signature,
                            string.format(
                                "%s:%s:%s:%s:%s:%s:%s:%s",
                                surface.id,
                                surface.serial,
                                surface.generation,
                                to_pixels[1],
                                to_pixels[5] + surface.x,
                                to_pixels[6] + surface.y,
                                surface.width,
                                surface.height
                            )
                        )
                    end
                end
            end
        else
            table.insert(plan, { no_signal = true, shift = source.shift })
        end
    end
    -- The launcher, when it is on this monitor.
    local overlay = menu.plan(monitor, t)
    if overlay then
        if overlay.animated then
            animated = true
        else
            table.insert(signature, overlay.key)
        end
    end
    local key = table.concat(signature, "|")
    if not animated and screen.signature == key then
        return false
    end
    screen.signature = if animated then nil else key

    gfx.begin_pass({ color = { { image = screen.image, load = "clear", clear = { 0, 0, 0, 1 } } } })
    for _, step in plan do
        if step.no_signal then
            gfx.set_pipeline(nosignal_pipeline)
            gfx.bind(0, text_slot)
            gfx.bind(1, nearest_sampler)
            buffer.writef32(nosignal_push, 0, width)
            buffer.writef32(nosignal_push, 4, height)
            buffer.writef32(nosignal_push, 8, step.shift)
            buffer.writef32(nosignal_push, 12, 0)
            buffer.writef32(nosignal_push, 16, t)
            buffer.writef32(nosignal_push, 20, monitor.index * 0.37)
            buffer.writef32(nosignal_push, 24, R.settings.no_signal_gain)
            gfx.push(0, nosignal_push)
            gfx.draw(3)
        else
            gfx.set_pipeline(window_pipeline)
            gfx.bind(1, linear_sampler)
            draw_surface(step.to_pixels, step.surface.width, step.surface.height, step.surface, 1, width, height, R.settings.screen_brightness)
        end
    end
    if overlay then
        menu.draw(overlay, width, height)
    end
    gfx.end_pass()
    gfx.generate_mipmaps(screen.image)
    screen.ready = true
    local light = screen.light
    if light then
        -- The picture again, smaller, with smoothly blurred mip levels.
        buffer.writef32(copy_push, 0, light.width)
        buffer.writef32(copy_push, 4, light.height)
        gfx.begin_pass({ color = { { image = light.image, load = "dont-care" } } })
        gfx.set_pipeline(copy_pipeline)
        gfx.bind(0, screen.slot)
        gfx.bind(1, light_sampler)
        gfx.push(0, copy_push)
        gfx.draw(3)
        gfx.end_pass()
        ltc.update_light(light)
    end
    return animated
end

local function write_vec4(at, v, w)
    buffer.writef32(frame_data, at, v[1])
    buffer.writef32(frame_data, at + 4, v[2])
    buffer.writef32(frame_data, at + 8, v[3])
    buffer.writef32(frame_data, at + 12, w or 0)
end

local function write_frame(pose, aspect, t, output)
    local vp = camera.view_projection(pose, aspect)
    for i = 1, 16 do
        buffer.writef32(frame_data, (i - 1) * 4, vp[i])
    end
    write_vec4(64, pose.eye, t)
    buffer.writef32(frame_data, 80, #scene.monitors)
    buffer.writef32(frame_data, 84, R.settings.ambient)
    buffer.writef32(frame_data, 88, R.settings.light)
    buffer.writef32(frame_data, 92, R.settings.gloss)
    buffer.writef32(frame_data, 96, R.settings.adapt)
    buffer.writef32(frame_data, 100, R.settings.adapt_to)
    buffer.writef32(frame_data, 104, 0.01)
    -- How blurred the mip levels of the lights are (the constants of the
    -- module `ltc`: LTC_SIGMA_PREFILTER and LTC_SIGMA_MIPMAPS).
    buffer.writef32(frame_data, 108, if smooth_lights then 1.1 else 0.5)
    local desk = scene.desk
    write_vec4(112, desk.center, 0)
    write_vec4(128, { desk.half_width, 0, 0 }, if R.settings.shadows then 1 else 0)
    write_vec4(144, { 0, 0, -desk.half_depth }, 0)
    -- The color of the wood, part of the way to grey: the light it throws
    -- back keeps some of the color of the screens.
    local wood = { linear_srgb(scene.materials.desk.albedo) }
    local grey = (wood[1] + wood[2] + wood[3]) / 3
    local blockers = math.min(#scene.blockers, MAX_BLOCKERS)
    write_vec4(160, m3.scale(m3.lerp(wood, { grey, grey, grey }, 0.6), R.settings.bounce), blockers)
    for index = 1, blockers do
        local blocker = scene.blockers[index]
        local at = BLOCKERS_AT + (index - 1) * 32
        write_vec4(at, { blocker.x, blocker.z, blocker.half_x }, blocker.half_z)
        write_vec4(at + 16, { math.cos(blocker.angle), math.sin(blocker.angle), blocker.height }, 0)
    end
    local lamps_moving = false
    for index, monitor in scene.monitors do
        local at = MONITORS_AT + (index - 1) * 64
        -- How far a change of its picture is, and how much it has the focus.
        local change = 0
        local transition = wm.slot(index).transition
        if transition then
            local p = (t - transition.start) / wm.settings.switch_duration
            if p > 0 and p < 1 then
                change = p
            end
        end
        local focus, moving = wm.focus_of(index, t)
        lamps_moving = lamps_moving or moving
        write_vec4(at, monitor.center, 0)
        write_vec4(at + 16, m3.scale(monitor.right, monitor.width / 2), change)
        write_vec4(at + 32, m3.scale(monitor.up, monitor.height / 2), focus)
        write_vec4(at + 48, monitor.normal, monitor.onto_desk)
    end
    for row = 1, 3 do
        write_vec4(SRGB_AT + (row - 1) * 16, srgb_rows[row], 0)
    end
    -- How bright the output can get, in multiples of reference white.
    local ceiling = 0
    if R.settings.shoulder then
        local info = output.color
        local peak, reference = info and info.luminance_max, info and info.luminance_reference
        ceiling = if peak and reference and reference > 0 then math.max(peak / reference, 1) else 1
    end
    write_vec4(SRGB_AT + 48, luma, ceiling)
    gfx.write_buffer(frame_buffer, 0, frame_data)
    return lamps_moving
end

-- Moves the camera a little with the pointer, as a head moves: sideways
-- and up and down, without turning. Only in a view of the room; up close
-- the picture stands still.
local function with_parallax(pose, output)
    local amount = R.settings.parallax * pose.room
    local x, y = hz.pointer()
    if amount <= 0 or not x then
        return pose
    end
    local nx = math.clamp((x - output.x) / output.logical_width * 2 - 1, -1, 1)
    local ny = math.clamp((y - output.y) / output.logical_height * 2 - 1, -1, 1)
    local right = m3.quat_rotate(pose.rotation, { 1, 0, 0 })
    local up = m3.quat_rotate(pose.rotation, { 0, 1, 0 })
    local eye = m3.madd(m3.madd(pose.eye, right, nx * amount), up, -ny * amount * 0.6)
    return { eye = eye, rotation = pose.rotation, focal = pose.focal, room = pose.room }
end

-- Writes the picture of the room (`buffers.scene`) into `composite`, the
-- buffer that goes out, as through a lens: the light of the bright parts
-- spread around them, the corners darker, by `amount` (0..1). With 0 the
-- picture is copied as it is.
local function lens(output, composite, buffers, amount)
    local s = R.settings
    local spread = amount > 0.001 and s.bloom > 0
    buffer.writef32(post_push, 8, if spread then s.bloom * amount else 0)
    buffer.writef32(post_push, 12, s.vignette * amount)
    buffer.writef32(post_push, 16, s.bloom_from)
    if spread then
        buffer.writef32(post_push, 0, math.max(output.width // 2, 1))
        buffer.writef32(post_push, 4, math.max(output.height // 2, 1))
        gfx.begin_pass({ color = { { image = buffers.bright, load = "dont-care" } } })
        gfx.set_pipeline(bright_pipeline)
        gfx.bind(0, buffers.scene_slot)
        gfx.bind(1, linear_sampler)
        gfx.push(0, post_push)
        gfx.draw(3)
        gfx.end_pass()
        gfx.generate_mipmaps(buffers.bright)
        buffers.bright_ready = true
    end

    buffer.writef32(post_push, 0, output.width)
    buffer.writef32(post_push, 4, output.height)
    gfx.begin_pass({ color = { { image = composite.image, load = "dont-care" } } })
    gfx.set_pipeline(combine_pipeline)
    gfx.bind(0, buffers.scene_slot)
    gfx.bind(1, light_sampler)
    -- Without spread light, the bright parts are not looked at.
    gfx.bind(2, if buffers.bright_ready then buffers.bright_slot else black_slot)
    gfx.push(0, post_push)
    gfx.draw(3)
    gfx.end_pass()
end

-- The cursor and what is dragged with it, flat over the picture.
local function draw_cursor(output)
    local cursor = hz.cursor()
    local pointer_x, pointer_y = hz.pointer()
    if not pointer_x then
        return
    end
    local to_output = affine.multiply(affine.scale(output.scale), affine.translate(-output.x, -output.y))
    local function at(x, y)
        local px, py = affine.apply(to_output, pointer_x + x, pointer_y + y)
        return { output.scale, 0, 0, output.scale, math.round(px), math.round(py) }
    end
    gfx.set_pipeline(cursor_pipeline)
    gfx.bind(1, linear_sampler)
    for _, surface in hz.drag_icon() do
        if surface.texture and surface.uv then
            draw_surface(at(surface.x, surface.y), surface.width, surface.height, surface, 1, output.width, output.height)
        end
    end
    if cursor then
        if cursor.texture then
            local arrow = { texture = cursor.texture, color = hz.default_color() }
            draw_surface(at(0, 0), cursor.width, cursor.height, arrow, 1, output.width, output.height)
        else
            for _, surface in cursor.surfaces do
                if surface.texture then
                    draw_surface(at(surface.x, surface.y), surface.width, surface.height, surface, 1, output.width, output.height)
                end
            end
        end
    end
end

-- Sends the camera to where the window manager wants it, when that
-- changed: up close to the focused monitor, or back in the chair, turned
-- towards it. The move starts at time t. `aspect` is the output's width /
-- height (when left out, that of the last frame).
function R.aim(aspect, t)
    aspect = aspect or (R.last and R.last.aspect)
    if not aspect then
        return
    end
    -- With the launcher open, a little closer to its monitor.
    local launcher = if menu.is_open() then menu.monitor_at(t) else nil
    local key = string.format("%d:%s:%g:%s", wm.focused(), tostring(wm.zoomed()), aspect, tostring(launcher))
    if R.goal_key == key then
        return
    end
    R.goal_key = key
    local focused = scene.monitors[wm.focused()]
    if wm.zoomed() then
        camera.go(camera.full_pose(focused, aspect), t)
    elseif launcher and scene.monitors[launcher] then
        local monitor = scene.monitors[launcher]
        local room = camera.room_pose(monitor, aspect, scene.view_points())
        camera.go(camera.between(room, camera.full_pose(monitor, aspect), menu.settings.closer), t)
    else
        camera.go(camera.room_pose(focused, aspect, scene.view_points()), t)
    end
end

-- Draws a frame of an output for time t. Returns whether something in the
-- picture moves by itself (another frame is wanted).
function R.frame(output, target, t)
    if not text_uploaded then
        -- Once, in the first frame: images get their contents while a
        -- frame is recorded.
        local _, data = text_pixels()
        gfx.upload_image(text_image, data)
        gfx.begin_pass({ color = { { image = black_image, load = "clear", clear = { 0, 0, 0, 1 } } } })
        gfx.end_pass()
        text_uploaded = true
    end
    local aspect = output.width / output.height

    -- Where the camera is at this time.
    R.aim(aspect, t)
    local pose, moving = camera.sample(t)
    pose = with_parallax(pose, output)

    -- Up close, only the focused monitor is in the picture: the others are
    -- left as they are (a NO SIGNAL picture that nobody sees does not move,
    -- and asks for no frames), until the camera sets off again.
    local only_focused = pose.room <= 0 and not moving
    R.only_focused = only_focused
    local animated = moving
    for _, monitor in scene.monitors do
        local wanted = not only_focused or monitor.index == wm.focused() or not screens[monitor.index].ready
        if wanted and draw_screen(monitor, t) then
            animated = true
        end
    end

    local composite = color.composite_for(output)
    local buffers = targets_for(output)
    if write_frame(pose, aspect, t, output) then
        animated = true
    end

    local pipelines = scene_pipelines_for(buffers.samples)
    local attachment
    if buffers.color then
        attachment = { image = buffers.color, load = "clear", clear = { 0, 0, 0, 1 }, store = false, resolve = buffers.scene }
    else
        attachment = { image = buffers.scene, load = "clear", clear = { 0, 0, 0, 1 } }
    end
    gfx.begin_pass({ color = { attachment }, depth = { image = buffers.depth, load = "clear", clear = 1.0, store = false } })
    gfx.set_pipeline(pipelines.room)
    gfx.bind(BIND_FRAME, frame_slot)
    gfx.bind(BIND_SCREEN_SAMPLER, screen_sampler)
    gfx.bind(BIND_LIGHT_SAMPLER, light_sampler)
    gfx.bind(BIND_LIGHTS, light_slots[1])
    gfx.bind(BIND_SCREENS, screen_slots[1])
    ltc.bind_tables(tables, BIND_LTC_MAT, BIND_LTC_AMP, BIND_LTC_SAMPLER)
    gfx.set_vertex_buffer(0, vertex_buffer)
    gfx.set_index_buffer(index_buffer, "u32")
    gfx.draw_indexed(index_count)
    gfx.set_pipeline(pipelines.screen)
    gfx.bind(BIND_FRAME, frame_slot)
    gfx.bind(BIND_SCREEN_SAMPLER, screen_sampler)
    gfx.bind(BIND_SCREENS, screen_slots[1])
    for _, monitor in scene.monitors do
        -- The screen, then the lamp under it.
        buffer.writeu32(screen_push, 0, monitor.index - 1)
        buffer.writeu32(screen_push, 4, 0)
        gfx.push(0, screen_push)
        gfx.draw(4)
        buffer.writeu32(screen_push, 4, 1)
        gfx.push(0, screen_push)
        gfx.draw(4)
    end
    gfx.end_pass()

    lens(output, composite, buffers, pose.room)
    gfx.begin_pass({ color = { { image = composite.image, load = "load" } } })
    draw_cursor(output)
    gfx.end_pass()

    color.present(composite, output, target)

    R.last = {
        pose = pose,
        aspect = aspect,
        x = output.x,
        y = output.y,
        width = output.logical_width,
        height = output.logical_height,
    }
    return animated
end

-- ----- where the pointer is ---------------------------------------------

-- The ray from the camera of the last frame through a point of the screen
-- (global logical coordinates).
local function ray_at(x, y)
    local last = R.last
    if not last then
        return nil
    end
    local nx = (x - last.x) / last.width * 2 - 1
    local ny = (y - last.y) / last.height * 2 - 1
    return m3.camera_ray(last.pose.eye, last.pose.rotation, last.pose.focal, last.aspect, nx, ny)
end

-- The monitor under a point of the screen, and the point in that
-- monitor's pixels. The nearest one, when several are behind the point.
function R.pick(x, y)
    local origin, direction = ray_at(x, y)
    if not origin then
        return nil
    end
    local best, best_t, best_u, best_v = nil, math.huge, 0, 0
    for _, monitor in scene.monitors do
        local t, u, v = m3.ray_rectangle(origin, direction, monitor.center, monitor.right, monitor.up, monitor.width, monitor.height)
        if t and t < best_t then
            best, best_t, best_u, best_v = monitor, t, u, v
        end
    end
    if not best then
        return nil
    end
    return best, best_u * best.pixels_x, best_v * best.pixels_y
end

-- Where a point of a monitor's pixels is seen on the screen (global
-- logical coordinates), through the camera of the last frame: the other
-- way from `R.pick_on`. Nil before the first frame and behind the camera.
function R.project_on(monitor, px, py)
    local last = R.last
    if not last then
        return nil
    end
    local u, v = px / monitor.pixels_x - 0.5, 0.5 - py / monitor.pixels_y
    local point = m3.madd(m3.madd(monitor.center, monitor.right, u * monitor.width), monitor.up, v * monitor.height)
    local cx, cy, _, cw = m3.mat4_transform(camera.view_projection(last.pose, last.aspect), point)
    if cw <= 1e-9 then
        return nil
    end
    return last.x + (cx / cw + 1) / 2 * last.width, last.y + (cy / cw + 1) / 2 * last.height
end

-- A point of the screen in the pixels of one given monitor, also outside
-- its rectangle. Nil when the view ray never meets the monitor's plane.
function R.pick_on(monitor, x, y)
    local origin, direction = ray_at(x, y)
    if not origin then
        return nil
    end
    local facing = m3.dot(direction, monitor.normal)
    if facing >= -1e-9 then
        return nil
    end
    local t = m3.dot(m3.sub(monitor.center, origin), monitor.normal) / facing
    if t <= 0 then
        return nil
    end
    local offset = m3.sub(m3.madd(origin, direction, t), monitor.center)
    local u = m3.dot(offset, monitor.right) / monitor.width + 0.5
    local v = 0.5 - m3.dot(offset, monitor.up) / monitor.height
    return u * monitor.pixels_x, v * monitor.pixels_y
end

return R
