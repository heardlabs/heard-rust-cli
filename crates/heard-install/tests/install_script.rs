//! Runs `tests/install_sh.sh` — install.sh end to end in a temp HOME and
//! prefix against fake file:// releases — so `cargo test` covers it too.

use std::path::Path;
use std::process::Command;

#[test]
fn install_sh_end_to_end() {
    if !cfg!(target_os = "macos") {
        eprintln!("skipped: install.sh supports macOS only");
        return;
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut cmd = Command::new("sh");
    cmd.arg(root.join("tests/install_sh.sh"))
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ELEVENLABS_API_KEY")
        .env_remove("HEARD_INSTALL_BASE_URL");
    // Every other credential-shaped variable, too.
    for (k, _) in std::env::vars_os() {
        if k.to_str()
            .is_some_and(|k| k.ends_with("_API_KEY") || k.ends_with("_TOKEN"))
        {
            cmd.env_remove(k);
        }
    }
    let out = cmd.output().expect("run tests/install_sh.sh");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "tests/install_sh.sh failed\n--- stdout\n{stdout}\n--- stderr\n{stderr}"
    );
    assert!(stdout.contains(" passed, 0 failed"), "{stdout}");
}
