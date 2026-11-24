//! `hz.desktop_entries`: the applications installed on the machine, for a
//! script that offers to start them (a launcher, a dock, a menu).

use mlua::{Lua, Table, Value};

use super::api::{SharedRef, arg_error};
use crate::desktop_entry::{DesktopEntry, Locale, Scan, Search, desktop_names};

/// What the last call read, and for what.
#[derive(Debug, Default)]
pub(crate) struct AppsState {
    cached: Option<(Search, Scan)>,
}

fn entry_table(lua: &Lua, entry: &DesktopEntry) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("id", entry.id.as_str())?;
    t.set("name", entry.name.as_str())?;
    t.set("default_name", entry.default_name.as_str())?;
    t.set("generic_name", entry.generic_name.as_deref())?;
    t.set("comment", entry.comment.as_deref())?;
    t.set(
        "exec",
        lua.create_sequence_from(entry.exec.iter().map(String::as_str))?,
    )?;
    t.set("icon", entry.icon.as_deref())?;
    t.set("terminal", entry.terminal)?;
    t.set("working_dir", entry.working_dir.as_deref())?;
    t.set(
        "categories",
        lua.create_sequence_from(entry.categories.iter().map(String::as_str))?,
    )?;
    t.set(
        "keywords",
        lua.create_sequence_from(entry.keywords.iter().map(String::as_str))?,
    )?;
    t.set("file", entry.file.to_string_lossy().as_ref())?;
    Ok(t)
}

/// Where to look and what to match, from the compositor's environment and
/// what the call overrides.
fn search(options: Option<&Table>) -> mlua::Result<Search> {
    let mut search = Search::from_env(|name| std::env::var(name).ok());
    let Some(options) = options else {
        return Ok(search);
    };
    match options.get::<Value>("locale")? {
        Value::Nil => {}
        Value::String(locale) => search.locale = Locale::parse(&locale.to_str()?),
        _ => return Err(arg_error("hz.desktop_entries: locale is a string")),
    }
    match options.get::<Value>("desktops")? {
        Value::Nil => {}
        Value::String(names) => search.desktops = desktop_names(&names.to_str()?),
        Value::Table(names) => {
            search.desktops = names
                .sequence_values::<String>()
                .collect::<mlua::Result<_>>()?
        }
        _ => {
            return Err(arg_error(
                "hz.desktop_entries: desktops is a list of names, or one string with colons",
            ));
        }
    }
    Ok(search)
}

pub(super) fn register(lua: &Lua, hz: &Table, shared: &SharedRef) -> mlua::Result<()> {
    let state = std::rc::Rc::clone(shared);
    hz.set(
        "desktop_entries",
        lua.create_function(move |lua, options: Option<Table>| {
            let refresh = match &options {
                Some(options) => options.get::<Option<bool>>("refresh")?.unwrap_or(false),
                None => false,
            };
            let search = search(options.as_ref())?;
            let mut shared = state.borrow_mut();
            let cache = &mut shared.apps.cached;
            // Read again when asked, when the places or the matching
            // changed, or when a directory read last time changed.
            let fresh = matches!(cache, Some((last, scan))
                if !refresh && *last == search && !scan.outdated());
            if !fresh {
                let scan = Scan::read(&search);
                tracing::debug!(
                    entries = scan.entries.len(),
                    left_out = scan.problems.len(),
                    "read the desktop entries"
                );
                for (path, problem) in &scan.problems {
                    tracing::debug!(path = %path.display(), "a desktop entry was left out: {problem}");
                }
                *cache = Some((search, scan));
            }
            let (_, scan) = cache.as_ref().expect("just filled");
            let list = lua.create_table_with_capacity(scan.entries.len(), 0)?;
            for entry in &scan.entries {
                list.push(entry_table(lua, entry)?)?;
            }
            Ok(list)
        })?,
    )?;
    Ok(())
}
