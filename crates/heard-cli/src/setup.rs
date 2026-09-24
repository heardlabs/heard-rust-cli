//! `heard setup`: model → voice (+ preview) → mode → hooks.
//! Without a terminal it prints the equivalent commands instead of prompting.

use crate::commands::{daemon, install};
use crate::doctor::which;
use crate::models;
use crate::pick;
use crate::settings::{self, Ctx};
use crate::ui::{self, CliResult};

/// Agent CLIs found on PATH, as install targets.
pub fn detected_agents() -> Vec<(install::Target, &'static str)> {
    let mut v = Vec::new();
    if which("claude").is_some() {
        v.push((install::Target::ClaudeCode, "claude"));
    }
    if which("codex").is_some() {
        v.push((install::Target::Codex, "codex"));
    }
    v
}

/// The non-interactive instructions.
pub fn plan_text(ctx: &Ctx) -> String {
    let src = models::Source::from_env().ok();
    let have_model = src
        .as_ref()
        .is_some_and(|s| models::installed(&ctx.paths.models_dir, &s.files));
    let mut s = String::from(
        "heard setup is interactive and needs a terminal. To set up from a script, run:\n\n",
    );
    if have_model {
        s.push_str("  # voice model already installed\n");
    } else {
        s.push_str("  heard models download        # the Kokoro voice, about 354 MB\n");
    }
    s.push_str("  heard voice bm_george        # or `heard voice --list`\n");
    s.push_str("  heard mode copilot           # or companion, focus\n");
    let agents = detected_agents();
    if agents.is_empty() {
        s.push_str("  heard install all            # after installing claude or codex\n");
    }
    for (t, bin) in agents {
        s.push_str(&format!(
            "  heard install {:<14} # `{bin}` found on PATH\n",
            t.name()
        ));
    }
    s
}

/// Run setup.
pub fn run(ctx: &Ctx) -> CliResult<()> {
    // Setup IS the CLI edition's onboarding: mark it done first, so even an
    // abandoned setup never leaves narration held (see `crate::edition`).
    install::mark_onboarded(&ctx.paths)?;
    if !ui::interactive() {
        print!("{}", plan_text(ctx));
        return Ok(());
    }
    println!("{}", ui::bold("Heard setup"));
    println!(
        "{}\n",
        ui::dim("Four steps: voice model, voice, mode, agent hooks. Esc skips a step.")
    );

    // 1. Model.
    println!("{} Voice model", ui::cyan("1/4"));
    let src = models::Source::from_env()?;
    if models::installed(&ctx.paths.models_dir, &src.files) {
        println!(
            "    already installed in {}",
            ctx.paths.models_dir.display()
        );
    } else if pick::confirm("Download the Kokoro voice model now (about 354 MB)?", true)? {
        models::download(&src, &ctx.paths.models_dir, true)?;
        println!("    {} model installed", ui::green("✓"));
    } else {
        println!("    skipped — run `heard models download` later");
    }

    // 2. Voice.
    println!("\n{} Voice", ui::cyan("2/4"));
    let cfg = ctx.load()?;
    if let Some(v) = pick::voice(&settings::effective_voice(&ctx.paths, &cfg))? {
        println!("    {}", settings::set_voice(ctx, &v)?);
        if pick::confirm("Hear a preview?", true)? {
            if !ctx.daemon.is_up() {
                let _ = daemon::start(ctx, std::time::Duration::from_secs(3));
            }
            match settings::preview_voice(ctx, &v) {
                Ok(m) => println!("    {m}"),
                Err(e) => e.report(),
            }
        }
    }

    // 3. Mode.
    println!("\n{} Mode", ui::cyan("3/4"));
    if let Some(m) = pick::mode(&settings::current_mode(&cfg))? {
        println!("    {}", settings::set_mode(ctx, m)?);
    }

    // 4. Hooks.
    println!("\n{} Agent hooks", ui::cyan("4/4"));
    let agents = detected_agents();
    if agents.is_empty() {
        println!("    no `claude` or `codex` on PATH — run `heard install` after installing one");
    }
    for (t, bin) in agents {
        if pick::confirm(
            &format!("`{bin}` found. Install Heard's hooks for it?"),
            true,
        )? {
            match install::install(&ctx.paths, t, false) {
                Ok(m) => println!("    {m}"),
                Err(e) => e.report(),
            }
        }
    }

    println!(
        "\n{} Open the console with `heard`, or check everything with `heard doctor`.",
        ui::green("Done.")
    );
    Ok(())
}
