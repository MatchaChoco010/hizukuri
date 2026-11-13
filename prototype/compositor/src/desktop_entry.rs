//! The applications installed on the machine: XDG desktop entries
//! (Desktop Entry Specification 1.5), read the way a menu or a launcher
//! reads them.
//!
//! - Where: `$XDG_DATA_HOME/applications` (by default
//!   `~/.local/share/applications`), then `applications` in every directory
//!   of `$XDG_DATA_DIRS` (by default `/usr/local/share:/usr/share`), with
//!   their subdirectories. A file's desktop file ID is its path below
//!   `applications/` with `/` replaced by `-`; of the files with the same ID,
//!   the one found first is the one that counts, also when it hides the
//!   entry (`Hidden`, `NoDisplay`): that is how a user takes an application
//!   of the system out of the menus.
//! - Left out: entries that are not of `Type=Application`, `Hidden`,
//!   `NoDisplay`, without `Name`, without a command (`Exec`), with a command
//!   the specification calls invalid (an unknown field code, an open
//!   quote), whose `TryExec` program is not there, and those `OnlyShowIn` and
//!   `NotShowIn` keep away from the desktops named (`XDG_CURRENT_DESKTOP`).
//! - The command is split into arguments by the quoting rules of `Exec`,
//!   and its field codes are expanded for a start without files:
//!   [`parse_exec`].
//!
//! Nothing here starts anything; the script does, with `hz.spawn`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// One application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopEntry {
    /// The desktop file ID (`org.example.App.desktop`).
    pub id: String,
    /// `Name`, in the locale (see [`Locale`]).
    pub name: String,
    /// `Name` without a locale: for when the one in the locale cannot be
    /// shown (a font without its characters).
    pub default_name: String,
    pub generic_name: Option<String>,
    pub comment: Option<String>,
    /// The command to start it with, as arguments (field codes expanded for
    /// a start without files).
    pub exec: Vec<String>,
    /// `Icon`: an icon name, or an absolute path.
    pub icon: Option<String>,
    /// Whether it runs in a terminal (`Terminal`).
    pub terminal: bool,
    /// The working directory to start it in (`Path`).
    pub working_dir: Option<String>,
    pub categories: Vec<String>,
    /// `Keywords`, in the locale.
    pub keywords: Vec<String>,
    /// Where the desktop file is.
    pub file: PathBuf,
}

/// Where to look and what to match against: what the environment says,
/// unless the caller says otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Search {
    /// `applications` directories, most important first.
    pub dirs: Vec<PathBuf>,
    pub locale: Option<Locale>,
    /// The names of the current desktop (`XDG_CURRENT_DESKTOP`), in order.
    pub desktops: Vec<String>,
    /// `PATH`, for `TryExec`.
    pub path: Vec<PathBuf>,
}

impl Search {
    /// From an environment (`var` looks a variable up).
    pub fn from_env(var: impl Fn(&str) -> Option<String>) -> Search {
        Search {
            dirs: application_dirs(&var),
            locale: messages_locale(&var),
            desktops: var("XDG_CURRENT_DESKTOP")
                .map(|value| desktop_names(&value))
                .unwrap_or_default(),
            path: var("PATH")
                .map(|value| {
                    value
                        .split(':')
                        .filter(|d| !d.is_empty())
                        .map(PathBuf::from)
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

/// A colon-separated list of desktop names.
pub fn desktop_names(value: &str) -> Vec<String> {
    value
        .split(':')
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The `applications` directories of the XDG base directories, most
/// important first. Relative paths in the variables are ignored, as the
/// base directory specification says.
fn application_dirs(var: &impl Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    let absolute = |value: &str| -> Option<PathBuf> {
        let path = PathBuf::from(value);
        path.is_absolute().then_some(path)
    };
    let mut bases = Vec::new();
    match var("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        Some(home) => bases.extend(absolute(&home)),
        None => {
            if let Some(home) = var("HOME").filter(|v| !v.is_empty()) {
                bases.extend(absolute(&home).map(|h| h.join(".local/share")));
            }
        }
    }
    let dirs = var("XDG_DATA_DIRS")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_owned());
    bases.extend(dirs.split(':').filter_map(absolute));
    let mut seen = HashSet::new();
    bases
        .into_iter()
        .map(|base| base.join("applications"))
        .filter(|dir| seen.insert(dir.clone()))
        .collect()
}

/// A POSIX locale, as far as matching localized keys goes:
/// `lang_COUNTRY.ENCODING@MODIFIER` without the encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Locale {
    pub lang: String,
    pub country: Option<String>,
    pub modifier: Option<String>,
}

impl Locale {
    /// Parses `lang_COUNTRY.ENCODING@MODIFIER` (all but `lang` may be
    /// missing). `C` and `POSIX` are no locale.
    pub fn parse(value: &str) -> Option<Locale> {
        let (rest, modifier) = match value.split_once('@') {
            Some((rest, modifier)) => (rest, Some(modifier.to_owned())),
            None => (value, None),
        };
        let rest = rest.split_once('.').map_or(rest, |(rest, _)| rest);
        let (lang, country) = match rest.split_once('_') {
            Some((lang, country)) => (lang, Some(country.to_owned())),
            None => (rest, None),
        };
        if lang.is_empty() || lang == "C" || lang == "POSIX" {
            return None;
        }
        Some(Locale {
            lang: lang.to_owned(),
            country: country.filter(|c| !c.is_empty()),
            modifier: modifier.filter(|m| !m.is_empty()),
        })
    }

    /// The locale postfixes to try, best first (the specification's table
    /// of `LC_MESSAGES` values and the keys matched for them).
    fn candidates(&self) -> Vec<String> {
        let lang = &self.lang;
        let mut list = Vec::new();
        if let (Some(country), Some(modifier)) = (&self.country, &self.modifier) {
            list.push(format!("{lang}_{country}@{modifier}"));
        }
        if let Some(country) = &self.country {
            list.push(format!("{lang}_{country}"));
        }
        if let Some(modifier) = &self.modifier {
            list.push(format!("{lang}@{modifier}"));
        }
        list.push(lang.clone());
        list
    }
}

/// The locale of the `LC_MESSAGES` category: `LC_ALL`, else `LC_MESSAGES`,
/// else `LANG` (the first of them that is set and not empty).
pub fn messages_locale(var: &impl Fn(&str) -> Option<String>) -> Option<Locale> {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|name| var(name).filter(|v| !v.is_empty()))
        .and_then(|value| Locale::parse(&value))
}

/// The `[Desktop Entry]` group of a file: key (with its locale postfix, if
/// any) to the value as written (escapes not undone).
#[derive(Debug, Default)]
struct Group {
    values: HashMap<String, String>,
}

impl Group {
    fn parse(text: &str) -> Option<Group> {
        let mut group = Group::default();
        let mut inside = false;
        let mut found = false;
        for line in text.lines() {
            let line = line.trim_suffix('\r');
            let trimmed = line.trim_start();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if let Some(header) = trimmed.strip_prefix('[') {
                if inside {
                    // The group ends at the next header.
                    break;
                }
                inside = header.trim_end().strip_suffix(']') == Some("Desktop Entry");
                found |= inside;
                continue;
            }
            if !inside {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            if !valid_key(key) {
                continue;
            }
            // Of keys written twice, the first counts.
            group
                .values
                .entry(key.to_owned())
                .or_insert_with(|| value.trim().to_owned());
        }
        found.then_some(group)
    }

    fn raw(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    fn string(&self, key: &str) -> Option<String> {
        self.raw(key).map(unescape)
    }

    fn boolean(&self, key: &str) -> bool {
        matches!(self.raw(key), Some("true" | "1"))
    }

    fn list(&self, key: &str) -> Vec<String> {
        self.raw(key).map(split_list).unwrap_or_default()
    }

    /// The value of a localized key in the best matching locale.
    fn localized_raw(&self, key: &str, locale: Option<&Locale>) -> Option<&str> {
        if let Some(locale) = locale {
            for candidate in locale.candidates() {
                if let Some(value) = self.localized_exact(key, &candidate) {
                    return Some(value);
                }
            }
        }
        self.raw(key)
    }

    /// The value of `key[postfix]`, where the postfix written in the file
    /// may carry an encoding (which is not matched).
    fn localized_exact(&self, key: &str, postfix: &str) -> Option<&str> {
        if let Some(value) = self.raw(&format!("{key}[{postfix}]")) {
            return Some(value);
        }
        self.values.iter().find_map(|(k, v)| {
            let written = k.strip_prefix(key)?.strip_circumfix('[', ']')?;
            (strip_encoding(written) == postfix).then_some(v.as_str())
        })
    }

    fn localized(&self, key: &str, locale: Option<&Locale>) -> Option<String> {
        self.localized_raw(key, locale).map(unescape)
    }

    fn localized_list(&self, key: &str, locale: Option<&Locale>) -> Vec<String> {
        self.localized_raw(key, locale)
            .map(split_list)
            .unwrap_or_default()
    }
}

fn strip_encoding(postfix: &str) -> String {
    match postfix.split_once('.') {
        Some((before, after)) => match after.split_once('@') {
            Some((_, modifier)) => format!("{before}@{modifier}"),
            None => before.to_owned(),
        },
        None => postfix.to_owned(),
    }
}

/// A key name, with an optional locale postfix: `A-Za-z0-9-`, then
/// `[...]`.
fn valid_key(key: &str) -> bool {
    let name = match key.split_once('[') {
        Some((name, rest)) => {
            if !rest.ends_with(']') || rest.len() < 2 {
                return false;
            }
            name
        }
        None => key,
    };
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Undoes the escapes of the string types: `\s`, `\n`, `\t`, `\r`, `\\`.
/// Other backslashes stay as they are (the quoting of `Exec` reads them
/// next).
pub fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Splits a list value at `;` (`\;` is a semicolon in a value) and undoes
/// the escapes of each value. Empty values are dropped.
pub fn split_list(value: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut current = String::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(';') => current.push(';'),
                Some(other) => {
                    current.push('\\');
                    current.push(other);
                }
                None => current.push('\\'),
            },
            ';' => items.push(std::mem::take(&mut current)),
            c => current.push(c),
        }
    }
    items.push(current);
    items
        .iter()
        .map(|item| unescape(item))
        .filter(|item| !item.is_empty())
        .collect()
}

/// What an `Exec` value fills in for its field codes.
pub struct ExecContext<'a> {
    /// `Name` in the locale (`%c`).
    pub name: &'a str,
    /// `Icon` (`%i`).
    pub icon: Option<&'a str>,
    /// The desktop file (`%k`).
    pub file: &'a Path,
}

/// Why an `Exec` value cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecError {
    Empty,
    /// A double quote that is not closed.
    OpenQuote,
    /// A field code the specification does not define: the command line is
    /// invalid and must not be processed.
    UnknownFieldCode(char),
}

/// Splits an `Exec` value (with the string escapes already undone) into
/// arguments, and expands its field codes for a start without files or
/// URLs.
///
/// - Arguments are separated by spaces (and tabs). An argument in double
///   quotes may hold anything; in it, a backslash makes the next `"`, `` ` ``,
///   `$` or `\` literal. Outside quotes, a backslash makes the next character
///   literal (not allowed by the specification, but written by some files).
/// - Quoting is undone first, then the field codes of each argument are
///   expanded: `%f %F %u %U` (files and URLs) and the deprecated
///   `%d %D %n %N %v %m` are removed, `%i` is `--icon <Icon>` (nothing
///   without an icon), `%c` the name, `%k` the desktop file, `%%` a `%`.
///   An argument that held nothing but removed codes is dropped.
pub fn parse_exec(value: &str, context: &ExecContext) -> Result<Vec<String>, ExecError> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut quoted = false;
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' => quoted = false,
                '\\' => match chars.next() {
                    Some(next @ ('"' | '`' | '$' | '\\')) => current.push(next),
                    Some(next) => {
                        current.push('\\');
                        current.push(next);
                    }
                    None => return Err(ExecError::OpenQuote),
                },
                c => current.push(c),
            }
            continue;
        }
        match c {
            ' ' | '\t' | '\n' => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            '"' => {
                quoted = true;
                in_word = true;
            }
            '\\' => {
                in_word = true;
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            c => {
                in_word = true;
                current.push(c);
            }
        }
    }
    if quoted {
        return Err(ExecError::OpenQuote);
    }
    if in_word {
        words.push(current);
    }

    let mut argv = Vec::new();
    for word in words {
        if word == "%i" {
            if let Some(icon) = context.icon.filter(|i| !i.is_empty()) {
                argv.push("--icon".to_owned());
                argv.push(icon.to_owned());
            }
            continue;
        }
        let mut out = String::new();
        let mut removed = false;
        let mut chars = word.chars();
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('%') => out.push('%'),
                Some('f' | 'F' | 'u' | 'U' | 'd' | 'D' | 'n' | 'N' | 'v' | 'm') => removed = true,
                Some('c') => out.push_str(context.name),
                Some('k') => out.push_str(&context.file.to_string_lossy()),
                Some('i') => out.push_str(context.icon.unwrap_or("")),
                Some(other) => return Err(ExecError::UnknownFieldCode(other)),
                None => return Err(ExecError::UnknownFieldCode(' ')),
            }
        }
        if removed && out.is_empty() {
            continue;
        }
        argv.push(out);
    }
    if argv.is_empty() || argv[0].is_empty() {
        return Err(ExecError::Empty);
    }
    Ok(argv)
}

/// Whether `OnlyShowIn` and `NotShowIn` let an entry show on the desktops
/// named (`XDG_CURRENT_DESKTOP`, in order): the first name that either list
/// has decides; when none does, it shows unless it has `OnlyShowIn`.
pub fn shown_in(only: &[String], not: &[String], desktops: &[String]) -> bool {
    for desktop in desktops {
        if only.iter().any(|d| d == desktop) {
            return true;
        }
        if not.iter().any(|d| d == desktop) {
            return false;
        }
    }
    only.is_empty()
}

/// Whether `TryExec` names a program that is there: an absolute path, or a
/// name found in `PATH`, of an executable file.
fn program_exists(program: &str, path: &[PathBuf]) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let executable = |candidate: &Path| {
        std::fs::metadata(candidate)
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if program.contains('/') {
        let candidate = Path::new(program);
        return candidate.is_absolute() && executable(candidate);
    }
    path.iter().any(|dir| executable(&dir.join(program)))
}

/// Reads one desktop file into an entry, or says why it is left out. The
/// ID is given; `Hidden` entries come back as `Ok(None)`.
pub fn read_entry(
    id: &str,
    file: &Path,
    text: &str,
    search: &Search,
) -> Result<Option<DesktopEntry>, String> {
    let group = Group::parse(text).ok_or("no [Desktop Entry] group")?;
    if group.boolean("Hidden") {
        return Ok(None);
    }
    if group.raw("Type") != Some("Application") {
        return Ok(None);
    }
    if group.boolean("NoDisplay") {
        return Ok(None);
    }
    let locale = search.locale.as_ref();
    let Some(name) = group.localized("Name", locale).filter(|n| !n.is_empty()) else {
        return Err("no Name".to_owned());
    };
    if !shown_in(
        &group.list("OnlyShowIn"),
        &group.list("NotShowIn"),
        &search.desktops,
    ) {
        return Ok(None);
    }
    if let Some(program) = group.string("TryExec").filter(|p| !p.is_empty())
        && !program_exists(&program, &search.path)
    {
        return Ok(None);
    }
    let Some(exec) = group.string("Exec") else {
        // A D-Bus activatable application without a command can only be
        // started over D-Bus.
        return Err("no Exec".to_owned());
    };
    let icon = group.localized("Icon", locale).filter(|i| !i.is_empty());
    let exec = parse_exec(
        &exec,
        &ExecContext {
            name: &name,
            icon: icon.as_deref(),
            file,
        },
    )
    .map_err(|error| format!("Exec: {error:?}"))?;
    Ok(Some(DesktopEntry {
        id: id.to_owned(),
        generic_name: group
            .localized("GenericName", locale)
            .filter(|n| !n.is_empty()),
        comment: group.localized("Comment", locale).filter(|c| !c.is_empty()),
        exec,
        icon,
        terminal: group.boolean("Terminal"),
        working_dir: group.string("Path").filter(|p| !p.is_empty()),
        categories: group.list("Categories"),
        keywords: group.localized_list("Keywords", locale),
        file: file.to_owned(),
        default_name: group
            .string("Name")
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| name.clone()),
        name,
    }))
}

/// The desktop files below one `applications` directory: (ID, path), in
/// order of path. Directories it went through (for [`Scan::stamp`]) are
/// added to `dirs`.
fn files_below(
    root: &Path,
    dirs: &mut Vec<(PathBuf, Option<SystemTime>)>,
) -> Vec<(String, PathBuf)> {
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    walk(root, root, 0, &mut files, dirs, &mut seen);
    files
}

fn walk(
    root: &Path,
    dir: &Path,
    depth: usize,
    files: &mut Vec<(String, PathBuf)>,
    dirs: &mut Vec<(PathBuf, Option<SystemTime>)>,
    seen: &mut HashSet<PathBuf>,
) {
    let modified = std::fs::metadata(dir).and_then(|m| m.modified()).ok();
    dirs.push((dir.to_owned(), modified));
    // Directories linked into themselves are gone through once.
    let Ok(canonical) = std::fs::canonicalize(dir) else {
        return;
    };
    if depth > 8 || !seen.insert(canonical) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort();
    for path in paths {
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        if metadata.is_dir() {
            walk(root, &path, depth + 1, files, dirs, seen);
        } else if metadata.is_file() && path.extension().is_some_and(|e| e == "desktop") {
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            let id = relative.to_string_lossy().replace('/', "-");
            files.push((id, path));
        }
    }
}

/// The result of reading every directory once.
#[derive(Debug, Clone, Default)]
pub struct Scan {
    pub entries: Vec<DesktopEntry>,
    /// Files that were left out because they could not be read or were
    /// malformed: path and reason.
    pub problems: Vec<(PathBuf, String)>,
    /// Every directory read (and every `applications` directory that does
    /// not exist), with when it last changed.
    pub stamp: Vec<(PathBuf, Option<SystemTime>)>,
}

impl Scan {
    /// Reads the entries. Sorted by name (ignoring case), then ID.
    pub fn read(search: &Search) -> Scan {
        let mut scan = Scan::default();
        let mut taken = HashSet::new();
        for root in &search.dirs {
            for (id, path) in files_below(root, &mut scan.stamp) {
                // The first file with an ID is the one that counts, whatever
                // it says.
                if !taken.insert(id.clone()) {
                    continue;
                }
                let text = match std::fs::read(&path).map(String::from_utf8) {
                    Ok(Ok(text)) => text,
                    Ok(Err(_)) => {
                        scan.problems.push((path, "not UTF-8".to_owned()));
                        continue;
                    }
                    Err(error) => {
                        scan.problems.push((path, error.to_string()));
                        continue;
                    }
                };
                match read_entry(&id, &path, &text, search) {
                    Ok(Some(entry)) => scan.entries.push(entry),
                    Ok(None) => {}
                    Err(problem) => scan.problems.push((path, problem)),
                }
            }
        }
        scan.entries.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.id.cmp(&b.id))
        });
        scan
    }

    /// Whether a directory it read changed since (a file was added,
    /// removed or replaced; editing a file in place is not seen).
    pub fn outdated(&self) -> bool {
        self.stamp.iter().any(|(dir, modified)| {
            std::fs::metadata(dir).and_then(|m| m.modified()).ok() != *modified
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> ExecContext<'static> {
        ExecContext {
            name: "Some App",
            icon: Some("some-icon"),
            file: Path::new("/data/applications/some.desktop"),
        }
    }

    fn exec(value: &str) -> Result<Vec<String>, ExecError> {
        parse_exec(&unescape(value), &context())
    }

    #[test]
    fn exec_splits_at_spaces_and_undoes_quoting() {
        assert_eq!(exec("foo  -a   b").unwrap(), ["foo", "-a", "b"]);
        assert_eq!(
            exec(r#""/opt/my app/run" --title "a b""#).unwrap(),
            ["/opt/my app/run", "--title", "a b"]
        );
        // Inside quotes, \" \` \$ and \\ are the characters. In the file,
        // the backslash itself is written twice (the string escape comes
        // first).
        assert_eq!(
            exec(r#"sh -c "echo \\"hi\\" \\$HOME \\\\ \\`x\\`""#).unwrap(),
            ["sh", "-c", r#"echo "hi" $HOME \ `x`"#]
        );
        assert_eq!(exec(r#"foo """#).unwrap(), ["foo", ""]);
        assert_eq!(exec(r#"foo a"b c"d"#).unwrap(), ["foo", "ab cd"]);
        assert_eq!(exec(r"foo\sbar").unwrap(), ["foo", "bar"]);
        assert_eq!(exec(r#"foo "open"#), Err(ExecError::OpenQuote));
        assert_eq!(exec("   "), Err(ExecError::Empty));
    }

    #[test]
    fn exec_field_codes() {
        // Files and URLs: none at a start from a launcher.
        assert_eq!(exec("foo %U").unwrap(), ["foo"]);
        assert_eq!(exec("foo %f --x").unwrap(), ["foo", "--x"]);
        assert_eq!(exec("foo --open=%u").unwrap(), ["foo", "--open="]);
        // Deprecated ones are removed.
        assert_eq!(exec("foo %d %D %n %N %v %m").unwrap(), ["foo"]);
        assert_eq!(exec("foo %i").unwrap(), ["foo", "--icon", "some-icon"]);
        assert_eq!(exec("foo %c").unwrap(), ["foo", "Some App"]);
        assert_eq!(
            exec("foo %k").unwrap(),
            ["foo", "/data/applications/some.desktop"]
        );
        assert_eq!(exec("foo 100%% %%f").unwrap(), ["foo", "100%", "%f"]);
        assert_eq!(exec("foo %x"), Err(ExecError::UnknownFieldCode('x')));
        let no_icon = ExecContext {
            icon: None,
            ..context()
        };
        assert_eq!(parse_exec("foo %i", &no_icon).unwrap(), ["foo"]);
    }

    #[test]
    fn string_escapes_and_lists() {
        assert_eq!(unescape(r"a\sb\tc\nd\\e\x"), "a b\tc\nd\\e\\x");
        assert_eq!(split_list("A;B;C;"), ["A", "B", "C"]);
        assert_eq!(split_list(r"A\;B;C"), ["A;B", "C"]);
        assert_eq!(split_list(r"x\sy;;"), ["x y"]);
    }

    #[test]
    fn locales() {
        assert_eq!(
            Locale::parse("ja_JP.UTF-8"),
            Some(Locale {
                lang: "ja".into(),
                country: Some("JP".into()),
                modifier: None
            })
        );
        assert_eq!(Locale::parse("C.UTF-8"), None);
        assert_eq!(Locale::parse("POSIX"), None);
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, v)| (*v).to_owned())
            }
        };
        // LC_ALL wins over LC_MESSAGES, which wins over LANG.
        assert_eq!(
            messages_locale(&env(&[("LANG", "de_DE.UTF-8"), ("LC_MESSAGES", "fr_FR")]))
                .unwrap()
                .lang,
            "fr"
        );
        assert_eq!(
            messages_locale(&env(&[("LC_ALL", "ja_JP.UTF-8"), ("LC_MESSAGES", "fr_FR")]))
                .unwrap()
                .lang,
            "ja"
        );
        assert_eq!(messages_locale(&env(&[("LANG", "")])), None);
    }

    #[test]
    fn localized_keys_are_chosen_best_first() {
        let text = "[Desktop Entry]\nName=Foo\nName[sr_YU]=yu\nName[sr@Latn]=latn\nName[sr]=sr\n\
                    Name[ja]=ja\nName[de_DE.UTF-8]=de\n";
        let group = Group::parse(text).unwrap();
        let name = |locale: &str| group.localized("Name", Locale::parse(locale).as_ref());
        // The example of the specification.
        assert_eq!(name("sr_YU@Latn").unwrap(), "yu");
        assert_eq!(name("sr@Latn").unwrap(), "latn");
        assert_eq!(name("sr_CS").unwrap(), "sr");
        assert_eq!(name("ja_JP.UTF-8").unwrap(), "ja");
        // The encoding written in a key is not matched.
        assert_eq!(name("de_DE").unwrap(), "de");
        assert_eq!(name("en_US").unwrap(), "Foo");
        assert_eq!(group.localized("Name", None).unwrap(), "Foo");
    }

    #[test]
    fn only_and_not_show_in() {
        let names = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(shown_in(&[], &[], &[]));
        assert!(!shown_in(&names(&["GNOME"]), &[], &[]));
        assert!(!shown_in(&names(&["GNOME"]), &[], &names(&["KDE"])));
        assert!(shown_in(&names(&["GNOME"]), &[], &names(&["KDE", "GNOME"])));
        assert!(!shown_in(&[], &names(&["KDE"]), &names(&["KDE"])));
        assert!(shown_in(&[], &names(&["KDE"]), &names(&["GNOME"])));
        // The first name either list has decides.
        assert!(!shown_in(
            &names(&["B"]),
            &names(&["A"]),
            &names(&["A", "B"])
        ));
    }

    /// A directory tree for one test, removed at the end.
    struct Tree(PathBuf);

    impl Tree {
        fn new(name: &str) -> Tree {
            let dir = std::env::temp_dir().join(format!(
                "hizukuri-desktop-entry-{name}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Tree(dir)
        }

        fn write(&self, path: &str, text: &str) {
            let path = self.0.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn app(name: &str, exec: &str, extra: &str) -> String {
        format!(
            "# comment\n[Desktop Entry]\nType=Application\nName={name}\nExec={exec}\n{extra}\n[Desktop Action new]\nName=Other\nExec=other\n"
        )
    }

    #[test]
    fn scan_orders_hides_and_filters() {
        let tree = Tree::new("scan");
        let home = tree.0.join("home/applications");
        let system = tree.0.join("system/applications");
        let bin = tree.0.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(bin.join("present"), "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(bin.join("present"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        // The user's own copy wins over the system's.
        tree.write(
            "home/applications/editor.desktop",
            &app("My Editor", "edit --mine", ""),
        );
        tree.write(
            "system/applications/editor.desktop",
            &app("Editor", "edit", ""),
        );
        // A user hides an application of the system.
        tree.write(
            "home/applications/hidden.desktop",
            "[Desktop Entry]\nType=Application\nName=X\nExec=x\nHidden=true\n",
        );
        tree.write(
            "system/applications/hidden.desktop",
            &app("Hidden App", "hidden", ""),
        );
        // A subdirectory gives an ID with a dash.
        tree.write("system/applications/vendor/tool.desktop", &app("Tool", "tool %F", "Terminal=true\nPath=/srv\nIcon=tool\nCategories=Utility;Development;\nKeywords=a;b;\nKeywords[ja]=あ;い;\nName[ja]=道具\nGenericName=Thing\nComment=Does things"));
        tree.write(
            "system/applications/nodisplay.desktop",
            &app("NoDisplay", "nd", "NoDisplay=true"),
        );
        tree.write(
            "system/applications/link.desktop",
            "[Desktop Entry]\nType=Link\nName=L\nURL=http://example.org\n",
        );
        tree.write(
            "system/applications/missing.desktop",
            &app("Missing", "m", "TryExec=hizukuri-no-such-program"),
        );
        tree.write(
            "system/applications/present.desktop",
            &app("Present", "p", "TryExec=present"),
        );
        tree.write(
            "system/applications/gnome.desktop",
            &app("Gnome Only", "g", "OnlyShowIn=GNOME;"),
        );
        tree.write(
            "system/applications/notkde.desktop",
            &app("Not KDE", "k", "NotShowIn=KDE;"),
        );
        tree.write("system/applications/bad.desktop", &app("Bad", "bad %z", ""));
        tree.write(
            "system/applications/dbus.desktop",
            "[Desktop Entry]\nType=Application\nName=D\nDBusActivatable=true\n",
        );
        tree.write("system/applications/readme.txt", "not a desktop file");

        let search = Search {
            dirs: vec![
                home.clone(),
                system.clone(),
                tree.0.join("nowhere/applications"),
            ],
            locale: Locale::parse("ja_JP.UTF-8"),
            desktops: desktop_names("KDE"),
            path: vec![bin.clone()],
        };
        let scan = Scan::read(&search);
        let ids: Vec<&str> = scan.entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            ids,
            ["editor.desktop", "present.desktop", "vendor-tool.desktop"],
            "{scan:#?}"
        );
        assert_eq!(scan.entries[0].name, "My Editor");
        assert_eq!(scan.entries[0].exec, ["edit", "--mine"]);
        assert_eq!(scan.entries[0].file, home.join("editor.desktop"));
        let tool = &scan.entries[2];
        assert_eq!(tool.name, "道具");
        assert_eq!(tool.default_name, "Tool");
        assert_eq!(tool.exec, ["tool"]);
        assert!(tool.terminal);
        assert_eq!(tool.working_dir.as_deref(), Some("/srv"));
        assert_eq!(tool.icon.as_deref(), Some("tool"));
        assert_eq!(tool.categories, ["Utility", "Development"]);
        assert_eq!(tool.keywords, ["あ", "い"]);
        assert_eq!(tool.generic_name.as_deref(), Some("Thing"));
        assert_eq!(tool.comment.as_deref(), Some("Does things"));
        // Malformed ones are reported.
        let problems: Vec<_> = scan
            .problems
            .iter()
            .map(|(p, _)| p.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(problems, ["bad.desktop", "dbus.desktop"]);
        assert!(!scan.outdated());

        // Shown with another desktop.
        let gnome = Scan::read(&Search {
            desktops: desktop_names("GNOME"),
            ..search.clone()
        });
        let ids: Vec<&str> = gnome.entries.iter().map(|e| e.id.as_str()).collect();
        assert!(
            ids.contains(&"gnome.desktop") && ids.contains(&"notkde.desktop"),
            "{ids:?}"
        );

        // A new file is noticed.
        std::thread::sleep(std::time::Duration::from_millis(20));
        tree.write("system/applications/new.desktop", &app("New", "new", ""));
        assert!(scan.outdated());
    }

    #[test]
    fn search_from_the_environment() {
        let env = |name: &str| match name {
            "HOME" => Some("/srv/u".to_owned()),
            "XDG_DATA_DIRS" => Some("/a:relative:/b:/a".to_owned()),
            "XDG_CURRENT_DESKTOP" => Some("KDE:GNOME".to_owned()),
            "PATH" => Some("/bin:/usr/bin".to_owned()),
            _ => None,
        };
        let search = Search::from_env(env);
        assert_eq!(
            search.dirs,
            [
                PathBuf::from("/srv/u/.local/share/applications"),
                PathBuf::from("/a/applications"),
                PathBuf::from("/b/applications"),
            ]
        );
        assert_eq!(search.desktops, ["KDE", "GNOME"]);
        let defaults =
            Search::from_env(|name| (name == "XDG_DATA_HOME").then(|| "/data".to_owned()));
        assert_eq!(
            defaults.dirs,
            [
                PathBuf::from("/data/applications"),
                PathBuf::from("/usr/local/share/applications"),
                PathBuf::from("/usr/share/applications"),
            ]
        );
    }
}
