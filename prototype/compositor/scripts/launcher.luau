-- launcher: what a launcher does, without what it looks like.
--
-- The applications of the machine (`hz.desktop_entries`), narrowed down by
-- what is typed, in a useful order, and started (`hz.spawn`). Nothing here
-- draws or decides which keys open a launcher: a project shows the result
-- its own way and calls these functions from its `key` and pointer
-- callbacks. Any project can use it (`hz.include("launcher")`); a project
-- with a module of the same name uses its own instead.
--
-- The simplest way: one state, driven by the keys.
--
--   local launcher = hz.include("launcher")
--   local menu = launcher.new({ rows = 8 })
--
--   -- logo+D:
--   if menu.open then launcher.close(menu) else launcher.open(menu) end
--
--   -- in `key`, while `menu.open`:
--   local what = launcher.key(menu, keycode, pressed, mods, k)
--   -- what: "typed" (the query changed and the list with it), "moved"
--   -- (another row is chosen), "started" (an application was started and
--   -- the menu closed), "closed" (Escape), or nil (a key it does not use).
--   -- Every key reaches the menu, none the clients: return true from `key`.
--
--   -- in `frame` (keys held down repeat; ask for frames while one is):
--   local what = launcher.repeat_keys(menu, time)
--   if menu.held then hz.request_frame() end
--
--   -- to draw it:
--   menu.query               what is typed
--   launcher.visible(menu)   the rows in view: { { app, index, chosen }, ... }
--   menu.results             every match, best first: { app, score }
--   menu.selected            the chosen one (an index into menu.results)
--   launcher.label(app, font)  the name to show (see below)
--
--   -- a click on a row:
--   launcher.start_at(menu, index)
--
-- Or the parts on their own:
--
--   launcher.applications(options)  the entries, ready for `search`
--   launcher.search(apps, query, memory)  the matches, best first
--   launcher.command(app)           the argv and `hz.spawn` options
--   launcher.start(app, memory)     starts it, and remembers that it did
--   launcher.edit(query, mods, k)   a query after a key
--
-- An application `app` is what `hz.desktop_entries` gives (`id`, `name`,
-- `default_name`, `generic_name`, `exec`, `terminal`, `working_dir`, ...).
--
-- Matching. A query is split at spaces; every word must be found, ignoring
-- case (ASCII letters only: Luau's `string.lower`), in the name, the name
-- without a locale, the generic name, the keywords or the id. Where it is
-- found counts: at the start of the text (best), at the start of a word in
-- it (after a space, `-`, `_`, `.`, `/`, or where a capital follows a small
-- letter: "Office" in "LibreOffice"), or anywhere. So does in what: the
-- name first, then the generic name, the keywords, the id.
--
-- Order. Best match first; among matches about as good, what was started
-- more often and more lately. With nothing typed, what was started comes
-- first (most often and most lately), then the rest by name. What was
-- started is remembered for the session (`menu.memory`, a plain table: a
-- project hands it on over a reload with `unload` and `init`). It is not
-- written to a file: scripts have no way to write files.
--
-- Terminal applications (`Terminal=true`) are started in the terminal the
-- session uses: the words of `HIZUKURI_TERMINAL` (or `foot`), then `-e`,
-- then the application's command.

local L = {}

-- Scores. A better kind of match always wins over a better field.
local KIND = { prefix = 300, word = 200, inside = 100 }
local FIELD = { name = 40, default_name = 38, generic_name = 25, keyword = 15, id = 10 }
-- How much having been started counts, at most (less than one kind).
local USE_WEIGHT, USE_LIMIT = 12, 6

-- Whether position i of `text` starts a word (`text` as written, before
-- lowering the case).
local function starts_word(text, i)
    if i == 1 then
        return true
    end
    local before = string.sub(text, i - 1, i - 1)
    if string.find(before, "[%s%-_%./:]") then
        return true
    end
    local here = string.sub(text, i, i)
    return string.find(before, "%l") ~= nil and string.find(here, "%u") ~= nil
end

local function field(text, weight)
    return { text = text, lower = string.lower(text), weight = weight }
end

-- The texts of an application a query is matched against.
local function fields_of(app)
    local list = { field(app.name, FIELD.name) }
    if app.default_name and app.default_name ~= app.name then
        table.insert(list, field(app.default_name, FIELD.default_name))
    end
    if app.generic_name then
        table.insert(list, field(app.generic_name, FIELD.generic_name))
    end
    for _, keyword in app.keywords or {} do
        table.insert(list, field(keyword, FIELD.keyword))
    end
    table.insert(list, field((string.gsub(app.id, "%.desktop$", "")), FIELD.id))
    return list
end

-- The applications of the machine, each with what `search` matches
-- against. `options` go to `hz.desktop_entries` (`refresh`, `locale`,
-- `desktops`).
function L.applications(options)
    local apps = hz.desktop_entries(options)
    for _, app in apps do
        app.search_fields = fields_of(app)
    end
    return apps
end

-- The words of a query, lowered.
local function words_of(query)
    local words = {}
    for word in string.gmatch(string.lower(query), "%S+") do
        table.insert(words, word)
    end
    return words
end

-- How well one word matches one text (0: not at all).
local function match_word(f, word)
    local best = 0
    local at = 1
    while true do
        local i = string.find(f.lower, word, at, true)
        if not i then
            break
        end
        local kind = if i == 1 then KIND.prefix elseif starts_word(f.text, i) then KIND.word else KIND.inside
        best = math.max(best, kind + f.weight)
        if kind == KIND.prefix then
            break
        end
        at = i + 1
    end
    return best
end

-- How much having been started counts for an application.
local function use_of(memory, id)
    local use = memory and memory.uses[id]
    if not use then
        return 0
    end
    -- Often, and lately: the last one started counts as much as three
    -- more starts.
    local age = memory.count - use.last
    return math.min(use.count + 3 / (1 + age), USE_LIMIT)
end

-- The applications that match `query`, best first: { { app, score } ... }.
-- `memory` (from `L.memory`, may be nil) brings what was started up.
function L.search(apps, query, memory)
    local words = words_of(query or "")
    local results = {}
    for _, app in apps do
        local fields = app.search_fields or fields_of(app)
        local score = 0
        local matched = true
        for _, word in words do
            local best = 0
            for _, f in fields do
                best = math.max(best, match_word(f, word))
            end
            if best == 0 then
                matched = false
                break
            end
            score += best
        end
        if matched then
            local use = use_of(memory, app.id)
            -- Each word counts once; having been started, at most a kind
            -- less than a better match.
            if #words > 0 then
                score = score / #words
            end
            table.insert(results, { app = app, score = score + use * USE_WEIGHT, use = use })
        end
    end
    table.sort(results, function(a, b)
        if a.score ~= b.score then
            return a.score > b.score
        end
        local an, bn = string.lower(a.app.name), string.lower(b.app.name)
        if #words > 0 and #an ~= #bn then
            -- Of two names that match alike, the shorter is the closer.
            return #an < #bn
        end
        if an ~= bn then
            return an < bn
        end
        return a.app.id < b.app.id
    end)
    return results
end

-- What was started in this session: { uses = { [id] = { count, last } }, count }.
function L.memory(saved)
    if type(saved) == "table" and type(saved.uses) == "table" then
        return saved
    end
    return { uses = {}, count = 0 }
end

function L.remember(memory, app)
    memory.count += 1
    local use = memory.uses[app.id] or { count = 0, last = 0 }
    use.count += 1
    use.last = memory.count
    memory.uses[app.id] = use
end

-- The words of the session's terminal command.
function L.terminal()
    local argv = {}
    for word in string.gmatch(hz.env("HIZUKURI_TERMINAL") or "foot", "%S+") do
        table.insert(argv, word)
    end
    return argv
end

-- How to start an application: the argv, and the options of `hz.spawn`.
function L.command(app)
    local argv = {}
    if app.terminal then
        argv = L.terminal()
        table.insert(argv, "-e")
    end
    for _, word in app.exec do
        table.insert(argv, word)
    end
    return argv, { cwd = app.working_dir }
end

-- Starts an application. Returns the process id, or nil and why not.
function L.start(app, memory)
    local argv, options = L.command(app)
    local pid, problem = hz.spawn(argv, options)
    if pid then
        if memory then
            L.remember(memory, app)
        end
    else
        hz.log("launcher: " .. app.id .. " did not start: " .. tostring(problem))
    end
    return pid, problem
end

-- UTF-8, by hand (the script's VM has no `utf8` library). A byte that
-- starts a character is not one of 0x80 .. 0xBF.
local function starts_character(text, i)
    local b = string.byte(text, i)
    return b ~= nil and (b < 0x80 or b >= 0xC0)
end

-- How many characters a text has.
function L.length(text)
    local count = 0
    for i = 1, #text do
        if starts_character(text, i) then
            count += 1
        end
    end
    return count
end

-- The first `count` characters of a text.
function L.cut(text, count)
    local seen = 0
    for i = 1, #text do
        if starts_character(text, i) then
            if seen == count then
                return string.sub(text, 1, i - 1)
            end
            seen += 1
        end
    end
    return text
end

-- Removes the last character of a text.
function L.erase(text)
    local i = #text
    while i > 0 and not starts_character(text, i) do
        i -= 1
    end
    if i <= 0 then
        return ""
    end
    return string.sub(text, 1, i - 1)
end

-- Removes the last word of a text, and the spaces after it.
function L.erase_word(text)
    return (string.gsub(text, "%S*%s*$", "", 1))
end

-- The query after a key: typed text is added; BackSpace takes the last
-- character away (with Ctrl, the last word); Ctrl+U empties it. Returns
-- the new query, and whether the key was one of these.
function L.edit(query, mods, k)
    local sym = k and k.sym
    if sym == "BackSpace" then
        return (if mods.ctrl then L.erase_word(query) else L.erase(query)), true
    end
    if mods.ctrl and (sym == "u" or sym == "U") then
        return "", true
    end
    if k and k.text and not (mods.ctrl or mods.alt or mods.logo) then
        return query .. k.text, true
    end
    return query, false
end

-- What a key means to a menu, by its keysym: "up", "down", "page_up",
-- "page_down", "first", "last", "start", "close", or nil.
function L.action(mods, k)
    local sym = k and k.sym
    if not sym then
        return nil
    end
    if sym == "Up" or sym == "KP_Up" or sym == "ISO_Left_Tab" or (mods.ctrl and (sym == "p" or sym == "P")) then
        return "up"
    elseif sym == "Down" or sym == "KP_Down" or sym == "Tab" or (mods.ctrl and (sym == "n" or sym == "N")) then
        return "down"
    elseif sym == "Prior" or sym == "KP_Prior" then
        return "page_up"
    elseif sym == "Next" or sym == "KP_Next" then
        return "page_down"
    elseif sym == "Home" or sym == "KP_Home" then
        return "first"
    elseif sym == "End" or sym == "KP_End" then
        return "last"
    elseif sym == "Return" or sym == "KP_Enter" then
        return "start"
    elseif sym == "Escape" then
        return "close"
    end
    return nil
end

-- A name to show for an application with `font` (from `hz.load_font`):
-- its name, or the name without a locale when the font has no glyphs for
-- the first (or without a font).
function L.label(app, font)
    if font and hz.missing_glyphs and hz.missing_glyphs(font, app.name) > 0 then
        return app.default_name or app.name
    end
    return app.name
end

-- The one-letter mark that stands in for an icon: the first letter of the
-- name that can be shown, as a capital.
function L.initial(app, font)
    local name = L.label(app, font)
    local first = string.match(name, "%w") or L.cut(name, 1)
    return string.upper(first)
end

-- A menu --------------------------------------------------------------------

-- A menu's state. `options.rows`: how many rows are in view (8).
function L.new(options)
    options = options or {}
    return {
        open = false,
        query = "",
        apps = {},
        results = {},
        selected = 1,
        -- The first row in view (an index into `results`).
        top = 1,
        rows = options.rows or 8,
        memory = L.memory(options.memory),
        -- The options of `hz.desktop_entries`.
        entries = options.entries,
        -- When the menu last opened, closed, or changed (`hz.time()`).
        changed_at = 0,
    }
end

local function refilter(menu)
    menu.results = L.search(menu.apps, menu.query, menu.memory)
    menu.selected = 1
    menu.top = 1
end

-- Keeps the chosen row in view.
local function follow(menu)
    local count = #menu.results
    if count == 0 then
        menu.selected, menu.top = 1, 1
        return
    end
    menu.selected = math.clamp(menu.selected, 1, count)
    if menu.selected < menu.top then
        menu.top = menu.selected
    elseif menu.selected >= menu.top + menu.rows then
        menu.top = menu.selected - menu.rows + 1
    end
    menu.top = math.clamp(menu.top, 1, math.max(1, count - menu.rows + 1))
end

-- Opens the menu: reads the applications (again, if they changed), empties
-- the query.
function L.open(menu)
    menu.apps = L.applications(menu.entries)
    menu.query = ""
    menu.open = true
    menu.changed_at = hz.time()
    refilter(menu)
end

function L.close(menu)
    menu.open = false
    menu.held = nil
    menu.changed_at = hz.time()
end

-- Chooses another row: by `delta` rows, wrapping around at the ends.
function L.move(menu, delta)
    local count = #menu.results
    if count == 0 then
        return
    end
    menu.selected = (menu.selected - 1 + delta) % count + 1
    follow(menu)
end

-- Chooses a row (an index into `results`), without wrapping.
function L.choose(menu, index)
    menu.selected = index
    follow(menu)
end

-- Starts the application of a row and closes the menu. Returns the
-- process id, or nil.
function L.start_at(menu, index)
    local result = menu.results[index]
    if not result then
        return nil
    end
    local pid = L.start(result.app, menu.memory)
    L.close(menu)
    return pid
end

-- The rows in view: { { app, index, chosen } ... }.
function L.visible(menu)
    local rows = {}
    for index = menu.top, math.min(menu.top + menu.rows - 1, #menu.results) do
        table.insert(rows, { app = menu.results[index].app, index = index, chosen = index == menu.selected })
    end
    return rows
end

-- A key held down repeats, as it would in a client: after `REPEAT_DELAY`
-- seconds, every `REPEAT_INTERVAL` (the seat's 400 ms and 25 per second).
-- The core repeats no key for the script; `L.repeat_keys` does, from the
-- project's `frame` (which asks for frames while `menu.held`).
L.REPEAT_DELAY = 0.4
L.REPEAT_INTERVAL = 0.04

local press

-- The repeats of the held key that are due at time t. Returns what the
-- last of them did ("typed", "moved"), or nil.
function L.repeat_keys(menu, t)
    local held = menu.held
    local what = nil
    local count = 0
    while held and menu.open and held.next <= t and count < 50 do
        what = press(menu, held.keycode, held.mods, held.k) or what
        held.next += L.REPEAT_INTERVAL
        count += 1
    end
    return what
end

-- A key while the menu is open (see the top of this file).
function L.key(menu, keycode, pressed, mods, k)
    if not menu.open then
        return nil
    end
    if not pressed then
        if menu.held and menu.held.keycode == keycode then
            menu.held = nil
        end
        return nil
    end
    local what = press(menu, keycode, mods, k)
    -- What types or moves repeats while held; nothing else does.
    if what == "typed" or what == "moved" then
        menu.held = { keycode = keycode, mods = mods, k = k, next = hz.time() + L.REPEAT_DELAY }
    else
        menu.held = nil
    end
    return what
end

function press(menu, keycode, mods, k)
    local action = L.action(mods, k)
    if action == "close" then
        L.close(menu)
        return "closed"
    elseif action == "start" then
        if L.start_at(menu, menu.selected) then
            return "started"
        end
        return nil
    elseif action == "up" or action == "down" then
        L.move(menu, if action == "up" then -1 else 1)
        return "moved"
    elseif action == "page_up" or action == "page_down" then
        L.choose(menu, menu.selected + (if action == "page_up" then -menu.rows else menu.rows))
        return "moved"
    elseif action == "first" or action == "last" then
        L.choose(menu, if action == "first" then 1 else #menu.results)
        return "moved"
    end
    local query, edited = L.edit(menu.query, mods, k)
    if edited then
        if query ~= menu.query then
            menu.query = query
            menu.changed_at = hz.time()
            refilter(menu)
        end
        return "typed"
    end
    return nil
end

return L
