# Codenotch for Windows

A Windows port of [Codenotch](https://github.com/vinzdg/codenotch) — the usage notch that
sits on the edge of your screen and answers two questions at a glance:
**how much of my AI allowance is left**, and **is anything still working**.

Same design language as the macOS original (inverse-rounded pill, colour-graded rings,
hover card with per-window bars), rebuilt for Windows in Rust + Tauri 2 / WebView2.
No code is copied from the Swift app; the providers are reimplemented from their
documented behaviour and the wire formats.

## The two provider slots this build replaces

Upstream's `claude` and `grok` cells are gone, along with everything that existed only to
serve them: the Claude Code hooks, the local HTTP endpoint those hooks posted to, the
transcript watcher, the session state machine, the "Sign in" button and the automatic
token renewal. There is no `codenotch-hook.exe` to install any more, and no session list
on the hover card.

In their place:

| Cell | Source | How it reads it |
|---|---|---|
| **WorkBuddy** | Two planes, and the cell carries both. **Balance:** the WorkBuddy desktop app's own session, read only, never refreshed — `%LOCALAPPDATA%\CodeBuddyExtension\Data\Public\auth\workbuddy-desktop.info` (falling back to `%APPDATA%`), then `POST https://copilot.tencent.com/v2/billing/meter/get-user-resource`, or `/get-enterprise-user-usage` when the session carries an enterprise id. **Tokens:** the app's own session transcripts, read locally — `~/.workbuddy/projects/**/*.jsonl`, and `~/.workbuddy-ai/projects/**/*.jsonl` for the root 5.5 moved to. | The Credits still held across every active package, as one balance. Only `Status 0` rows count; a package whose quota cannot be read fails the whole reading rather than quietly understating it. A `.logged-out` marker in the canonical directory means signed out, and an unavailable session is never reported as one. WorkBuddy 5.6.0+ seals its credential with a key its own runtime holds: Codenotch cannot open that envelope and says so, and the token windows — which need no credential at all — take the cell over rather than sending you to a sign-in screen that cannot help. |
| **DeepSeek Harness** | Its own session transcripts under `$DSH_HOME` (or `~/.dsh`): `sessions/<project>/<session>/session[.<v>].jsonl[.zstd]` | Tokens spent today, in the last 7 days and in the last 30 days — a count with no denominator, so the ring draws its track undrawn and the cell prints the number. Zstandard transcripts are read without decompressing the whole file; a half-written tail frame is dropped rather than failing the read; forked sessions have their inherited prefix subtracted; replayed attempts are de-duplicated. Nothing leaves the machine and no credential is involved. |

Providers that are not installed simply do not get a cell. A config saved with the old
`claude` / `grok` slots is migrated to `workbuddy` / `dsh` on load, in place.

## What it shows

| Cell | Source | How it reads it |
|---|---|---|
| **Codex** | The local Codex sign-in in `~/.codex/auth.json` (read only, never refreshed), falling back to the newest session snapshot | Live primary/secondary windows (5h + weekly on paid plans, a monthly window on free) while Codex is signed in; Spark and Code review appear on the hover card when Codex reports them; otherwise the last snapshot, marked stale by its own timestamp. |
| **Cursor** | The editor's own session from `state.vscdb` → `cursor.com/api/usage-summary` | Included usage / API usage / on-demand, reset at billing-cycle end. Nothing to sign into: it borrows the editor's session, so there is only ever one account. |
| **OpenCode** | OpenCode's own sign-in, read only: the `opencode-go` key in `~/.local/share/opencode/auth.json` → `opencode.ai/zen/go/v1/usage`, or — since OpenCode 1.18 — the OAuth sign-in in `opencode.db` (`credential` table) → `opencode.ai/inference/go/v1/usage` | The Go plan's 5-hour, weekly and monthly windows. A sign-in without a Go plan shows "No OpenCode Go subscription" instead of a ring; Zen pay-as-you-go credit has no balance or usage API, so it is not shown. |
| **z.ai (GLM)** | The existing Z.AI tool credentials — the GLM Coding Plan key in the environment, in the CLI's config, or in a `~/.claude/settings.json` that points `ANTHROPIC_BASE_URL` at a Z.ai console | The plan's session / weekly windows. |
| **Antigravity** | Official `agy` CLI `/usage` print when installed; otherwise the existing local `language_server` bridge, Google Cloud Code API, or transcript model count | Official four quota rows (Gemini & Claude/GPT 5h/weekly) without running the full IDE. When CLI is absent, falls back to legacy local bridge/API. |

The working-state arc (the thin spinning line inside a ring, the amber pulse when something
wants your input, and the green pulse when a run has just finished) is drawn for every provider
whose state can be established from what it leaves on disk. Cursor and the DeepSeek Harness
*state* their state — Cursor in its composer rows, the harness through the `turn/start` /
`turn/end` pair it writes around every turn — so those two are read rather than inferred, and the
harness is the only one that can say "waiting on your approval" with certainty. Codex, WorkBuddy
and Antigravity are inferred from their transcripts: the last entry says which part of a turn they
stopped in, and how long the file has been quiet says whether they are still there. z.ai and
OpenCode have no state to read and simply show no arc.

Being seen and being fully described are two different questions, and each provider is drawn in
only the states its own files can settle. The green "just finished" ring lasts ten seconds, which
is what upstream gives it in all three places it produces the state — and it is only drawn when
something actually said the work finished: for the harness that is `turn/end` with
`reason.kind == "completed"` (an aborted, failed or token-limited turn also closes, and none of
those is a completion), and for Codex it is an explicit `task_complete` rather than the
`turn_aborted` sitting next to it. Cursor is read the same way upstream reads it, off the
conversation checkpoint. WorkBuddy is never drawn green, and that is a deliberate refusal rather
than a gap: its transcripts have no record that marks the end of a turn — the last record of a
finished turn is an ordinary assistant message, which is also exactly what mid-turn narration is
written as, 555 times out of 566 measured — so the two cannot be told apart until the file stops
moving, and a file that stopped moving is not evidence that anything finished.

### Codex quota recovery

The direct usage endpoint remains the first choice. If it fails, Codenotch can
ask an installed **native** `codex.exe` via the documented
[`account/rateLimits/read`](https://learn.chatgpt.com/docs/app-server#6-rate-limits-chatgpt)
app-server method before falling back to a rollout snapshot. The desktop's
`%LOCALAPPDATA%\OpenAI\Codex\bin` installation is checked as well as native CLI
candidates. No `.cmd`/Node wrapper is launched. The owned process is hidden,
limited to 20 seconds, and terminated/reaped after the read; no inference or
login command is sent. Existing HTTP 429 backoff and five-minute polling remain.

The main ring/tray selects only core `primary`, never a weekly, Spark or
code-review replacement. If `primary` is absent the headline stays blank;
`secondary` remains available to the separate weekly ring. App-server
multi-bucket replies prefer `codex`. Rollout fallback ignores explicitly different
bucket ids and, like macOS, reads the latest eight non-archived paths from
`state_5.sqlite` using a read-only, WAL-aware connection (50 ms busy timeout).
This finds resumed threads without scanning every session file. If the index
is unavailable, the original three-date-directory scan remains the fallback;
old resumed threads cannot be discovered through that scan alone. Missing data is
not a zero. Percentages retain the existing **used** semantics; this is quota
utilization, not an exact token count or a model-specific allowance.

Why launch a process at all? A borrowed stored-token HTTP read can fail while
the installed Codex client can still authenticate. The native client owns its
managed OAuth lifecycle and can recover live quotas without Codenotch copying
its refresh logic. This is not guaranteed for externally managed credentials
that require a host app: if it cannot read the quota, the usual stale/missing
rollout status remains. Unlike the old unconditional wrapper-based path, this
recovery runs only after HTTP failure, directly owns a native executable, and
does not use `taskkill` or launch a Node/cmd tree. Codenotch sends no login or
explicit token-refresh request; Codex may perform its own normal managed refresh.

Regression checks: `cargo test --locked` from `windows/codenotch/`, plus `node --test`
on each of `test-codex-headline.cjs`, `test-count-format.cjs`, `test-light-surface.cjs`,
`test-card-i18n-coverage.cjs` and `scripts/test-ko-i18n.cjs` from `windows/`. Tests use
synthetic quota fixtures, not account credentials.
The optional `cargo test --release --locked codex::tests::live_native_quota -- --ignored`
checks the actual native transport against an already signed-in local client;
it prints no account credentials or quota values and is not run by CI.

### WorkBuddy sessions

There is nothing to sign in to from Codenotch. The WorkBuddy desktop app owns the
session; Codenotch reads the file it writes and never refreshes, renews or rewrites
it, and the token never reaches a log line, an event payload or the UI. Sign in — or
out — in WorkBuddy itself, and the reading follows within a poll.

Two states are worth telling apart, and are:

- **Absent** — no session file and no logout marker, so WorkBuddy either is not
  installed or has never signed in. Clicking its cell offers the app's own page.
- **Unreadable** — the file is there but sealed (`5.6.0` and later encrypt each
  credential field with a key their runtime holds). That is reported as its own
  thing, never as "sign in again", because signing in again cannot change it.

Neither state costs the cell its number, because the balance is only one of the two
planes WorkBuddy is read through. The other is the app's own session transcripts,
totalled locally as tokens for today, the last seven days and the last thirty. They
need no credential at all, so a sealed session loses you the balance and nothing
else: the token windows lead the ring and the cell says why the balance is missing.

#### Reading the balance back, by hand

`Unreadable` is a real dead end rather than a prompt: the seal is symmetric and the key
never leaves the WorkBuddy process, so no amount of reading the file will open it. The
way through is to hand Codenotch a credential once. Turn WorkBuddy's cell on in
**Settings → Accounts** and a small row appears under it — paste a credential, save, and
the balance fills in. The token windows lead the ring either way, so the cell is useful
before and after.

##### Where a credential comes from

This is the awkward part, because WorkBuddy does not offer a supported way to look at its
own token, and two obvious routes are closed: the desktop build ships with its developer
tools disabled (the only `openDevTools` calls in it belong to embedded third-party web
views and are development-only), and the web build authenticates with a cookie rather than
a bearer token, so a browser's network panel has nothing to copy. That leaves:

- **A plaintext session left behind by an older build.** Before `5.6.0` the app wrote the
  same session file unsealed, and upgrading does not delete what it wrote. Any
  `%LOCALAPPDATA%\CodeBuddyExtension\Data\Public\auth\workbuddy-desktop*.info` whose
  `auth.accessToken` is a string rather than a `{$wbEncrypted: 1, …}` object still holds a
  usable one: copy that field's value. The `auth.expiresAt` beside it (ms epoch) says how
  long it lasts; those sessions were issued for roughly 55 days. It pays to check it is the
  account you think it is — `account.uid` is in the same file, and a machine that has held
  more than one sign-in leaves a file per account, named after the moment it was written.
- **Capture one request.** Otherwise the token has to be read off the wire: a debugging
  proxy whose root certificate is trusted (Fiddler, Charles, mitmproxy), then copy the
  `Authorization` header from any
  `POST https://copilot.tencent.com/v2/billing/meter/get-user-resource` the app makes while
  the account page is open.

#### The request contract

The gateway in front of the billing endpoint screens on `User-Agent` **before it looks at
the token**, and refuses a request it does not recognise with `403 {"code":10085}` — plain
"请求不合法", naming neither the header nor the reason, which reads exactly like a bad
credential and sends you back to re-paste a token that was never the problem. Measured
against the live endpoint with one working credential: no header, an empty one, `ureq/…`
and `python-requests/…` were all refused, while `Mozilla/5.0`, `curl/8.0`, `CodeBuddy/1.0`
and a full browser string all reached the balance. ureq's own default sits in the refused
set, so the header is set explicitly rather than left to the client.

The rest of the contract:

- The credential is written to `%APPDATA%\codenotch\workbuddy-credential.json`, in your
  own profile, and is sent to nothing but `copilot.tencent.com`. The enterprise and user
  ids are optional; supply them when the session is an enterprise one.
- `Bearer ` and surrounding whitespace are stripped, so pasting either the whole header
  value or just the token works.
- A pasted credential is treated as having no declared expiry: it is used until the
  endpoint refuses it, and then the cell says the credential was refused and to paste a
  fresh one — it will not tell you to sign in again, which was the thing that could not
  help.
- Typing it in is the only way in. There is no import from the sealed file, no
  clipboard sniffing, and **Remove** deletes the file outright.
- A pasted credential outranks the app's own session file. If the balance still will not
  read, the credential is the one being refused, and the old session is not quietly
  consulted behind your back.

The counts are the tokens WorkBuddy actually had to process — per call,
`(input − cached input) + output`. `input` is the whole context that call re-sent, so
adding `total_tokens` up would charge the same history once per call and inflate a
long session by two orders of magnitude. A transcript is folded once and cached
against its size and mtime, and a file last written before the window is skipped
without being opened, because records are appended and can never be newer than the
file that holds them.

### DeepSeek Harness sessions

Nothing to sign in to, and for the transcripts nothing to configure either. Its sessions live under
`$DSH_HOME`, or `~/.dsh` when that is unset — one directory per project, one per session inside it,
holding `session.jsonl`, or `session.v3.jsonl` when a generation has rewritten the format, with a
`.zstd` suffix where the writer compresses. Only those names are read: a `session.summary.jsonl` or
a `.bak` beside them is not a transcript.

The three windows are counts, not shares: tokens today, in the last seven days and in the
last thirty. There is no allowance to divide by, so the ring draws its track undrawn and the
cell prints the figure. Records older than thirty-one days are dropped on the way in, so a
session running since spring costs one pass over the file rather than a growing sum.

Four details of the format change the number, so they are worth stating:

- **A transcript is a concatenation of frames, not one archive.** The writer appends a whole
  zstd frame per flush, so a live `session.v3.jsonl.zstd` is dozens of frames laid end to end.
  Frame boundaries are located by walking the framing without decompressing, so every frame is
  read and a torn last frame — the normal state of a session being written right now — is
  skipped instead of ending the parse. A reader that decodes the file as a single frame gets
  the session header back and nothing else.
- **Reasoning is inside output, not beside it.** DSH reports `outputTokens` inclusive of
  reasoning, so subtracting reasoning would under-count every reasoning-heavy session by
  exactly its reasoning tokens. The total is `input + output + cacheRead + cacheWrite`.
- **A replayed record is not a second charge.** The writer can re-append a record it already
  flushed. Records are de-duplicated on message identity, time, routing and the token
  signature, so a replay folds to one charge.
- **A forked session is not charged for its parent.** A fork's log opens with a copy of the
  parent's events, and the log says where the copy ends: a legacy header carries
  `seedLength`, a current one carries `isSeeded` and puts the cut on the *last*
  `session/end-seed` whose data says `inherited: true`. Either way the cut is a seq number —
  the first event the child itself owns — and everything strictly below it is dropped. Two
  cases look alike and are not: a seeded log with no end-seed marker anywhere cannot say
  where its copy ended, and that session is charged nothing rather than billing the parent's
  prefix to the child; a marker that is present but untagged has declared outright that
  nothing was inherited, so its history stands and is counted.

A transcript the writer is midway through looks truncated, and that is normal: the harness
appends one Zstandard frame per flush, so frame boundaries are located without decompressing
the file and only the half-written frame at the cut is dropped.

#### The balance, which is not an estimate

`GET /user/balance` is the one vendor figure this app states rather than works out. Unlike
WorkBuddy's it needs no ceremony to reach: the key is a DeepSeek platform key, nothing seals it,
and the harness keeps it in the clear in `$DSH_HOME/.credentials.yaml` under `refs.DEEPSEEK_API_KEY`
— so on a machine that has run the harness there is nothing to paste. Where there is no harness, the
key field under **Settings → Accounts → DeepSeek Harness** takes one, and a key can be minted on the
platform in a minute.

The key is looked for in this order, and a pasted one wins:

1. `%APPDATA%\codenotch\deepseek-credential.json` — a pasted key, written by this app and sent
   nowhere but `api.deepseek.com`. Its own file rather than a field in `config.json`, for the same
   reason WorkBuddy's credential has one.
2. `DEEPSEEK_API_KEY` in the environment.
3. `refs.DEEPSEEK_API_KEY` in the harness's own credentials file.

A pasted key outranking the others is deliberate. It is the one the person chose, and if the
endpoint is refusing it, falling back quietly would report a balance from an account they did not
mean. The card says the key was refused instead.

**Why there is a top-up figure as well, and why it is optional.** The endpoint publishes what is
left and says nothing about what was put in, so a remainder on its own contains no percentage — and
a `0 %` ring over a full account is a picture of something untrue rather than a missing picture. The
card therefore prints the remainder as a figure and draws no ring at all, and *Paid in so far* is
what turns it into a fraction: supply it and the ring means "used this much of what you have paid
in"; leave it blank and the figure stands alone. It is deliberately not inferred from anything,
because there is no transaction history to read and a guessed denominator would put a
confident-looking arc under a number nobody could check.

Two details worth knowing:

- **The endpoint quotes its balances as strings** (`"total_balance": "40.29"`). That is its own
  documented shape rather than a mistake to route around. A document with no currency in it at all
  is reported as unreadable instead of becoming a `¥0`.
- **An account holding several currencies shows CNY**, falling back to the first entry. CNY is the
  account's home currency and the one the rate card is quoted in.

A request that fails keeps the last figure that was read rather than emptying the row: the token
counts beside it are local and always answer, so a balance blinking out would read as the account
having been drained rather than as one call having failed.

#### What those tokens came to

DeepSeek publishes no usage or billing endpoint — its API reference offers `GET /user/balance`
and `GET /models` and nothing else — so there is no way to ask what *a month cost*. What is left on
the account is a different question, and that one is answered exactly, above. So the figure here is
**our own estimate, and says so**: the tokens already counted above, multiplied by DeepSeek's
published rate card, converted to CNY. The 30-day total appears in full under the count rows on the
hover card, broken into cache-hit input, cache-miss input and output, with the three token counts
beside their own share.

The rate card is read from the vendor's *Models & Pricing* page and comes in four rows, in CNY
per million tokens:

| Model | | Cache hit | Cache miss | Output |
|---|---|---|---|---|
| Flash | off-peak | 0.02 | 1.00 | 4.00 |
| Flash | peak | 0.04 | 2.00 | 8.00 |
| Pro | off-peak | 0.15 | 4.50 | 13.50 |
| Pro | peak | 0.30 | 9.00 | 27.00 |

- **The three rates are never blended.** The vendor charges fifty times more for a cache-miss
  input token than a cache-hit one and four times more again for an output token, so a single
  average rate would be wrong for every session — too high for a well-cached one, too low for a
  cold one. Each kind of token is priced on its own row and the shares are added.
- **Peak is decided per event, not per day.** Peak is Beijing time, Monday to Friday,
  09:00–12:00 and 14:00–18:00; every other hour is off-peak at half price. A day's tokens
  therefore cannot be summed first and priced once — the fold splits each day's usage into a
  peak and an off-peak charge as it goes.
- **Public holidays are read as ordinary weekdays**, because which days are holidays is
  published afresh each year and a table baked into this build is guaranteed to expire. The
  error is one-directional: a holiday read as a peak weekday overstates that day's cost, and
  never understates it. Overstating is the safe direction for an estimate.
- **An unpriced model is named, not guessed at.** A model with no rate card here is dropped
  from every amount — never priced off a sibling's card, which would produce a number that looks
  authoritative and is not the one the vendor would bill — and the card lists it under
  *Not priced:* so the omission is visible rather than silent. The retired aliases
  `deepseek-v4-flash` and `deepseek-v4-flash-vision-exp` are served as Flash and billed at Flash
  rates, so they share its card; a record with no model name at all is reported as
  `(unlabelled)`.
- The estimate covers the last 30 days, matching the window named on the row above it, and is
  flagged `estimated` all the way through to the card.

For reference, this machine's own 30 days — about 3.57 M tokens at a 95 % cache-hit rate —
comes to roughly **¥0.4**. A cache-hit-heavy session run during off-peak hours is cheap; a cold
one at peak prices is not.

### Antigravity

- **Official CLI (Preferred)**: When the official Antigravity CLI (`agy.exe`) is installed (`%LOCALAPPDATA%\agy\bin\agy.exe` or on `PATH`) and signed in, Codenotch reads official quotas directly without keeping the full IDE running.
- **Execution**: Runs the official CLI in a hidden Windows pseudo-console, with a 70-second timeout and cleanup of its process tree. It does not need PowerShell scripts or a separate service.
- **Refresh**: Checks at startup and on hover/explicit request when readings are at least five minutes old; failed attempts are also limited to once per five minutes. It keeps previous readings on failure, without switching to legacy APIs. The CLI is not launched periodically while idle.
- **Fallback**: When the official CLI is not installed, Codenotch preserves the legacy local bridge (`language_server`), Credential Manager, and transcript model turn counting to maintain compatibility with existing installations.
- **Official CLI Reference**: Standalone `/usage` printing is described in the [official Antigravity CLI documentation](https://www.antigravity.google/docs/cli/headless). Note: no categorical Terms of Service guarantee is made.

Restart Codenotch after installing or removing `agy`: the source is selected at startup.
The CLI's text report is parsed defensively; an unsupported format or failed sign-in
shows an error or the last reading marked stale. Codenotch does not automate sign-in.

## Install / build

Download [`Codenotch-Setup.exe`](https://github.com/vinzdg/codenotch/releases/latest/download/Codenotch-Setup.exe)
from the latest upstream release. It installs for the current user without administrator rights and fetches
WebView2 if Windows does not already have it. The installer is not code-signed, so SmartScreen
stops it the first time with *Windows protected your PC*: choose **More info**, then **Run anyway**.

### Building this fork

This fork publishes no releases, so the download link above does not carry its installer.
Both Windows workflows build on any push touching `windows/**`, and either can be started by
hand with **Run workflow** on the Actions tab. The manual entry point exists because
`tauri build` compiles the binary only and never the `#[cfg(test)]` targets, so the unit
tests would otherwise never run. The packaging job leaves `Codenotch-Setup.exe` on the run
as an artifact.

### Updates

Codenotch looks for a newer release about twenty seconds after it starts, and again whenever
**Check for updates** is pressed in Settings → General. The feed is `latest.json` on the newest
release, written by the Windows Package workflow beside the installer it describes, so publishing
a release is the whole of shipping an update.

Nothing about this nags. A check that fails — no network, an unreachable feed — leaves the app
as it was and says so only next to the version. There is no dialogue and no badge.

The download is a minisign-signed archive, and the signature is checked against the public key in
`tauri.conf.json` before anything is run. This is what stands in for code signing here: the
installer itself is unsigned, so SmartScreen still warns on a first manual install, but an update
delivered to an already-installed copy is verified.

Before the first signed release, the key has to exist:

```powershell
npx --yes @tauri-apps/cli@2.11.4 signer generate -w $env:USERPROFILE\.tauri\codenotch.key
```

Put the **private** key in the repository secret `TAURI_SIGNING_PRIVATE_KEY` and its password in
`TAURI_SIGNING_PRIVATE_KEY_PASSWORD`, and paste the **public** key into `plugins.updater.pubkey`
in `codenotch/tauri.conf.json`, replacing `REPLACE_WITH_TAURI_PUBLIC_KEY`. Until that is done the
app skips the check entirely rather than reporting a failure nobody can act on; the packaging job
builds an ordinary installer and warns that it made no feed, and a `v*` release fails loudly rather
than going out with an update path nobody can use.

Keep the private key. Losing it means no installed copy can be updated again, because every one of
them checks against the public key it shipped with — they would all have to reinstall by hand.

To build from source instead — prerequisites: Rust (MSVC toolchain), WebView2 runtime (ships with Windows 11).

```powershell
# from this directory (the repo root here; `windows/` inside the upstream repo)
cargo build --release
.\target\release\codenotch.exe          # pill appears on the right edge of the primary monitor
.\target\release\codenotch.exe doctor   # self-diagnosis: config, credentials, data sources, icons
```

To build the installer the way the Windows Package workflow does:

```powershell
cd codenotch
npx @tauri-apps/cli@2 build
# → ..\target\release\bundle\nsis\Codenotch_<version>_x64-setup.exe
```

Tray menu: the readings themselves — a line per provider with its headline figure, and under it
one line per limit window — then **Refresh all**, **Settings…** and **Quit Codenotch**. Clicking a
provider's line re-reads that provider. Everything else is in the settings window: which rings the
notch shows, its size, the weekly ring, which screen edge it sits on and which screen,
start with Windows, the language, reset
position, and the data folder (`%APPDATA%\codenotch` — logs, persisted readings, icon overrides).

Notch: clicking a ring re-reads that provider, as on the Mac. Right-clicking the notch or its card
offers **Refresh now**, the provider's usage page (**Open codebuddy.ai**, **Open chatgpt.com**, …)
and **Quit Codenotch**. A provider that is inside its own rate-limit wait keeps it: asking early
would spend a request and double the wait.

### Where the notch sits

The notch pins to one edge of one screen. The arc above the pill carries it: hold it, and the four
places it can go are outlined on the screen; release on one and the notch lands there, centred.
**Appearance → Show move handle** hides that arc. **Appearance → Edge** picks left, right, top or bottom:
it stands upright on the left and right edges with the hover card opening sideways, and lies flat
on the top and bottom ones with the card opening below or above. **Appearance → Screen** appears
once more than one monitor is attached.

Dragging does both at once: pick the pill up, drop it anywhere, and it snaps to the nearest edge
of the screen it was dropped on — across monitors, and across a change of DPI between them. The
choice is stored as `notch_edge`, `notch_monitor` (the device name, e.g. `\\.\DISPLAY2`) and
`notch_y` (the position along the edge, 0–1) in `config.json`. A monitor that is no longer
attached falls back to the primary one, so unplugging a screen cannot strand the notch off-screen;
**Recentre** centres it on the edge it is on, or on the primary screen's right-hand edge when the screen it was on is gone.

Folded (**Appearance → Show → Show on hover**), the notch rests as a small pill at the edge, in
**Theme**'s colour, with an edge that shows even against a backdrop of that colour.
**Appearance → Adaptive pill**, off unless switched on, makes it follow what is behind it instead:
light over a dark backdrop, black over a light one, the way the iPhone's home indicator does. To tell
which, Codenotch reads a thin strip of the screen beside the pill twice a second while it is folded,
and keeps only its average brightness, which is never stored or sent. With the switch off, the notch
open, or Show set to Always show, nothing is read.

### Icons

Provider marks are the SVGs from [`@lobehub/icons-static-svg`](https://github.com/lobehub/lobe-icons)
(MIT), embedded unmodified — see `codenotch/glyphs/NOTICE.md`. Drop your own
`workbuddy|codex|cursor|dsh|gemini|opencode.svg` (or `.png`) into `%APPDATA%\codenotch\glyphs\`
to override. The marks remain the trademarks of their owners.

### Translations

Three surfaces draw their own text, so each keeps its own table:

| Surface | Table | Languages today |
|---|---|---|
| Tray menu | `codenotch/src/i18n.rs` (`tr`), `codenotch/src/traymenu.rs` (`label`) | en · ru · zh · ja · ko · uk · pt-BR |
| Hover card | `codenotch/ui/notch.html` (`TEXT`, `PATTERNS`, `UI`) | en · ru · zh · zh-Hant · ko · uk · pt-BR |
| Settings window | `codenotch/ui/settings.html` (`STATIC_TEXT`, `STATUS_TEXT`) | en · ru · zh · ja · ko · uk · pt-BR |

Help is welcome on the gaps, which fall back to English rather than breaking anything:

- the hover card has no Japanese;
- the provider notes — the sentences `workbuddy.rs` and `dsh.rs` put on the card — are translated
  into the two Chinese variants only, so another language shows them in English. Each one is a whole
  sentence, so it arrives through `PATTERNS` rather than `TEXT`.

Keys are the exact English string. A string the Mac also shows should be taken from
`Sources/Localizable.xcstrings` rather than translated afresh, so both platforms word it the same
way. One catalog feeding all three tables is the intended fix; until then a test in `traymenu.rs`
fails if the menu and the card stop naming the same window.

## Layout

```
.
├── codenotch/          the Windows app (pill, hover card, settings, providers)
│   ├── src/            Rust: one module per provider, plus the window, tray and settings plumbing
│   ├── ui/             the three pages (notch, settings, dropzones)
│   └── glyphs/         the provider marks compiled into the exe
└── scripts/            Node checks that run over the pages (cargo never reads them)
```

A pull request that touches this tree is built and tested; the check is skipped
inside forks until the pull request is opened here.

## Relationship to upstream

This port follows the upstream design and provider semantics. It is developed at
[Im-Midi/codenotch-windows](https://github.com/Im-Midi/codenotch-windows) and offered to the
upstream project as its `windows/` tree; the two are kept in sync — apart from the two provider
slots above, which this build rewrites for WorkBuddy and DeepSeek Harness.

The WorkBuddy and DeepSeek Harness adapters are ported from
[Token Monitor](https://github.com/Javis603/token-monitor) (MIT): the WorkBuddy billing client
follows its `src/shared/providers/workbuddy/` and `src/electron/providers/workbuddy/localAuth.js`,
its token plane follows the `workbuddy` source roots in `src/shared/clientSources.js` (read
directly rather than through tokscale), and the DeepSeek Harness transcript reader follows its
`src/shared/providers/dsh/`.

## License

MIT — see `LICENSE`. The Codenotch design and name belong to the upstream author.
