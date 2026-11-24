//! Starting the applications of the machine from a script: the desktop
//! entries (`hz.desktop_entries`), what a key types (the fourth argument of
//! `key`), and the launchers of the projects built on them.
//!
//! The applications are desktop entries the tests write into directories of
//! their own (`XDG_DATA_HOME`, `XDG_DATA_DIRS` of the compositor); their
//! commands start the test client. Nothing of the machine's applications is
//! read or started.

mod common;

use std::path::{Path, PathBuf};

use common::{DRAW_ONLY_SESSION, Device, Session, script_project};
use proto_compositor::control::Request;
use proto_test_client::ClientEvent;
use serde_json::json;

const KEY_A: u32 = 30;
const KEY_B: u32 = 48;
const KEY_N: u32 = 49;
const KEY_ENTER: u32 = 28;
const KEY_LEFTCTRL: u32 = 29;
const KEY_LEFTSHIFT: u32 = 42;

/// A keymap that does not depend on the machine.
const KEYMAP: &[(&str, &str)] = &[
    ("XKB_DEFAULT_RULES", "evdev"),
    ("XKB_DEFAULT_MODEL", "pc105"),
    ("XKB_DEFAULT_LAYOUT", "us"),
    ("XKB_DEFAULT_VARIANT", ""),
    ("XKB_DEFAULT_OPTIONS", ""),
];

/// Directories of desktop entries for one test, removed at the end.
pub struct Applications {
    root: PathBuf,
}

impl Applications {
    pub fn new(name: &str, device: Device) -> Applications {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("applications")
            .join(format!("{name}-{}", device.name()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home/applications")).unwrap();
        std::fs::create_dir_all(root.join("system/applications")).unwrap();
        Applications { root }
    }

    /// The compositor's environment that makes it read these directories
    /// (and only these), with a known locale and desktop.
    pub fn env(&self) -> Vec<(&'static str, String)> {
        let mut env = vec![
            (
                "XDG_DATA_HOME",
                self.root.join("home").display().to_string(),
            ),
            (
                "XDG_DATA_DIRS",
                self.root.join("system").display().to_string(),
            ),
            ("XDG_CURRENT_DESKTOP", "hizukuri-test".to_owned()),
            ("LC_ALL", "".to_owned()),
            ("LC_MESSAGES", "".to_owned()),
            ("LANG", "ja_JP.UTF-8".to_owned()),
        ];
        env.extend(KEYMAP.iter().map(|(k, v)| (*k, (*v).to_owned())));
        env
    }

    /// Writes `<dir>/applications/<path>`; `dir` is `home` or `system`.
    pub fn write(&self, dir: &str, path: &str, text: &str) {
        let path = self.root.join(dir).join("applications").join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Under another name first: a scan never reads half a file.
        let temporary = path.with_extension("tmp");
        std::fs::write(&temporary, text).unwrap();
        std::fs::rename(&temporary, &path).unwrap();
    }
}

impl Drop for Applications {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Starts a session with the compositor environment `env`.
fn start(name: &str, device: Device, args: &[&str], env: &[(&'static str, String)]) -> Session {
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    Session::start_with_env(name, device, args, &env)
}

const ENTRIES_PROJECT: &str = r#"
hz.include("session")
function entries(options)
    return hz.desktop_entries(options)
end
"#;

/// The desktop entries of the data directories, as a script reads them:
/// the user's copy before the system's, localized names, commands split
/// into arguments, and what is not to be shown left out.
fn desktop_entries_are_read_from_the_data_dirs(device: Device) {
    let apps = Applications::new("entries", device);
    apps.write(
        "system",
        "org.example.Editor.desktop",
        "[Desktop Entry]\nType=Application\nName=Editor\nName[ja]=エディター\nGenericName=Text Editor\n\
         Comment=Edits text\nExec=\"/opt/the editor/edit\" --new-window %U\nIcon=accessories-text-editor\n\
         Categories=Utility;TextEditor;\nKeywords=text;write;\nPath=/tmp\n",
    );
    apps.write(
        "system",
        "tools/shell.desktop",
        "[Desktop Entry]\nType=Application\nName=Shell\nExec=sh %i\nTerminal=true\nIcon=utilities-terminal\n",
    );
    apps.write(
        "system",
        "hidden.desktop",
        "[Desktop Entry]\nType=Application\nName=Hidden\nExec=hidden\n",
    );
    // The user hides one of the system's.
    apps.write(
        "home",
        "hidden.desktop",
        "[Desktop Entry]\nType=Application\nName=Hidden\nExec=hidden\nHidden=true\n",
    );
    apps.write(
        "system",
        "elsewhere.desktop",
        "[Desktop Entry]\nType=Application\nName=Elsewhere\nExec=x\nOnlyShowIn=GNOME;\n",
    );
    apps.write(
        "system",
        "settings.desktop",
        "[Desktop Entry]\nType=Application\nName=Settings Helper\nExec=x\nNoDisplay=true\n",
    );
    let project = script_project(
        "entries",
        device,
        &[("main", ENTRIES_PROJECT), ("session", DRAW_ONLY_SESSION)],
    );
    let mut session = start(
        "launcher-entries",
        device,
        &["--script", project.to_str().unwrap()],
        &apps.env(),
    );
    let list = session.script("entries", json!([]));
    let list = list.as_array().expect("a list of entries");
    let ids: Vec<&str> = list.iter().map(|e| e["id"].as_str().unwrap()).collect();
    assert_eq!(
        ids,
        ["tools-shell.desktop", "org.example.Editor.desktop"],
        "{list:#?}"
    );
    let shell = &list[0];
    assert_eq!(shell["name"], "Shell");
    assert_eq!(shell["exec"], json!(["sh", "--icon", "utilities-terminal"]));
    assert_eq!(shell["terminal"], true);
    let editor = &list[1];
    assert_eq!(editor["name"], "エディター");
    assert_eq!(editor["generic_name"], "Text Editor");
    assert_eq!(editor["comment"], "Edits text");
    assert_eq!(
        editor["exec"],
        json!(["/opt/the editor/edit", "--new-window"])
    );
    assert_eq!(editor["icon"], "accessories-text-editor");
    assert_eq!(editor["terminal"], false);
    assert_eq!(editor["working_dir"], "/tmp");
    assert_eq!(editor["categories"], json!(["Utility", "TextEditor"]));
    assert_eq!(editor["keywords"], json!(["text", "write"]));
    assert!(
        editor["file"]
            .as_str()
            .unwrap()
            .ends_with("org.example.Editor.desktop")
    );

    // Another locale and another desktop, asked for by the script.
    let list = session.script("entries", json!([{ "locale": "C", "desktops": "GNOME" }]));
    let names: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Editor", "Elsewhere", "Shell"]);

    // An application installed meanwhile is there on the next call.
    apps.write(
        "home",
        "new.desktop",
        "[Desktop Entry]\nType=Application\nName=Another\nExec=another\n",
    );
    let list = session.script("entries", json!([]));
    assert_eq!(list.as_array().unwrap().len(), 3, "{list:#?}");
    let list = session.script("entries", json!([{ "refresh": true }]));
    assert_eq!(list[0]["name"], "Another");
    session.assert_script_healthy();
    session.quit();
}

const KEYS_PROJECT: &str = r#"
hz.include("session")
local heard = {}
local keep = false
function key(keycode, pressed, mods, k)
    table.insert(heard, { keycode = keycode, pressed = pressed, sym = k.sym or false, text = k.text or false })
    return keep
end
function heard_keys()
    local h = heard
    heard = {}
    return h
end
function keep_keys(on)
    keep = on
end
"#;

fn press(session: &mut Session, keycode: u32, pressed: bool) {
    session.send(Request::Key { keycode, pressed });
}

/// What `key` hears of a key: its keysym, and what it types on a press,
/// with the modifiers that are held, also those the script kept from the
/// clients.
fn keys_say_what_they_type(device: Device) {
    let project = script_project(
        "keys",
        device,
        &[("main", KEYS_PROJECT), ("session", DRAW_ONLY_SESSION)],
    );
    let env: Vec<(&'static str, String)> =
        KEYMAP.iter().map(|(k, v)| (*k, (*v).to_owned())).collect();
    let mut session = start(
        "launcher-keys",
        device,
        &["--script", project.to_str().unwrap()],
        &env,
    );
    let (mut client, _) =
        session.mapped_client(common::options("keys", 200, 100, [90, 90, 90, 255]));
    let heard = |session: &mut Session| -> Vec<(u64, bool, String, Option<String>)> {
        session
            .script("heard_keys", json!([]))
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|k| {
                (
                    k["keycode"].as_u64().unwrap(),
                    k["pressed"].as_bool().unwrap(),
                    k["sym"].as_str().unwrap_or("").to_owned(),
                    k["text"].as_str().map(str::to_owned),
                )
            })
            .collect()
    };
    let s = |text: &str| Some(text.to_owned());

    for (keycode, pressed) in [
        (KEY_A, true),
        (KEY_A, false),
        (KEY_LEFTSHIFT, true),
        (KEY_A, true),
        (KEY_A, false),
        (KEY_LEFTSHIFT, false),
        (KEY_ENTER, true),
        (KEY_ENTER, false),
        (KEY_LEFTCTRL, true),
        (KEY_N, true),
        (KEY_N, false),
        (KEY_LEFTCTRL, false),
    ] {
        press(&mut session, keycode, pressed);
    }
    // (A release reaches the script only when it kept the press.)
    assert_eq!(
        heard(&mut session),
        [
            (30, true, "a".into(), s("a")),
            (42, true, "Shift_L".into(), None),
            (30, true, "A".into(), s("A")),
            (28, true, "Return".into(), None),
            (29, true, "Control_L".into(), None),
            // Ctrl+N types no text: a script reads it by its keysym.
            (49, true, "n".into(), None),
        ]
    );

    // The script keeps every key, Shift too: the keymap still sees it.
    session.script("keep_keys", json!([true]));
    press(&mut session, KEY_LEFTSHIFT, true);
    press(&mut session, KEY_A, true);
    press(&mut session, KEY_A, false);
    assert_eq!(
        heard(&mut session),
        [
            (42, true, "Shift_L".into(), None),
            (30, true, "A".into(), s("A")),
            (30, false, "A".into(), None),
        ]
    );

    // Given back to the client while Shift is still held: the client hears
    // of Shift before the next key it gets, and types a capital. (Another
    // key than before: what the client got of the keys above may still be
    // on its way.)
    session.script("keep_keys", json!([false]));
    client.clear_events();
    press(&mut session, KEY_B, true);
    press(&mut session, KEY_B, false);
    let event = client
        .wait_for(common::TIMEOUT, |e| {
            matches!(
                e,
                ClientEvent::Key {
                    keycode: 48,
                    pressed: true,
                    ..
                }
            )
        })
        .expect("pumping the client")
        .expect("the key reaches the client");
    match event {
        ClientEvent::Key { text, .. } => assert_eq!(text, "B"),
        other => panic!("{other:?}"),
    }
    // Shift was kept from the client on its way down, so its release is
    // kept too.
    session.script("keep_keys", json!([true]));
    press(&mut session, KEY_LEFTSHIFT, false);
    session.script("keep_keys", json!([false]));
    client.clear_events();
    press(&mut session, KEY_B, true);
    let event = client
        .wait_for(common::TIMEOUT, |e| {
            matches!(
                e,
                ClientEvent::Key {
                    keycode: 48,
                    pressed: true,
                    ..
                }
            )
        })
        .expect("pumping the client")
        .expect("the key reaches the client");
    match event {
        ClientEvent::Key { text, .. } => assert_eq!(text, "b"),
        other => panic!("{other:?}"),
    }
    press(&mut session, KEY_B, false);
    session.assert_script_healthy();
    session.quit();
}

// ----- the launcher of `fancy` ------------------------------------------------

const KEY_ESC: u32 = 1;
const KEY_BACKSPACE: u32 = 14;
const KEY_D: u32 = 32;
const KEY_UP: u32 = 103;
const KEY_DOWN: u32 = 108;
const KEY_LEFTMETA: u32 = 125;
const BTN_LEFT: u32 = 272;

/// The evdev code of a lower-case letter or a space (US layout).
fn keycode_of(c: char) -> u32 {
    const LETTERS: &[u32] = &[
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17,
        45, 21, 44,
    ];
    match c {
        'a'..='z' => LETTERS[c as usize - 'a' as usize],
        ' ' => 57,
        _ => panic!("no key for {c:?}"),
    }
}

fn type_text(session: &mut Session, text: &str) {
    for c in text.chars() {
        tap(session, keycode_of(c));
    }
}

fn tap(session: &mut Session, keycode: u32) {
    press(session, keycode, true);
    press(session, keycode, false);
}

/// logo+D: the launcher of the project.
fn logo_d(session: &mut Session) {
    press(session, KEY_LEFTMETA, true);
    tap(session, KEY_D);
    press(session, KEY_LEFTMETA, false);
}

fn advance(session: &mut Session, seconds: f64) {
    session.send(Request::AdvanceClock {
        milliseconds: seconds * 1000.0,
    });
}

fn click(session: &mut Session, x: f64, y: f64) {
    session.send(Request::PointerMotion { x, y });
    session.send(Request::PointerButton {
        button: BTN_LEFT,
        pressed: true,
    });
    session.send(Request::PointerButton {
        button: BTN_LEFT,
        pressed: false,
    });
}

/// The path of the test client binary, which the desktop entries start.
fn test_client() -> PathBuf {
    let client =
        Path::new(env!("CARGO_BIN_EXE_proto-compositor")).with_file_name("proto-test-client");
    assert!(
        client.exists(),
        "{} is missing: the workspace's binaries are built by `cargo test --workspace` \
         (or `cargo build -p proto-test-client`)",
        client.display()
    );
    client
}

/// Stops the programs the script started, by their process ids (the
/// state's `script.spawned`).
fn stop_spawned(session: &mut Session) {
    let state = session.script_state();
    for spawned in state["spawned"].as_array().cloned().unwrap_or_default() {
        if let Some(pid) = spawned["pid"].as_u64() {
            let _ = std::process::Command::new("kill")
                .arg(pid.to_string())
                .status();
        }
    }
}

fn fancy_project() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("projects/fancy")
}

/// A session of `fancy` on the manual clock, reading the applications of
/// `apps`.
fn start_fancy(name: &str, device: Device, apps: &Applications, size: &str) -> Session {
    start_fancy_with(name, device, apps, size, &[])
}

fn start_fancy_with(
    name: &str,
    device: Device,
    apps: &Applications,
    size: &str,
    more: &[&str],
) -> Session {
    let project = fancy_project();
    static SHADERS: std::sync::Once = std::sync::Once::new();
    SHADERS.call_once(|| common::build_shaders(&project));
    // logo+Return would start the terminal stand-in, never a real one.
    common::use_terminal_stand_in();
    let mut arguments = vec![
        "--output-size",
        size,
        "--clock",
        "manual",
        "--script",
        project.to_str().unwrap(),
    ];
    arguments.extend(more);
    start(name, device, &arguments, &apps.env())
}

/// A desktop entry whose command opens a window of the test client with
/// `title`, in `session`.
fn client_entry(session: &Session, name: &str, generic: &str, title: &str, color: &str) -> String {
    let socket = session.runtime_dir.join("hizukuri-test");
    format!(
        "[Desktop Entry]\nType=Application\nName={name}\nGenericName={generic}\n\
         Exec=\"{}\" --socket \"{}\" --title {title} --size 480x320 --color {color} --duration 60\n",
        test_client().display(),
        socket.display()
    )
}

fn launcher_state(session: &mut Session) -> serde_json::Value {
    session.script("fancy_launcher", json!([]))
}

fn row_ids(state: &serde_json::Value) -> Vec<String> {
    state["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

/// The launcher of `fancy`: logo+D opens it, typing narrows the list and
/// reaches no window, the arrows choose, Return starts the application
/// (its window opens like any other, on the workspace in view, with the
/// focus) and closes the launcher, Escape and logo+D close it.
fn the_fancy_launcher_starts_an_application(device: Device) {
    let apps = Applications::new("fancy", device);
    let mut session = start_fancy("launcher-fancy", device, &apps, "1280x800");
    let entries = [
        (
            "alpha.desktop",
            "Alpha Forge",
            "Furnace",
            "alpha-app",
            "c4402eff",
        ),
        (
            "beta.desktop",
            "Beta Anvil",
            "Hammer",
            "beta-app",
            "40a060ff",
        ),
        (
            "gamma.desktop",
            "Gamma Bellows",
            "Air",
            "gamma-app",
            "406ec0ff",
        ),
    ];
    for (file, name, generic, title, color) in entries {
        apps.write(
            "system",
            file,
            &client_entry(&session, name, generic, title, color),
        );
    }
    let (mut victim, _) =
        session.mapped_client(common::options("victim", 300, 200, [70, 78, 96, 255]));

    logo_d(&mut session);
    let state = launcher_state(&mut session);
    assert_eq!(state["open"], true, "{state:#}");
    assert_eq!(state["total"], 3, "{state:#}");
    assert_eq!(state["query"], "");
    advance(&mut session, 0.5);

    // Typing narrows the list; no key reaches the window.
    session.wait(&mut [&mut victim], "the clients are served", |_| true);
    victim.clear_events();
    type_text(&mut session, "anv");
    let state = launcher_state(&mut session);
    assert_eq!(state["query"], "anv", "{state:#}");
    assert_eq!(row_ids(&state), ["beta.desktop"], "{state:#}");
    for _ in 0..3 {
        tap(&mut session, KEY_BACKSPACE);
    }
    let state = launcher_state(&mut session);
    assert_eq!(state["query"], "");
    assert_eq!(state["count"], 3);
    // The start of a name counts more than the start of a later word: "b"
    // is Beta Anvil first, then Gamma Bellows.
    type_text(&mut session, "b");
    let state = launcher_state(&mut session);
    assert_eq!(
        row_ids(&state),
        ["beta.desktop", "gamma.desktop"],
        "{state:#}"
    );
    tap(&mut session, KEY_DOWN);
    assert_eq!(launcher_state(&mut session)["selected"], 2);
    tap(&mut session, KEY_UP);
    assert_eq!(launcher_state(&mut session)["selected"], 1);
    // Up from the first goes round to the last.
    tap(&mut session, KEY_UP);
    assert_eq!(launcher_state(&mut session)["selected"], 2);
    session.wait(&mut [&mut victim], "the clients are served", |_| true);
    let leaked: Vec<_> = victim
        .events()
        .iter()
        .filter(|e| matches!(e, ClientEvent::Key { .. }))
        .cloned()
        .collect();
    assert!(leaked.is_empty(), "keys reached the window: {leaked:?}");

    // Return starts Gamma (chosen) and closes the launcher.
    tap(&mut session, KEY_ENTER);
    let state = launcher_state(&mut session);
    assert_eq!(state["open"], false, "{state:#}");
    let state = session.wait(&mut [&mut victim], "the application's window", |s| {
        common::window(s, "gamma-app").is_some_and(|w| {
            w["surfaces"][0]["buffer_size"].is_array() && s["focus"]["window"] == w["id"]
        })
    });
    let started = common::window(&state, "gamma-app").unwrap()["id"]
        .as_u64()
        .unwrap();
    // Placed like any window: on the workspace in view.
    let spaces = session.script("fancy_spaces", json!([]));
    let active = spaces["active"].as_u64().unwrap() as usize;
    let order = spaces["spaces"][active - 1]["order"].as_array().unwrap();
    assert!(
        order.iter().any(|w| w.as_u64() == Some(started)),
        "{spaces:#}"
    );

    // What was started comes first next time; Escape closes.
    logo_d(&mut session);
    let state = launcher_state(&mut session);
    assert_eq!(state["rows"][0]["id"], "gamma.desktop", "{state:#}");
    tap(&mut session, KEY_ESC);
    assert_eq!(launcher_state(&mut session)["open"], false);
    // logo+D closes it too.
    logo_d(&mut session);
    assert_eq!(launcher_state(&mut session)["open"], true);
    logo_d(&mut session);
    assert_eq!(launcher_state(&mut session)["open"], false);
    advance(&mut session, 1.0);
    session.assert_script_healthy();
    stop_spawned(&mut session);
    session.quit();
}

/// A click on a row of the launcher starts that application; a click
/// beside the plate closes it, and starts nothing.
fn a_click_on_the_fancy_launcher_starts_a_row(device: Device) {
    let apps = Applications::new("fancy-click", device);
    let mut session = start_fancy("launcher-fancy-click", device, &apps, "1280x800");
    apps.write(
        "system",
        "alpha.desktop",
        &client_entry(&session, "Alpha Forge", "Furnace", "alpha-app", "c4402eff"),
    );
    apps.write(
        "system",
        "beta.desktop",
        &client_entry(&session, "Beta Anvil", "Hammer", "beta-app", "40a060ff"),
    );
    logo_d(&mut session);
    advance(&mut session, 1.0);
    session.screenshot_png("open");
    let state = launcher_state(&mut session);
    assert_eq!(state["rows"][1]["id"], "beta.desktop", "{state:#}");
    let row = &state["drawn"]["rows"][1];
    let x = row["x"].as_f64().unwrap() + 40.0;
    let y = row["y"].as_f64().unwrap() + row["h"].as_f64().unwrap() / 2.0;
    click(&mut session, x, y);
    assert_eq!(launcher_state(&mut session)["open"], false);
    session.wait(&mut [], "the application's window", |s| {
        common::window(s, "beta-app").is_some()
    });

    logo_d(&mut session);
    advance(&mut session, 1.0);
    click(&mut session, 20.0, 780.0);
    assert_eq!(launcher_state(&mut session)["open"], false);
    let spawned = session.script_state()["spawned"].clone();
    assert_eq!(spawned.as_array().unwrap().len(), 1, "{spawned:#}");
    session.assert_script_healthy();
    stop_spawned(&mut session);
    session.quit();
}

/// Waits until the launcher of `fancy` says `done` (it changes in frames,
/// which come on their own).
fn wait_launcher(session: &mut Session, what: &str, done: impl Fn(&serde_json::Value) -> bool) {
    let deadline = std::time::Instant::now() + common::TIMEOUT;
    loop {
        let state = launcher_state(session);
        if done(&state) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for: {what}\n{state:#}"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// A key held down in the launcher of `fancy` repeats after 0.4 s, 25
/// times a second, until it is let go.
fn a_held_key_repeats_in_the_fancy_launcher(device: Device) {
    let apps = Applications::new("fancy-repeat", device);
    let mut session = start_fancy("launcher-fancy-repeat", device, &apps, "1280x800");
    apps.write(
        "system",
        "gamma.desktop",
        &client_entry(&session, "Gamma Bellows", "Air", "gamma-app", "406ec0ff"),
    );
    logo_d(&mut session);
    type_text(&mut session, "gamma");
    press(&mut session, KEY_BACKSPACE, true);
    assert_eq!(launcher_state(&mut session)["query"], "gamm");
    // Not yet: the delay.
    advance(&mut session, 0.3);
    session.screenshot_png("held");
    assert_eq!(launcher_state(&mut session)["query"], "gamm");
    // At 0.4, 0.44 and 0.48 s.
    advance(&mut session, 0.2);
    wait_launcher(&mut session, "three repeats", |s| s["query"] == "g");
    press(&mut session, KEY_BACKSPACE, false);
    advance(&mut session, 0.5);
    session.screenshot_png("let-go");
    assert_eq!(launcher_state(&mut session)["query"], "g");
    session.assert_script_healthy();
    session.quit();
}

device_tests!(
    desktop_entries_are_read_from_the_data_dirs,
    keys_say_what_they_type,
    the_fancy_launcher_starts_an_application,
    a_click_on_the_fancy_launcher_starts_a_row,
    a_held_key_repeats_in_the_fancy_launcher
);

/// Pictures of the launcher of `fancy` at given times, over a few windows
/// (in the session's output directory). Not a check: for looking at.
#[test]
#[ignore]
fn gallery_fancy_launcher() {
    launcher_gallery("launcher-fancy-gallery", "1280x800", &[]);
}

/// The same on an output scaled by 1.5.
#[test]
#[ignore]
fn gallery_fancy_launcher_scaled() {
    launcher_gallery(
        "launcher-fancy-gallery-scaled",
        "1920x1200",
        &["--output-scale", "1.5"],
    );
}

fn launcher_gallery(name: &str, size: &str, more: &[&str]) {
    let device = Device::new(common::Gpu::Lavapipe);
    let apps = Applications::new(name, device);
    let mut session = start_fancy_with(name, device, &apps, size, more);
    let names = [
        ("Text Editor", "Edits text"),
        ("Terminal", "Command line"),
        ("Files", "File manager"),
        ("Image Viewer", "Shows pictures"),
        ("Calculator", "Arithmetic"),
        ("Settings", "System settings"),
        ("Music Player", "Plays music"),
        ("Video Player", "Plays videos"),
        ("Disk Usage", "Disk space"),
        ("Clocks", "World clocks and timers"),
        ("Calendar", "Dates"),
        ("Bellows", "Air for the forge"),
    ];
    for (i, (name, generic)) in names.iter().enumerate() {
        apps.write(
            "system",
            &format!("app{i}.desktop"),
            &client_entry(&session, name, generic, &format!("app-{i}"), "806040ff"),
        );
    }
    let colors = [[70, 78, 96, 255], [214, 190, 140, 255], [64, 110, 190, 255]];
    let mut clients = Vec::new();
    for (i, color) in colors.iter().enumerate() {
        let (client, _) =
            session.mapped_client(common::options(&format!("w{i}"), 300, 200, *color));
        clients.push(client);
    }
    advance(&mut session, 3.0);
    session.wait(
        &mut clients.iter_mut().collect::<Vec<_>>(),
        "served",
        |_| true,
    );

    logo_d(&mut session);
    for (index, step) in [0.03, 0.04, 0.05, 0.08].into_iter().enumerate() {
        advance(&mut session, step);
        session.screenshot_png(&format!("launcher-01-rising-{index}"));
    }
    advance(&mut session, 1.0);
    session.screenshot_png("launcher-02-open");
    type_text(&mut session, "pl");
    advance(&mut session, 0.05);
    session.screenshot_png("launcher-03-typed-flare");
    advance(&mut session, 1.0);
    session.screenshot_png("launcher-04-typed");
    tap(&mut session, KEY_DOWN);
    advance(&mut session, 0.04);
    session.screenshot_png("launcher-05-moving");
    advance(&mut session, 1.0);
    tap(&mut session, KEY_ESC);
    for (index, step) in [0.05, 0.08, 0.12].into_iter().enumerate() {
        advance(&mut session, step);
        session.screenshot_png(&format!("launcher-06-cooling-{index}"));
    }
    session.assert_script_healthy();
    println!("pictures in {}", session.output_dir.display());
    session.quit();
}
