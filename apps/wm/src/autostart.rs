//! The autostart list: `~/.makepad/wm/autostart`, one app id per line,
//! `#` comments (the format of the launcher's hide list, shell/launcher.rs
//! `hides`). The WM reads it once at startup and opens each listed app as
//! if it were picked from the menu (`App::run_autostart` in lib.rs). The
//! file does not exist by default, so nothing starts until it is written;
//! `set_enabled`/`toggle` edit it, keeping every other line as it was.
//!
//! Everything here is pure except `read_text`, `save` and `toggle`.

use std::io::Write;
use std::path::{Path, PathBuf};

/// What a freshly created file starts with.
pub const HEADER: &str = "# Apps the window manager opens at login, one app id per line.\n# Lines starting with # are comments. Setup > Start at login edits this file.\n";

/// One listed id, looked up in the app registry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolved {
    /// The registry's own id (a binary name resolves to its app's id).
    App(String),
    Unknown,
    /// Known, but this build cannot start it (the menu would not list it).
    Unavailable,
}

/// One thing to do at startup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Launch(String),
    Skip { id: String, reason: &'static str },
}

/// `~/.makepad/wm/autostart` (`MAKEPAD_HOME` moves it).
pub fn path() -> PathBuf {
    crate::theme::makepad_home().join("wm/autostart")
}

/// The ids, in file order: each line trimmed; blank lines and lines
/// starting with `#` skipped. A `#` later in a line is part of the id.
/// Duplicates are kept (`plan` drops them).
pub fn parse(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect()
}

pub fn is_enabled(text: &str, id: &str) -> bool {
    parse(text).iter().any(|listed| listed == id)
}

/// Ids come from the registry; this only guards against an id that could
/// not be written as one line of the file.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && !id.starts_with('#') && !id.chars().any(char::is_whitespace)
}

/// The file's new text with `id` on or off. Every other line is kept byte
/// for byte: comments, blank lines, other and unknown ids, line endings.
pub fn set_enabled(text: &str, id: &str, on: bool) -> String {
    if !valid_id(id) {
        return text.to_string();
    }
    if on {
        if is_enabled(text, id) {
            return text.to_string();
        }
        if text.trim().is_empty() {
            return format!("{HEADER}{id}\n");
        }
        let mut out = text.to_string();
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(id);
        out.push('\n');
        out
    } else {
        // A comment line never trims to a valid id, so `# id` stays.
        text.split_inclusive('\n').filter(|line| line.trim() != id).collect()
    }
}

/// The file's text; a missing file is `Ok("")`. Any other error
/// (permissions, not UTF-8) is `Err`.
pub fn read_text(path: &Path) -> Result<String, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(err) => Err(format!("{}: {err}", path.display())),
    }
}

/// Atomic write: create the parent dir, write `.autostart.<pid>.tmp` in it
/// (so the rename stays on one filesystem), sync, rename over the target.
/// A failure at any step removes the temporary and leaves the target as it
/// was (the pattern of shell/system_linux.rs `write_display_setting`).
pub fn save(path: &Path, text: &str) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(dir).map_err(|err| format!("create {}: {err}", dir.display()))?;
    let temp = dir.join(format!(".autostart.{}.tmp", std::process::id()));
    let written = std::fs::File::create(&temp).and_then(|mut file| {
        file.write_all(text.as_bytes())?;
        file.sync_all()
    });
    if let Err(err) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(format!("write {}: {err}", temp.display()));
    }
    if let Err(err) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(format!("replace {}: {err}", path.display()));
    }
    Ok(())
}

/// `read_text` + `set_enabled` + `save`, reading the file fresh so hand
/// edits are kept. Nothing is written when nothing changes, and an
/// unreadable file is never replaced: `Err` leaves the file as it was.
pub fn toggle(path: &Path, id: &str, on: bool) -> Result<(), String> {
    let text = read_text(path)?;
    let next = set_enabled(&text, id, on);
    if next == text {
        return Ok(());
    }
    save(path, &next)
}

/// What to do at startup, in file order. Pure: `resolve` is the registry
/// lookup. An id seen before is dropped, compared by the registry's id
/// (`terminal` and `makepad-app-terminal` count once) or, when it does not
/// resolve, by its text: an `always_new` app would really open twice.
pub fn plan(ids: &[String], resolve: impl Fn(&str) -> Resolved) -> Vec<Step> {
    let mut seen: Vec<String> = Vec::new();
    let mut steps = Vec::new();
    for id in ids {
        let resolved = resolve(id);
        let key = match &resolved {
            Resolved::App(app) => app.clone(),
            _ => id.clone(),
        };
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        steps.push(match resolved {
            Resolved::App(app) => Step::Launch(app),
            Resolved::Unknown => Step::Skip { id: id.clone(), reason: "not in the app registry" },
            Resolved::Unavailable => {
                Step::Skip { id: id.clone(), reason: "not available in this build" }
            }
        });
    }
    steps
}

/// Why autostart is off for this run, or `None`. Test scenes and scripted
/// runs share the user's HOME and must not open the user's apps. `env`
/// reads a variable (`std::env::var(..).ok()` in the WM).
pub fn suppressed(args: &[String], env: impl Fn(&str) -> Option<String>) -> Option<&'static str> {
    if env("MAKEPAD_WM_NO_AUTOSTART").is_some_and(|value| !value.is_empty()) {
        return Some("MAKEPAD_WM_NO_AUTOSTART is set");
    }
    if env("MAKEPAD_WM_TEST_APP").is_some() {
        return Some("a test scene (MAKEPAD_WM_TEST_APP)");
    }
    if args.iter().any(|arg| arg == "--test-action") {
        return Some("a scripted run (--test-action)");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// A fresh directory per test (tests run in parallel), never ~/.makepad.
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("wm-autostart-test-{}", std::process::id()))
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Remove a test's directory, and the per-process parent once it is
    /// empty (`remove_dir` fails harmlessly while another test uses it).
    fn clean(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
        if let Some(parent) = dir.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }

    #[test]
    fn parse_skips_comments_and_blank_lines() {
        let text = "# head\n\nspacecraft\n  clock  \n#terminal\n";
        assert_eq!(parse(text), ids(&["spacecraft", "clock"]));
        assert_eq!(parse(&text.replace('\n', "\r\n")), ids(&["spacecraft", "clock"]));
    }

    #[test]
    fn parse_keeps_an_inline_hash_as_part_of_the_id() {
        assert_eq!(parse("clock # mine"), ids(&["clock # mine"]));
    }

    #[test]
    fn turning_on_into_an_empty_file_writes_the_header() {
        assert_eq!(set_enabled("", "spacecraft", true), HEADER.to_string() + "spacecraft\n");
        assert_eq!(set_enabled("  \n\n", "spacecraft", true), HEADER.to_string() + "spacecraft\n");
    }

    #[test]
    fn turning_on_appends_and_keeps_everything_else() {
        let text = "# mine\n\nnope\n# spacecraft\nclock";
        assert_eq!(
            set_enabled(text, "spacecraft", true),
            "# mine\n\nnope\n# spacecraft\nclock\nspacecraft\n"
        );
        let text = "# mine\nclock\n";
        assert_eq!(set_enabled(text, "spacecraft", true), "# mine\nclock\nspacecraft\n");
    }

    #[test]
    fn turning_on_twice_changes_nothing() {
        let once = set_enabled("# mine\nclock\n", "spacecraft", true);
        assert_eq!(set_enabled(&once, "spacecraft", true), once);
        // An indented listing counts as enabled too.
        let text = "  spacecraft  \n";
        assert_eq!(set_enabled(text, "spacecraft", true), text);
    }

    #[test]
    fn turning_off_removes_every_copy_and_only_that_id() {
        assert_eq!(
            set_enabled("# c\nclock\nspacecraft\n\nclock\n", "clock", false),
            "# c\nspacecraft\n\n"
        );
        assert_eq!(
            set_enabled("# clock\nclock\nnope\n", "clock", false),
            "# clock\nnope\n"
        );
        // The last line without a newline goes too.
        assert_eq!(set_enabled("spacecraft\nclock", "clock", false), "spacecraft\n");
    }

    #[test]
    fn turning_off_keeps_crlf_line_endings() {
        assert_eq!(
            set_enabled("# c\r\nclock\r\nspacecraft\r\n\r\n", "clock", false),
            "# c\r\nspacecraft\r\n\r\n"
        );
    }

    #[test]
    fn round_trip() {
        let original = "# mine\n\n# clock\nterminal\n";
        assert!(!is_enabled(original, "clock"));
        let on = set_enabled(original, "clock", true);
        assert!(is_enabled(&on, "clock"));
        assert!(is_enabled(&on, "terminal"));
        let off = set_enabled(&on, "clock", false);
        assert!(!is_enabled(&off, "clock"));
        assert_eq!(off, original);
    }

    #[test]
    fn a_bad_id_changes_nothing() {
        let text = "# mine\nclock\n";
        for bad in ["", "a b", "#x"] {
            assert_eq!(set_enabled(text, bad, true), text, "on {bad:?}");
            assert_eq!(set_enabled(text, bad, false), text, "off {bad:?}");
            assert_eq!(set_enabled("", bad, true), "", "empty on {bad:?}");
        }
    }

    #[test]
    fn plan_launches_in_file_order_and_dedupes() {
        let resolve = |id: &str| match id {
            "clock" => Resolved::App("clock".to_string()),
            "terminal" | "makepad-app-terminal" => Resolved::App("terminal".to_string()),
            "spacecraft" => Resolved::Unavailable,
            _ => Resolved::Unknown,
        };
        let list = ids(&["clock", "nope", "terminal", "makepad-app-terminal", "spacecraft", "clock"]);
        assert_eq!(
            plan(&list, resolve),
            vec![
                Step::Launch("clock".to_string()),
                Step::Skip { id: "nope".to_string(), reason: "not in the app registry" },
                Step::Launch("terminal".to_string()),
                Step::Skip { id: "spacecraft".to_string(), reason: "not available in this build" },
            ]
        );
    }

    #[test]
    fn plan_of_nothing_is_nothing() {
        assert_eq!(plan(&[], |_| Resolved::Unknown), Vec::<Step>::new());
    }

    #[test]
    fn suppressed_reasons() {
        let plain = ids(&["wm"]);
        let none = |_: &str| None;
        assert_eq!(suppressed(&plain, none), None);
        assert_eq!(
            suppressed(&plain, |k: &str| (k == "MAKEPAD_WM_NO_AUTOSTART").then(|| "1".to_string())),
            Some("MAKEPAD_WM_NO_AUTOSTART is set")
        );
        assert_eq!(
            suppressed(&plain, |k: &str| (k == "MAKEPAD_WM_NO_AUTOSTART").then(String::new)),
            None
        );
        assert_eq!(
            suppressed(&plain, |k: &str| (k == "MAKEPAD_WM_TEST_APP").then(|| "terminal:3".to_string())),
            Some("a test scene (MAKEPAD_WM_TEST_APP)")
        );
        assert_eq!(
            suppressed(&ids(&["wm", "--test-action", "ai"]), none),
            Some("a scripted run (--test-action)")
        );
    }

    #[test]
    fn path_is_under_makepad_home() {
        assert!(path().ends_with("wm/autostart"));
    }

    #[test]
    fn read_text_of_a_missing_file_is_empty() {
        let dir = temp_dir("missing");
        assert_eq!(read_text(&dir.join("wm/autostart")), Ok(String::new()));
    }

    #[test]
    fn toggle_writes_atomically_and_reads_back() {
        let dir = temp_dir("toggle");
        let file = dir.join("wm/autostart");
        toggle(&file, "clock", true).unwrap();
        assert_eq!(read_text(&file).unwrap(), HEADER.to_string() + "clock\n");
        toggle(&file, "clock", false).unwrap();
        assert_eq!(read_text(&file).unwrap(), HEADER);
        let leftovers: Vec<_> = std::fs::read_dir(dir.join("wm"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".autostart.") && name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        clean(&dir);
    }

    #[test]
    fn toggle_does_not_overwrite_an_unreadable_file() {
        let dir = temp_dir("unreadable");
        let file = dir.join("wm/autostart");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let bytes = [0xffu8, 0xfe, b'\n', 0xc3];
        std::fs::write(&file, bytes).unwrap();
        assert!(toggle(&file, "clock", true).is_err());
        assert_eq!(std::fs::read(&file).unwrap(), bytes);
        clean(&dir);
    }

    #[test]
    fn toggle_without_a_change_does_not_write() {
        let dir = temp_dir("nochange");
        let file = dir.join("wm/autostart");
        toggle(&file, "clock", false).unwrap();
        assert!(!file.exists());
        assert!(!dir.join("wm").exists());
        clean(&dir);
    }
}
