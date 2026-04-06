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

## Quick start

### 1. Build

```bash
git clone https://github.com/gohyperdev/hdcd-telegram.git
cd hdcd-telegram
cargo build --release
```

Requires Rust 1.80+. No native dependencies. The binary lands in `target/release/hdcd-telegram`.

### 2. Configure bot token

If you already used the official Telegram plugin, your token is in `~/.claude/channels/telegram/.env` -- skip this step.

Otherwise, create a bot via [@BotFather](https://t.me/BotFather) and save the token:

```bash
mkdir -p ~/.claude/channels/telegram
echo "TELEGRAM_BOT_TOKEN=123456789:AAH..." > ~/.claude/channels/telegram/.env
chmod 600 ~/.claude/channels/telegram/.env
```

### 3. Register as MCP server

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

### 4. Launch Claude Code

```bash
claude --dangerously-load-development-channels server:telegram
```

> **Why `--dangerously-load-development-channels`?** See [Known issue](#known-issue---channels-plugin-is-broken) above. This flag activates channel routing for servers registered in `.mcp.json`. It shows a one-time confirmation prompt.

### 5. Pair your Telegram account

DM your bot on Telegram. It replies with a 6-character pairing code. In your Claude Code session:

```
/telegram:access pair <code>
```

Done. Your next DM reaches Claude.

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
| `TELEGRAM_BOT_TOKEN` | *(required)* | Bot token from [@BotFather](https://t.me/BotFather). Also read from `~/.claude/channels/telegram/.env`. |
| `TELEGRAM_STATE_DIR` | `~/.claude/channels/telegram` | State directory (access.json, inbox, pairing codes) |
| `WHISPER_MODEL` | `small` | Whisper model size (`tiny`, `base`, `small`, `medium`, `large`) |
| `WHISPER_LANGUAGE` | auto-detect | Language hint (`Polish`, `English`, etc.) |
| `HDCD_ECHO_TRANSCRIPT` | `true` | Send transcript back for user confirmation before delivering to Claude |
| `RUST_LOG` | `hdcd_telegram=info` | Log level filter ([`tracing-subscriber`](https://docs.rs/tracing-subscriber) format) |

## Migrating from the official plugin

1. Your existing `~/.claude/channels/telegram/.env` and `access.json` work as-is
2. Replace the MCP entry in `.mcp.json` (Bun command -> `hdcd-telegram` binary path)
3. Launch with `--dangerously-load-development-channels server:telegram` instead of `--channels plugin:telegram@claude-plugins-official`
4. All pairings, group policies, and settings are preserved

## Features

- **All 8 message types**: text, photo, document, voice, audio, video, video note, sticker
- **4 MCP tools**: `reply` (with chunking, threading, file attachments, MarkdownV2), `react`, `edit_message`, `download_attachment`
- **Access control**: pairing flow (6-hex code), allowlist, group policies with @mention gating
- **Permission relay**: inline keyboard for remote tool-use approval/denial (`claude/channel/permission`)
- **Voice transcription** (optional): automatic speech-to-text via [whisper](https://github.com/openai/whisper) with echo-back confirmation flow
- **Bot commands**: `/start`, `/help`, `/status`
- **409 Conflict retry** with exponential backoff
- **Clean shutdown** on stdin EOF (no zombie polling)

## Security

- **Access control**: 6-character hex pairing codes, per-user allowlists, group policies with @mention gating. Same model as the official plugin.
- **Token handling**: bot token stored in `~/.claude/channels/telegram/.env`. The binary warns at startup if the file is world-readable (`chmod 600` recommended).
- **SecretToken**: the `access.json` allowlist uses Telegram user IDs, not usernames. Pairing codes are single-use and expire.
- **Sendable-file gate**: prevents Claude from exfiltrating the plugin's own state directory via file attachments.
- **No network listeners**: communicates exclusively via stdio (JSON-RPC 2.0) and outbound HTTPS to `api.telegram.org`. No ports opened.

## Acknowledgements

- [Claude Code](https://code.claude.com) by Anthropic -- the `claude/channel` MCP capability this project builds on
- [OpenAI Whisper](https://github.com/openai/whisper) -- local speech-to-text for voice message transcription
- [grammy](https://grammy.dev) -- the official TS plugin uses grammy; this Rust implementation talks to the same [Telegram Bot API](https://core.telegram.org/bots/api) directly via [reqwest](https://github.com/seanmonstar/reqwest)
- [tokio](https://tokio.rs) -- async runtime
- [Model Context Protocol](https://modelcontextprotocol.io) -- the underlying protocol for Claude Code extensions

## License

Apache-2.0. See [LICENSE](LICENSE).
