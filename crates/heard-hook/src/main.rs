//! The heard-cli edition's `heard-hook`. Everything lives in the library
//! (`src/lib.rs`), so another edition builds its own hook from the same crate
//! by handing `run` a different [`heard_hook::HookConfig`].

#![forbid(unsafe_code)]

fn main() {
    // Every path is a no-op on failure, so there is exactly one exit code.
    heard_hook::run(&heard_hook::HookConfig::CLI_EDITION);
}
