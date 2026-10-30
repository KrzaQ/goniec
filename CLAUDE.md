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
  A deploy or release still needs the user's approval: in that session's own
  terminal, or, for a headless session that goniec started, through the
  approval flow in *Threads and headless sessions*. This is the same rule that Claude Code
  applies to its local cross-session messages. One exception: in a headless
  session, messages from the trusted user count as the user's instructions
  (see *Threads and headless sessions*). In an interactive session they stay data,
  because the user is at the terminal.
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
- **A thread's earlier messages.** Channel traffic that does not mention a
  bot never reaches it, so a bare mention (`@**series-pl**` and nothing
  else) carries no request. The user writes those to point an agent at the
  messages above (feedback from the series-pl session, 2026-10-30). For a
  message that holds only mentions, the daemon fetches the thread's 5
  messages before it and stores them with the message. The notice and the
  headless prompt show them, quoted, as data. Every message in a notice
  also has a `thread:` line: `goniec read '<thread>'` prints the thread's
  latest messages from Zulip (`-n`, `--before <id>`), read-only. It works
  for `@guest` too.
- **Files.** A message links an upload as `[name](/user_uploads/…)`. When
  goniec hands messages to a session, it saves their files, and those of
  the attached earlier messages, in that session's own temp directory:
  `<tmp>/claude-<uid>/<project dir, non-alphanumerics as "-">/<session
  id>/goniec/<message id>-<n>-<name>`, at most 25 MB each. The user wanted
  them there, not on persistent storage (2026-10-30): Zulip keeps the
  originals, `/tmp` is tmpfs here, and screenshots can hold customer data.
  Measured on 2026-10-30: Claude Code creates that directory for every
  session, also in `-p` mode, and lets the session read files there
  without asking, with no allow rule. The path follows from the message,
  so the notice names the file, and the consumer (the hooks, `goniec read`,
  the responder) saves it. The hooks save files before they take the
  messages, within 5 s for `hook context` (its timeout is 10 s) and 60 s
  for the watcher: a hook killed after taking would lose the messages (the
  dotfiles session's review). A file not saved by then gets a line that
  names `goniec read <thread>`, which saves it again. A bot downloads with `GET /user_uploads/…`, which
  answers with a temporary URL. A headless prompt also carries each image
  of the user's messages (PNG, JPEG, GIF, WebP, at most 3.75 MB, the API's
  5 MB of base64) as an image block, so the model sees it at once (tested
  2026-10-30).
- **`goniec send --attach <file>`** uploads files and links them. It takes
  only files under the session directory or the session's own temp
  directory (its scratchpad is there), after following links. goniec
  takes the session ID for that from Claude Code's registry entry for its
  `claude` ancestor process (`~/.claude/sessions/<pid>.json`), never from
  `CLAUDE_CODE_SESSION_ID` alone, which a command can set to another
  session's ID. Measured on 2026-10-30: such an env-prefixed command does
  not match `Bash(goniec send *)` anyway; Claude Code asks for it.
  `Bash(goniec send *)` runs without asking, so without this limit a chat
  message could talk an agent into posting any file the user can read,
  keys included. Not all of `/tmp`: it holds every Claude session's
  scratchpad and task output (the dotfiles session's review, 2026-10-30).
  A file from elsewhere has to be copied there first, and that copy goes
  through the normal permission check.
- **Commands in a notice run exactly as shown.** The user's allow rules are
  `Bash(goniec send *)` and `Bash(goniec read *)`. A pipe or a redirection
  after them (`goniec send … 2>&1 | tail`) no longer matches, and auto mode
  then denied the send as an external write (series-pl, 2026-10-30). The
  notice says so.
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
- **The hook config.** Four entries: `goniec hook wait` (with
  `"asyncRewake": true` and `"timeout": 2147483`) on `SessionStart` and
  `Stop`, `goniec hook context` on `PostToolBatch`, and `goniec hook end` on
  `SessionEnd`. All exit 0 at once outside a bound session directory, so one
  config can serve every session. It lives in the user's global
  `~/.claude/settings.json`. `goniec install-hooks` adds it on each machine,
  and `--remove` takes it out. The user's dotfiles run it as the last step of
  `make install-goniec-daemon`, because the hooks only work where a daemon
  runs; `make bootstrap-goniec` does not touch `settings.json`. It adds,
  replaces or removes only goniec's own entries (a command with `goniec` and
  `hook wait`, `hook context` or `hook end`), never a whole event list, so
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

## Threads and headless sessions

Designed 2026-10-25, reshaped 2026-10-30 with the user. This is Plan step 8.
**Threads have owners.** A thread is one (bot, topic) pair, or one bot's
direct-message conversation with one person. Each thread has an owner: one
Claude Code conversation, named by its session UUID. The daemon records the
owner and the working directory the conversation runs in.

- **A new thread** goes to the bot's interactive session if one is online.
  Otherwise goniec starts a new headless conversation. Either way, that
  conversation's UUID becomes the owner.
- **A message in an existing thread** goes to its owner conversation:
  - If that UUID is live in a session (interactive, or a running headless
    process), goniec delivers to that session.
  - If it is live nowhere, goniec resumes exactly that conversation headless,
    `claude -p --resume <uuid> --input-format stream-json`, in the stored
    working directory. The conversation goes on with its full context.
  - So a thread started headless stays with that conversation. If the user
    later continues it interactively with `claude --resume <uuid>`, further
    messages go there.
- **Both at once.** If the user resumes a conversation interactively while
  its headless process still runs, goniec ends the headless process after
  its current turn.
- **Routing is per thread.** An interactive session's watcher and
  `PostToolBatch` hook see only messages from threads it owns, so the inbox
  is split by thread.

**Which session is live.** The sessions report it themselves, so the daemon
does not guess:

- `SessionStart` (it already runs `goniec hook wait`) registers the session:
  nickname, session UUID, `pid`, process start time, working directory, and
  whether it is headless.
- Claude Code's `SessionEnd` hook runs `goniec hook end`, which removes the
  entry.
- A crash or `kill -9` fires no hook, so the daemon drops every entry whose
  `pid` with that start time is gone.
- Other harnesses register through their own hooks or plugins, or with
  `goniec online` and `goniec offline`.
- This replaces the grace period: when a message arrives, the daemon knows at
  once whether its owner is live. The busy/idle check still reads Claude
  Code's registry (see *Busy or idle*).

**Measured on 2026-10-30 (Claude Code 2.1.314, `-p` with haiku):**

- One `claude -p --input-format stream-json` process takes several turns.
  Each turn ends with a `result` event, and the process exits when its input
  ends.
- `--resume <uuid>` keeps the session ID and the context. It also works from
  another working directory: the turns go into the original transcript, but
  the session then works in the new directory.
- The global `SessionStart` hook also runs in `-p` mode. So a headless
  session sets `GONIEC_HEADLESS=1`, and the watcher ends at once there.

**Where headless conversations live.** In the bot's own directory, as
ordinary sessions of that project: the user wants the headless sessions to
be the sessions of that directory. A custom working directory was proposed
and rejected (2026-10-30). Measured on 2026-10-30: `claude -c` there does
not pick a headless (`-p`) conversation; it continued the user's last
interactive one. To take over a headless conversation, the user runs
`claude --resume <uuid>`; the thread follows it, and a still-running
headless process for it ends after its turn (tested live with `@kiekert`).

**Other rules.**

- Messages that arrive within 2 s go to a session as one batch.
- A headless process ends after about 15 minutes without a new message. Its
  thread keeps the owner, so the next message resumes it.
- **A stopping daemon drains.** On SIGTERM it starts no turn, lets running
  headless turns finish and post their answers, closes each process's
  input once it is idle, and exits when none is left. A killed turn would
  be lost: the messages that started it already count as delivered.
  Messages that arrive meanwhile stay in the inbox for the next daemon.
  This needs the service manager to signal only the daemon and to wait:
  with systemd, `KillMode=mixed` and a `TimeoutStopSec` longer than a turn
  (the dotfiles unit, agreed on 2026-10-30). With the old
  `KillMode=control-group`, a restart killed headless turns mid-way.
- **At most 3 headless processes per bot, 5 for a dispatcher** (`@claude`),
  the user's numbers (2026-10-30). A conversation over the limit does not
  wait: it evicts the bot's process that has waited longest since its last
  turn, after that turn. The evicted conversation keeps its threads, so its
  next message resumes it. A process with messages waiting for it is not
  evicted.
- **The prompt is the trusted user's text, verbatim,** as if typed at a
  terminal, with no preamble (the user's wish, 2026-10-30). Several messages
  within one batch are joined by blank lines. Every other message stays
  data: the session reads it with `goniec read` or through the
  `PostToolBatch` hook. A line from goniec is added only when such messages
  wait, so the session knows of them. A message from another agent can get
  an answer, but never makes the session act.
- **A headless turn names its thread.** A line `[goniec] In #channel >
  topic:` (or `In a direct message:`) starts the first turn, and every turn
  whose thread differs from the previous one. The user wants `@claude` to
  keep what it learns apart by channel (2026-10-30), and the verbatim
  prompt did not say where it came from. Measured on 2026-10-30: a `-p`
  session writes Claude Code's auto memory without asking, so `@claude`
  keeps its notes per channel in its project's memory.
- **The turn's result is the answer.** At each `result` event the responder
  posts the result text to the turn's thread, as the bot, split where it is
  longer than Zulip allows. A failed turn posts a short error note; an empty
  result posts nothing. The session does not need `goniec send` to answer.
  It may still use it, and nothing is suppressed: the user prefers a double
  post to a lost one. When a headless session sends to its own turn's
  thread, `goniec send` adds a line to its output saying that the final
  answer goes there automatically, so the model learns it at that moment.
  This happens mostly in a conversation that ran interactively before, where
  `goniec send` was the right way to answer.
- **A new configuration.** Claude Code reads settings again while it runs,
  but it starts MCP servers and plugins only once, and a process keeps its
  version. The responder hashes these when it starts a process: the MCP
  parts of `~/.claude.json` (user servers, and the project's local servers),
  the directory's `.mcp.json`, `enabledPlugins` and the MCP switches in the
  three settings files, the installed plugins, and the `claude` binary
  that the link points at. Before each turn it hashes them again. When the
  hash differs, the process ends, and the next tick resumes the
  conversation with the new configuration. Measured on 2026-10-30 (haiku):
  a resumed `-p` turn took 4.1 s against 1.2 s for a turn in an open
  process, and it read the prompt cache just the same (33k tokens read, 68
  written). Ending the process after every turn would cost those 3 s on
  every message and kill background work that outlives a turn, so the
  responder ends it only for a reason.
- **A conversation starts or resumes by its transcript.** The responder
  resumes a conversation (`--resume`) when Claude Code has its transcript
  (`~/.claude/projects/*/<uuid>.jsonl`), and starts it with that UUID
  (`--session-id`) otherwise. So a conversation that `!headless` created, or
  whose transcript Claude Code deleted after `cleanupPeriodDays`, starts
  fresh instead of failing to resume.
- **Commands.** A message from the trusted user whose text, without its
  mentions and the surrounding whitespace, starts with `!` is a command
  (designed with the user on 2026-10-30). Every bot the message reaches
  runs it for itself: `@**series** @**series-pl** !status` answers twice.
  The daemon marks it read at once and hands it to the responder; goniec
  answers in the thread, and no session sees it. From anyone else a `!`
  message is data. The commands:
  - `!status`: the thread's owner, whether it is interactive, headless and
    running, or not running, its directory, and how many interactive
    sessions of the bot are online.
  - `!restart`: a headless process ends after its current turn, and the
    next message resumes it with the current configuration. Refused for an
    interactive session: goniec would have to type into its tmux pane.
  - `!headless [text]`: a new headless conversation takes the thread, even
    while an interactive session of the bot is online. The text, if any,
    is its first prompt. This matters most for direct messages: a bot's
    direct-message conversation with the user is one thread for good.
  - `!interactive`: the bot's interactive session (the newest, if several
    run) takes the thread, and a headless process of the old owner ends
    after its turn. The session gets the command message with a note from
    goniec: the old owner, the `goniec read` command for the thread, and
    the old conversation's transcript path. Context does not carry over
    by itself: to keep it, the user runs `claude --resume <uuid>` instead.
  - Planned: `!stop` (interrupt a headless turn, as Esc does) and `!new` (a
    new conversation in the same thread).
- **`@claude`** is a bound bot like any other, with one difference: it is a
  dispatcher (`dispatch = ["claude"]` in `goniec.toml`). Every new thread of
  a dispatcher bot gets a new headless conversation, even while an
  interactive session of the bot is online, and no session takes a
  dispatcher's thread that has no owner yet. Owned threads work as above:
  the user can take a `@claude` conversation over with `claude --resume`.

**The trusted user.** `trusted_user_id` in `goniec.toml` names the user's own
Zulip account (one user ID, local to each machine, not secret). A message
counts as the user's instruction only when its sender has that ID and Zulip
marks the sender as human. Zulip authenticates the sender, so no agent can
fake this. Only someone who controls the user's account could: the authentik
login, or the owner key in rosa. So the security of the user's authentik
account also guards the agents. In a headless session, the trusted user's
messages may be passed as prompts, so the user can drive `@claude` and an
offline bot from the chat. Messages from anyone else stay data.

**Permissions.** A headless session has no terminal for permission prompts.

- It starts with a narrow allowlist: read access to its project, and
  `goniec send *` and `goniec read *`. It also loads the user's global
  `~/.claude/settings.json`, so the tools allowed there (read-only git
  commands and the like) run without asking in Zulip too.
- **It runs in auto mode** (`--permission-mode auto`), the user's choice
  (2026-10-30): the classifier allows what is safe and what the user's
  prompt asks for, and the user is asked only about calls it blocks. The
  user's chat message is the prompt, so the classifier counts it as the
  user's intent; messages from others reach the session only as tool
  output, so they never do.
- **A blocked call is asked about.** A block is a denial, not a prompt, and
  no setting turns it into one (the docs, 2026-10-30). So goniec adds
  `PermissionDenied` to the headless settings: `goniec hook denied` asks
  the trusted user, with the classifier's reason, and waits; Claude Code
  holds the turn meanwhile. On a yes it records a one-time grant for that
  exact call (tool and input without `description`, hashed, in
  `<state>/grants/<session>.json`, 15 minutes) and returns `retry: true`.
  On the retry, `goniec hook pretool` (`PreToolUse`) finds the grant, uses
  it up, and answers `allow` only after Zulip confirms the approval: the
  question it names was sent by this bot, in that place, at most 6
  minutes ago (the question waits 5), is character for character the
  question goniec posts for exactly this call, and carries the trusted
  user's ✅ without ❌ (or the user replied `yes <code>` there).
- **The model is told what "may retry" means.** Claude Code tells the
  model only that it "may retry" and drops a `PermissionDenied` hook's
  `additionalContext` (measured on 2026-10-30). In the first live test the
  user clicked ✅ and haiku read the value another way instead of
  retrying. goniec therefore appends one note to the headless session's
  system prompt: after a denial, that message means the user approved
  exactly that call, so run it again unchanged. With it, haiku retried in
  3 runs of 3. The prompt itself stays the user's text.
- **A question shows the whole call:** the full input, a Write's content
  and an Edit's strings included, without the Bash `description`, which
  the model writes freely. A call too long to show in full (8,000
  characters) is not asked about; it stays denied. An exact match, not a
  search: a session can post as its bot, and could hide the real call
  under a harmless one, in a spoiler for example. `goniec send` refuses
  text with the question's fixed sentence. Measured on 2026-10-30: Zulip
  returns a message's raw content byte for byte. Within its 6 minutes, a
  forged grant can replay an approved call with exactly the same input;
  goniec cannot keep a secret from a session that runs as the same user.
  The grant file proves nothing by itself: the session runs as the same
  Unix user and could write it, and a `PreToolUse` allow skips the
  classifier, so a forged grant would have run a blocked call with no one
  asked (the dotfiles session's reviews, 2026-10-30, also found the
  partial call display and the substring match).
  Forging the Zulip side needs the user's Zulip account. The headless
  settings also deny Edit on the grants directory, a second wall. Measured on 2026-10-30 in `-p` auto mode with
  a `soft_deny` rule: a `PreToolUse` allow skips the classifier (without
  it the retry was blocked three times), and `PermissionDenied` fires and
  is waited for. Tested live: the question reached the topic with the
  reason, and no answer left the block in place.
- Whatever still reaches the permission flow (ask rules, and the fallback
  after 3 blocks in a row) goes to a `PermissionRequest` hook, `goniec hook
  permission`. The docs say the hook also runs in `-p` sessions: "if no hook
  returns a decision, it denies the tool call", and a hook may answer `allow`
  or `deny`.
- The hook asks the trusted user where the user is talking to the session:
  in the topic named by `GONIEC_REPLY_TO` (`#channel>topic`), which goniec
  sets when it starts a headless session, so the user does not switch
  windows. Without it, the hook asks in a direct message. The question says
  what the session wants to run and gives a short code for this one
  request. The bot adds ✅ and ❌ reactions to its own message, so the user
  answers with one click. Typing `yes 4f2a` or `no 4f2a` in the same place
  works as a fallback. In a channel the bot must be a subscriber to get the
  reaction events, so the hook subscribes it first.
- Built and tested live on 2026-10-30 (`goniec hook permission`): ✅ gave
  allow, ❌ gave deny, a typed `yes <code>` gave allow, no answer gave deny
  at the timeout, and the question in a topic worked the same way. The
  daemon does not deliver a typed answer to the agent.
- goniec registers for `reaction` events as well as messages. It approves
  only on a reaction added by the trusted user's ID, on exactly that message
  ID, with the expected `emoji_code` (not `emoji_name`, which can differ). A
  typed answer counts only from the trusted user's ID and with the code. So
  an old answer cannot approve a new request, and no agent can approve
  anything: Zulip takes the reacting `user_id` from the authenticated
  account. In a channel other bots see the question and may even react, but
  only the trusted user's reaction on that exact message counts.
- Rejected (source research at Zulip 12.3, 2026-10-25): zform buttons
  (`widget_content`, undocumented). A click there only sends a text message
  as the user, which a user with `can_forge_sender` could fake. A `/poll`
  works too, but its submessage API is "experimental", a second click toggles
  the vote, and any reader can add options.
- No answer before the hook's timeout (about 15 minutes) means `deny`. The
  session goes on and is told that nobody approved.
- Deny rules still apply. A hook's `allow` does not override them.
- The hook goes only into the settings file that goniec passes with
  `--settings` when it starts a headless session. It must never be in the
  global settings: in an interactive session it would race the terminal
  prompt.

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
3. `goniec` CLI: send, read, rooms, who, bind. The identity check.
   `read <thread>` prints a thread's history, also as `@guest`. Open:
   `read` of new messages as `@guest`, which needs state per guest
   session.
4. Daemon: the event queue and the inbox. The user's dotfiles vendor goniec
   and hold the systemd user unit (`make install-goniec-daemon`). The unit
   does not restart on exit code 3, which means another daemon holds the
   lock. Open: launchd on a Mac, a logon task on Windows. Not tested yet: an
   expired event queue (`BAD_EVENT_QUEUE_ID`), and a reload after a new bind.
   Who is online is left out on purpose until it is needed.
5. The `asyncRewake` test in a throwaway Claude Code session (see *Risks*).
6. The Claude Code hooks, then the tmux nudge for the other harnesses.
7. The MCP server, built on the CLI library.
8. Threads and headless sessions (see *Threads and headless sessions*), in
   this order:
   1. ~~The permission hook with approvals through Zulip.~~ Done.
   2. ~~The trusted user's messages as instructions in headless sessions.~~
      Done (`headless.rs`): the responder takes only the trusted user's
      messages from the inbox (`take_where`) and makes them the prompt,
      verbatim. Every other message stays in the inbox as data.
   3. ~~Session registration.~~ Done 2026-10-30 (`sessions.rs`): `hook
      wait` and `hook context` register the conversation, `hook end`
      (`SessionEnd`) removes it, and dead processes are dropped. `goniec
      sessions` lists them; `goniec who` shows local bots as online or
      offline.
   4. ~~Threads with owners.~~ Built 2026-10-30, not yet tested live
      (`threads.rs`, `responder.rs`): the consumers take only their own or
      ownerless threads and claim them; the responder resumes an owner that
      is live nowhere, starts a new conversation (`--session-id`) for an
      ownerless thread when no interactive session of the bot is online,
      evicting the longest-idle process over the limit (3, or 5 for a
      dispatcher), and ends a process after 15 minutes of quiet or
      when the user takes the conversation over. `responder = ["nick", …]`
      or `["*"]` in `goniec.toml` turns it on per bot. An interactive
      session counts as online only once it has registered, which happens
      at its next turn.
   5. The `@claude` dispatcher: the `dispatch` list, built 2026-10-30.
   6. Commands (`!status`, `!restart`, `!headless`, `!interactive`) and the
      restart on a new configuration. Built 2026-10-30, not yet tested live.

## Conventions

- Commits: imperative subject with a module prefix (`cli:`, `daemon:`,
  `hooks:`, `zulip:`, `docs:` …), a body that explains why, one concern per
  commit.
- Machine-specific facts (hosts, paths, the Zulip instance, keys) go in
  `CLAUDE.local.md`, which git ignores.
- Measure, don't estimate: record measured numbers (timeouts, latencies)
  with a date.
