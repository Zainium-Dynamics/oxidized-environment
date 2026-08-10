//! oxidized-environment — a native, TOML-based replacement for `/etc/profile.d`.
//!
//! # Why this exists
//!
//! Traditional `/etc/profile.d/*.sh` only reaches interactive login shells
//! (bash/zsh source it — nothing else does), so every other process tree —
//! services, sessions started some other way — has to re-derive the same
//! variables by hand.
//!
//! This crate carries **zero** hardcoded paths, distro names, or default
//! values — no compiled-in root, no compiled-in fallback table. It doesn't
//! know what "syshub", "quantra", or "Zainium" mean, doesn't parse any other
//! tool's config, and depends on nothing but `toml`. Every caller supplies
//! its own root explicitly; a distro-specific consumer (like `quantra`,
//! ZainiumOS's PID 1) owns its own defaults in its own source, not here.
//! That's what makes this a library any Linux distro's init system could
//! link, not a Zainium stub with a generic name.
//!
//! It is both a library — a PID 1 links it directly and calls [`resolve`]
//! once at boot with its own root, no subprocess, no shell — and (via the
//! separate `oxidized-environment` CLI crate) a binary, so an installer, a
//! control CLI, or a plain shell script can call `oxidized-environment
//! resolve` / `init` / `verify` without linking Rust at all.
//!
//! # Schema
//!
//! ```text
//! <root>/etc/oxidized_environment/environment.toml       ← base table
//! <root>/etc/oxidized_environment/environment.d/*.toml   ← overrides, filename order, later wins
//! ```
//!
//! `<root>` is whatever path the caller passes to [`resolve`]/[`init`] —
//! there is no compiled-in default. The [`ROOT_OVERRIDE_ENV`] name is a
//! convention callers may choose to honor (the CLI does — see its own
//! docs), not something this crate reads itself.
//!
//! Each TOML file may mix two shapes:
//!
//! - **Bare keys, or a `[Profile]` table** — become env vars verbatim, case
//!   preserved, no prefix. Use this for real env var names (`PATH`,
//!   `LD_LIBRARY_PATH`, ...) — the two forms are identical, `[Profile]` is
//!   just a heading to organize the file under:
//!   ```toml
//!   [Profile]
//!   PATH = "/overlayer/syshub/bin"
//!   ```
//! - **Any other table, at any nesting depth** — flattened into one env var
//!   per string leaf, named by joining every segment down to the leaf key
//!   with `_` and upper-casing. Use this for organized reference data that
//!   would collide with real env var names if left bare:
//!   ```toml
//!   [Path.Syshub]
//!   bindir = "/overlayer/syshub/bin"   # → env var PATH_SYSHUB_BINDIR
//!   ```
//!
//! A package that needs to contribute environment variables (the one real
//! use case profile.d snippets served) drops a flat TOML file into
//! `environment.d/`:
//!
//! ```toml
//! JAVA_HOME = "/overlayer/zexlib/union/lib/jvm/current"
//! ```
//!
//! No shell is ever invoked to apply it. There is no `${VAR}`-style
//! interpolation anywhere in this schema — every value is a literal,
//! already-resolved string. A build script's `LDFLAGS`/`CFLAGS` (which
//! typically reference other shell variables) must be written with those
//! references expanded to concrete paths before landing in this file.
//!
//! # No hidden fallback
//!
//! If `environment.toml` is missing or fails to parse, [`resolve`] returns
//! whatever it could load — an empty map in the worst case — never a
//! compiled-in guess about what a "default" environment should contain. A
//! consumer that needs a safety net for "file missing at boot" (e.g. a PID 1
//! that must never start with an empty `PATH`) owns that fallback table
//! itself and merges it in only if `resolve` comes back missing keys it
//! considers required. See [`verify`] for checking a file is well-formed
//! *before* relying on it.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Conventional name for an environment variable that overrides the root a
/// caller resolves against, with no recompile required. This crate does not
/// read it itself — a consumer (or the `oxidized-environment` CLI) opts in
/// to the convention explicitly.
pub const ROOT_OVERRIDE_ENV: &str = "OXIDIZED_ENVIRONMENT_ROOT";

const ENV_FILE_REL: &str = "etc/oxidized_environment/environment.toml";
const ENV_D_REL: &str = "etc/oxidized_environment/environment.d";

fn env_file(root: &Path) -> PathBuf {
    root.join(ENV_FILE_REL)
}

fn env_d(root: &Path) -> PathBuf {
    root.join(ENV_D_REL)
}

/// Render `env` as flat `KEY = "value"` TOML text, header comment included.
/// What [`init`] writes.
pub fn render_toml(env: &[(&str, &str)]) -> String {
    let mut out = String::from(
        "# environment.toml — managed by oxidized-environment\n\
         # Every process on the system inherits this. Hand-edit freely.\n\
         # Package-specific additions belong in environment.d/*.toml instead\n\
         # of editing this file.\n\n",
    );
    for (k, v) in env {
        out.push_str(&format!("{k} = {v:?}\n"));
    }
    out
}

/// One parse failure found by [`verify`].
#[derive(Debug)]
pub struct VerifyError {
    /// The file that failed to parse.
    pub path: PathBuf,
    /// What went wrong.
    pub message: String,
}

/// Result of [`verify`]: every key that would be in [`resolve`]'s output,
/// plus which file contributed it last, and every file that failed to parse
/// along the way (parse failures don't stop verification — every other file
/// is still checked and reported, matching how `resolve` silently skips a
/// bad file rather than aborting).
#[derive(Debug, Default)]
pub struct VerifyReport {
    /// Key → (value, path of the file it came from).
    pub resolved: HashMap<String, (String, PathBuf)>,
    /// Every file that exists but failed to parse as TOML.
    pub errors: Vec<VerifyError>,
}

impl VerifyReport {
    /// True if every file that exists parsed cleanly. A missing
    /// `environment.toml`/`environment.d` is not an error by itself — an
    /// empty environment is a valid (if probably unintended) result.
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Check every file [`resolve`] would read under `root`, without silently
/// swallowing parse errors — the CLI's `verify` subcommand is this function.
/// Returns which key came from which file, and every file that failed to
/// parse (a bad `environment.d` snippet doesn't stop the rest from being
/// checked).
pub fn verify(root: &Path) -> VerifyReport {
    let mut report = VerifyReport::default();

    let base = env_file(root);
    if base.is_file() {
        match load_table(&base) {
            Some(table) => {
                for (k, v) in table {
                    report.resolved.insert(k, (v, base.clone()));
                }
            }
            None => report.errors.push(VerifyError {
                path: base.clone(),
                message: parse_error_message(&base),
            }),
        }
    }

    let dir = env_d(root);
    if let Ok(entries) = fs::read_dir(&dir) {
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "toml"))
            .collect();
        files.sort();

        for path in files {
            match load_table(&path) {
                Some(table) => {
                    for (k, v) in table {
                        report.resolved.insert(k, (v, path.clone()));
                    }
                }
                None => report.errors.push(VerifyError {
                    path: path.clone(),
                    message: parse_error_message(&path),
                }),
            }
        }
    }

    report
}

/// Re-parse a file that [`load_table`] already gave up on, just to produce a
/// human-readable reason — `load_table` itself only needs to know "did this
/// work", not why it didn't.
fn parse_error_message(path: &Path) -> String {
    match fs::read_to_string(path) {
        Err(e) => format!("failed to read: {e}"),
        Ok(content) => match content.parse::<toml::Table>() {
            Err(e) => format!("invalid TOML: {e}"),
            Ok(_) => "unreadable".to_string(), // read+parsed fine on retry — transient, shouldn't happen
        },
    }
}

/// Load a TOML table into a flat env-var map. Two shapes are accepted, and
/// may be mixed in the same file:
///
/// - **Bare top-level keys**: `PATH = "..."` becomes env var `PATH`
///   verbatim, case preserved, no prefix. This is also what a `[Profile]`
///   table's keys become — `[Profile] PATH = "..."` is exactly equivalent
///   to bare top-level `PATH = "..."`, just organized under a heading.
/// - **Any other table**, at any nesting depth (`[Path.Syshub]`,
///   `[shell_console]`, ...): flattened into one env var per string leaf,
///   named by joining every segment from the table path down to the leaf
///   key with `_` and upper-casing, e.g. `[Path.Syshub] bindir = "..."` →
///   `PATH_SYSHUB_BINDIR`. This is what keeps free-form organizational
///   sections from colliding with real env var names — only `[Profile]`
///   (and bare top-level keys) ever produce an unprefixed name.
///
/// Non-string leaves (ints, bools, arrays, dates) are skipped — environment
/// variables have no other type. Returns `None` if the file can't be read
/// or isn't valid TOML.
fn load_table(path: &Path) -> Option<HashMap<String, String>> {
    let content = fs::read_to_string(path).ok()?;
    let table: toml::Table = content.parse().ok()?;

    let mut out = HashMap::new();
    for (key, value) in table {
        // Not collapsed into one `if key == ... && let Some(profile) = ...`:
        // a non-table "profile" key must still fall through to the generic
        // match below (treated as a plain value), not be silently dropped.
        #[allow(clippy::collapsible_if)]
        if key.eq_ignore_ascii_case("profile") {
            if let toml::Value::Table(profile) = &value {
                for (k, v) in profile {
                    if let Some(s) = v.as_str() {
                        out.insert(k.clone(), s.to_string());
                    }
                }
                continue;
            }
        }

        match &value {
            toml::Value::String(s) => {
                out.insert(key, s.clone());
            }
            toml::Value::Table(_) => {
                flatten_into(&key.to_uppercase(), &value, &mut out);
            }
            _ => {} // non-string, non-table leaf at top level — not representable as an env var
        }
    }
    Some(out)
}

/// Recursively flatten a TOML table under `prefix`, writing `PREFIX_KEY =
/// value` for every string leaf found. See [`load_table`] for the naming
/// rule this implements.
fn flatten_into(prefix: &str, value: &toml::Value, out: &mut HashMap<String, String>) {
    match value {
        toml::Value::Table(table) => {
            for (k, v) in table {
                flatten_into(&format!("{prefix}_{}", k.to_uppercase()), v, out);
            }
        }
        toml::Value::String(s) => {
            out.insert(prefix.to_string(), s.clone());
        }
        _ => {} // non-string leaf — not representable as an env var
    }
}

/// Scan `environment.d/*.toml` in filename order (later files win on
/// conflicting keys). Missing directory is not an error — `environment.d`
/// is an optional extension point, not a requirement. A file that fails to
/// parse is skipped, not fatal — see [`verify`] to find out about those
/// instead of silently losing them.
fn load_environment_d(root: &Path) -> Vec<(String, String)> {
    let dir = env_d(root);
    let mut files: Vec<_> = match fs::read_dir(&dir) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "toml"))
            .collect(),
        Err(_) => return Vec::new(),
    };
    files.sort();

    let mut out = Vec::new();
    for path in files {
        if let Some(table) = load_table(&path) {
            out.extend(table);
        }
    }
    out
}

/// Resolve the final environment for `root`: `environment.toml` merged with
/// `environment.d/*.toml` overrides. Returns an empty map if nothing exists
/// or nothing parses — no hidden fallback content. See the module docs'
/// "No hidden fallback" section for why, and [`verify`] for surfacing parse
/// errors this function silently skips past.
pub fn resolve(root: &Path) -> HashMap<String, String> {
    let mut env = load_table(&env_file(root)).unwrap_or_default();

    for (key, value) in load_environment_d(root) {
        env.insert(key, value);
    }

    env
}

/// Result of [`init`], for callers that want to report what happened.
#[derive(Debug, PartialEq, Eq)]
pub enum InitAction {
    /// `environment.toml` did not exist; `env` was written.
    Created,
    /// `environment.toml` already existed and `force` was not set; left
    /// untouched (an admin's hand edits are never silently clobbered).
    AlreadyExists,
    /// `environment.toml` existed and was overwritten because `force` was set.
    Overwritten,
}

/// Write `env` as a fresh `environment.toml` under `root`, creating
/// `environment.d/` alongside it. `env` is supplied by the caller — this
/// crate has no opinion about what a "default" environment should contain,
/// only how to read and write one. Never overwrites an existing file unless
/// `force` is set.
pub fn init(root: &Path, env: &[(&str, &str)], force: bool) -> io::Result<InitAction> {
    write_env_file(root, force, || render_toml(env))
}

/// Like [`init`], but the content is copied verbatim from an existing TOML
/// file (validated first) instead of rendered from a flat table — this is
/// what preserves a hand-authored file's `[Profile]`/`[Path.*]` section
/// structure through a copy. Used by the `oxidized-environment init --from
/// <file>` CLI flag, and by anything else seeding a fresh root from a
/// template it owns rather than one this crate would have to hardcode.
pub fn init_from_file(root: &Path, source: &Path, force: bool) -> io::Result<InitAction> {
    let content = fs::read_to_string(source)?;
    content.parse::<toml::Table>().map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: {e}", source.display()),
        )
    })?;
    write_env_file(root, force, || content)
}

/// Shared directory-creation + force/no-clobber logic for [`init`] and
/// [`init_from_file`]. `render` is only called once the decision to write
/// has already been made (lazy, so [`init_from_file`]'s file read doesn't
/// happen needlessly on the `AlreadyExists` path — not that it's expensive,
/// but there's no reason to do it).
fn write_env_file(
    root: &Path,
    force: bool,
    render: impl FnOnce() -> String,
) -> io::Result<InitAction> {
    let file = env_file(root);
    let d = env_d(root);

    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::create_dir_all(&d)?;

    if file.exists() && !force {
        return Ok(InitAction::AlreadyExists);
    }

    let existed = file.exists();
    fs::write(&file, render())?;
    Ok(if existed {
        InitAction::Overwritten
    } else {
        InitAction::Created
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_ENV: &[(&str, &str)] = &[("PATH", "/example/bin"), ("EXAMPLE_VAR", "1")];

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("oxidized-environment-test-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn resolve_returns_empty_map_when_file_missing() {
        let root = tempdir("missing");
        let env = resolve(&root);
        assert!(env.is_empty());
    }

    #[test]
    fn init_then_resolve_roundtrips() {
        let root = tempdir("roundtrip");
        let action = init(&root, SAMPLE_ENV, false).unwrap();
        assert_eq!(action, InitAction::Created);
        let env = resolve(&root);
        assert_eq!(env.get("EXAMPLE_VAR").map(String::as_str), Some("1"));

        // second init without force must not clobber
        fs::write(env_file(&root), "PATH = \"/custom\"\n").unwrap();
        let action2 = init(&root, SAMPLE_ENV, false).unwrap();
        assert_eq!(action2, InitAction::AlreadyExists);
        let env2 = resolve(&root);
        assert_eq!(env2.get("PATH").map(String::as_str), Some("/custom"));
    }

    #[test]
    fn init_force_overwrites() {
        let root = tempdir("force");
        init(&root, SAMPLE_ENV, false).unwrap();
        fs::write(env_file(&root), "PATH = \"/custom\"\n").unwrap();
        let action = init(&root, SAMPLE_ENV, true).unwrap();
        assert_eq!(action, InitAction::Overwritten);
        let env = resolve(&root);
        assert_eq!(env.get("PATH").map(String::as_str), Some("/example/bin"));
    }

    #[test]
    fn init_from_file_preserves_section_structure() {
        let root = tempdir("init-from-file");
        let template_dir = tempdir("init-from-file-template");
        let template = template_dir.join("template.toml");
        fs::write(
            &template,
            "[Profile]\nPATH = \"/tmpl/bin\"\n\n[Path.Syshub]\nbindir = \"/tmpl/syshub/bin\"\n",
        )
        .unwrap();

        let action = init_from_file(&root, &template, false).unwrap();
        assert_eq!(action, InitAction::Created);

        let env = resolve(&root);
        assert_eq!(env.get("PATH").map(String::as_str), Some("/tmpl/bin"));
        assert_eq!(
            env.get("PATH_SYSHUB_BINDIR").map(String::as_str),
            Some("/tmpl/syshub/bin")
        );
    }

    #[test]
    fn init_from_file_rejects_invalid_toml() {
        let root = tempdir("init-from-file-invalid");
        let template_dir = tempdir("init-from-file-invalid-template");
        let template = template_dir.join("bad.toml");
        fs::write(&template, "PATH = \n").unwrap();

        let result = init_from_file(&root, &template, false);
        assert!(result.is_err());
        // Must not have written a broken file
        assert!(!env_file(&root).exists());
    }

    #[test]
    fn environment_d_overrides_base() {
        let root = tempdir("override");
        init(&root, SAMPLE_ENV, false).unwrap();
        fs::create_dir_all(env_d(&root)).unwrap();
        fs::write(
            env_d(&root).join("50-custom.toml"),
            "LANG = \"en_US.UTF-8\"\nJAVA_HOME = \"/overlayer/zexlib/union/lib/jvm\"\n",
        )
        .unwrap();

        let env = resolve(&root);
        assert_eq!(env.get("LANG").map(String::as_str), Some("en_US.UTF-8"));
        assert_eq!(
            env.get("JAVA_HOME").map(String::as_str),
            Some("/overlayer/zexlib/union/lib/jvm")
        );
        // Untouched base keys survive
        assert_eq!(env.get("EXAMPLE_VAR").map(String::as_str), Some("1"));
    }

    #[test]
    fn environment_d_filename_order_later_wins() {
        let root = tempdir("order");
        init(&root, SAMPLE_ENV, false).unwrap();
        fs::create_dir_all(env_d(&root)).unwrap();
        fs::write(env_d(&root).join("10-a.toml"), "LANG = \"a\"\n").unwrap();
        fs::write(env_d(&root).join("20-b.toml"), "LANG = \"b\"\n").unwrap();

        let env = resolve(&root);
        assert_eq!(env.get("LANG").map(String::as_str), Some("b"));
    }

    #[test]
    fn render_toml_parses_back_to_input() {
        let text = render_toml(SAMPLE_ENV);
        let table: toml::Table = text.parse().unwrap();
        for (k, v) in SAMPLE_ENV {
            assert_eq!(table.get(*k).and_then(|x| x.as_str()), Some(*v));
        }
    }

    #[test]
    fn profile_section_is_unprefixed() {
        let root = tempdir("profile-section");
        fs::create_dir_all(env_file(&root).parent().unwrap()).unwrap();
        fs::write(
            env_file(&root),
            "[Profile]\nPATH = \"/custom/bin\"\nMUSL_TRIPLE = \"x86_64-zainium-linux-musl\"\n",
        )
        .unwrap();

        let env = resolve(&root);
        assert_eq!(env.get("PATH").map(String::as_str), Some("/custom/bin"));
        assert_eq!(
            env.get("MUSL_TRIPLE").map(String::as_str),
            Some("x86_64-zainium-linux-musl")
        );
        // Must not also appear prefixed
        assert_eq!(env.get("PROFILE_PATH"), None);
    }

    #[test]
    fn other_sections_flatten_with_prefix() {
        let root = tempdir("nested-sections");
        fs::create_dir_all(env_file(&root).parent().unwrap()).unwrap();
        fs::write(
            env_file(&root),
            "[Path.Syshub]\nprefix = \"/overlayer/syshub\"\nbindir = \"/overlayer/syshub/bin\"\n\n[shell_console]\n",
        )
        .unwrap();

        let env = resolve(&root);
        assert_eq!(
            env.get("PATH_SYSHUB_PREFIX").map(String::as_str),
            Some("/overlayer/syshub")
        );
        assert_eq!(
            env.get("PATH_SYSHUB_BINDIR").map(String::as_str),
            Some("/overlayer/syshub/bin")
        );
        // Empty table contributes nothing, doesn't error
        assert!(!env.contains_key("SHELL_CONSOLE"));
    }

    #[test]
    fn mixed_bare_and_sectioned_keys_in_same_file() {
        let root = tempdir("mixed");
        fs::create_dir_all(env_file(&root).parent().unwrap()).unwrap();
        fs::write(
            env_file(&root),
            "PATH = \"/bare/path\"\n\n[Path.zexlib]\nunion_dir = \"/overlayer/zexlib/union\"\n",
        )
        .unwrap();

        let env = resolve(&root);
        assert_eq!(env.get("PATH").map(String::as_str), Some("/bare/path"));
        assert_eq!(
            env.get("PATH_ZEXLIB_UNION_DIR").map(String::as_str),
            Some("/overlayer/zexlib/union")
        );
    }

    #[test]
    fn verify_reports_clean_file_with_no_errors() {
        let root = tempdir("verify-clean");
        init(&root, SAMPLE_ENV, false).unwrap();

        let report = verify(&root);
        assert!(report.is_ok());
        assert_eq!(
            report.resolved.get("EXAMPLE_VAR").map(|(v, _)| v.as_str()),
            Some("1")
        );
    }

    #[test]
    fn verify_reports_broken_environment_d_file_without_aborting() {
        let root = tempdir("verify-broken");
        init(&root, SAMPLE_ENV, false).unwrap();
        fs::create_dir_all(env_d(&root)).unwrap();
        // Invalid TOML: bare key with no value (the exact bug this feature
        // exists to catch).
        fs::write(env_d(&root).join("10-broken.toml"), "UNION_DIR = \n").unwrap();
        fs::write(env_d(&root).join("20-good.toml"), "LANG = \"C.UTF-8\"\n").unwrap();

        let report = verify(&root);
        assert!(!report.is_ok());
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].path.ends_with("10-broken.toml"));
        // The good file after the broken one is still checked
        assert_eq!(
            report.resolved.get("LANG").map(|(v, _)| v.as_str()),
            Some("C.UTF-8")
        );
    }

    #[test]
    fn verify_tracks_which_file_each_key_came_from() {
        let root = tempdir("verify-provenance");
        init(&root, SAMPLE_ENV, false).unwrap();
        fs::create_dir_all(env_d(&root)).unwrap();
        fs::write(
            env_d(&root).join("50-override.toml"),
            "PATH = \"/overridden\"\n",
        )
        .unwrap();

        let report = verify(&root);
        let (value, source) = report.resolved.get("PATH").unwrap();
        assert_eq!(value, "/overridden");
        assert!(source.ends_with("50-override.toml"));
    }
}
