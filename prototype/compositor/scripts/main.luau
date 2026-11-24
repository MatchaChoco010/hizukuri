-- The entry of the built-in project.
--
-- Everything is in `session`, so that another project can start from the
-- built-in behaviour with one line (`hz.include("session")`) and then
-- replace or wrap the functions it wants to change.
hz.include("session")
