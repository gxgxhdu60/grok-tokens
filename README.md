# grok-tokens

**Grok Build token usage CLI** — accurate per-turn totals from local session logs.

Native **Rust** binary. musl Linux builds avoid glibc issues.  
Inspired by the install UX of [grok-usage](https://github.com/simnova/grok-usage).

`grok_tokens.py` is **deprecated** (no new features; will be removed). Use the Rust binary.

---

## Install

No Rust, no git clone. Installers download a native binary from GitHub Releases.

**npm** (Win11 / Linux / macOS — if Node is already installed):

```bash
npm install -g grok-tokens
grok-tokens --version
grok-tokens daily
```

Before the package is on npmjs, install from GitHub (same `postinstall`, picks the matching Release binary):

```bash
npm install -g github:gxgxhdu60/grok-tokens
```

`postinstall` detects `win32` / `linux` / `darwin` + `x64` / `arm64` and downloads `grok-tokens-<target>.tar.gz`. Pin a tag with `GROK_TOKENS_TAG=v0.1.2`.

**Linux / macOS / WSL (curl):**

```bash
curl -fsSL https://github.com/gxgxhdu60/grok-tokens/releases/latest/download/install.sh | sh
```

If `raw.githubusercontent.com` works better on that machine:

```bash
curl -fsSL https://cdn.jsdelivr.net/gh/gxgxhdu60/grok-tokens@main/install.sh | sh
```

**Windows (PowerShell):**

```powershell
irm https://github.com/gxgxhdu60/grok-tokens/releases/latest/download/install.ps1 | iex
```

Installs to `~/.local/bin/grok-tokens` (Unix) or `%LOCALAPPDATA%\grok-tokens\grok-tokens.exe` (Windows). The script retries public GitHub mirrors if github.com is slow.

```bash
export PATH="$HOME/.local/bin:$PATH"
grok-tokens --version
grok-tokens daily
```

Env vars must be on the **right** side of the pipe:

```bash
curl -fsSL https://github.com/gxgxhdu60/grok-tokens/releases/latest/download/install.sh \
  | GROK_TOKENS_REPO=yourname/grok-tokens sh
```

### What the installer does

1. **`npm install -g`** → `postinstall` downloads the matching GitHub Release tarball  
2. **`curl | sh` / `irm | iex`** → GitHub Release asset for this OS/arch  
   - Linux: **`x86_64-unknown-linux-musl`** / `aarch64-unknown-linux-musl` (gnu fallback)
   - macOS: `aarch64-apple-darwin` / `x86_64-apple-darwin`
   - Windows: `x86_64-pc-windows-msvc`
3. **`./install.sh` in a clone** → `cargo build --release` (or existing `target/release`)

Force a Release download from a clone: `GROK_TOKENS_FORCE_DOWNLOAD=1 ./install.sh`

### Manual

Download a tarball from [Releases](https://github.com/gxgxhdu60/grok-tokens/releases/latest) and copy `grok-tokens` onto `PATH`.

```bash
# Rust (from source)
cargo install --git https://github.com/gxgxhdu60/grok-tokens --locked

# Or clone
git clone https://github.com/gxgxhdu60/grok-tokens.git
cd grok-tokens
cargo build --release
./install.sh
```

---

## Usage

```bash
grok-tokens daily
grok-tokens daily --cwd /path/to/project
grok-tokens daily --since 2026-07-27 -v
grok-tokens --since 20260727 daily
grok-tokens daily --json

grok-tokens session --usage-only
grok-tokens session --sort recent

# Account quota (same as Grok CLI /usage "Weekly limit: 24%")
grok-tokens limit

# Local login profiles (Grok CLI itself has no multi-account switch)
grok-tokens account whoami
grok-tokens account save              # name = email local-part
grok-tokens account save work
grok-tokens account list
grok-tokens account switch work
```

Account limit is read from `~/.grok/logs/unified.jsonl` (`billing: fetched credits config`), which the Grok CLI refreshes while sessions run. It is **not** derived from local token sums.

`daily` / `session` / `limit` label that quota with the current `auth.json` email (and profile name, if saved). With multiple saved profiles, they list each account’s last-known limit. Local token totals stay machine-wide — session logs are not tagged by account.

### Account profiles

Grok stores a single login in `~/.grok/auth.json`. `account` snapshots that file as named profiles (like `gcloud auth` / AWS profiles):

| Command | Action |
|---------|--------|
| `account whoami` | Current `email` / `user_id` |
| `account save [name]` | Copy live `auth.json` to a profile |
| `account list` | Saved profiles (`*` = matches live login) |
| `account switch <name>` | Atomically replace `auth.json` |
| `account remove <name>` | Delete a snapshot (does not log out) |
| `account export [name] -o FILE` | Write a portable file (auth + metadata) |
| `account import FILE [--name] [--no-switch]` | Load that file; default also replaces live `auth.json` |

Profiles: `$GROK_TOKENS_PROFILES` → `$XDG_DATA_HOME/grok-tokens/accounts` → `~/.local/share/grok-tokens/accounts`. Auth file: `$GROK_AUTH_PATH` → `$GROK_HOME/auth.json` → `~/.grok/auth.json`.

`switch` first writes the live login back to the matching profile (so refreshed tokens are not lost). Grok picks up the new file on the next API call; restart a running session if it does not.

Do **not** copy profile directories or export files into git or chat — they contain refresh tokens.

### WSL ↔ Windows (no re-login)

Export on the side that is already signed in, copy the file, import on the other. Same file also works as raw `auth.json`.

```bash
# In WSL (already logged in)
grok-tokens account export -o /mnt/c/Users/YOU/grok-account.json

# Point at the Windows Grok home and import (still from WSL — no Win32 binary needed)
GROK_HOME="/mnt/c/Users/YOU/.grok" grok-tokens account import /mnt/c/Users/YOU/grok-account.json
```

Or copy the file to Windows and import with the native binary (`npm install -g grok-tokens` or `install.ps1`):

```powershell
grok-tokens account import $env:USERPROFILE\grok-account.json
```

Grok CLI on that side picks up `auth.json` on the next API call. Refresh tokens expire; if import is rejected, run `grok login` once on that side.


| Flag | Description |
|------|-------------|
| `--cwd PATH` | Filter by project directory |
| `--since DATE` | On/after this UTC date (`YYYY-MM-DD` or `YYYYMMDD`) |
| `--limit N` | Max sessions scanned (default 200) |
| `--root DIR` | Sessions root override |
| `--json` | Machine-readable |
| `--usage-only` | Hide empty sessions |
| `-v` | daily: cache Saved$ · session: project path |
| `--no-color` | Disable colors |

Data: `$GROK_DATA_DIR` → `$GROK_HOME/sessions` → `~/.grok/sessions`.

---

## Columns

| Column | Meaning |
|--------|---------|
| **Input** | Σ `inputTokens` |
| **Cache** | Σ `cachedReadTokens` (⊂ Input) |
| **Hit%** | Cache / Input |
| **Fresh** | Input − Cache |
| **Output** | Σ `outputTokens` |
| **Total** | ≈ Input + Output |
| **NoCache** | Fresh + Output |
| **Billing** | `costUsdTicks / 1e10` — CLI / SuperGrok receipt |
| **API$** | Public list price on that row's token totals (200k 2× only when `modelCalls ≤ 1`) |
| **Saved** (`-v`) | API$ vs billing all input at the full input rate |

```text
fresh    = input - cachedRead
Billing  = costUsdTicks / 1e10
API$     = fresh×input_rate + cached×cached_rate + output×output_rate
```

A `turn_completed` row sums every API request in the turn, so API$ cannot place the 200k cliff per request. Rates follow [xAI pricing](https://docs.x.ai/developers/pricing).

---

## Why Rust

**musl** Linux binaries are static-friendly — avoids the “prebuilt needs GLIBC 2.3x” trap. One executable, no interpreter.

---

## Publish (maintainers)

```bash
# bump version in Cargo.toml and package.json
git tag v0.1.2
git push origin v0.1.2
# Actions builds linux/mac/windows tarballs and attaches install.sh + install.ps1
npm publish --access public   # optional; needs npm login
```

---

## Development

```bash
cargo run -- daily --no-color
cargo run -- session --usage-only
cargo build --release
./install.sh
npm run test:npm
```

```text
src/main.rs           # Rust CLI
install.sh            # Unix installer (curl | sh)
install.ps1           # Windows installer (irm | iex)
npm/                  # npm wrapper (downloads the matching Release binary)
.github/workflows/    # CI + multi-target release
grok_tokens.py        # deprecated; do not extend
```

## License

MIT — see [LICENSE](LICENSE).
