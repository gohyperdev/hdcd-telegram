# hdcd-telegram

Rust drop-in replacement for [`telegram@claude-plugins-official`](https://github.com/anthropics/claude-plugins-official/tree/main/external_plugins/telegram) -- the official Bun-based Telegram channel plugin for [Claude Code](https://code.claude.com).

3.5 MB static binary. ~5 MB RAM. <50 ms startup. Full feature parity. Zero migration needed.

## Why Rust?

The official plugin runs on [Bun](https://bun.sh). Fine for one agent, breaks down at scale:

| | Bun (official plugin) | Rust (this repo) |
|---|---|---|
| **RAM per instance** | ~100 MB (JS runtime + dependencies) | ~5 MB |
| **10 parallel agents** | ~1 GB just for Telegram bridges | ~50 MB |
| **Startup time** | 2-3s (`bun install` + boot) | <50 ms |
| **Zombie problem** | Bot keeps polling after session ends, next session gets [409 Conflict](https://core.telegram.org/bots/api#getting-updates) | Immediate shutdown on stdin EOF |
| **Dependencies** | Bun + node_modules (~30 MB per plugin) | Single static binary |

If you run a fleet of Claude Code agents (CI workers, parallel dev agents, orchestrator setups), each agent spawns its own MCP subprocess. With Bun, 10 agents = 10 Bun runtimes. With Rust, 10 agents = 10 lightweight processes that start instantly and die cleanly.

## Known issue: `--channels plugin:` is broken

The official plugin activation path has [multiple open bugs](https://github.com/anthropics/claude-code/issues?q=is%3Aissue+is%3Aopen+channels+inbound) in Claude Code:

- [#36503](https://github.com/anthropics/claude-code/issues/36503) -- "Channels not available", inbound ignored
- [#36477](https://github.com/anthropics/claude-code/issues/36477) -- Session stops processing after first response
- [#38259](https://github.com/anthropics/claude-code/issues/38259) -- Telegram stops after completing a turn
- [#38098](https://github.com/anthropics/claude-code/issues/38098) -- Plugin auto-spawns in all sessions

**The workaround**: register the server in `.mcp.json` and launch with `--dangerously-load-development-channels server:telegram` instead of `--channels plugin:telegram@claude-plugins-official`. This activates channel routing through a different code path that works reliably.

## Install via Claude Code plugin marketplace

The easiest way to install is through our plugin marketplace:

```
/plugin marketplace add gohyperdev/hyperdev-claude-plugins
/plugin install hdcd-telegram@hyperdev-plugins
```

Then configure and launch:
```
/hdcd-telegram:configure <your-bot-token>
claude --channels plugin:hdcd-telegram@hyperdev-plugins
```

> **Note:** The `--channels` path is currently gated by a server-side feature flag on Anthropic's side. If it doesn't work for your account, use the manual setup below with `--dangerously-load-development-channels` as a workaround.

## Manual setup

### 1. Get the binary

**Option A -- Pre-built binary** (recommended):

Download from [GitHub Releases](https://github.com/gohyperdev/hdcd-telegram/releases) for your platform (Linux x86_64/ARM64, macOS Intel/Apple Silicon, Windows). Then verify:

```bash
shasum -a 256 -c SHA256SUMS.txt
```

On macOS, remove the quarantine flag:
```bash
chmod +x ./hdcd-telegram
xattr -d com.apple.quarantine ./hdcd-telegram
```

**Option B -- Build from source:**

```bash
git clone https://github.com/gohyperdev/hdcd-telegram.git
cd hdcd-telegram
cargo build --release
# Binary: target/release/hdcd-telegram
```

Requires Rust 1.80+. No native dependencies.

### 2. Create a Telegram bot

If you already have a bot token (e.g. from the official plugin at `~/.claude/channels/telegram/.env`), skip to [step 3](#3-configure-the-bot-for-groups-important).

1. Open Telegram and message [@BotFather](https://t.me/BotFather)
2. Send `/newbot`
3. Choose a display name (e.g. "My Claude Agent")
4. Choose a username ending in `bot` (e.g. `my_claude_agent_bot`)
5. BotFather replies with your **bot token** -- save it:

```bash
mkdir -p ~/.claude/channels/telegram
echo "HDCD_TELEGRAM_BOT_TOKEN=123456789:AAH..." > ~/.claude/channels/telegram/.env
chmod 600 ~/.claude/channels/telegram/.env
```

> **How hdcd-telegram finds the token:** On startup, the binary checks `HDCD_TELEGRAM_BOT_TOKEN` first (env, then `.env` file), falling back to `TELEGRAM_BOT_TOKEN` for backward compatibility. Using the dedicated variable avoids conflict when running alongside the official Telegram plugin. If you need a custom state directory, set `TELEGRAM_STATE_DIR` (see [Configuration](#configuration)).

### 3. Configure the bot for groups (important)

> **This step is critical if you want the bot to work in group chats.** By default, Telegram bots in "privacy mode" only see messages that start with `/` or explicitly @mention the bot. You must disable privacy mode so the bot can see all messages in the group.

1. Message [@BotFather](https://t.me/BotFather) → send `/setprivacy`
2. Select your bot
3. Choose **Disable**

BotFather confirms: "Privacy mode is disabled."

> **Order matters:** If you already added the bot to a group _before_ disabling privacy mode, the change won't take effect for that group. You must **remove the bot from the group and re-add it** after changing the privacy setting.

After adding the bot to a group:

1. **Promote the bot to admin** (required for it to read all messages)
2. **Someone must send a message** in the group for the bot to register the `chat_id`

For **1:1 DMs** (private chats), no extra configuration is needed -- the bot sees all messages by default.

### 4. Register as MCP server

Add to your project's `.mcp.json` (or `~/.claude.json` for global):

```json
{
  "mcpServers": {
    "telegram": {
      "command": "/path/to/hdcd-telegram",
      "args": []
    }
  }
}
```

Replace `/path/to/hdcd-telegram` with the actual path to the binary (e.g. `./hdcd-telegram`, `/usr/local/bin/hdcd-telegram`, or the full path to `target/release/hdcd-telegram`).

> **No token in `.mcp.json`!** You do NOT pass the bot token here. The binary reads it automatically from `~/.claude/channels/telegram/.env` (see step 2). The `.mcp.json` only tells Claude Code how to start the binary.
>
> If you want to pass the token explicitly (e.g. in CI), you can use the `env` field:
> ```json
> {
>   "mcpServers": {
>     "telegram": {
>       "command": "/path/to/hdcd-telegram",
>       "args": [],
>       "env": {
>         "TELEGRAM_BOT_TOKEN": "123456789:AAH..."
>       }
>     }
>   }
> }
> ```

### 5. Launch Claude Code

```bash
claude --dangerously-load-development-channels server:telegram
```

> **Why `--dangerously-load-development-channels`?** See [Known issue](#known-issue---channels-plugin-is-broken) above. This flag activates channel routing for servers registered in `.mcp.json`. It shows a one-time confirmation prompt.

**Verify it works:** On startup, Claude Code should show the telegram MCP server as connected. If you see `--dangerously-load-development-channels ignored (server:telegram)`, check that:
- The server name in `.mcp.json` is exactly `"telegram"` (must match `server:telegram`)
- The `.mcp.json` is in the directory you launched `claude` from, or in `~/.claude.json` for global
- The binary path in `"command"` is correct and executable

### 6. Pair your Telegram account

DM your bot on Telegram. It replies with a 6-character pairing code. In your Claude Code session:

```
/telegram:access pair <code>
```

Done. Your next DM reaches Claude.

### How it all connects

```
┌─────────────┐     ┌──────────────────┐     ┌────────────┐
│  Telegram    │────▶│  hdcd-telegram   │────▶│ Claude Code│
│  (user DM)  │◀────│  (MCP server)    │◀────│ (session)  │
└─────────────┘     └──────────────────┘     └────────────┘
                    reads on startup:
                    ~/.claude/channels/telegram/.env     ← bot token
                    ~/.claude/channels/telegram/access.json ← who's allowed
```

1. **Claude Code** reads `.mcp.json`, starts `hdcd-telegram` as an MCP subprocess (stdio)
2. **hdcd-telegram** reads the bot token from `~/.claude/channels/telegram/.env` and starts polling Telegram
3. Incoming Telegram message → hdcd-telegram sends `notifications/claude/channel` via JSON-RPC → Claude sees it as a `<channel>` tag
4. Claude replies via the `reply` MCP tool → hdcd-telegram sends it back to Telegram

No ports opened. No webhooks. Everything runs locally over stdio + outbound HTTPS to `api.telegram.org`.

## Router mode (multi-session)

The default standalone mode supports one Claude Code session per bot token. If you run multiple sessions simultaneously (parallel agents, CI workers, different projects), each would need its own bot -- and only one can poll at a time (Telegram returns 409 Conflict otherwise).

**Router mode** solves this with a two-binary architecture:

```
┌─────────────┐     ┌────────────────┐     ┌──────────────────┐     ┌────────────┐
│  Telegram    │────▶│ hdcd-router  │────▶│  hdcd-telegram   │────▶│ Claude Code│
│  (forum      │◀────│ (single poll)  │◀────│  (--router mode) │◀────│ (session)  │
│   topics)    │     └────────────────┘     └──────────────────┘     └────────────┘
└─────────────┘       one process            one per session          one per session
```

- **hdcd-router** holds the single Telegram polling connection, creates a forum topic per session, and routes messages via filesystem IPC (JSONL mailbox files)
- **hdcd-telegram --router** runs in router mode -- no direct Telegram polling, reads from inbox, writes to outbox
- Each session gets its own forum topic in a Telegram supergroup
- Topics start with a placeholder title (project folder + short session ID, e.g. `hdcd-telegram #a1b2c3`) and Claude renames them via the `set_topic_title` MCP tool once the conversation topic becomes clear

### Router setup

#### 1. Create a Telegram supergroup with topics

1. Create a new group in Telegram (any name, e.g. "Claude Sessions")
2. Open group settings > **Group Type** > set to **Public** or **Private** (this converts it to a supergroup)
3. Go to settings > **Topics** > toggle **ON** (this option only appears after the group is a supergroup)
4. Add your bot to the group
5. Promote the bot to admin with **Manage Topics** enabled (required to create, close, and reopen forum topics). Other admin permissions are optional.

> **Tip:** If you don't see the Topics toggle, make sure you completed step 2 first — Topics are only available in supergroups, not regular groups.

#### 2. Get your supergroup ID and user ID

Send any message in the group, then query the bot API:

```bash
curl -s "https://api.telegram.org/bot<YOUR_TOKEN>/getUpdates" | jq '.result[-1].message'
```

From the response:
- **supergroup ID**: `.chat.id` — negative, starts with `-100` (e.g. `-1001234567890`)
- **your user ID**: `.from.id` — positive number (e.g. `123456789`)

#### 3. Configure the router

```bash
mkdir -p ~/.claude/channels/telegram-router
cat > ~/.claude/channels/telegram-router/config.json << 'EOF'
{
  "bot_token": "YOUR_BOT_TOKEN",
  "supergroup_id": "-100XXXXXXXXXX",
  "allowed_users": ["YOUR_TELEGRAM_USER_ID"]
}
EOF
```

Optional config fields:

| Field | Default | Description |
|---|---|---|
| `close_topic_on_disconnect` | `true` | Close forum topic when session disconnects |
| `outbox_poll_interval_ms` | `200` | How often to check outbox files |
| `health_check_interval_s` | `30` | How often to check if session PIDs are alive |
| `auto_shutdown_delay_s` | `60` | Shut down router after this many seconds with no active sessions (0 = stay running) |

#### 4. Launch Claude Code sessions

The router starts automatically when needed. `hdcd-telegram --router` checks `router.lock` on startup -- if the router isn't running, it spawns `hdcd-router` as a background process (both binaries must be in the same directory). The router shuts down automatically after 60 seconds with no active sessions.

To start the router manually instead: `./hdcd-router`

Each session uses `hdcd-telegram` in router mode. Add to `.mcp.json`:

```json
{
  "mcpServers": {
    "telegram": {
      "command": "/path/to/hdcd-telegram",
      "args": ["--router"]
    }
  }
}
```

Then launch as usual:

```bash
claude --dangerously-load-development-channels server:telegram
```

Each session automatically registers with the router, gets a forum topic, and starts receiving messages.

#### Router commands

Send these in the General topic of your supergroup:

| Command | Description |
|---|---|
| `/status` | List active sessions with PIDs, topic IDs, and working directories |
| `/kill <session-id>` | Close a session's forum topic |
| `/help` | Show available commands |

### How router mode works

```
~/.claude/channels/telegram-router/
  config.json          ← router config
  sessions.json        ← persistent session registry
  router.lock          ← heartbeat file (PID + timestamp)
  register/            ← session registration files
    <session-id>.json
  inbox/               ← Telegram → session (router writes, MCP reads)
    <session-id>.jsonl
  outbox/              ← session → Telegram (MCP writes, router reads)
    <session-id>.jsonl
```

1. `hdcd-telegram --router` writes a registration file to `register/`
2. `hdcd-router` detects it, creates a forum topic, updates `sessions.json`
3. Telegram messages in the topic are written to `inbox/<session-id>.jsonl`
4. MCP server reads inbox, converts to `notifications/claude/channel`
5. Claude's replies (via `reply` tool) are written to `outbox/<session-id>.jsonl`
6. Router reads outbox, sends to the correct forum topic
7. On disconnect (stdin EOF or dead PID), topic is closed

## Troubleshooting

### Authentication: channels require claude.ai OAuth

**Channels only work with claude.ai OAuth authentication.** API keys and Anthropic Console authentication do not work -- channel registration silently fails and inbound messages are never delivered.

If you're running in Docker, CI, or any headless environment, make sure you're logged in via the OAuth flow:

```bash
claude login
```

Verify your auth method:
```bash
claude config get authMethod
# Should return "oauth", not "apiKey"
```

This is the most common hidden blocker -- if everything looks correct but messages don't arrive, check your auth method first.

### "Channels are not currently available"

This is a Claude Code issue, not a plugin issue. The `--dangerously-load-development-channels` flag should bypass this. If it doesn't:

1. **Check your auth method**: must be claude.ai OAuth (see above).
2. **Check Claude Code version**: run `claude --version`. Versions before 2.1.90 may not support the flag correctly.
3. **Check `.mcp.json` is loaded**: the telegram server must appear in the MCP server list at startup.

### Bot shows "typing" but never replies

The plugin is receiving your message and forwarding it to Claude Code, but Claude Code isn't routing it into the conversation.

**Most likely cause: wrong auth method.** Channels require claude.ai OAuth -- if you're using an API key, the MCP server works (typing indicator) but channel notifications are silently dropped. See [Authentication](#authentication-channels-require-claudeai-oauth) above.

If auth is correct, check:
- Are you using `--dangerously-load-development-channels server:telegram`? (Not `--channels plugin:...`)
- Is the startup output showing the channel as active (not "ignored")?
- Try restarting Claude Code -- some sessions lose channel routing after the first turn

### 409 Conflict / bot doesn't respond

Another process is polling the same bot token. This happens when:
- A previous Claude Code session didn't shut down cleanly
- You're running multiple instances with the same bot token

Fix: kill any leftover `hdcd-telegram` or `bun` processes, then restart.

```bash
pkill -f hdcd-telegram
pkill -f "bun.*telegram"
```

### Bot doesn't see messages in group

See [step 3](#3-configure-the-bot-for-groups-important) -- privacy mode must be disabled, and the bot must be re-added to the group after the change.

## Voice transcription (optional)

`hdcd-telegram` can transcribe voice messages using [OpenAI Whisper](https://github.com/openai/whisper) locally. Transcription runs entirely on your machine -- no data leaves your network.

### Prerequisites

**macOS:**
```bash
pip install openai-whisper
brew install ffmpeg
```

**Linux (Ubuntu/Debian):**
```bash
pip install openai-whisper
sudo apt install ffmpeg
```

**Windows:**
```bash
pip install openai-whisper
choco install ffmpeg
```

### How it works

1. Voice message arrives from Telegram
2. `hdcd-telegram` downloads the `.oga` file, converts to WAV via `ffmpeg`, runs `whisper`
3. Bot sends transcription back to chat: *"Transcription: '...' -- reply 'ok' to confirm or correct the text"*
4. User confirms (`ok` / `yes` / `tak`) or sends corrected text
5. Confirmed transcription is delivered to Claude as a `<channel>` notification

If whisper or ffmpeg are not installed, voice messages are forwarded as `"(voice message)"` with the `attachment_file_id` in metadata -- Claude can still use the `download_attachment` tool to fetch the raw file.

## Configuration

| Env var | Default | Description |
|---|---|---|
| `HDCD_TELEGRAM_BOT_TOKEN` | *(preferred)* | Bot token from [@BotFather](https://t.me/BotFather). Takes priority over `TELEGRAM_BOT_TOKEN`. Avoids conflict when running alongside the official Telegram plugin. Also read from `~/.claude/channels/telegram/.env`. |
| `TELEGRAM_BOT_TOKEN` | *(fallback)* | Bot token — backward compatible. Used if `HDCD_TELEGRAM_BOT_TOKEN` is not set. |
| `TELEGRAM_STATE_DIR` | `~/.claude/channels/telegram` | State directory (access.json, inbox, pairing codes) |
| `WHISPER_MODEL` | `small` | Whisper model size (`tiny`, `base`, `small`, `medium`, `large`) |
| `WHISPER_LANGUAGE` | auto-detect | Language hint (`Polish`, `English`, etc.) |
| `HDCD_ECHO_TRANSCRIPT` | `true` | Send transcript back for user confirmation before delivering to Claude |
| `ROUTER_STATE_DIR` | `~/.claude/channels/telegram-router` | Router state directory (config.json, sessions, mailbox) |
| `RUST_LOG` | `hdcd_telegram=info` | Log level filter ([`tracing-subscriber`](https://docs.rs/tracing-subscriber) format) |

## Running alongside the official Telegram plugin

Since v0.1.2, hdcd-telegram supports `HDCD_TELEGRAM_BOT_TOKEN` as a dedicated env var. This lets you run both plugins with separate bots — no polling conflict:

```bash
# ~/.claude/channels/telegram/.env
HDCD_TELEGRAM_BOT_TOKEN=<your-hdcd-bot-token>      # hdcd-telegram reads this first
TELEGRAM_BOT_TOKEN=<official-plugin-bot-token>      # official plugin reads this
```

Launch with both channels:
```bash
claude --dangerously-load-development-channels plugin:hdcd-telegram@hyperdev-plugins \
       --dangerously-load-development-channels plugin:telegram@claude-plugins-official
```

If only `TELEGRAM_BOT_TOKEN` is set, hdcd-telegram uses it as before — fully backward compatible.

## Migrating from the official plugin

1. Your existing `~/.claude/channels/telegram/.env` and `access.json` work as-is
2. Replace the MCP entry in `.mcp.json` (Bun command -> `hdcd-telegram` binary path)
3. Launch with `--dangerously-load-development-channels server:telegram` instead of `--channels plugin:telegram@claude-plugins-official`
4. All pairings, group policies, and settings are preserved

## Features

- **All 8 message types**: text, photo, document, voice, audio, video, video note, sticker
- **5 MCP tools**: `reply` (with chunking, threading, file attachments, MarkdownV2), `react`, `edit_message`, `download_attachment`, `set_topic_title` (router mode — lets Claude rename its forum topic when the conversation shifts)
- **Access control**: pairing flow (6-hex code), allowlist, group policies with @mention gating
- **Permission relay**: inline keyboard for remote tool-use approval/denial (`claude/channel/permission`)
- **Voice transcription** (optional): automatic speech-to-text via [whisper](https://github.com/openai/whisper) with echo-back confirmation flow
- **Bot commands**: `/start`, `/help`, `/status`
- **Router mode**: multi-session support via `hdcd-router` + forum topics (one topic per session, `/status`, `/kill` commands)
- **409 Conflict retry** with exponential backoff
- **Clean shutdown** on stdin EOF (no zombie polling)

## Security

- **Access control**: 6-character hex pairing codes, per-user allowlists, group policies with @mention gating. Same model as the official plugin.
- **Token handling**: bot token stored in `~/.claude/channels/telegram/.env`. The binary warns at startup if the file is world-readable (`chmod 600` recommended).
- **SecretToken**: the `access.json` allowlist uses Telegram user IDs, not usernames. Pairing codes are single-use and expire.
- **Sendable-file gate**: prevents Claude from exfiltrating the plugin's own state directory via file attachments.
- **No network listeners**: communicates exclusively via stdio (JSON-RPC 2.0) and outbound HTTPS to `api.telegram.org`. No ports opened.

## Pre-built binaries: trust and verification

Pre-built binaries are published on the [GitHub Releases](https://github.com/gohyperdev/hdcd-telegram/releases) page for Linux (x86_64, ARM64), macOS (Intel, Apple Silicon), and Windows.

### Verifying downloads

Every release includes a `SHA256SUMS.txt` file and per-archive `.sha256` files. After downloading:

```bash
shasum -a 256 -c SHA256SUMS.txt
```

This confirms the file you downloaded matches what the CI pipeline produced. The checksums are generated in GitHub Actions, so they are only as trustworthy as the CI pipeline itself. For maximum assurance, build from source.

### VirusTotal

[VirusTotal](https://www.virustotal.com) is a free service by Google that scans files against 70+ antivirus engines simultaneously. If you downloaded a binary and want to verify it is clean, upload it to [virustotal.com](https://www.virustotal.com) before running it. We encourage this -- there is nothing to hide.

You can also check the SHA256 hash directly: go to virustotal.com, click "Search", and paste the SHA256 from `SHA256SUMS.txt`. If someone has already scanned that exact file, you will see the results without re-uploading.

### macOS Gatekeeper

macOS blocks unsigned binaries downloaded from the internet. After extracting, you will see:

> "hdcd-telegram" can't be opened because Apple cannot check it for malicious software.

This happens because the binary is not signed with an Apple Developer ID certificate ($99/year). To allow it:

```bash
xattr -d com.apple.quarantine ./hdcd-telegram
```

Or: System Settings > Privacy & Security > scroll down > "Allow Anyway".

### Windows SmartScreen

Windows may show a "Windows protected your PC" warning for unsigned executables. Click "More info" > "Run anyway". This happens because the binary is not signed with an Authenticode certificate.

### Building from source (recommended for production)

If you prefer not to trust pre-built binaries:

```bash
git clone https://github.com/gohyperdev/hdcd-telegram.git
cd hdcd-telegram
cargo build --release
# Binary: target/release/hdcd-telegram
```

This way you control the entire build chain. Requires Rust 1.80+.

### Roadmap to signed releases

| Level | Status | Description |
|---|---|---|
| SHA256 checksums | Done | Every release includes checksums |
| VirusTotal | Manual | We encourage users to verify on virustotal.com |
| Apple code signing + notarization | Planned | Eliminates macOS Gatekeeper warning |
| Windows Authenticode signing | Planned | Eliminates SmartScreen warning |
| GPG-signed releases | Planned | Cryptographic proof of publisher identity |
| Reproducible builds | Planned | Anyone can verify binary matches source |

## Acknowledgements

- [Claude Code](https://code.claude.com) by Anthropic -- the `claude/channel` MCP capability this project builds on
- [OpenAI Whisper](https://github.com/openai/whisper) -- local speech-to-text for voice message transcription
- [grammy](https://grammy.dev) -- the official TS plugin uses grammy; this Rust implementation talks to the same [Telegram Bot API](https://core.telegram.org/bots/api) directly via [reqwest](https://github.com/seanmonstar/reqwest)
- [tokio](https://tokio.rs) -- async runtime
- [Model Context Protocol](https://modelcontextprotocol.io) -- the underlying protocol for Claude Code extensions

## License

Apache-2.0. See [LICENSE](LICENSE).
