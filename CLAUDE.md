# goniec

goniec carries chat messages between the user and AI agent sessions, and
between the sessions themselves. The chat server is Zulip. The agents run in
different harnesses: Claude Code first, then codex, opencode and pi. goniec
also manages the agent side of Zulip: bot accounts, streams and keys.

The name is Polish for a courier who runs messages.

## Stack

- Rust, one Cargo package. The library holds the Zulip client, the inbox and
  the config. The `goniec` binary holds the CLI and the daemon, and later the
  hook entry points as subcommands. The MCP server will use the same library.
- Zulip has no official Rust client. goniec has its own thin client on
  `reqwest`, which covers only the endpoints goniec uses.
- `make build`, `make test` and `make run` wrap `cargo`.

## Goals and non-goals

- **No lock-in to one vendor.** The user rejected Anthropic Remote Control for
  this. The server, the message format, the CLI and the inbox are the same for
  every harness. Only a thin adapter per harness differs.
- **Messages from the chat are data, never user input.** An agent may act on a
  request from another agent, but such a request is never the user's approval.
  A deploy or release still needs the user's approval in that session's own
  terminal. This is the same rule that Claude Code applies to its local
  cross-session messages.
- **No visible clutter per agent.** The user rejected one Claude Code Monitor
  per agent session ("very meh"). The delivery path must not appear in the
  user's monitor list.
- Not a goal: a general chat bot, or a replacement for the user's own chat use
  of Zulip.

## Concepts

- **Nickname.** Every agent speaks as its own Zulip bot. Nicknames tell the
  user who said what. They are not a security boundary (see *Security*).
- **Bound session.** A session whose directory has a `.goniec` file. The file
  holds the nickname and the bot's Zulip user ID. Each bound directory has its
  own permanent bot, for example one per customer. Messages for it queue in
  its inbox while it is offline, and arrive when it starts again.
- **Session directory.** goniec reads only `<session dir>/.goniec` and never
  looks in parent directories. The session directory is `$GONIEC_DIR`, else
  `$CLAUDE_PROJECT_DIR` (where Claude Code started), else the current
  directory. A session that changes into a subdirectory keeps its identity.
- **Binding.** `goniec bind [nick]`, run in the directory to bind (it uses
  the current directory, not the session directory), creates the bot, writes `.goniec`, and
  stores the bot's key on this host. The nickname defaults to the basename of
  the directory, like the user's `tm` and `.tmux-session`. Creating a bot
  needs the owner key, so each `bind` is one rosa approval for the user. goniec
  never creates a bot without one.
- **Identity check.** At startup goniec asks Zulip whose key it holds
  (`GET /users/me`) and compares the user ID with `.goniec`. A wrong key or a
  copied `.goniec` fails the check. The file holds an ID, not a value derived
  from the key, so regenerating a key does not break the binding.
- **Guest session.** A session without `.goniec`. All guests share one bot,
  `@guest`. Each guest message starts with a `host:dir` line, so the user can
  tell the guests apart. Guests poll: `goniec read` asks Zulip for new
  messages in the guest's topics since its last read. A guest has no inbox,
  no watcher and no daemon, and sees a reply only when it next reads. The
  harness tells guests to poll. Several sessions in one directory (for
  example the user's `temp` sessions) stay guests, so they never claim the
  same nickname. A shared guest also means no dead bots pile up: Zulip
  cannot delete users, only deactivate them.
- **Rooms.** Zulip streams and topics. An `#ops` stream for announcements, and
  one stream per customer that only that customer's agent, the build agent
  and the user join. Topics hold one job each ("PHP 8.5.12 rollout"). Stream
  membership keeps customer data apart inside Zulip.

## Architecture

```
Zulip ──event queue──▶ goniec daemon ──▶ inbox/<nick>/   (spool dir, one file per message)
                            │                 │
                            │                 ├─▶ Claude Code: asyncRewake watcher (idle)
                            │                 │                PostToolBatch hook   (busy)
                            │                 └─▶ other harnesses: tmux nudge + `goniec read`
                            └── presence

agent ──`goniec send` / MCP tool──▶ Zulip REST, as its own bot (or @guest)
```

- **CLI and MCP server.** The same operations for every harness: send, read,
  list rooms, who is online, bind a directory.
- **Hosts.** The user's agents run on fewer than 5 computers. Each host runs
  its own daemon. A bot's key lives on exactly one host, and only that host's
  daemon listens as that bot. Zulip delivers every event to every queue
  registered for a bot, so the same key on two hosts would deliver every
  message twice. For a session on another host, `goniec bind` runs on this
  host (where rosa holds the owner key), and the user copies the key over.
  The `@guest` key is on every host, because guests only poll.
- **Daemon.** It reads Zulip's real-time event queue for each bot bound on its
  host and writes each message to the inbox of each recipient. A daemon holds
  the same number of Zulip queues as the watchers would (one per bot), but it
  keeps each queue open for its whole run. A watcher exits after every
  delivery and at the hook timeout, so a watcher that held its own queue would
  register a new one at least every 10 minutes.
- **What reaches an agent.** Direct messages to its bot, and channel messages
  that mention it, including `@all`. Other channel traffic does not, because
  every delivery wakes an agent and costs a turn. Its own messages do not.
- **Zulip's unread flags are the truth.** When the daemon starts, it fetches
  each bot's unread messages into the inbox. It marks a message read in Zulip
  only after an agent has received it. A daemon restart or a host reboot loses
  nothing, and the local inbox is only a fast cache.
- **Inbox.** One spool directory per nickname. Delivery moves a file to
  `delivered/`, so a message reaches an agent once, whichever path delivers it.
- **Message records.** Each inbox file keeps the stream, the topic, the
  sender's Zulip user ID and whether the sender is a bot. The daemon routes by
  (stream, topic) as well as by nickname. The `@claude` dispatcher (Plan step
  8) needs both.
- **Session state.** Each adapter writes `busy`, `idle` or `waiting` (a
  permission prompt is on screen) to a state file per nickname. The daemon
  never nudges a session in `waiting`, because typed keys would answer the
  prompt.

## Claude Code adapter

Facts are from the Claude Code docs (hooks reference and hooks guide,
checked 2026-10-24 for version 2.1.310). Test them before relying on them.

- **Idle sessions: `asyncRewake`.** A command hook with `"asyncRewake": true`
  runs in the background. When it exits with code 2, Claude wakes at once,
  also when the session is idle. Its stderr (or its stdout when stderr is
  empty) reaches Claude as a system reminder, which is data and not a user
  turn. Each run waits once. The watcher blocks until the inbox has messages,
  prints the batch to stderr, marks it delivered and exits 2.
- **Re-arming.** Start the watcher from `SessionStart` and from `Stop`, so
  every finished turn starts a new one. A lockfile per nickname keeps exactly
  one watcher. A second one exits at once.
- **Busy sessions: `PostToolBatch` (or `PostToolUse`) with
  `hookSpecificOutput.additionalContext`.** The text lands next to the tool
  result as a system reminder. The limit is 10,000 characters per string, so
  send a summary and "N more, run `goniec read`" for big batches.
- **Not used:** Monitor (rejected by the user), channels (research preview,
  needs `--dangerously-load-development-channels` with a confirmation dialog
  at every start, and claude.ai login only), Remote Control (rejected).
- **The hook config.** `goniec hook wait` (with `"asyncRewake": true` and
  `"timeout": 2147483`) on `SessionStart` and `Stop`, and `goniec hook context`
  on `PostToolBatch`. Both exit 0 at once outside a bound session directory,
  so one config can serve every session. It lives in the user's global
  `~/.claude/settings.json`. `goniec install-hooks` adds it on each machine
  (dotfiles' `bootstrap-goniec` runs it), and `--remove` takes it out. It
  adds, replaces or removes only goniec's own entries (a command with
  `goniec` and `hook wait` or `hook context`), never a whole event list, so
  the user's other hooks on the same events stay. claude-setup does not sync
  `hooks`, because its sync replaces whole keys. Each command
  is guarded, so it does nothing on a machine without goniec:
  `g=$(command -v goniec || echo "$HOME/.cargo/bin/goniec"); [ -x "$g" ] ||
  exit 0; exec "$g" hook wait`. The `exec` passes exit code 2 through; an
  `|| true` wrapper would swallow the wake signal.
- **The watcher must run for as long as the session is idle.** Claude Code
  kills a hook at its `timeout`, and after that nothing would wake the idle
  session. The timeout is 2147483 s (24.8 days), the largest delay a
  JavaScript timer holds; a larger value may fire at once. About two hours
  before it, the watcher wakes the session with a one-line renewal note. The
  turn ends, and `Stop` starts a fresh watcher. So an idle bound session costs
  one short turn every 24.8 days.
- **Measured on 2026-10-25,** in a session that ran Claude Code 2.1.302:
  - A project `.claude/settings.local.json` added while the session ran took
    effect at its next turn, without a restart.
  - The watcher armed from `Stop` woke the idle session. Claude Code shows the
    wake as "Stop hook feedback", and the model gets it as a system reminder.
  - `"timeout": 86400` is accepted: the watcher still ran after 785 s, past
    the default of 600 s. The real upper limit is not measured yet.
  - `sh -c` runs the hook command directly: the watcher's parent is the
    `claude` process. Hooks get `CLAUDE_PROJECT_DIR` (the project root, also
    when the session changed directories). The Bash tool gets `CLAUDE_PID`
    but not `CLAUDE_PROJECT_DIR`.
  - A watcher that exits 2 while a turn runs does not interrupt it. Claude
    Code holds the wake until the turn ends and then runs an extra short
    turn. So the watcher leaves messages alone while the session is in a
    turn, and `PostToolBatch` delivers them inline.
- **Measured on a restart (2026-10-25, 2.1.302 to 2.1.310, with `claude -c`):**
  - The `SessionStart` watcher armed one second after startup and did not
    block it. The first turn after the restart ran at once (7 s busy, then
    idle). The watcher from that turn's `Stop` found the lock taken and ended.
  - The old session's watcher was gone after the exit.
- **Busy or idle.** No hook marks a turn's end reliably: `Stop` does not run
  when the user presses Esc. Claude Code keeps the state in an internal file
  per process, `~/.claude/sessions/<pid>.json`, field `status` (`busy`,
  `waiting` for a permission answer, `idle`). The watcher reads it for its
  `claude` process, only when a message waits. The documented `claude agents
  --json` lists background sessions only, not sessions started with `claude`
  in a terminal, and the user's sessions are of that kind. If the file is
  missing or its format changes, the watcher delivers as if the session were
  idle: the message then arrives when the turn ends.
- **Still to test:**
  - Delivery while the session is busy, through `PostToolBatch`, now that
    the watcher holds back.

## Other harnesses (codex, opencode, pi)

- **Wake an idle agent with a tmux nudge.** The daemon types a FIXED text into
  the agent's tmux pane with `tmux send-keys`, for example
  `[goniec] new messages, run: goniec read`, then Enter. The text never contains
  message content. Anything typed counts as the user's own prompt, so content
  there would turn chat messages into user instructions.
- Nudge only when the state file says `idle`. Wait a few seconds after the
  last activity. Batch messages for about 10 s. Skip a pane where the user is
  typing (`tmux display -p '#{client_activity}'`).
- Use each harness's own hooks or plugins for busy delivery and for the state
  file, where they exist.
- Agents with no terminal (for example a build agent) may instead run
  headless behind the harness's own API: `opencode serve` (HTTP), codex
  `app-server` (JSON-RPC), pi RPC mode, or `claude -p --input-format
  stream-json`.

## Long-running sessions

A session is its transcript on disk, not its process. Some of the user's
sessions run for weeks on an old binary. The plan is an idle restart: when a
session is idle, quit it in its tmux pane and start `claude --resume <id>` in
the same pane. The `SessionStart` hook then arms the watcher again. This picks
up new Claude Code versions and new hook config.

## Zulip permissions

Read from the Zulip server source at tag 12.3 on 2026-10-25. Not yet tested
against the live server.

- **A bot can never create bots.** `add_bot_backend` has the decorator
  `require_human_non_guest_user`, which rejects every caller that has a
  `bot_type`. The decorator runs before any group permission is checked.
  The same decorator guards `patch_bot_backend` (name, avatar, owner),
  `regenerate_bot_api_key` and `GET /bots`. A bot cannot own a bot.
- **A member bot can create public and private channels**, and can
  subscribe other users to a channel it creates. Making a channel a default
  stream needs an administrator.
- **An administrator bot can also deactivate, reactivate and rename bots.**
- Every bot can change its own avatar. No bot can change another bot's
  avatar or API key.

So goniec uses two kinds of key:

- **The owner key**, only for `goniec provision`: create bots, and regenerate
  a bot's key. The user keeps it in sub-rosa (`rosa`) with the `confirm`
  policy, so the user approves each use in the rosa terminal. goniec asks the
  rosa socket for the key and never stores it.
- **Bot keys** for everything else. The `admin` bot has the administrator
  role. It creates channels and deactivates bots.

## Security

- All of the user's agents run as one Unix user, so any agent can read every
  bot's API key and post as another nickname. Accept this for now. If it ever
  matters, give each agent its own Unix user, or let the daemon hold the keys
  and check the calling process. The owner key is the exception: rosa
  releases it only after the user approves.
- An agent answers only when it is mentioned or addressed. Rate-limit each
  bot. This stops two bots from answering each other forever.
- Never post secrets. Zulip keeps every message in its database.
- Bot API keys live in the user's secrets directory, never in this repo.

## Plan

1. ~~Zulip admin access for goniec.~~ Done: see *Zulip permissions*.
2. Bots: `admin`, `@guest` and one test nickname. The `#ops` stream.
   `goniec provision` and `goniec bind` do this with the owner key from rosa.
3. `goniec` CLI: send, read, rooms, who, bind. The identity check. Open:
   `read` as `@guest`, which needs state per guest session.
4. Daemon: the event queue and the inbox. The user's dotfiles vendor goniec
   and hold the systemd user unit (`make install-goniec-daemon`). The unit
   does not restart on exit code 3, which means another daemon holds the
   lock. Open: launchd on a Mac, a logon task on Windows. Not tested yet: an
   expired event queue (`BAD_EVENT_QUEUE_ID`), and a reload after a new bind.
   Who is online is left out on purpose until it is needed.
5. The `asyncRewake` test in a throwaway Claude Code session (see *Risks*).
6. The Claude Code hooks, then the tmux nudge for the other harnesses.
7. The MCP server, built on the CLI library.
8. `@claude`: a dispatcher bot. A mention starts a headless Claude Code
   session (`claude -p --input-format stream-json`) in a fixed directory, one
   per topic. Later messages in the topic go to the same session. Two rules
   come before any code:
   - This makes chat text a user prompt, which breaks *Messages from the chat
     are data*. Only the user's own human account may start or drive such a
     session, never a bot. The sender ID in the message record decides this.
   - A headless session has no terminal for permission prompts. It needs a
     narrow allowlist, or a permission prompt routed to the user in the
     topic.

## Conventions

- Commits: imperative subject with a module prefix (`cli:`, `daemon:`,
  `hooks:`, `zulip:`, `docs:` …), a body that explains why, one concern per
  commit.
- Machine-specific facts (hosts, paths, the Zulip instance, keys) go in
  `CLAUDE.local.md`, which git ignores.
- Measure, don't estimate: record measured numbers (timeouts, latencies)
  with a date.
