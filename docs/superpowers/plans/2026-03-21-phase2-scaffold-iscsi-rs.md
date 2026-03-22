# Phase 2: Scaffold iscsi-rs Repo — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restructure the project from `iscsi-fuse` (single binary + FUSE) into `iscsi-rs` (Cargo workspace with daemon + CLI binaries, plus Xcode project for DriverKit dext).

**Architecture:** Cargo workspace with two binary crates (`iscsid` daemon, `iscsi-rs` CLI) sharing a library crate (`iscsi-lib`). Xcode project for the DriverKit dext lives alongside. FUSE code is removed entirely.

**Tech Stack:** Rust (Cargo workspace), Xcode (DriverKit), C++

**Spec:** `docs/superpowers/specs/2026-03-21-iscsi-rs-driverkit-design.md` (Phase 2, lines 500-507)

---

## File Map — Target Structure

```
iscsi-rs/                          (repo root, renamed from iscsi)
├── Cargo.toml                     (workspace manifest)
├── crates/
│   ├── iscsi-lib/                 (shared library crate)
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs             (pub mod declarations)
│   │       ├── iscsi/             (copied from src/iscsi/, unchanged)
│   │       │   ├── mod.rs
│   │       │   ├── pdu.rs
│   │       │   ├── transport.rs
│   │       │   ├── login.rs
│   │       │   ├── session.rs
│   │       │   ├── command.rs
│   │       │   ├── pipeline.rs
│   │       │   ├── recovery.rs
│   │       │   ├── digest.rs
│   │       │   └── config.rs
│   │       ├── block/             (refactored from block_device.rs)
│   │       │   ├── mod.rs         (BlockDevice, BlockDeviceWorker, DirtyMap)
│   │       │   └── cache.rs       (moved from cache.rs, unchanged)
│   │       └── proto/
│   │           └── ring.rs        (placeholder for shared memory ring structs)
│   ├── iscsid/                    (daemon binary crate)
│   │   ├── Cargo.toml
│   │   └── src/
│   │       └── main.rs            (daemon entry point, placeholder)
│   └── iscsi-rs-cli/              (CLI binary crate)
│       ├── Cargo.toml
│       └── src/
│           └── main.rs            (CLI entry point, placeholder)
├── dext/                          (Xcode project, placeholder)
│   └── README.md
├── tests/
│   └── write_persistence.rs       (moved from tests/, updated imports)
├── docs/                          (existing docs)
└── .github/                       (existing)
```

**What is NOT in this phase:**
- No daemon logic (Phase 5)
- No CLI logic (Phase 5)
- No DriverKit C++ code (Phase 3)
- No dext bridge (Phase 4)
- No new features — purely structural

---

### Task 1: Rename Package and Set Up Cargo Workspace

**Files:**
- Modify: `Cargo.toml` (convert to workspace manifest)
- Create: `crates/iscsi-lib/Cargo.toml`
- Create: `crates/iscsid/Cargo.toml`
- Create: `crates/iscsi-rs-cli/Cargo.toml`

- [ ] **Step 1: Convert root Cargo.toml to workspace manifest**

Replace the contents of `Cargo.toml` with:

```toml
[workspace]
members = [
    "crates/iscsi-lib",
    "crates/iscsid",
    "crates/iscsi-rs-cli",
]

[workspace.dependencies]
tokio = { version = "1", features = ["full"] }
bytes = "1"
anyhow = "1"
thiserror = "2"
serde = { version = "1", features = ["derive"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt"] }
clap = { version = "4", features = ["derive"] }
libc = "0.2"
```

Note: workspace.dependencies defines shared versions once. Member crates reference them with `.workspace = true`.

**Toolchain requirement:** edition 2024 requires Rust 1.85+. Verify with `rustc --version`. The project already uses edition 2024 so this is not a new requirement.

- [ ] **Step 2: Create the library crate manifest**

Create `crates/iscsi-lib/Cargo.toml`:

```toml
[package]
name = "iscsi-lib"
version = "0.5.0"
edition = "2024"
license = "AGPL-3.0-or-later"

[dependencies]
tokio = { workspace = true }
bytes = { workspace = true }
anyhow = { workspace = true }
thiserror = { workspace = true }
serde = { workspace = true }
tracing = { workspace = true }
tracing-subscriber = { workspace = true }
libc = { workspace = true }
crc32c = "0.6"
socket2 = "0.6"
moka = { version = "0.12", features = ["future"] }
toml = "0.8"
```

Note: `toml` IS included — `config.rs` uses `toml::from_str` for config parsing. `fuser`, `num_cpus`, `clap`, `ctrlc`, `dirs` are NOT included — they were FUSE/binary-specific. `clap` and `dirs` were used by `CliArgs` which will be split out of the library (see Task 2 Step 4a).

- [ ] **Step 3: Create the daemon binary crate manifest**

Create `crates/iscsid/Cargo.toml`:

```toml
[package]
name = "iscsid"
version = "0.5.0"
edition = "2024"
license = "AGPL-3.0-or-later"

[dependencies]
iscsi-lib = { path = "../iscsi-lib" }
tokio = { workspace = true }
bytes = { workspace = true }
anyhow = { workspace = true }
tracing = { workspace = true }
tracing-subscriber = { workspace = true }
clap = { workspace = true }
serde = { workspace = true }
ctrlc = "3"
dirs = "6"
```

- [ ] **Step 4: Create the CLI binary crate manifest**

Create `crates/iscsi-rs-cli/Cargo.toml`:

```toml
[package]
name = "iscsi-rs"
version = "0.5.0"
edition = "2024"
license = "AGPL-3.0-or-later"

[[bin]]
name = "iscsi-rs"
path = "src/main.rs"

[dependencies]
anyhow = { workspace = true }
clap = { workspace = true }
serde = { workspace = true }
```

- [ ] **Step 5: Create directory structure**

```bash
mkdir -p crates/iscsi-lib/src
mkdir -p crates/iscsid/src
mkdir -p crates/iscsi-rs-cli/src
```

- [ ] **Step 6: Verify workspace parses**

Run: `cargo metadata --format-version 1 --no-deps | head -5`
Expected: No errors (may warn about missing src files)

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml crates/
git commit -m "chore: set up Cargo workspace with iscsi-lib, iscsid, iscsi-rs-cli crates"
```

---

### Task 2: Move iSCSI Library Code to iscsi-lib

**Files:**
- Create: `crates/iscsi-lib/src/lib.rs`
- Move: `src/iscsi/` → `crates/iscsi-lib/src/iscsi/`
- Move: `src/block_device.rs` → `crates/iscsi-lib/src/block/mod.rs`
- Move: `src/cache.rs` → `crates/iscsi-lib/src/block/cache.rs`
- Create: `crates/iscsi-lib/src/proto/ring.rs` (placeholder)
- Create: `crates/iscsi-lib/src/proto/mod.rs`

- [ ] **Step 1: Create lib.rs**

Create `crates/iscsi-lib/src/lib.rs`:

```rust
pub mod block;
pub mod iscsi;
pub mod proto;
```

- [ ] **Step 2: Copy iSCSI modules**

```bash
cp -r src/iscsi/ crates/iscsi-lib/src/iscsi/
```

- [ ] **Step 3: Create block module from block_device.rs and cache.rs**

```bash
mkdir -p crates/iscsi-lib/src/block
cp src/block_device.rs crates/iscsi-lib/src/block/mod.rs
cp src/cache.rs crates/iscsi-lib/src/block/cache.rs
```

- [ ] **Step 4: Update block/mod.rs imports and remove fuser::Errno (20 sites)**

In `crates/iscsi-lib/src/block/mod.rs`:
- Add `pub mod cache;` at the top of the file
- Change `use crate::cache::BlockCache;` to `use crate::block::cache::BlockCache;`
- Remove `use fuser::Errno;`
- Remove `use tokio::runtime::Handle;` (FUSE bridge, no longer needed)

**Errno migration strategy:** There are ~20 uses of `fuser::Errno` in block_device.rs. Add a helper at the top of the file and replace all occurrences:

```rust
use std::io::{Error as IoError, ErrorKind};

/// Helper to create an I/O error (replaces fuser::Errno::EIO).
fn io_error(msg: &str) -> IoError {
    IoError::new(ErrorKind::Other, msg)
}
```

Then replace all `Errno` usage:
- `Result<T, Errno>` → `Result<T, IoError>`
- `Err(Errno::EIO)` → `Err(io_error("..."))`  with a descriptive message
- `oneshot::Sender<Result<Bytes, Errno>>` → `oneshot::Sender<Result<Bytes, IoError>>`
- `oneshot::Sender<Result<u32, Errno>>` → `oneshot::Sender<Result<u32, IoError>>`
- `oneshot::Sender<Result<(), Errno>>` → `oneshot::Sender<Result<(), IoError>>`

Also remove the `block_on` calls in `read_bytes`, `write_bytes`, `flush` — these were FUSE thread bridge methods. The block device API will be called from async context in the daemon. Either make these methods `async` or keep the sync wrappers but note they'll be refactored in Phase 4.

- [ ] **Step 4a: Split CliArgs out of config.rs**

The file `src/iscsi/config.rs` contains both `Config` (library) and `CliArgs` (binary). `CliArgs` uses `clap::Parser` and `dirs::home_dir()` which are not library dependencies.

In `crates/iscsi-lib/src/iscsi/config.rs`:
- Remove the `CliArgs` struct and its `impl` block (everything that uses `clap` or `dirs`)
- Remove `use clap::Parser;` and `use dirs;` imports
- Remove the `CliArgs`-related test if present
- Keep `Config`, `TuningConfig`, `RecoveryConfig`, `CacheConfig`, `CONFIG_TEMPLATE`, `default_*` functions

In `crates/iscsi-lib/src/iscsi/mod.rs`:
- Remove `CliArgs` from the re-export: change `pub use config::{CliArgs, Config};` to `pub use config::Config;`
- Keep all other re-exports

`CliArgs` will be recreated in the `iscsid` crate when daemon logic is implemented (Phase 5).

- [ ] **Step 4b: Verify mod.rs re-exports are consistent**

Review `crates/iscsi-lib/src/iscsi/mod.rs` and ensure all re-exported types still exist after the CliArgs removal. The file should look like:

```rust
pub mod command;
pub mod config;
pub mod digest;
pub mod login;
pub mod pdu;
pub mod pipeline;
pub mod recovery;
pub mod session;
pub mod transport;

pub use config::Config;
#[allow(unused_imports)]
pub use login::{LoginManager, LoginResult, NegotiatedParams};
pub use pipeline::Pipeline;
pub use recovery::RecoveryManager;
pub use session::Session;
#[allow(unused_imports)]
pub use transport::{DigestConfig, TransportReader, TransportWriter};
```

- [ ] **Step 5: Create proto module placeholder**

Create `crates/iscsi-lib/src/proto/mod.rs`:

```rust
pub mod ring;
```

Create `crates/iscsi-lib/src/proto/ring.rs`:

```rust
//! Shared memory ring buffer data structures for dext↔daemon IPC.
//! These structs must match the C++ layout in the DriverKit extension.
//!
//! Placeholder — implemented in Phase 4.
```

- [ ] **Step 6: Build the library crate**

Run: `cargo build -p iscsi-lib 2>&1`

Fix any compilation errors. Expected issues:
- `fuser::Errno` references in block/mod.rs
- `use crate::cache` → `use crate::block::cache`
- Possible visibility issues (`pub` needed on types used across modules)

- [ ] **Step 7: Run library tests**

Run: `cargo test -p iscsi-lib`
Expected: All iSCSI unit tests pass (90 tests from command.rs, pipeline.rs, session.rs, etc.)

- [ ] **Step 8: Commit**

```bash
git add crates/iscsi-lib/
git commit -m "feat: move iSCSI and block device code to iscsi-lib crate

Moves iscsi/, block_device.rs, cache.rs into crates/iscsi-lib.
Removes fuser dependency — block errors use std::io::Error.
All 90 unit tests pass."
```

---

### Task 3: Create Daemon and CLI Placeholders

**Files:**
- Create: `crates/iscsid/src/main.rs`
- Create: `crates/iscsi-rs-cli/src/main.rs`

- [ ] **Step 1: Create daemon placeholder**

Create `crates/iscsid/src/main.rs`:

```rust
use anyhow::Result;
use tracing::info;
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("iscsid=info")),
        )
        .init();

    info!("iscsid v{}", env!("CARGO_PKG_VERSION"));
    info!("Daemon not yet implemented — see Phase 5");

    Ok(())
}
```

- [ ] **Step 2: Create CLI placeholder**

Create `crates/iscsi-rs-cli/src/main.rs`:

```rust
use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "iscsi-rs", version, about = "macOS iSCSI initiator")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Discover iSCSI targets on a portal
    Discover {
        /// Target portal address (e.g., 192.168.1.100:3260)
        portal: String,
    },
    /// Login to an iSCSI target
    Login {
        /// Target IQN
        target: String,
    },
    /// Logout from an iSCSI target
    Logout {
        /// Target IQN
        target: String,
    },
    /// List active iSCSI sessions
    List,
    /// Show daemon and dext status
    Status,
    /// Activate the DriverKit system extension
    Activate,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Discover { portal } => {
            println!("discover {portal}: not yet implemented (Phase 5)");
        }
        Commands::Login { target } => {
            println!("login {target}: not yet implemented (Phase 5)");
        }
        Commands::Logout { target } => {
            println!("logout {target}: not yet implemented (Phase 5)");
        }
        Commands::List => {
            println!("list: not yet implemented (Phase 5)");
        }
        Commands::Status => {
            println!("status: not yet implemented (Phase 5)");
        }
        Commands::Activate => {
            println!("activate: not yet implemented (Phase 7)");
        }
    }

    Ok(())
}
```

- [ ] **Step 3: Build all workspace members**

Run: `cargo build --workspace 2>&1`
Expected: All three crates compile

- [ ] **Step 4: Verify binaries work**

Run: `cargo run -p iscsid` — prints version and exits
Run: `cargo run -p iscsi-rs -- status` — prints "not yet implemented"
Run: `cargo run -p iscsi-rs -- --help` — prints help with all subcommands

- [ ] **Step 5: Commit**

```bash
git add crates/iscsid/ crates/iscsi-rs-cli/
git commit -m "feat: add iscsid daemon and iscsi-rs CLI placeholder binaries

Both compile and run. CLI has subcommand structure for
discover, login, logout, list, status, activate.
All functionality deferred to Phase 5."
```

---

### Task 4: Remove FUSE Code and Old Binary

**Files:**
- Delete: `src/fuse_fs.rs`
- Delete: `src/auto_format.rs`
- Delete: `src/main.rs`
- Delete: `src/lib.rs`
- Modify: old root source files — all moved or deleted

- [ ] **Step 1: Remove old source files**

```bash
rm -f src/fuse_fs.rs src/auto_format.rs src/main.rs src/lib.rs
rm -f src/block_device.rs src/cache.rs
rm -rf src/iscsi/
rmdir src/ 2>/dev/null || true
```

- [ ] **Step 2: Cargo.lock note**

The existing `Cargo.lock` will be updated in-place by Cargo when the workspace is built. Do NOT delete it — Cargo handles the migration automatically.

- [ ] **Step 3: Update .gitignore if needed**

Ensure `target/` is in `.gitignore` (should already be there).

- [ ] **Step 4: Move integration test and update imports**

Move `tests/write_persistence.rs` to workspace-level test or update imports:

```bash
mkdir -p tests/
```

Update `tests/write_persistence.rs` — change all `use iscsi_fuse::` to `use iscsi_lib::`:

```rust
use iscsi_lib::iscsi::login::LoginManager;
use iscsi_lib::iscsi::pipeline::Pipeline;
use iscsi_lib::iscsi::session::{IttPool, Session, SessionState};
use iscsi_lib::iscsi::transport::Transport;
```

Note: workspace-level integration tests need a `[[test]]` section or a dependency. The simplest approach is to move the test into `crates/iscsi-lib/tests/write_persistence.rs` so it naturally tests the lib crate.

```bash
mkdir -p crates/iscsi-lib/tests/
mv tests/write_persistence.rs crates/iscsi-lib/tests/write_persistence.rs
rmdir tests/ 2>/dev/null || true
```

Then update imports from `iscsi_fuse::` to `iscsi_lib::`.

The integration test also imports `anyhow` and `bytes` directly — these are already in `iscsi-lib`'s `[dependencies]`, so integration tests in `crates/iscsi-lib/tests/` can use them without adding `[dev-dependencies]`. Verify this compiles.

- [ ] **Step 5: Build full workspace**

Run: `cargo build --workspace 2>&1`
Expected: Compiles with no errors

- [ ] **Step 6: Run all tests**

Run: `cargo test --workspace`
Expected: All unit tests pass (from iscsi-lib), integration test compiles but is `#[ignore]`

- [ ] **Step 7: Commit**

Use targeted git add — do NOT use `git add -A` as there are untracked directories (`.agents/`, `.claude/worktrees/`, `homebrew-iscsi-fuse/`) that should not be committed.

```bash
git add crates/ Cargo.toml Cargo.lock
git rm -r --cached src/ tests/ 2>/dev/null || true
git commit -m "refactor: remove FUSE code, complete workspace migration

Deleted: fuse_fs.rs, auto_format.rs, old main.rs/lib.rs
Removed fuser, num_cpus dependencies
Moved integration test to crates/iscsi-lib/tests/
All workspace tests pass."
```

---

### Task 5: Create Dext Xcode Project Placeholder

**Files:**
- Create: `dext/README.md`
- Create: `dext/iscsi-rs-dext.xcodeproj/` (via xcodebuild or manual)

Since Xcode projects are complex to create programmatically, this task creates the directory structure and a README with setup instructions. The actual Xcode project creation will happen at the start of Phase 3 using Xcode GUI.

- [ ] **Step 1: Create dext directory with README**

Create `dext/README.md`:

```markdown
# iscsi-rs DriverKit Extension

This directory will contain the Xcode project for the DriverKit system extension.

## Setup (Phase 3)

1. Open Xcode
2. File → New → Project → DriverKit → Driver
3. Product Name: iscsi-rs-dext
4. Bundle Identifier: com.peilinwu.iscsi-rs
5. Save to this directory
6. Add IOUserBlockStorageDevice framework

## Entitlements Required

- com.apple.developer.driverkit
- com.apple.developer.driverkit.family.block-storage-device
- com.apple.developer.driverkit.transport.userclient

## Architecture

See: docs/superpowers/specs/2026-03-21-iscsi-rs-driverkit-design.md
```

- [ ] **Step 2: Apply for DriverKit entitlements**

This is a manual step — go to https://developer.apple.com/account and request:
- `com.apple.developer.driverkit.family.block-storage-device`

Document the request date in the README.

- [ ] **Step 3: Commit**

```bash
git add dext/
git commit -m "chore: add dext directory placeholder and entitlement instructions"
```

---

### Task 6: Verify and Final Cleanup

- [ ] **Step 1: Run full workspace build**

Run: `cargo build --workspace`
Expected: All crates compile

- [ ] **Step 2: Run all tests**

Run: `cargo test --workspace`
Expected: All tests pass

- [ ] **Step 3: Run clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: No warnings

- [ ] **Step 4: Run fmt**

Run: `cargo fmt --all --check`
Expected: Clean

- [ ] **Step 5: Verify binary names**

Run: `cargo build --workspace && ls -la target/debug/iscsid target/debug/iscsi-rs`
Expected: Both binaries exist

- [ ] **Step 6: Commit any fixups**

```bash
git add crates/ Cargo.toml Cargo.lock
git commit -m "chore: clippy/fmt cleanup for workspace migration"
```

---

## Phase 2 Gate

All of these must be true before proceeding to Phase 3:

- [ ] Cargo workspace with 3 members: `iscsi-lib`, `iscsid`, `iscsi-rs-cli`
- [ ] `iscsi-lib` contains all iSCSI protocol + block layer code
- [ ] `iscsid` binary compiles and runs (placeholder)
- [ ] `iscsi-rs` binary compiles and runs with subcommands (placeholder)
- [ ] FUSE code completely removed (`fuse_fs.rs`, `auto_format.rs`, `fuser` dep)
- [ ] All unit tests pass
- [ ] clippy clean, fmt clean
- [ ] DriverKit entitlement requested from Apple
- [ ] Dext directory placeholder exists
