# stardust-mail

A native Windows app (Rust + Tauri 2) that shows the mail of **several Gmail accounts read-only**. The main use case is opening the app, finding the newest authentication-code email, and reading it.

Sister project: `C:\Users\ziggy\Projects\icloud-notes` (same stack and same workflow). Its `PLAN.md` and `spikes\*\FINDINGS.md` are good references for patterns: DPAPI storage, the Tauri host layout, the sandboxed-iframe renderer, the vanilla UI, and wave-based parallel plans. **Copy code from it, don't depend on it by path.**

## Workflow
1. Opus: choose the stack and write spike briefs (`SPIKES.md`).
2. Sonnet agents run the spikes in parallel. Each spike lives in `spikes\sN-<name>\` and ends with a `FINDINGS.md`.
3. Opus: write `PLAN.md` (Wave 0 contracts, then parallel work packages).
4. Sonnet agents run the work packages in parallel, each in its own git worktree and branch.

## Hard rules
- **Read-only, by construction.** Request only the `https://www.googleapis.com/auth/gmail.readonly` scope (plus `openid email` to identify the account). Never call an endpoint that modifies anything: no `modify`, `trash`, `delete`, `send`, `insert`, `import`, `batchModify`, `labels.create/update/delete`, `watch`, `settings.*` writes, or drafts. Reading must not mark mail as read (the Gmail API never does this on `get`). The **only** non-GET call allowed is `POST https://oauth2.googleapis.com/revoke` on sign-out. It removes the app's access and doesn't touch mail.
- **Cache:** fetched rows and bodies are cached locally for offline reading, encrypted at rest (see S4). Signing out deletes an account's cache together with its token.
- **Local writes:** saving an attachment is allowed only as an explicit user action, through a save dialog that opens in the last folder used. Nothing is ever written to disk automatically except the encrypted cache, the token files and app settings.
- **Theme:** the app UI is dark. Email content always renders on white.
- **Agents:** write files with the Write/Edit tools, not shell heredocs. Windows Defender flags hostile-looking command lines (for example, test HTML with `<script>`).
- The build plan is `PLAN.md`. Spike evidence is in `spikes\*\FINDINGS.md`.
- **Secrets:** OAuth refresh tokens are stored only DPAPI-encrypted (`*.dpapi`, under `%LOCALAPPDATA%\<app-id>\`). The OAuth client file (`oauth-client.json` / `client_secret*.json`) is never committed or printed. Never log tokens, auth codes, or authorization headers.
- **Privacy in logs and reports:** never log subjects, senders, snippets, or bodies. Log ids, counts, sizes, label ids, and timings only. `spikes\*\out\` is git-ignored and may hold real data. Never commit it, and never quote its content in FINDINGS.md.
- **Test fixtures are synthetic.** Tests that touch real mail are `#[ignore]` and print stats only.
- Frontend: vanilla HTML/CSS/JS with no Node or npm. Build with cargo only.
- Toolchain: rustc 1.99, edition 2024, Tauri 2.12. Windows 11 + WebView2.

## Commands
- `cargo build` / `cargo test` / `cargo clippy -- -D warnings` inside each spike or crate.
- `cargo tauri dev` for Tauri spikes (`cargo-tauri` is installed).
