# AeroFTP - Agent Guidelines

## General Tool Use

When using the edit tool, you MUST provide parameters as flat strings. NEVER
nest oldString or newString inside a secondary JSON object. Use the exact code
block from the file for oldString.

## Build & Test Commands

### Core Commands
```bash
cargo build              # Build in debug mode
cargo build --release    # Optimized release build
cargo run                # Run the FTP server
cargo clippy             # Lint and check for issues
cargo fmt --check        # Verify code formatting
cargo test               # Run all tests
cargo test -- --test-threads=1  # Run single-threaded (avoid race conditions)
```

### Running a Single Test
```bash
cargo test <test_name>           # Run specific test by name
cargo test <module>::<test_name> # Run test in specific module
```

### Development Tips
- Use `pretty_env_logger` for console logging (initialized via `pretty_env_logger::init()`)
- Tokio Console available at `127.0.0.1:6669` when feature is enabled
- FTP server listens on port 21, HTTP metrics on `[::]:9090`
- If you need to work with version control, use the git tool. There is an MCP server available.

## Code Style Guidelines

### Import Organization (in order)
1. Standard library (`std`, `core`)
2. External crates (`tokio`, `anyhow`, etc.)
3. Current crate modules (`crate::`, `super::`)
4. Self imports at bottom with grouping comments

**Example:**
```rust
use std::{path::Path, sync::Arc};
use tokio::sync::Mutex;
use anyhow::{Context, Result};

use crate::{aws, ftp, metrics};
use super::utils;
```

### Formatting & Style
- Use `cargo fmt` to auto-format (Rustfmt defaults)
- Max line length: 100 characters
- Single imports on separate lines unless same module
- Group related items with blank lines

### Type System & Naming
- Use `snake_case` for functions, variables, modules
- Use `PascalCase` for types, structs, enums, traits
- Type parameters: single uppercase (`T`, `E`, `K`, `V`)
- Constants: `SCREAMING_SNAKE_CASE`
- Private helpers: prefix with `_` or use module scope

### Error Handling
- **CRITICAL**: Never use `.unwrap()` or `.expect()` in production code
- Use `anyhow::Result<T>` for function return types
- Chain errors with `?` operator and `.context("message")`
- Custom error types via `thiserror` for library APIs (if applicable)

**Correct:**
```rust
fn read_config(path: &Path) -> Result<Config> {
    let content = std::fs::read_to_string(path)?;
    serde_json::from_str(&content).context("Failed to parse config")
}
```

### Async Patterns (Tokio)
- Use `#[tokio::main]` for binary entry points
- Never hold locks across `.await` points
- Clone data before await, release locks first
- Use `spawn()` for fire-and-forget tasks
- Use channels (`mpsc`, `broadcast`) for task communication

**Example:**
```rust
let handle = tokio::spawn(async move {
    process_data(data).await;
});
```

### Module Organization
- One module per logical component (aws, ftp, http, signal, metrics)
- Use `mod.rs` files for multi-file modules
- Re-export public APIs via parent module
- Keep main.rs minimal (~60 lines after refactoring)

### AWS Credentials
- Use `CachingAwsCredentialLoader` for credential caching
- Credentials cached with 15-minute expiry check
- Supports EC2 metadata, ECS, and EKS credential providers

### Documentation
- Document all public items with `///` comments
- Include `# Examples`, `# Errors`, `# Panics` sections where relevant
- Use intra-doc links: `[Type]`, [`function()`]

## Project Structure
```
src/
├── main.rs          # Entry point, runtime setup (~60 lines)
├── aws/             # AWS credential loading
│   ├── mod.rs
│   └── creds.rs     # AwsCreds, CachingAwsCredentialLoader
├── ftp/             # FTP server configuration
│   ├── mod.rs
│   └── server.rs
├── http/            # HTTP metrics server
│   ├── mod.rs
│   └── server.rs    # HttpHandler with router
├── signal/          # Signal handling
│   ├── mod.rs
│   └── watcher.rs
└── metrics/         # Prometheus metrics
    ├── mod.rs
    └── prometheus.rs
```

## Anti-patterns to Avoid
- No `unwrap()` or direct panics for expected errors
- Don't hold `Mutex`/`RwLock` across `.await` points
- Avoid `&String` and `&Vec<T>`; prefer `&str` and `&[T]`
- Don't clone unnecessarily—use references where possible
- No empty error handling (`if let Err(_) = ... {}`)

## Release Profile
Optimized for size with LTO:
```toml
opt-level = "s"
lto = true
codegen-units = 1
strip = true
```
