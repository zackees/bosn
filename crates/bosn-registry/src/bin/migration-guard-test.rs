//! Test-only process endpoint for Python/Rust lock-interoperability evidence.

use std::io::{self, Write as _};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    let state_dir = arguments
        .next()
        .ok_or("usage: migration-guard-test STATE_DIR")?;
    let _guard = bosn_registry::acquire_legacy_migration_guard(state_dir)?;
    println!("ready");
    io::stdout().flush()?;
    // The Python test supplies one line only after its shared-lock acquisition
    // has demonstrably blocked. EOF is also a clean release for test cleanup.
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(())
}
