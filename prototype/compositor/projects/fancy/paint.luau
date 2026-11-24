-- The renderer: draws what `stage` describes.
--
-- Everything is drawn in the working space (linear light, 1.0 at the
-- reference white, alpha premultiplied), into the buffer the built-in
-- `color` module keeps for the output:
--
--   the backdrop, then, bottom to top, the plates of the workspaces, and
--   for every window its frame (shadow, rim, glow, fill) and its surfaces
--   (read through the core's `WindowTexture`, cut to the window's rounded
--   rectangle), then the workspace marks, the numbers of the workspaces
--   (when the session has a font), and what follows the pointer.
--
-- Frames can be filled with frosted glass (`look.frost`): what is behind
-- them, blurred. For that the picture so far is copied to an image of half
-- the size, whose smaller levels the core makes (`gfx.generate_mipmaps`);
-- the frame's shader reads one of those levels. The copy is made after the
-- backdrop, and once more before the first window that lies over others.
--
-- `color.present` then converts that buffer to what the output takes.
--
-- Every color in this file's inputs is linear light in the working space,
-- not premultiplied (`K.color` makes one from sRGB values).

local affine = hz.include("affine")
local color = hz.include("color")

local K = {}

local window_pipeline, frame_pipeline, backdrop_pipeline, text_pipeline, half_pipeline, sampler
local frosts = {} -- output id -> { image, slot, width, height }: the blurred copy
local frost_sampler
local FROST_FORMAT = "rgba16float"
local FROST_LEVELS = 6
local texts = {} -- string -> what `hz.render_text` made of it, or false
-- Texts made at a given size: key -> { made (or false), used (frame) }.
-- Those not used in the last frames are let go once there are many.
local sized = {}
local sized_count = 0
local frame_count = 0
local font = nil
local text_push = buffer.create(64)
local window_push = buffer.create(192)
local frame_push = buffer.create(144)
local backdrop_push = buffer.create(96)

function K.init()
    local function shader(stage, file, entry)
        return gfx.create_shader({ stage = stage, shader = file, entry = entry })
    end
    local format = color.format()
    window_pipeline = gfx.create_render_pipeline({
        vertex = shader("vertex", "shaders/window.slang", "vs_main"),
        fragment = shader("fragment", "shaders/window.slang", "fs_main"),
        targets = { { format = format, blend = "premultiplied" } },
        topology = "triangle-strip",
    })
    frame_pipeline = gfx.create_render_pipeline({
        vertex = shader("vertex", "shaders/frame.slang", "vs_main"),
        fragment = shader("fragment", "shaders/frame.slang", "fs_main"),
        targets = { { format = format, blend = "premultiplied" } },
        topology = "triangle-strip",
    })
    backdrop_pipeline = gfx.create_render_pipeline({
        vertex = shader("vertex", "shaders/backdrop.slang", "vs_main"),
        fragment = shader("fragment", "shaders/backdrop.slang", "fs_main"),
        targets = { { format = format } },
    })
    text_pipeline = gfx.create_render_pipeline({
        vertex = shader("vertex", "shaders/text.slang", "vs_main"),
        fragment = shader("fragment", "shaders/text.slang", "fs_main"),
        targets = { { format = format, blend = "premultiplied" } },
        topology = "triangle-strip",
    })
    half_pipeline = gfx.create_render_pipeline({
        vertex = shader("vertex", "shaders/half.slang", "vs_main"),
        fragment = shader("fragment", "shaders/half.slang", "fs_main"),
        targets = { { format = FROST_FORMAT } },
    })
    sampler = gfx.create_sampler({ mag = "linear", min = "linear" })
    frost_sampler = gfx.create_sampler({ mag = "linear", min = "linear", mip = "linear" })
    frosts = {}
    -- The session's font, if it has one: without, nothing is written.
    font = hz.load_font and hz.load_font() or nil
    texts = {}
    sized = {}
    sized_count = 0
end

-- A short text as a texture (made once): { texture, width, height }, or
-- nil without a font.
local function text_of(text)
    local made = texts[text]
    if made == nil then
        made = font and hz.render_text(font, text, { size = 200 }) or false
        texts[text] = made
    end
    return made or nil
end

-- A text made `size` pixels high (for text drawn at about that size on
-- screen), or nil without a font.
local function text_at(text, size)
    local key = size .. "\0" .. text
    local entry = sized[key]
    if not entry then
        local made = font and hz.render_text(font, text, { size = size }) or false
        entry = { made = made, used = frame_count }
        sized[key] = entry
        sized_count += 1
    end
    entry.used = frame_count
    return entry.made or nil
end

-- How wide `text` is when drawn `height` logical units high (made at
-- `size` pixels; at 200 without). 0 without a font.
function K.text_width(text, height, size)
    local made = if size then text_at(text, size) else text_of(text)
    if not made then
        return 0
    end
    return height * made.width / made.height
end

-- Lets go of sized texts that the last frames did not use.
local function forget_texts()
    frame_count += 1
    if sized_count <= 96 then
        return
    end
    for key, entry in sized do
        if frame_count - entry.used > 2 then
            if entry.made then
                gfx.destroy(entry.made.texture)
                gfx.destroy(entry.made.image)
            end
            sized[key] = nil
            sized_count -= 1
        end
    end
end

-- Draws `text`, `height` logical units high, at (x, y) of the coordinates
-- `to_pixels` maps from (its middle, or with `align` "left" / "right" the
-- middle of that side), in a color { r, g, b } with `alpha`. `size`: the
-- size it is made at, in pixels.
local function draw_text(to_pixels, text, x, y, height, tint, alpha, align, size)
    local made = if size then text_at(text, size) else text_of(text)
    if not made then
        return
    end
    local width = height * made.width / made.height
    local push = text_push
    local a, b, c, d = to_pixels[1], to_pixels[2], to_pixels[3], to_pixels[4]
    local left = if align == "left" then x elseif align == "right" then x - width else x - width / 2
    local top = y - height / 2
    buffer.writef32(push, 0, a)
    buffer.writef32(push, 4, b)
    buffer.writef32(push, 8, a * left + b * top + to_pixels[5])
    buffer.writef32(push, 12, width)
    buffer.writef32(push, 16, c)
    buffer.writef32(push, 20, d)
    buffer.writef32(push, 24, c * left + d * top + to_pixels[6])
    buffer.writef32(push, 28, height)
    buffer.writef32(push, 32, tint[1] * alpha)
    buffer.writef32(push, 36, tint[2] * alpha)
    buffer.writef32(push, 40, tint[3] * alpha)
    buffer.writef32(push, 44, alpha)
    gfx.push(0, push)
    gfx.bind(0, made.texture)
    gfx.draw(4)
end

-- An sRGB color (values 0..1, as one writes them for a display) as linear
-- light in the working space: { r, g, b }.
function K.color(r, g, b)
    local lr, lg, lb = hz.color(r, g, b)
    return { lr, lg, lb }
end

-- Draws one surface. `to_pixels` maps the window's coordinates to pixels of
-- the target; the quad starts at (x, y) there. `mask`, when given, is the
-- window's rectangle as drawn { width, height, radius }, and `fit_x`,
-- `fit_y` how much the window's contents are stretched to it. `brightness`
-- scales the light of the contents (1 when not given); `glow` is a color
-- the contents glow in, and how much.
local light = { brightness = 1 }
local function draw_surface(to_pixels, x, y, width, height, surface, opacity, mask, fit_x, fit_y, brightness, glow)
    local push = window_push
    light.brightness = brightness or 1
    color.write_window_params(push, 0, surface, light)
    local a, b, c, d = to_pixels[1], to_pixels[2], to_pixels[3], to_pixels[4]
    buffer.writef32(push, 96, a)
    buffer.writef32(push, 100, b)
    buffer.writef32(push, 104, a * x + b * y + to_pixels[5])
    buffer.writef32(push, 108, width)
    buffer.writef32(push, 112, c)
    buffer.writef32(push, 116, d)
    buffer.writef32(push, 120, c * x + d * y + to_pixels[6])
    buffer.writef32(push, 124, height)
    buffer.writef32(push, 128, x)
    buffer.writef32(push, 132, y)
    if mask then
        buffer.writef32(push, 144, mask[1] / 2)
        buffer.writef32(push, 148, mask[2] / 2)
        buffer.writef32(push, 152, mask[3])
        buffer.writef32(push, 156, 1)
        buffer.writef32(push, 160, fit_x)
        buffer.writef32(push, 164, fit_y)
    else
        buffer.writef32(push, 156, 0)
    end
    buffer.writef32(push, 168, opacity)
    if glow then
        buffer.writef32(push, 176, glow[1])
        buffer.writef32(push, 180, glow[2])
        buffer.writef32(push, 184, glow[3])
        buffer.writef32(push, 188, glow[4])
    else
        buffer.writef32(push, 188, 0)
    end
    gfx.push(0, push)
    gfx.bind(0, surface.texture)
    gfx.draw(4)
end

local BLACK = { 0, 0, 0 }

-- Draws a rounded rectangle with its surroundings. `to_pixels` maps the
-- coordinates the rectangle (0, 0, width, height) is in to pixels.
-- `look`: {
--   radius, margin (how far the shadow and glow reach),
--   fill = { r, g, b }, fill_alpha,
--   rim = { r, g, b }, line (strength), line_width,
--   glow_color = { r, g, b }, glow (strength), glow_width, flow, phase,
--   shadow (strength), softness, drop, opacity,
--   frost (how much the fill is frosted glass; `fill_alpha` is then how
--   strongly the glass is tinted with `fill`), frost_level (how blurred),
-- }
local function draw_frame(to_pixels, width, height, look)
    local push = frame_push
    local hw, hh = width / 2, height / 2
    local a, b, c, d = to_pixels[1], to_pixels[2], to_pixels[3], to_pixels[4]
    buffer.writef32(push, 0, a)
    buffer.writef32(push, 4, b)
    buffer.writef32(push, 8, a * hw + b * hh + to_pixels[5])
    buffer.writef32(push, 16, c)
    buffer.writef32(push, 20, d)
    buffer.writef32(push, 24, c * hw + d * hh + to_pixels[6])
    buffer.writef32(push, 32, hw)
    buffer.writef32(push, 36, hh)
    buffer.writef32(push, 40, look.radius or 0)
    buffer.writef32(push, 44, look.margin or 0)
    local fill, fill_alpha = look.fill or BLACK, look.fill_alpha or 0
    buffer.writef32(push, 48, fill[1] * fill_alpha)
    buffer.writef32(push, 52, fill[2] * fill_alpha)
    buffer.writef32(push, 56, fill[3] * fill_alpha)
    buffer.writef32(push, 60, fill_alpha)
    local rim = look.rim or BLACK
    buffer.writef32(push, 64, rim[1])
    buffer.writef32(push, 68, rim[2])
    buffer.writef32(push, 72, rim[3])
    buffer.writef32(push, 76, look.line or 0)
    local glow = look.glow_color or rim
    buffer.writef32(push, 80, glow[1])
    buffer.writef32(push, 84, glow[2])
    buffer.writef32(push, 88, glow[3])
    buffer.writef32(push, 92, look.glow or 0)
    buffer.writef32(push, 96, look.shadow or 0)
    buffer.writef32(push, 100, look.softness or 1)
    buffer.writef32(push, 104, look.drop or 0)
    buffer.writef32(push, 108, look.opacity or 1)
    buffer.writef32(push, 112, look.line_width or 1)
    buffer.writef32(push, 116, look.glow_width or 1)
    buffer.writef32(push, 120, look.phase or 0)
    buffer.writef32(push, 124, look.flow or 0)
    buffer.writef32(push, 136, look.frost or 0)
    buffer.writef32(push, 140, look.frost_level or 2)
    gfx.push(0, push)
    gfx.draw(4)
end

-- The image the blurred copy of an output's picture is kept in.
local function frost_for(output)
    local width, height = (output.width + 1) // 2, (output.height + 1) // 2
    local frost = frosts[output.id]
    if frost and (frost.width ~= width or frost.height ~= height) then
        gfx.destroy(frost.slot)
        gfx.destroy(frost.image)
        frost = nil
    end
    if not frost then
        local levels = math.min(FROST_LEVELS, math.floor(math.log(math.max(width, height), 2)) + 1)
        local image = gfx.create_image({
            format = FROST_FORMAT,
            width = width,
            height = height,
            mip_levels = levels,
            usage = { "sampled", "color_target", "copy_src", "copy_dst" },
        })
        frost = { image = image, slot = gfx.create_texture_slot(image), width = width, height = height }
        frosts[output.id] = frost
    end
    return frost
end

-- Draws a frame of an output. `scene` is `stage.frame`'s:
-- {
--   backdrop = { top, bottom, ember, ember_strength, spark, spark_strength,
--                shift (logical pixels), phase, zoom },
--   frost = whether anything is filled with frosted glass,
--   frames = { { transform, width, height, look } ... }   under the windows
--   windows = { { id, over (it lies over other windows), transform, width, height, fit_x, fit_y, radius,
--               opacity, brightness, glow = { r, g, b, amount }, look } ... }   (the rectangle, and how much the
--               window's contents are stretched to fill it)
--   marks = { { transform, width, height, look } ... }    above the windows
--   labels = { { text, x, y, height, color, alpha } ... } above those
--               (centered at x, y, global logical coordinates)
-- }
-- Transforms map to global logical coordinates. Lists are bottom to top.
function K.frame(output, target, scene)
    local composite = color.composite_for(output)
    local to_output = affine.multiply(affine.scale(output.scale), affine.translate(-output.x, -output.y))

    local push = backdrop_push
    local back = scene.backdrop
    for i, c in { back.top, back.bottom, back.ember, back.spark } do
        local at = (i - 1) * 16
        buffer.writef32(push, at, c[1])
        buffer.writef32(push, at + 4, c[2])
        buffer.writef32(push, at + 8, c[3])
    end
    buffer.writef32(push, 44, back.ember_strength)
    buffer.writef32(push, 60, back.spark_strength)
    buffer.writef32(push, 64, output.width)
    buffer.writef32(push, 68, output.height)
    buffer.writef32(push, 72, back.shift * output.scale)
    buffer.writef32(push, 76, 0)
    buffer.writef32(push, 80, back.phase)
    buffer.writef32(push, 84, back.zoom)

    -- Asked for before the pass: the core may have to make the cursor's
    -- texture first.
    local pointer_x, pointer_y = hz.pointer()
    local cursor = hz.cursor()
    -- Likewise the textures of texts that are new.
    for _, label in scene.labels or {} do
        text_of(label.text)
    end
    local overlay = scene.overlay
    if overlay then
        for _, label in overlay.labels do
            text_at(label.text, label.size)
        end
    end

    gfx.begin_pass({ color = { { image = composite.image, load = "dont-care" } } })
    gfx.set_pipeline(backdrop_pipeline)
    gfx.push(0, push)
    gfx.draw(3)

    -- The blurred copy of the picture so far, for what is drawn from here
    -- on. The pass is interrupted for it.
    local frost = frost_for(output)
    local function copy_behind()
        gfx.end_pass()
        gfx.begin_pass({ color = { { image = frost.image, load = "dont-care" } } })
        gfx.set_pipeline(half_pipeline)
        gfx.bind(0, composite.slot)
        gfx.draw(3)
        gfx.end_pass()
        gfx.generate_mipmaps(frost.image)
        gfx.begin_pass({ color = { { image = composite.image, load = "load" } } })
    end
    local function frames()
        gfx.set_pipeline(frame_pipeline)
        gfx.bind(0, frost.slot)
        gfx.bind(1, frost_sampler)
    end
    if scene.frost then
        copy_behind()
    end
    local copied_again = false

    buffer.writef32(frame_push, 128, output.width)
    buffer.writef32(frame_push, 132, output.height)
    buffer.writef32(window_push, 136, output.width)
    buffer.writef32(window_push, 140, output.height)
    buffer.writef32(text_push, 48, output.width)
    buffer.writef32(text_push, 52, output.height)

    if #scene.frames > 0 then
        frames()
        for _, item in scene.frames do
            draw_frame(affine.multiply(to_output, item.transform), item.width, item.height, item.look)
        end
    end

    for _, item in scene.windows do
        local to_pixels = affine.multiply(to_output, item.transform)
        if item.over and scene.frost and not copied_again then
            -- From here on windows lie over others: their glass shows those.
            copy_behind()
            copied_again = true
        end
        if item.look then
            frames()
            draw_frame(to_pixels, item.width, item.height, item.look)
        end
        gfx.set_pipeline(window_pipeline)
        gfx.bind(1, sampler)
        local mask = { item.width, item.height, item.radius }
        -- The contents, stretched to the rectangle.
        local fit_x, fit_y = item.fit_x, item.fit_y
        to_pixels = {
            to_pixels[1] * fit_x,
            to_pixels[2] * fit_y,
            to_pixels[3] * fit_x,
            to_pixels[4] * fit_y,
            to_pixels[5],
            to_pixels[6],
        }
        for _, surface in hz.surfaces(item.id) do
            if surface.texture and surface.uv then
                -- Menus and the input method's window reach out of the
                -- window: they are not cut.
                local inside = surface.role == "toplevel" or surface.role == "subsurface"
                draw_surface(
                    to_pixels,
                    surface.x,
                    surface.y,
                    surface.width,
                    surface.height,
                    surface,
                    item.opacity,
                    if inside then mask else nil,
                    fit_x,
                    fit_y,
                    item.brightness,
                    item.glow
                )
            end
        end
    end

    if #scene.marks > 0 then
        frames()
        for _, item in scene.marks do
            draw_frame(affine.multiply(to_output, item.transform), item.width, item.height, item.look)
        end
    end

    if scene.labels and #scene.labels > 0 then
        gfx.set_pipeline(text_pipeline)
        gfx.bind(1, sampler)
        for _, label in scene.labels do
            draw_text(to_output, label.text, label.x, label.y, label.height, label.color, label.alpha)
        end
    end

    -- Over everything else (a launcher): its frames, then its texts. Its
    -- glass shows what is drawn so far.
    if overlay and (#overlay.frames > 0 or #overlay.labels > 0) then
        if overlay.frost then
            copy_behind()
        end
        if #overlay.frames > 0 then
            frames()
            for _, item in overlay.frames do
                draw_frame(affine.multiply(to_output, item.transform), item.width, item.height, item.look)
            end
        end
        if #overlay.labels > 0 then
            gfx.set_pipeline(text_pipeline)
            gfx.bind(1, sampler)
            for _, label in overlay.labels do
                draw_text(to_output, label.text, label.x, label.y, label.height, label.color, label.alpha, label.align, label.size)
            end
        end
    end

    -- What follows the pointer goes on top, snapped to whole pixels: the
    -- icon of a drag, then the cursor.
    if pointer_x then
        local px, py = affine.apply(to_output, pointer_x, pointer_y)
        local at = { output.scale, 0, 0, output.scale, math.round(px), math.round(py) }
        gfx.set_pipeline(window_pipeline)
        gfx.bind(1, sampler)
        for _, surface in hz.drag_icon() do
            if surface.texture and surface.uv then
                draw_surface(at, surface.x, surface.y, surface.width, surface.height, surface, 1, nil)
            end
        end
        if cursor then
            if cursor.texture then
                local arrow = { texture = cursor.texture, color = hz.default_color() }
                draw_surface(at, 0, 0, cursor.width, cursor.height, arrow, 1, nil)
            else
                for _, surface in cursor.surfaces do
                    if surface.texture then
                        draw_surface(at, surface.x, surface.y, surface.width, surface.height, surface, 1, nil)
                    end
                end
            end
        end
    end
    gfx.end_pass()

    color.present(composite, output, target)
    forget_texts()
end

return K
