//! `oxidized-environment` — CLI for the native, TOML-based environment
//! resolver (see `oxidized-environment-core` for the schema and the "why").
//!
//! Exists so anything that can spawn a process — an installer, a control
//! CLI, a TOML service, a plain shell — can read, seed, or check the
//! environment without linking Rust. A distro's PID 1 links
//! `oxidized-environment-core` directly for the boot-critical path; this
//! binary is for everyone else.
//!
//! No path is hardcoded anywhere in this binary — `--root` has no compiled
//! default, only the `OXIDIZED_ENVIRONMENT_ROOT` convention (opt-in, via
//! `env =` below); `init` has no compiled-in "default environment" either —
//! it copies content from `--from <file>`, which the caller supplies.

use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "oxidized-environment",
    version,
    about = "Native, TOML-based environment resolver — the replacement for /etc/profile.d"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the resolved environment.
    Resolve {
        /// Root to resolve against (no default — pass it, or set
        /// OXIDIZED_ENVIRONMENT_ROOT)
        #[arg(long, env = oxidized_environment::ROOT_OVERRIDE_ENV)]
        root: PathBuf,
        /// Output format
        #[arg(long, value_enum, default_value_t = Format::Export)]
        format: Format,
    },
    /// Seed a fresh environment.toml by copying it from a template file.
    Init {
        /// Root to write environment.toml under
        #[arg(long, env = oxidized_environment::ROOT_OVERRIDE_ENV)]
        root: PathBuf,
        /// Template file to copy (validated as TOML first). This tool has
        /// no built-in default content — the caller always supplies one.
        #[arg(long)]
        from: PathBuf,
        /// Overwrite an existing environment.toml instead of leaving it alone
        #[arg(long, default_value_t = false)]
        force: bool,
    },
    /// Check environment.toml and every environment.d/*.toml file parse
    /// cleanly, without applying anything. Exits non-zero if any do not.
    Verify {
        /// Root to verify
        #[arg(long, env = oxidized_environment::ROOT_OVERRIDE_ENV)]
        root: PathBuf,
        /// Also print every resolved key and which file it came from
        #[arg(long, default_value_t = false)]
        verbose: bool,
    },
    /// Exec a command with the resolved environment applied.
    Apply {
        /// Root to resolve against
        #[arg(long, env = oxidized_environment::ROOT_OVERRIDE_ENV)]
        root: PathBuf,
        /// Command and arguments to exec (after --)
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
}

#[derive(Copy, Clone, clap::ValueEnum)]
enum Format {
    /// KEY=value, one per line
    Kv,
    /// export KEY="value", one per line — `source`-able by a POSIX shell
    Export,
    /// JSON object
    Json,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Command::Resolve { root, format } => cmd_resolve(&root, format),
        Command::Init { root, from, force } => cmd_init(&root, &from, force),
        Command::Verify { root, verbose } => cmd_verify(&root, verbose),
        Command::Apply { root, command } => cmd_apply(&root, command),
    }
}

fn cmd_resolve(root: &Path, format: Format) -> ExitCode {
    let env = oxidized_environment::resolve(root);
    let mut keys: Vec<_> = env.keys().collect();
    keys.sort();

    match format {
        Format::Kv => {
            for k in keys {
                println!("{k}={}", env[k]);
            }
        }
        Format::Export => {
            for k in keys {
                println!("export {k}={:?}", env[k]);
            }
        }
        Format::Json => {
            print!("{{");
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    print!(",");
                }
                print!("{:?}:{:?}", k, env[*k]);
            }
            println!("}}");
        }
    }
    ExitCode::SUCCESS
}

fn cmd_init(root: &Path, from: &Path, force: bool) -> ExitCode {
    match oxidized_environment::init_from_file(root, from, force) {
        Ok(oxidized_environment::InitAction::Created) => {
            eprintln!(
                "environment.toml created under {} (from {})",
                root.display(),
                from.display()
            );
            ExitCode::SUCCESS
        }
        Ok(oxidized_environment::InitAction::Overwritten) => {
            eprintln!(
                "environment.toml overwritten under {} (from {})",
                root.display(),
                from.display()
            );
            ExitCode::SUCCESS
        }
        Ok(oxidized_environment::InitAction::AlreadyExists) => {
            eprintln!(
                "environment.toml already exists under {} (use --force to overwrite)",
                root.display()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("oxidized-environment init: {e}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_verify(root: &Path, verbose: bool) -> ExitCode {
    let report = oxidized_environment::verify(root);

    if verbose {
        let mut keys: Vec<_> = report.resolved.keys().collect();
        keys.sort();
        for k in keys {
            let (value, source) = &report.resolved[k];
            println!("{k}={value:?}  ({})", source.display());
        }
    }

    if report.is_ok() {
        eprintln!(
            "verify: ok — {} key(s) resolved under {}",
            report.resolved.len(),
            root.display()
        );
        ExitCode::SUCCESS
    } else {
        for err in &report.errors {
            eprintln!("verify: {} — {}", err.path.display(), err.message);
        }
        eprintln!(
            "verify: {} file(s) failed to parse under {}",
            report.errors.len(),
            root.display()
        );
        ExitCode::FAILURE
    }
}

#[cfg(unix)]
fn cmd_apply(root: &Path, command: Vec<String>) -> ExitCode {
    use std::os::unix::process::CommandExt;

    let env = oxidized_environment::resolve(root);
    let Some((program, args)) = command.split_first() else {
        eprintln!("oxidized-environment apply: no command given");
        return ExitCode::FAILURE;
    };

    // execvp() replaces this process — it only returns on error.
    let err = std::process::Command::new(program)
        .args(args)
        .env_clear()
        .envs(&env)
        .exec();
    eprintln!("oxidized-environment apply: exec {program}: {err}");
    ExitCode::FAILURE
}

#[cfg(not(unix))]
fn cmd_apply(root: &Path, command: Vec<String>) -> ExitCode {
    let env = oxidized_environment::resolve(root);
    let Some((program, args)) = command.split_first() else {
        eprintln!("oxidized-environment apply: no command given");
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
            eprintln!("oxidized-environment apply: exec {program}: {e}");
            ExitCode::FAILURE
        }
    }
}
