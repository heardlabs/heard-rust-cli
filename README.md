# heard

**Hear your coding agents.** Heard narrates Claude Code and Codex CLI out loud
while they work. It tells you when a task finishes, when something fails, and
when an agent is waiting on you, so you can look away from the screen.

- **Local voice.** Kokoro runs on your Mac. No account, no cloud, no API key.
- **CLI only.** One small binary and a hook. No app, no microphone.
- **Fast.** The hook adds a few milliseconds to each tool call and never blocks your agent.
- **Open source.** Apache-2.0.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/heardlabs/heard-rust-cli/main/install.sh | sh
```

The installer puts `heard` and `heard-hook` in `~/.local/bin`, then runs
`heard setup`. Setup downloads the voice model (about 350 MB, checked by
SHA-256), lets you pick a voice and a mode, and adds Heard's hooks to Claude
Code and Codex.

The installer checks the download against the release's `SHA256SUMS`. That
catches a broken download, but both files come from the same release. To
check that a release tarball was really built by this repository's CI, verify
its signed provenance with the GitHub CLI:

```sh
gh attestation verify heard-macos-universal.tar.gz --repo heardlabs/heard-rust-cli
```

From a clone, `./install.sh` builds from source instead. It needs Rust.
Run `./install.sh --help` for options.

**Requirements:** macOS 13 or later, on Apple silicon or Intel.

## Use

Type `heard` to open the console. Type `/` to see the commands.

```
 heard · co-pilot · jarvis (bm_george) · 1.0× · alerts: both
 ─────────────────────────────────────────────────────────
 14:02:11  claude-code  Tests pass. Pushing the branch.
 14:02:40  codex        Needs approval to run rm -rf build.
 ─────────────────────────────────────────────────────────
 > /mode focus
```

Any text you type without a `/` is spoken aloud.

Every console command is also a regular command you can script:

```sh
heard mode focus          # co-pilot · companion · focus
heard voice               # arrow-key picker; --preview to hear one
heard persona friday      # jarvis · aria · friday · atlas, or your own .md
heard speed 1.2
heard alerts both         # voice · notify · both · off
heard pause               # and: heard resume
heard say "back in five"
heard status
heard doctor              # finds problems and tells you the fix
```

### Modes

| Mode | What you hear |
|---|---|
| **Co-pilot** (default) | Short updates while you're at the screen. Decisions and results get more detail. |
| **Companion** | Fuller briefings for when you're away from the screen, driving or cooking. |
| **Focus** | Only what needs you: approvals, blockers, failures and questions. |

### From inside Claude Code

After `heard install claude-code`, you can change Heard without leaving your
session:

```
/heard-mode focus
/heard-voice af_heart
/heard-pause
```

These commands are handled by Heard's own hook, so they don't reach the
model. Claude Code's `! heard mode focus` also works.

## Uninstall

```sh
heard uninstall all          # remove the hooks
./install.sh --uninstall     # also remove the binaries; add --purge for models and settings
```

## How it works

Claude Code and Codex call `heard-hook` on each tool call, prompt and stop.
The hook hands the event to the `heard` daemon over a Unix socket and exits
right away. The daemon starts automatically the first time it is needed. It
decides what is worth saying, shapes the line for speech, and speaks it with
Kokoro through ONNX Runtime. Nothing leaves your machine unless you choose
ElevenLabs by setting your own key with `heard config set elevenlabs_api_key`.

Everything is stored under `~/Library/Application Support/heard-cli/`.

## Develop

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The architecture and extension points are in `docs/CORE-API.md`.

## License

Apache-2.0. The voice model and other third-party components are listed in
`THIRD-PARTY-NOTICES.md`.
