-- A project that draws and leaves window management alone.
--
-- `session` here replaces the built-in one: the built-in renderer, without
-- the tiling. Window placement and focus are then the core's fallback.
hz.include("session")
