//! `oxidized-environment` (`oxienv`) — the ZainiumOS environment auto
//! handler. Replaces `/etc/profile.d`.
//!
//! Unlike `oxidized-environment-core` (which stays a generic, root-agnostic
//! library any consumer can link with its own root — see `quantra` and
//! `quantra-ctl`), this binary is Zainium-specific: it knows about
//! `/overlayer/syshub` directly, so there's nothing to pass on every
//! invocation.
//!
//! Two flags total:
//!
//!   oxienv                    resolve + print a report + apply to self
//!   oxienv --init              bootstrap oxienv.toml if it doesn't exist yet
//!   oxienv --apply -- <cmd..>  exec cmd with the resolved environment applied
//!
//! Config file location is auto-detected, not passed:
//!   - `/overlayer/syshub/etc/oxienv.toml` on a real ZainiumOS system —
//!     config lives under `etc/` same as every other package (`apparmor`,
//!     `passwd`, `resolv.conf`, ...)
//!   - `/etc/oxienv.toml` on any other Linux host (lets this exact binary
//!     be built and smoke-tested on a normal dev machine)

use clap::Parser;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The Zainium default table `--init` seeds a fresh `oxienv.toml` from —
/// mirrors `oxienv.toml.sample`. Owned here, not in the core crate: this
/// binary is the Zainium-specific consumer that gets to have an opinion.
const ZAINIUM_DEFAULTS: &[(&str, &str)] = &[
    ("MUSL_TRIPLE", "x86_64-zainium-linux-musl"),
    ("MUSL_SYSDIR", "/overlayer/syshub/x86_64-zainium-linux-musl"),
    (
        "MUSL_LIB",
        "/overlayer/syshub/x86_64-zainium-linux-musl/lib",
    ),
    ("RELIBC_DIR", "/overlayer/syshub/lib"),
    (
        "MUSL_LD",
        "/overlayer/syshub/x86_64-zainium-linux-musl/lib/ld-musl-x86_64.so.1",
    ),
    ("OXIDIZED_ENV", "1"),
    ("LIBC_HYBRID", "musl+relibc"),
    ("NO_FHS", "1"),
    ("RESOLVED_SH", "/overlayer/syshub/bin/sh"),
    (
        "PATH",
        "/overlayer/syshub/bin:/overlayer/syshub/sbin:/overlayer/syshub/x86_64-zainium-linux-musl/bin:/overlayer/zexlib/union/bin",
    ),
    (
        "LD_LIBRARY_PATH",
        "/overlayer/syshub/lib:/overlayer/syshub/x86_64-zainium-linux-musl/lib:/overlayer/zexlib/union/lib",
    ),
    ("LANG", "C.UTF-8"),
    ("LC_ALL", "C.UTF-8"),
];

#[derive(Parser)]
#[command(
    name = "oxienv",
    version,
    about = "ZainiumOS environment auto handler — the replacement for /etc/profile.d"
)]
struct Cli {
    /// Bootstrap oxienv.toml from the built-in Zainium defaults if it
    /// doesn't exist yet. Never overwrites an existing file.
    #[arg(long)]
    init: bool,

    /// Exec a command with the resolved environment applied instead of
    /// applying it to this process. Everything after `--` is the command.
    #[arg(long, requires = "apply_cmd")]
    apply: bool,

    #[arg(trailing_var_arg = true, hide = true)]
    apply_cmd: Vec<String>,
}

/// Where `oxienv.toml` lives: configuration on ZainiumOS always lives under
/// `etc/` (same as every other package's config — `apparmor`, `passwd`,
/// `resolv.conf`, ...), so this is `/overlayer/syshub/etc/oxienv.toml` on a
/// real ZainiumOS system, or `/etc/oxienv.toml` (the same convention, at
/// the generic Linux root) on any other host — so this binary is buildable
/// and testable on any Linux host, not just Zainium.
fn oxienv_dir() -> PathBuf {
    let zainium = Path::new("/overlayer/syshub");
    if zainium.is_dir() {
        zainium.join("etc")
    } else {
        PathBuf::from("/etc")
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let dir = oxienv_dir();

    if cli.init {
        return cmd_init(&dir);
    }
    if cli.apply {
        return cmd_apply(&dir, &cli.apply_cmd);
    }
    cmd_default(&dir)
}

fn cmd_init(dir: &Path) -> ExitCode {
    match oxidized_environment::init(dir, ZAINIUM_DEFAULTS, false) {
        Ok(oxidized_environment::InitAction::Created) => {
            println!("oxienv: created {}", dir.join("oxienv.toml").display());
            ExitCode::SUCCESS
        }
        Ok(oxidized_environment::InitAction::AlreadyExists) => {
            println!(
                "oxienv: {} already exists, left untouched",
                dir.join("oxienv.toml").display()
            );
            ExitCode::SUCCESS
        }
        Ok(oxidized_environment::InitAction::Overwritten) => unreachable!("force is always false"),
        Err(e) => {
            eprintln!("oxienv: init failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Default (no-flag) behavior: report what was found, then apply it to this
/// process. Seeing the report *is* the confirmation that apply is about to
/// do the right thing — no separate "just show me" flag needed.
fn cmd_default(dir: &Path) -> ExitCode {
    let file = dir.join("oxienv.toml");
    let on_zainium = Path::new("/overlayer/syshub").is_dir();

    println!(
        "oxienv: {} ({})",
        if on_zainium {
            "ZainiumOS detected"
        } else {
            "not on ZainiumOS — using /etc fallback"
        },
        if on_zainium {
            "/overlayer/syshub"
        } else {
            "/etc"
        }
    );
    println!(
        "oxienv: config file: {} ({})",
        file.display(),
        if file.is_file() { "found" } else { "missing" }
    );

    let report = oxidized_environment::verify(dir);
    if !report.errors.is_empty() {
        for err in &report.errors {
            eprintln!("oxienv: {}: {}", err.path.display(), err.message);
        }
        return ExitCode::FAILURE;
    }

    let mut keys: Vec<_> = report.resolved.keys().collect();
    keys.sort();
    let width = keys.iter().map(|k| k.len()).max().unwrap_or(0);
    println!("oxienv: {} key(s) resolved:", keys.len());
    for k in &keys {
        let (value, _) = &report.resolved[*k];
        println!("  {:<width$}  {value}", k, width = width);
    }

    let env = oxidized_environment::resolve(dir);
    for (k, v) in &env {
        // SAFETY: single-threaded at this point in main(), no other code
        // has spawned threads that could race on the environment.
        unsafe { std::env::set_var(k, v) };
    }
    println!("oxienv: applied to this process");

    ExitCode::SUCCESS
}

#[cfg(unix)]
fn cmd_apply(dir: &Path, command: &[String]) -> ExitCode {
    use std::os::unix::process::CommandExt;

    let env = oxidized_environment::resolve(dir);
    let Some((program, args)) = command.split_first() else {
        eprintln!("oxienv: --apply requires a command after --");
        return ExitCode::FAILURE;
    };

    // execvp() replaces this process — it only returns on error.
    let err = std::process::Command::new(program)
        .args(args)
        .env_clear()
        .envs(&env)
        .exec();
    eprintln!("oxienv: exec {program}: {err}");
    ExitCode::FAILURE
}

#[cfg(not(unix))]
fn cmd_apply(dir: &Path, command: &[String]) -> ExitCode {
    let env = oxidized_environment::resolve(dir);
    let Some((program, args)) = command.split_first() else {
        eprintln!("oxienv: --apply requires a command after --");
        return ExitCode::FAILURE;
    };

    match std::process::Command::new(program)
        .args(args)
        .env_clear()
        .envs(&env)
        .status()
    {
        Ok(status) if status.success() => ExitCode::SUCCESS,
        Ok(_) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("oxienv: exec {program}: {e}");
            ExitCode::FAILURE
        }
    }
}
