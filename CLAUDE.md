# Claude Code conventions for this repo

This file orients Claude Code in this project. It complements `AGENTS.md` (architecture, debugging recipes, bug history) and `README.md` (user-facing intro).

## Read these first

- `AGENTS.md` — architecture in one paragraph, debugging recipes, bug history, gotchas, known open issues.
- `README.md` — user-facing description, quality presets, platform matrix.

When asked to fix a bug, scan the "Bugs resolved" section in `AGENTS.md` first — there's a good chance the symptom matches something already debugged.

## Working directory

The cargo project root is `client/`, not the repo root. Most cargo commands need to be run from there:

```bash
cd /Users/planet/claude/code/screenshare/client
cargo check --release         # not from screenshare/ — there's no top-level Cargo.toml
```

For backend work:
```bash
cd /Users/planet/claude/code/screenshare/backend
npx wrangler tail --format pretty   # live logs from the deployed Worker
npx wrangler deploy                  # production deploy
```

## What CI does (and how to wait on it)

`.github/workflows/build.yml` builds `macos-arm64` and `windows-x64` release binaries on every push to `main`. Artifacts named `screenshare-windows-x64` / `screenshare-macos-arm64`, 14-day retention. Typical run: 3-4 minutes.

```bash
gh run list --branch main --limit 3
gh run watch <id> --interval 25 --exit-status   # use run_in_background:true so you're notified when it finishes
gh run view <id> --json conclusion,status,displayTitle
```

`cargo check --release` from macOS only catches macOS errors; Windows-only code (anything `#[cfg(target_os = "windows")]`, `wasapi`, `windows-sys`) only gets checked in CI. **Always push and watch CI** after touching Windows-specific code.

## Commit + deploy rhythm

Single-purpose commits with a short imperative subject and a few-sentence body explaining the *why*. Existing log examples:

- `Relay viewer: drop WebRTC connect timeout from 15s to 4s`
- `Client: AIMD bandwidth estimator + frame pacing on relay path`
- `Relay: keyframe on viewer join; viewer gates decode`

Always include this trailer:
```
Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
```

User flow for shipping changes during a session:
1. Land code, `cargo check --release` clean.
2. Commit (one logical change per commit if it splits cleanly).
3. `git push origin main` — triggers CI build for the Windows artifact.
4. If backend changed, `cd backend && npx wrangler deploy`.
5. `gh run watch <id> --exit-status` in the background; surface the conclusion when done.

The user generally wants both client push *and* backend deploy when relay code changes — both ends need to move.

## Pinned things, don't bump without asking

- `windows-capture = "=1.4.4"` — scap 0.0.8's Windows backend straddles two API eras of this crate; 1.4.4 is the only one that fits.
- `vpx-encode = "0.5"` — sender.rs reaches through its struct layout to call `vpx_codec_control_(VP8E_SET_CPUUSED, 8)`. Changing the version may break that.
- `webrtc = "0.11"` — known DTLS 1.3 issue against modern browsers; upgrade is the long-term fix but is a large change.

## Sandbox limitations

- Cannot run the client on macOS without Screen Recording permission, which rebuilds invalidate. Use `cargo run --release -- probe` to check capture state.
- Cannot run the Windows binary at all. End-to-end Windows verification has to come from the user.
- Wrangler deploy needs the user's CF credentials, which are stored in `.wrangler/`. Don't commit them.

## When in doubt

- Architecture or design question → re-read `AGENTS.md` § "Architecture in one paragraph" and § "Key client files".
- "Why is X broken" → grep `AGENTS.md` "Bugs resolved" for the symptom.
- "What does Y do" → read the relevant module; comments lead with *why*, not what.
- Anything platform-specific → check the platform matrix in `README.md`, then the `#[cfg(...)]` blocks in capture/audio.

Don't add new architectural docs files. Prefer extending `AGENTS.md` over creating a new `ARCHITECTURE.md` or similar; this keeps the doc surface small and the cross-links predictable.
