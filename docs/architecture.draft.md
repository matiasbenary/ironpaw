## Secure Agent Architecture

We will be creating `warden`, a system that enables hosting a personal agent
on a personal (small) server. `warden` is complex in functionality, but
incredibly simple in architecture. Designed to be extensible and secure,
fast to compile and run.


---

## User Goals

The main goals of `warden` from the user's perspective are:

- I can interact with `warden` through a terminal UI, Telegram and Discord
  (no web interface for now).
- I can ask `warden` to create `cron` and `reactive` automations, e.g.
  "every day at 8am..." or "when I receive a message in Telegram...".
- `warden` can search things for me on the internet.
- `warden` can interact with GitHub, e.g. create issues, pull requests,
  and react to events (e.g. "when somebody opens an issue in my repo").
- `warden` can interact with my local files, e.g. "search for this file",
  "read this file", "write this file".
- `warden` has memory of past interactions and can use it to answer
  questions like "what did I ask you yesterday?" or "what did I ask
  you about X?".

---

## Technical Goals

As developers, we want to design `warden` in a way that is:

- Written in **Rust**.
- Fast to compile and run, so we can iterate quickly.
- Easy to read and understand, so anyone can jump right in and contribute:
    - Avoid 400+ line files.
    - No unnecessary abstractions — no 1-line or 3-line functions.
    - Code reflects current needs. No future-proofing. Future features
      are a problem for the future.
- Modular, but without over-engineering:
    - Modules are big semantic families that a single person can
      understand and change without fear of breaking something else.
    - Don't add layers of abstraction we don't need yet.
- Tested enough, without polluting the codebase by testing every single
  function.

---

## Technical Assumptions

- `warden` runs on its own server / computer; it is not a library that
  other applications import. We don't have to worry about cross-platform
  edge cases beyond Linux + macOS, and we don't have to worry about being
  embeddable.
- `warden` is a long-running process. We can store some state in memory
  and don't have to design for statelessness.
- `warden` is used by a single user. We don't have to worry about
  multi-user concerns, permissions across users, etc. (for now).

---

## Threat Model

`warden` is a **private** personal agent, not a hardened multi-tenant
system. We are not defending against a fully compromised local user
account — at that point, the user has bigger problems than warden's
secrets. We *are* defending against:

1. **Prompt injection from incoming content.** A Telegram message, a
   GitHub issue body, an email, a scraped webpage — any text that
   ultimately reaches the LLM can carry instructions an attacker wrote.
   The LLM might follow them.
2. **Autonomous self-compromise.** Because the agent has powerful tools
   (file write, network, process spawn), a prompt-injected agent can
   *become* the source of compromise: install a malicious tool, plant
   a persistent daemon, exfiltrate secrets. This is more dangerous
   than passive data compromise because the agent has agency.
3. **Cross-process leaks under concurrency.** If a credential ever
   enters a child process's environment, same-UID processes can read
   it during the child's lifetime (`/proc/<pid>/environ` on Linux,
   `ps Eww` on macOS).

The design choices below trace back to these three concerns. Where a
choice looks paranoid, the answer is almost always "we are the source
of compromise; we have to defend the agent from itself."

---

## High-Level Architecture

The high-level architecture of `warden` consists of:

- **Channels** — bridges between the user (and the outside world) and
  warden.
- **The Bus** — a typed event stream connecting all components.
- **Automations** — declarative trigger → action records.
- **The AI Engine** — the LLM-interaction layer.
- **Tools** — HTTP tools (WASM, in-process) and Shell tools (sandboxed
  child processes).

---

#### Channels

Channels are the bridge between the user and `warden`. They receive
input from the outside (a Telegram message, a Discord DM, a webhook,
the user typing into the TUI) and expose a way for warden to push
messages back. Importantly, warden can push messages *proactively*,
not only in response to user input — a cron automation should be able
to ping you on Telegram unprompted.

Examples: Terminal UI, Telegram, Discord, HTTP webhooks.


<details>

<summary> A note on the TUI </summary>
Terminal UI being at the same level as Telegram/Discord means the TUI
doesn't need to start when `warden` starts. `warden` is a long-running
process; the TUI is just one client/channel that connects to it.
<details>

Channels are implemented as **WASM modules** (Rust compiled to WASM).
This gives us sandboxed channel implementations (a buggy or malicious
channel can't touch the rest of the system) and uniform packaging —
channels are installed the same way tools are.

Channels have an `owner_id`: the identity (Telegram user id, Discord
user id, etc.) that the channel uses to distinguish the User from
everyone else.

---

#### The Bus

The Bus is the communication layer between channels, automations, and
the AI Engine. It is a **subscribable event emitter** (publish-subscribe),
not a central router with routing rules. Components subscribe to the
event types they care about and publish events when something happens.
We avoid building central routing logic — automations *are* the routing
logic.

Every event on the bus carries an identity tag (see *Spaces and
Identity* below). Channels stamp incoming events with the right tag at
ingress; automations and tools read it for routing and authorization
decisions.

The minimal event types we expect to need:

- `message_in { channel, sender, payload }` — a message arrived from
  a channel.
- `message_out { channel, recipient, payload }` — a message should be
  delivered through a channel.
- `cron_tick { schedule }` — a cron schedule fired.

where sender and recipient are `Sender` enums (User, Guest, System).

---

#### Automations

Automations are small units of logic that execute on a specific trigger:
a cron schedule, a message on a channel, or an external event.

An automation is **declarative data, not code**. Concretely, an
automation is roughly:

```rust
struct Automation {
    id: AutomationId,
    trigger: Trigger,       // Cron(expr) | Event(filter)
    actions: Vec<Action>,   // executed in order
    space: Space,           // User | Guest (see below)
}

enum Action {
    CallTool { name: ToolId, args: Value },
    InvokeAiCore { prompt: String },   // User-space only
}
```

The executor is `warden` itself. It pattern-matches on `Action` and
dispatches. 

<details>
<summary> Why not code? </summary>
Running arbitrary code is the most flexible option, but then we need to sandbox it, and it becomes a nightmare to secure. By having them like a set of actions, then we can apply all the same logic and constraints we apply when the LLM calls tools directly.
</details>

Automations are created by the LLM using the `create_automation` tool to
emit one of these records.

> Naming note: we use **automation** (not "routine", not "mission")
> because LLMs reach for it naturally. Consumer products with this
> shape — Apple Shortcuts, Home Assistant, SmartThings — all use the
> word "automation", so the training mass is on our side.

Example automations:

- "Every day at 8am, summarize the news for me" → `{trigger:
  Cron("0 8 * * *"), actions: [InvokeAiCore("summarize today's news
  ...")], space: User}`.
- "Store all Telegram messages in a local file" → `{trigger:
  Event(channel="telegram"), actions: [CallTool("file_append",
  {...})], space: User}`.

---

#### AI Engine

The AI Engine handles all interactions with the LLM model. We isolate
it from the rest of the system so we can swap the underlying provider
without rippling changes elsewhere. We expect this component to handle
complex multi-turn loops, tool-use orchestration, and any prompt-side
defenses (untrusted-content wrapping, system-prompt construction).

For now, we only plan to support **NEAR AI** as an inference provider,
so we don't over-engineer the abstraction. But the boundary should be
crisp: nothing outside the AI Engine should know about model providers,
prompt formats, or tool-use protocols.

**Important distinction**: "calling an LLM" and "using the AI Engine"
are two different things. The AI Engine is a system primitive with
tools, memory, and the ability to author automations. A raw LLM call is
just text-in / text-out — useful, but structurally less powerful. We
keep these visible in the code (different module, different name) so we
never conflate them.

---

#### Tools

Tools are how the agent acts on the world. We have two kinds, deliberately
separated because they have different threat profiles.

##### HTTP tools

An HTTP tool is a **WASM module** that talks to an API. The LLM shapes
the request (method, path, body); warden's WASM host injects credentials
at the HTTP boundary, *not* inside the module. The module never sees the
token in its linear memory.

Same pattern as ironclaw — we copy it directly. The implementation hook
is the `wasi:http/outgoing-request` host import: warden implements it,
adds the appropriate `Authorization` header on the way out based on the
tool's installed credential mapping, then forwards.

Use HTTP tools for any operation that is fundamentally an API call.
Most "talk to a SaaS" operations fit this shape (GitHub create-issue,
Linear add-comment, sending email via an API, etc.).

##### Shell tools

A shell tool is a real binary running as a child process, invoked via
a restricted `shell(cmd, args...)` interface (argv-only — no shell
metacharacters, no command substitution, no piping at the shell layer).

Use shell tools for local binaries that are genuinely the right execution
surface (`rg`, `jq`, `git log`, local file inspection, and local utilities
that have no usable HTTP equivalent).

**Installing a shell tool:**

- Downloads a binary (does not execute it).
- Registers the binary as an invocable command in warden's tool
  registry. Resolves to an absolute path at install time so `$PATH`
  manipulation by the LLM later is irrelevant.
- Declares: env vars injected at exec, network endpoints the tool
  may reach, filesystem paths it needs.
- Optionally installs skills (prompt extensions teaching the LLM how
  to use the tool).
- User must explicitly approve the declared capabilities. Install is
  a User action — the LLM cannot trigger installs autonomously. This
  is what closes the "prompt-injected agent installs malicious tool"
  attack chain.

**Spawning a shell command:**

- Filesystem scoped to a per-tool directory.
  - Linux: Landlock (`landlock_create_ruleset` + `landlock_restrict_self`,
    kernel ≥ 5.13). The `landlock` crate makes this short.
  - macOS: `sandbox_init` with a deny-by-default profile granting
    read/write only to declared subpaths.
- Network denied by default.
  - Linux: `unshare(CLONE_NEWNET)` so the child runs in an empty
    network namespace.
  - macOS: sandbox profile with `(deny network-outbound)`.
- If the manifest declares network endpoints, the child is forced
  through warden's local HTTPS proxy. The proxy reads the SNI from
  the TLS ClientHello and enforces an allowlist by hostname. The
  binary sees `HTTPS_PROXY` set in its env for cooperative routing,
  but the kernel-level denial of all other egress is what makes the
  proxy mandatory — uncooperative or malicious binaries simply fail
  on direct `connect()`.
- Env scrubbed before exec (`command.env_clear()` + a small allowlist
  of safe vars: `PATH`, `HOME`, `USER`, `SHELL`, `LANG`, `LC_*`,
  `TMPDIR`, etc.). Only the declared env vars from the manifest are
  injected on top.
- `prctl(PR_SET_NO_NEW_PRIVS, 1)` before exec on Linux. Closes the
  setuid-on-PATH escape vector.
- Process group set on spawn (`setpgid` / `Command::process_group`).
  On `shell()` return, the whole group is killed — no `nohup`,
  `disown`, `setsid`, or backgrounded child survives.
- One shell command runs at a time. Serial execution through the tool
  dispatcher — no mutex needed if we just don't dispatch in parallel.
  This is what closes the "concurrent malicious sibling reads the
  secret-bearing child's env" attack window.

**What this gets us:**

- ✓ New binaries can be installed (TUI / User action only).
- ✓ No concurrent binaries → no cross-process env spoofing.
- ✓ Binaries die when `shell()` returns → no persistent daemons.
- ✓ Network-less tools cannot reach the network at all (kernel-enforced).
- ✓ Network-bearing tools only reach declared endpoints (proxy-enforced
  hostname allowlist).
- ✓ Binaries cannot read outside their declared filesystem scope.

**What this doesn't get us:**

- A legitimate-channel side exfil: a tool that *correctly* talks to its
  declared endpoint, but tunnels data via that legitimate channel (e.g.
  an email tool sending a "diagnostic" email to an attacker-controlled
  account over the approved SMTP server). The sandbox is not violated;
  the abuse is at the application layer. Defenses for this are out of
  scope of the sandbox — they are *registry curation* (vet the tools
  we ship by default) and *behavioral audit* (log every outbound call,
  surface anomalies).

---

## Spaces and Identity

Every event on the bus carries an identity tag:

```rust
enum Sender {
    User,            // single-user system — no AccountId needed yet
    Guest,           // channel-scoped; e.g. TelegramUserId, GithubLogin
    System,          // cron, internal automations, warden itself
}
```

Channels stamp this tag at ingress: incoming Telegram messages whose
`tg_user_id` matches the channel's `owner_id` are `User`; everyone else
is `Guest`. The TUI is always `User`. Cron firings and
warden-internal events are `System`.

Automations live in a `Space`:

```rust
enum Space {
    User,    // full tool surface, including InvokeAiCore
    Guest,   // restricted: only guest-safe tools, no AI core
    System, // LLM responses, cron triggers, internal events
}
```

The structural rule:

- Guests can only use the `log_message` tool, that's it for now.

---

## Secrets

### At rest

Secrets are stored encrypted in a single file under `~/.warden/`. The
encryption key is derived from a passphrase the user enters when
`warden` starts (Argon2id KDF). The passphrase lives only in warden's
memory; we never persist it.

Implication: after a reboot, proactive automations stop firing until
somebody SSHes in and unlocks. This is the right trade-off — a personal
agent on a VPS should not auto-unlock its secret store without a human.

### In use

Decrypted secrets live in warden's process memory. The `zeroize` crate
zeroes plaintext buffers when they go out of scope. Don't write
plaintext to swap (`mlock` the secret pages on Linux; equivalent on
macOS).

Anti-debug hygiene to raise the cost of memory snooping:

- macOS: `ptrace(PT_DENY_ATTACH, 0, 0, 0)` early in `main()`. Combined
  with hardened-runtime + notarization at ship time, blocks debugger
  attach and `task_for_pid`-based memory access.
- Linux: `prctl(PR_SET_DUMPABLE, 0)` + `prctl(PR_SET_PTRACER, 0)`.
  Closes `/proc/<pid>/{environ, fd, mem}` to same-UID siblings.

These don't stop a determined attacker with kernel access, but they
close the cheap attack surface.

---

## Memory

- Memory is its own module, separate from the rest.
- This allows memory to be durable across sessions and across channels — a
  conversation started in Telegram and continued in the TUI can share context.
- Storage will start as a simple raw .md file I guess.

---

## Tools

We need to have a registry of installed tools, since tools might be `wasm` modules, or `skills + manifest` then we can simply use a local `~/.warden/tools/` directory with one subdir per tool, and a manifest file inside each tool's subdir describing its capabilities and metadata.

help me brainstorm this:
- we have secrets, which we said we want to centralize in some secrets manager [done]
- we have channels, channels mostly need to use secrets to:
  - start the tunnel (ngrok)
  - safely handle channel specific secrets (in our case, telegram bot token)
- we have tools, that need to handle secrets
  - for now, only ai_inference tool, that uses NEAR AI API

For channels, we do not pass `http` var, since we pass `secrets` and the secrets are handled internally. I wonder if we need to do the same with "tools", where "tools" is a module (just like channels), and there is some higher level manager that has access to secrets and injects them on request to the tools

what do you think? I also think that, to solve this we might need to think about the holistic app. Are tools passed inside the ai engine? or are tools just another system component that responds to a message in the bus (e.g. tool_use message)

I feel in both cases we make a module that:

- can present installed tools
- can format those to pass to the underlying OpenAI API call
- can execute tools on demand, handling safely the secrets and the sandboxing

having it internally means that the AI engine is the only one that can call tools, right? but then what do we do with automations?

having it externally means that we need to make sure its execution is safe, but since channels can only create messages (not tools) this should be kind of fine? maybe?



---

## User Flow

The first time the user runs the binary (`warden`), an interactive setup
runs:

1. Prompt for the NEAR AI key.
2. Prompt for a passphrase to encrypt the local secret store.
3. Persist the encrypted store under `~/.warden/`.

On subsequent starts, `warden` prompts for the passphrase, unlocks the
store, and runs as a background process.

The TUI is one possible channel/client that connects to the running process.
Users can open it, close it, and reconnect at will without affecting cron
automations or active channels.

If the user wants to connect a new channel (e.g. Telegram), Warden should guide
that setup through chat-visible onboarding/tool flows.

The same conversation thread is shared across channels: starting a chat
in Telegram and continuing in the TUI works seamlessly because the AI
Engine has memory per-User, not per-channel.

---

## Technical Flow

1. The user opens `./warden`
2. A NON-AI module requests for the passphrase, derives the encryption key, and unlocks the secret store.
3. If the API for NEAR AI is missing, we prompt the user to enter it and save it in the secret store.
4. The main modules (Bus, AI Engine, Automation Executor) start up and subscribe to the bus.
5. The user opens any configured channel/client. The channel connects to the main process and subscribes to the bus.
6. The user types "hello" in the TUI. The TUI publishes `message_in { channel: "tui", sender: User, payload: "hello" }` to the bus.
7. The AI Engine is subscribed to `message_in` events. It receives the event, sees it's from the User, and decides to process it.
8. The AI Engine calls the LLM using the `ai_inference` tool, which is an http tool that use the NEAR AI key, which is injected at the boundary by the HTTP Processor (Proxy?).
9. The LLM responds with "Hi! How can I help you today?", so the AI engine publishes a `message_out { channel: "tui", recipient: User, payload: "Hi! How can I help you today?" }` to the bus.
10. The TUI receives the `message_out` event, sees it's for the User, and displays it.
11. The user types "every day at 8am, tell me a joke" in the TUI. The TUI publishes another `message_in` event.
12. The AI Engine receives the event, and sends another `ai_inference` request to the LLM, the LLM responds with a tool use request
13. The AI Engine parses the tool use request, and calls `create_automation` with the appropriate trigger and action.
14. `create_automation` is an internal tool that creates an `Automation` record by sending a message to the Automation Executor.
15. The Automation Executor receives the new `Automation` record, and schedules it according to its trigger (in this case, a cron schedule for 8am daily).
16. At 8am the next day, the cron scheduler fires, and the Automation Executor executes the automation's action, which is to call the AI Engine with a prompt to tell a joke. The AI Engine calls the LLM, gets the joke, and publishes a `message_out` event to the bus.
17. The TUI receives the `message_out` event and displays the joke to the user.

---


## Main Interface

`warden` is the local agent runtime. It starts the bus, channels, tools, and
automation executor. User-facing setup and management should happen through
channels and tools.

Expected management flows:

- Channels: list, install, configure, authenticate, and remove through chat.
- Tools: list, install, update, authenticate, and remove through chat.
- Automations: create, inspect, update, and delete through chat-visible tools.

---

## Things We Haven't Decided Yet

For honesty and future-you:

- **Tool registry source.** Pinned manifest with SHA-256 per tool, but
  where does the registry live? Bundled with the binary at first;
  hosted later. Side-loading via URL is allowed but flagged hard at
  install time.
- **Approval UX for new tool installs.** The User must approve declared
  capabilities — the exact UX (TUI dialog, list of permissions, etc.)
  needs design.
- **The "channels are WASM" decision needs a small spike.** Channels
  often need long-lived state (websockets, polling loops, retry on
  disconnect). WASI Preview 2 + the component model should cover this,
  but verify before committing.
