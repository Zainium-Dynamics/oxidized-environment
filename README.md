# oxidized-environment

A native, TOML-based environment resolver — the replacement for
`/etc/profile.d`. Zero hardcoded paths, zero compiled-in defaults: every
caller supplies its own root and its own default content explicitly. A
distro-specific consumer (ZainiumOS's `quantra`, PID 1) owns its own
defaults in its own source, not here.

`/etc/profile.d/*.sh` only reaches interactive login shells, so every other
process tree (services, sessions started some other way) has to re-derive
PATH/LD_LIBRARY_PATH/toolchain variables by hand — which already caused
drift on Zainium. The fix: **one schema, read the same way everywhere.** A
distro's PID 1 links `oxidized-environment-core` directly and applies it to
its own process, no subprocess, no shell. Everything else calls the
`oxidized-environment` CLI binary without linking Rust at all.

## Layout

Two-crate workspace:

```
oxidized-environment/
├── Cargo.toml                          ← workspace root + the CLI package
├── src/main.rs                         ← CLI binary (depends on clap)
└── crates/oxidized-environment-core/
    └── src/lib.rs                      ← the resolver (depends only on toml)
```

A boot-critical PID 1 links `oxidized-environment-core` only — `clap` never
enters that build graph.

## On-disk schema

```
<root>/oxienv.toml   ← the one and only file this crate reads
```

`<root>` is whatever path the caller passes to `resolve()`/`init()` — no
compiled-in default. The `oxienv` CLI passes
`/overlayer/syshub/etc/oxidized-environment` on a real ZainiumOS system, or
`/etc/oxidized-environment` otherwise. `ROOT_OVERRIDE_ENV`
(`OXIDIZED_ENVIRONMENT_ROOT`) is a naming convention callers may opt into;
the library never reads it itself.

Two shapes, freely mixed in the same file:

```toml
# Bare keys, or a [Profile] table — env vars verbatim, case preserved:
[Profile]
PATH = "/overlayer/syshub/bin"

# Any other table, at any depth — flattened to PREFIX_KEY:
[Path.Syshub]
bindir = "/overlayer/syshub/bin"   # → env var PATH_SYSHUB_BINDIR
```

Empty tables are valid and contribute no keys. There's no `${VAR}`
interpolation — every value must already be a resolved literal string.

If `oxienv.toml` is missing or fails to parse, `resolve()` returns whatever
it could load — never a compiled-in guess. A consumer needing a boot-safety
net (e.g. a PID 1 that must never start with an empty `PATH`) owns that
fallback table itself and merges it in only for keys `resolve()` came back
missing. See `verify()` to check a file is well-formed first.

## Sample file

[`etc/oxidized-environment/oxienv.toml`](./etc/oxidized-environment/oxienv.toml)
— the complete path table (every `union`/`syshub`/musl-toolchain
directory), laid out exactly where the `oxienv` CLI expects to find it —
ready to seed a fresh install.

## CLI (`oxienv`)

Zainium-specific — the one consumer here allowed an opinion about where
things live, since it's what everything that can't link Rust actually
runs.

```sh
oxienv                     # resolve + print a report + apply to this process
oxienv --init              # bootstrap oxienv.toml from built-in defaults
                            #   if it doesn't exist yet (never overwrites)
oxienv --apply -- <cmd..>  # exec cmd with the resolved environment applied
```

Config location is auto-detected:
`/overlayer/syshub/etc/oxidized-environment/oxienv.toml` if
`/overlayer/syshub` exists, else `/etc/oxidized-environment/oxienv.toml`
(so the binary is buildable/testable off-Zainium too).

The default run prints the report before applying — seeing it *is* the
confirmation that apply is about to do the right thing.

## Library API

```rust
use std::path::Path;

// A distro's PID 1, before anything is spawned:
let env = oxidized_environment::resolve(Path::new("/overlayer/syshub/etc/oxidized-environment"));
for (k, v) in env {
    std::env::set_var(k, v); // unsafe in newer Rust editions
}

// An installer seeding a fresh disk from its own default table:
oxidized_environment::init(root, &[("PATH", "/overlayer/syshub/bin")], false)?;

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
| `quantra` (PID 1) | `oxidized-environment-core` | Boot-critical — no subprocess, no `clap`. |
| `quantra-ctl` | `oxidized-environment-core` | Reads straight from disk, even if `quantra` isn't running. |
| `zainium-installer` | `oxidized-environment-core` | Seeds `oxienv.toml` on a fresh disk via `init()`. |
| Anything else | `oxidized-environment` (CLI) | No Rust linking required. |

Every consumer above owns its own root constant and default table — never
inside `oxidized-environment-core` itself.

## Testing

```sh
cargo test --workspace
```
