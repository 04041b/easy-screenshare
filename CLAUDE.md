# Claude Code conventions for this repo

This file orients Claude Code in this project. It complements `AGENTS.md` (architecture, debugging recipes, bug history) and `README.md` (user-facing intro).

## Read these first

- `AGENTS.md` — architecture in one paragraph, debugging recipes, bug history, gotchas, known open issues.
- `README.md` — user-facing description, quality presets, platform matrix.

When asked to fix a bug, scan the "Bugs resolved" section in `AGENTS.md` first — there's a good chance the symptom matches something already debugged.

## Working directory

The cargo project root is `client/`, not the repo root. Most cargo commands need to be run from there:

```bash
cd client
cargo check --release         # not from screenshare/ — there's no top-level Cargo.toml
```

For backend work:
```bash
cd backend
npx wrangler tail --format pretty   # live logs from the deployed Worker
npx wrangler deploy                  # production deploy
npm run typecheck                    # tsc --noEmit
npm run db:migrate                   # apply backend/migrations/ to the remote D1
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

Always include a `Co-Authored-By:` trailer naming the Claude model that wrote the commit, e.g.:
```
Co-Authored-By: Claude <noreply@anthropic.com>
```

User flow for shipping changes during a session:
1. Land code, `cargo check --release` clean.
2. Commit (one logical change per commit if it splits cleanly).
3. `git push origin main` — triggers CI build for the Windows artifact.
4. If backend changed, `cd backend && npx wrangler deploy`.
5. `gh run watch <id> --exit-status` in the background; surface the conclusion when done.

The user generally wants both client push *and* backend deploy when relay code changes — both ends need to move.

## Pinned things, don't bump without asking

- `screencapturekit = "1.5"` — 3.x+ pulls `apple-metal`, which needs the macOS 26 SDK and doesn't build on the macOS 14 CI runner. See the comment in `client/Cargo.toml`.
- `vpx-encode = "0.5"` — sender.rs reaches through its struct layout to call `vpx_codec_control_(VP8E_SET_CPUUSED, 8)`. Changing the version may break that.
- `webrtc = "0.11"` — its transitive `webrtc-dtls = "0.10"` is overridden by `client/vendor/webrtc-dtls/` (verbatim 0.10.0 + one ported curve-selection fix from 0.12.0). See AGENTS.md § Gotchas → "Vendored `webrtc-dtls`". When bumping `webrtc`, check whether the new release pulls `webrtc-dtls >= 0.12` and drop the vendor if so.

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
