# oxidized-environment

A native, TOML-based environment resolver — the replacement for
`/etc/profile.d`. Not tied to any one distro: **zero hardcoded paths, zero
compiled-in defaults, zero opinions about what your environment should
contain.** Every caller supplies its own root and its own default content
explicitly; a distro-specific consumer (ZainiumOS's `quantra`, PID 1) owns
its own defaults in its own source, not here.

Traditional `/etc/profile.d/*.sh` only reaches interactive login shells
(bash/zsh source it — nothing else does), so every other process tree —
background services, sessions started some other way — has to re-derive the
same PATH/LD_LIBRARY_PATH/toolchain variables by hand. On Zainium that gap
already caused real drift: `quantra`'s `console-shell.toml` and its Rust-side
bootstrap fallback each hand-duplicated the same `PATH` string and went out
of sync.

This is the fix: **one schema, read the same way everywhere.** A distro's
PID 1 links `oxidized-environment-core` directly and applies it to its own
process environment before spawning anything — no subprocess, no shell.
Everything else (an installer, a control CLI, a plain shell script) calls
the `oxidized-environment` CLI binary without linking Rust at all.

## Layout

This is a two-crate Cargo workspace:

```
oxidized-environment/
├── Cargo.toml                          ← workspace root + the CLI package
├── src/main.rs                         ← CLI binary (depends on clap)
└── crates/
    └── oxidized-environment-core/
        ├── Cargo.toml
        └── src/lib.rs                  ← the resolver (depends on nothing but toml)
```

A boot-critical PID 1 links `oxidized-environment-core` only — `clap`
belongs to the CLI's `[[bin]]` target and never enters that build graph.

## On-disk schema

```
<root>/oxienv.toml   ← the one and only file this crate reads
```

`<root>` is whatever path the caller passes to `resolve()`/`init()` — there
is **no compiled-in default anywhere in this crate**. A caller that wants
`/overlayer/syshub/etc/oxidized-environment/oxienv.toml` (its own
subdirectory of `etc/`, not loose in `etc/` itself) passes
`/overlayer/syshub/etc/oxidized-environment` as `root`; a caller that wants
`/etc/oxidized-environment/oxienv.toml` (the `oxienv` CLI's fallback when it
isn't running on a real ZainiumOS system — see below) passes
`/etc/oxidized-environment`.
`ROOT_OVERRIDE_ENV` (`OXIDIZED_ENVIRONMENT_ROOT`) is just a name this crate
exports as a convention callers may opt into; the library never reads it
itself.

### Two ways to write a key, in the same file

**Bare keys, or a `[Profile]` table** — become environment variables
verbatim, case preserved, no prefix. Use this for real env var names:

```toml
[Profile]
PATH = "/overlayer/syshub/bin"
LANG = "C.UTF-8"
```

is identical to:

```toml
PATH = "/overlayer/syshub/bin"
LANG = "C.UTF-8"
```

**Any other table, at any nesting depth** — flattened into one environment
variable per string leaf, named by joining every segment from the table path
down to the leaf key with `_` and upper-casing:

```toml
[Path.Syshub]
bindir = "/overlayer/syshub/bin"
```

becomes environment variable `PATH_SYSHUB_BINDIR`. Use this for organized
reference data that would collide with real env var names if left bare
(nothing stops you from writing `[Path] bin = "..."`, but a bare `BIN`
env var is far more likely to collide with something than a namespaced
`PATH_BIN`).

Empty tables (`[shell_console]` with nothing under it) are valid and
contribute no keys — they don't error.

### No interpolation

There is no `${VAR}`-style expansion anywhere in this schema — every value
is a literal, already-resolved string. A build script's `LDFLAGS`/`CFLAGS`
(which typically reference other shell variables, e.g.
`-Wl,-rpath=${RUNTIME_PREFIX}/lib`) must have those references expanded to
concrete paths *before* landing in `oxienv.toml`. This is deliberate:
adding a template engine would mean parsing arbitrary expressions at
boot-time, in PID 1, for no benefit a flat literal string doesn't already
give you.

### No hidden fallback

If `oxienv.toml` is missing or fails to parse, `resolve()` returns
whatever it could load — an empty map in the worst case, **never** a
compiled-in guess about what a "default" environment should contain. A
consumer that needs a safety net for "file missing at boot" (a PID 1 that
must never start with an empty `PATH`) owns that fallback table itself and
merges it in only for keys `resolve()` came back missing. See `quantra`'s
`environment.rs` for a worked example — its `BOOT_FALLBACK` constant is
Zainium's, not this crate's.

## Sample files

- [`oxienv.toml.sample`](./oxienv.toml.sample) — Zainium's flat `[Profile]`
  defaults, for illustration (this crate ships no default of its own).
- [`oxienv.full.toml.sample`](./oxienv.full.toml.sample) — the complete
  path table: every `union`/`syshub`/musl-toolchain directory (`bin`,
  `sbin`, `lib`, `include`, `share`) populated, no commentary, ready to
  seed a fresh install.

## CLI (`oxienv`)

Unlike the library crate, the CLI binary **is** Zainium-specific — it's the
one consumer in this repo allowed to have an opinion about where things
live, since it's the thing everything else that can't link Rust actually
runs. Two flags total, no `--root`:

```sh
oxienv                     # resolve + print a report + apply to this process
oxienv --init              # bootstrap oxienv.toml from the built-in Zainium
                            #   defaults, if it doesn't exist yet (never
                            #   overwrites an existing file)
oxienv --apply -- <cmd..>  # exec cmd with the resolved environment applied
```

The config file location is auto-detected, not passed in:

- `/overlayer/syshub/etc/oxidized-environment/oxienv.toml` if
  `/overlayer/syshub` exists (a real ZainiumOS system) — its own
  subdirectory of `etc/`, not loose in `etc/` itself
- `/etc/oxidized-environment/oxienv.toml` otherwise — lets this exact binary
  be built and smoke-tested on any ordinary Linux dev machine, no Zainium
  disk required

The default (no-flag) run prints which mode was detected, the exact file
path checked and whether it was found, every resolved key sorted with its
value, and then applies the environment to itself — seeing the report *is*
the confirmation that apply is about to do the right thing, so there's no
separate "just show me" flag.

## Library API

```rust
use std::path::Path;

// A distro's PID 1, at the very start of main(), before anything is spawned:
let env = oxidized_environment::resolve(Path::new("/overlayer/syshub/etc/oxidized-environment"));
for (k, v) in env {
    std::env::set_var(k, v); // unsafe in newer Rust editions — see quantra's environment.rs
}

// An installer seeding a fresh disk from its own default table:
const DEFAULT_ENV: &[(&str, &str)] = &[("PATH", "/overlayer/syshub/bin")];
oxidized_environment::init(root, DEFAULT_ENV, false)?;

// ...or from an existing template file, preserving [Profile]/[Path.*] structure:
oxidized_environment::init_from_file(root, Path::new("template.toml"), false)?;

// Checking a file is well-formed before relying on it:
let report = oxidized_environment::verify(root);
if !report.is_ok() {
    for err in &report.errors {
        eprintln!("{}: {}", err.path.display(), err.message);
    }
}
```

## Who links what

| Consumer | Links | Why |
|---|---|---|
| `quantra` (PID 1) | `oxidized-environment-core` (library) | Boot-critical — no subprocess, no `clap` in the build graph. Owns its own root constant and boot-safety fallback table. |
| `quantra-ctl` | `oxidized-environment-core` (library) | `quantra-ctl env` reads straight from disk, works even if `quantra` isn't running. Owns its own root constant. |
| `zainium-installer` | `oxidized-environment-core` (library) | Seeds `oxienv.toml` on a fresh disk via `init()`, passing its own default table. |
| Anything else | `oxidized-environment` (CLI binary) | No Rust linking required. |

Every one of these consumers is Zainium-specific and is where Zainium's
`/overlayer/syshub` root and default values actually live — never inside
`oxidized-environment-core` itself.

## Testing

```sh
cd oxidized-environment
cargo test --workspace
```
