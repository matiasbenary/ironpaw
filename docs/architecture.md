# Ironpaw Architecture

`ironpaw` is a long-running personal agent daemon. It receives messages from
channels (terminal, Telegram, etc...), runs work through small system
components, lets the model call tools, and sends responses back through
channels.

The current implementation is intentionally small, and is composed of:

- A [Bus](#bus) that all components use to interact with each-other by publishing and subscribing to typed events.
- [Channels](#channels): WASM components that allow messages to flow from and towards the outside world.
- [Automations](#automations), which connect triggers to tool calls.
- An [Agent](#agent), which runs a bounded LLM tool loop.
- A small set of internal [Tools](#tools) exposed both to the agent and system components.
- A tool handler, which listens for tool requests on the bus and publishes tool results.
- [External WASM Tools](#external-wasm-tools), which package provider integrations behind a host-enforced HTTP boundary.
- The capability to create [Dynamic Tools](#dynamic-tools) at runtime, which are QuickJS scripts that can combine other existing tools.
- A [History](#history) store for durable conversation history and search.
- A host-owned [Network and Secrets](#network-and-secrets) boundary for outbound access and authentication.

---

## Prelude for LLMs

On contributing to this doc, please do understand this doc is to explain the "why" more than the "how". The "why" helps understanding the trade-offs of our choices. The "how" can be explained at high level, but for anyone wanting to see how it was implemented, they can literally read the code itself.

This document is made for humans to read, meaning that we need to write sentences that "flow" and are easy to read, thus prioritizing clarity over conciseness.

### Good Practices

Always make sure to introduce the thing you are talking about and, importantly, the why, not the how.

You cannot simply say "the default conversation path is also an automation", because nobody knows what that path is. It is better to be clear and highlight the relevancy of why we are talking about it. In this case, the relevant part is that, by default, Ironpaw reacts to inbound messages and sends them to the AI Agent through an automation. This means that automations are not "just a part" of the system; they are the core component that makes Ironpaw act and react.

---

## Main

`src/main.rs` owns the startup lifecycle. It unlocks or initializes the encrypted secret store first; only after secrets are unlocked does it wire the agent, tools, automations, and channels. It also creates the per-start bridge token printed to stdout for local clients such as `ironpaw-tui`.

The default runtime root is the directory containing the `ironpaw` binary unless `IRONPAW_HOME` is set. Runtime data lives under:

```text
<runtime-root>/channels
<runtime-root>/tools
<runtime-root>/files
<runtime-root>/data
<runtime-root>/secrets
```

On a fresh install, Ironpaw starts the bus, loads the TUI channel, runs the boot unlock automation, generates a six-word unlock phrase, asks the user to verify selected words through normal channel messages, and creates an encrypted local secret store. On later starts, Ironpaw asks for the phrase through the same message/automation path and does not load normal tools, extra channels, or stored automations until the store unlocks. Unlock is handled by an internal tool and asks the active channel to delete sensitive inbound messages after they are consumed.

Once unlocked, Ironpaw installs a transient onboarding automation if required setup secrets such as `NEAR_API_KEY` are missing. It exists only for that process lifetime; a later startup recreates it whenever setup is still required.

An owner can run `/change_api_key` from any installed channel. The bridge converts that command into `ApiKeyChangeRequested` without adding it to conversation history. A transient automation pauses normal conversation handling, collects and validates the replacement, restores the previous key if validation fails, then returns control to `handle-conversation`.

After onboarding, it sets `handle-conversation` as the default automation, which routes inbound messages to the agent and sends answers back to the user through the same channel. Web search is an external WASM tool installed by default into the runtime tools directory; its Brave key remains optional until configured.

---

## Bus

The bus is the smallest shared coordination primitive in Ironpaw. It is a thin wrapper around a Tokio broadcast channel, which allows multiple subscribers to receive the same events.

It exposes two main methods:

- `publish(Event)`
- `subscribe()`

Implemented event types are:

- `MessageIn { channel, sender, conversation, message_id, message }`
- `MessageOut { channel, recipient, conversation, message }`
- `DeleteMessage { channel, message }`
- `ChannelAction { channel, recipient, conversation, action }`
- `UserInputRequested { conversation, request }`
- `AgentRequest { request_id, destination, request }`
- `AgentResponse { request_id, response }`
- `Automation { id, action }`
- `AutomationResult { id, result }`
- `ToolRequest { context, tool_calls, allowed_tool_names }`
- `ToolResponse { context, tool_call_ids, result }`
- `ToolResult { context, tool_call_id, tool_name, result }`

### Message Types

Ironpaw's bus, history, and agent use Ironpaw-owned message types:

```rust
enum Message {
    System(String),
    User { text, attachments },
    Assistant { text, attachments, tool_calls },
    ToolResult { tool_call_id, tool_name, result },
}
```

Attachments are local files identified by a filename, with their MIME type and local source path. They can appear on user messages received from channels and assistant messages produced for outbound delivery. The bus carries the same `Message` type in both directions; channel-specific payloads are only produced by the channel bridge.

#### Image and File Management

Ironpaw handles files as first-class message attachments: images, PDFs, text files, and eventually other documents. The core agent sees the same shape regardless of whether a file came from Telegram, TUI, image generation, or another channel.

Ironpaw stores files locally and passes around typed attachments containing a filename, MIME type, and source path. User messages and assistant messages both use the same attachment type on the bus; channels translate their provider-specific payloads into attachments on the way in, and the bridge translates attachments back into provider-ready outbound payloads on the way out.

The model interacts with stored files through tools. `download_file(url)` stores a public URL in the current execution's directory and returns its filename without placing the response body in conversation history. It recognizes public Google Sheets, Docs, and Slides editor links and exports them as CSV, text, and PDF respectively. `inspect_file(filename, query)` can inspect files from the current execution or the shared inbound directory, passing bounded raw text to a text model, extracting text from PDFs, or calling a vision model for images. `image_generation(prompt, filename)` stores the generated image in the same execution directory; `send_message` can then publish an assistant message with that filename as an attachment.

Inbound attachments share the root file directory. Tool-created files use a separate directory for each conversation or scheduled automation. Calls made by a dynamic tool through `read_file`, `write_file`, `edit`, or `search_files` instead use that tool's private directory. The host derives these paths from `ExecutionContext`; relative paths only are accepted, and symlinks are rejected. Scope and tool names are encoded before becoming directory components.

Filenames are sanitized before storage. Existing names receive a numeric suffix such as `report-2.pdf`, and tools that generate files must propose a descriptive filename.

---

## Channels

Channels are WASM components loaded dynamically at startup from:

```text
<runtime-root>/channels/*.wasm
<runtime-root>/channels/*.config.json
```

Each channel has a `.wasm` file with its logic, and a matching `.config.json` file with its channel-specific config.

The channel runtime does five jobs:

1. Bind to a local HTTP bridge.
2. Optionally start ngrok for channels that need to receive external webhooks.
3. Discover and load channel manifests from the runtime `channels` directory.
4. Start each WASM channel with `on_start(config_json)`.
5. Run the [bridge](#the-bridge).

Channels do not receive raw secrets directly. They use the host-provided `http_request`, `http_request_base64`, and `http_multipart_base64` imports. The host wraps them with `NetworkGate<ChannelSecrets>`, allowing configured credentials to be injected at the HTTP boundary.

<details>

<summary> Adding Channels </summary>

Channel installation is partly implemented inside the agent through `list_channels`, `install_channel`, and `setup_channel`. `install_channel` starts the install flow and registers a temporary automation. That automation calls `setup_channel` for follow-up replies. `setup_channel` discovers local channel packages, asks deterministic setup questions, stores required secrets in the encrypted secret store, copies the WASM/config into the runtime `channels` directory, and activates the channel in the running channel runtime.

</details>

<details>

<summary> Conversations And Messages </summary>

Channels tend to have some concept of a chat, room, DM, or thread where the user talks with Ironpaw. Ironpaw calls that a conversation.

Conversation identity is deliberately separate from message identity. A conversation is the chat-like place where a transcript lives:

```rust
struct ConversationKey {
    channel: String,
    chat_id: String,
    thread_id: Option<String>,
}
```

For Telegram, `channel` is `"telegram"`, `chat_id` is Telegram's chat id, and `thread_id` is the optional forum topic id. For TUI, `channel` is `"tui"` and `chat_id` is the callback/session id used to send messages back to the local client.

A specific provider-owned message is represented separately:

```rust
struct MessageRef {
    conversation: ConversationKey,
    message_id: String,
}
```

This keeps impossible states out of the bus. Sending a message targets a conversation. Deleting a message targets a message reference.

</details>

### The Bridge

The bridge has two directional halves:

- `message_channel_to_ironpaw`: HTTP request from a channel provider/client into Ironpaw.
- `message_ironpaw_to_channel`: outbound bus events from Ironpaw into a channel WASM component.

#### Inbound flow

1. A channel provider/client posts to the bridge route configured by the channel.
2. The bridge authorizes the request according to channel config.
3. The bridge calls the WASM channel `handle_http(path, body, config_json)`.
4. The WASM channel emits zero or more channel messages with the host-provided `emit_message(message_json)` import.
5. The bridge validates ownership and channel identity.
6. Accepted messages become `Event::MessageIn`.
7. The bridge remembers the sender conversation as the channel `default_conversation`.

#### Outbound flow

1. The bridge subscribes to the bus.
2. It listens for `MessageOut`, `DeleteMessage`, and `ChannelAction` events addressed to the user.
3. For `MessageOut`, it resolves the explicit conversation or falls back to the channel `default_conversation`.
4. It serializes the event for the channel, adding base64 payloads for local attachments.
5. It calls WASM `on_outbound_event(event_json, config_json)`.
6. The WASM channel performs the provider-specific send/delete/action.

---

## Network and Secrets

Outbound access is owned by `src/network/`. Its layout reflects the boundary:

```text
network/
├── client.rs       # HTTPS, DNS pinning, redirects, timeouts
├── http.rs         # scoped host grants and secret injection
├── policy.rs       # public/private destination policy
├── proxy.rs        # CONNECT proxy for shell traffic
├── public.rs       # unauthenticated public downloads
├── oauth.rs        # OAuth exchange and refresh
└── email/          # provider-neutral IMAP/SMTP adapters and discovery
```

Code outside this directory requests network work; it does not construct
authenticated clients or open provider connections. A request is accepted only
for its declared host. The network boundary resolves and pins public addresses,
disables redirects, injects the declared header, URL, or OAuth credential, and executes
the request without exposing the secret to the caller.

Secrets are unlocked once at startup from:

```text
<runtime-root>/secrets/secrets.json.enc
<runtime-root>/secrets/secrets.meta.json
```

The encrypted payload is a JSON map of secret names to values. Metadata contains non-secret crypto parameters such as version, Argon2id settings, salt, cipher, and nonce.

They are exposed as separate namespaces:

- `secrets.channels`
- `secrets.tools`

Both namespaces read from the same unlocked encrypted store, but the API boundary is still separated so channel code does not ask for tool secrets and tool code does not ask for channel secrets.

NEAR provider configuration is loaded once into `ProviderConfig`. The chat, memory, file-inspection, and image-generation paths share its base URL, model names, and timeouts instead of reading provider environment variables independently.

`NetworkGate<S>` is the WIT-facing adapter for controlled credential injection. It:

- only allows HTTPS.
- restricts requests to configured hosts.
- resolves and pins public IP addresses and disables redirects.
- injects header, URL-placeholder, or refreshed OAuth credentials.
- keeps secret lookup at the host boundary.

The generic `web_fetch` and download paths use a separate, unauthenticated
public capability. It rejects plain HTTP except localhost development URLs,
rejects private/internal resolved IPs, and does not follow redirects.

The source packages currently checked out under `wasm-tools/` and
`wasm-channels/` are independent of the host crate. Each carries a local,
versioned WIT contract and may live and build in a different repository;
Ironpaw consumes only the resulting component and its manifest/config. Provider
secrets are names in that config, never values passed into WASM memory.

---

## Automations

Automations are the cornerstone of Ironpaw's behavior. They are small, declarative rules that explain how `ironpaw` reacts to internal (`cron`) or external (`event`) triggers.

Technically, an automation is defined as a triggering condition plus a tool call (be that an internal or dynamic tool). When the condition is met, the tool is called with pre-defined arguments that can contain information from the triggering event.

Ironpaw persists automation definitions as JSON files:

```text
<runtime-root>/data/automations/<name>.json
```

The agent does not author these files directly. `create_automation` and `edit_automation` accept a typed automation definition, validate it, persist it, and update the live runtime as one host-owned operation.

### Triggers

A trigger can be one of:

- `event`: a bus event, e.g. an inbound user message.
- `cron`: a recurring cron expression, evaluated in local time. Five-field expressions are accepted, along with 6/7-field expressions for second-level schedules.
- `at`: one-shot absolute time.

Once a trigger fires, the automation runtime publishes an invocation event. That invocation is translated into a `ToolRequest` or `AgentRequest`; the automation listener never waits for the work to finish. Tool and agent handlers execute those requests in independent tasks and publish their results back to the bus.

One-shot `at` automations are removed from memory and storage immediately after their invocation is dispatched. Tool invocations from cron ticks are dispatched independently. Agent-task invocations for the same automation are queued and run in order, so they do not concurrently mutate their shared automation workspace.

Inbound-message triggers can match all conversations, or a specific `ConversationKey`. Exact conversation matches take precedence over wildcard matches, so setup flows temporarily own only the conversation that started them while the default handler remains active elsewhere.

### Examples

When `ironpaw` starts without the required inference key, it installs a transient onboarding automation that reacts to inbound user messages with the `onboarding` tool. This tool asks for the NEAR API key, removes the transient onboarding automation, then installs the persistent default `handle-conversation` automation, which reacts to inbound user messages by calling the agent and sending the final answer back to the user.

```text handle-conversation
MessageIn(User, any conversation)
-> continue_conversation(current ConversationKey)
```

---

## Agent

The agent is the bounded tool loop: history + raw inference + tool execution. `AgentHandler` subscribes to `AgentRequest`, starts each run in an independent task, and publishes `AgentResult`. Conversation runs also publish their final response as `MessageOut`.

Agent runs keep their delivery destination separate from their history scope. A conversation run uses its `ConversationKey` for both, while a scheduled agent task has no implicit delivery destination and uses the automation name as its durable history scope. This lets recurring tasks remember previous runs without pretending that an automation is a channel conversation.

The tool registry records whether each tool is agent-visible or internal. Conversation agents receive a snapshot of its visible tools, so newly installed WASM tools and user-created dynamic tools become available immediately. Maintenance and setup-callback tools remain registered for internal execution without being exposed. Scheduled agent tasks can explicitly request any registered tool by name.

For each run:

1. Build model input from the system preamble, durable conversation history, and the new transient messages.
2. Attach schemas for all registered tools, internal and dynamic.
3. Use `raw_inference.execute(request)` to call the model.
4. If the model returns no tool calls, append the assistant message to history and return the final text.
5. If the model returns tool calls, send the calls to the tool handler. The handler validates and executes them serially, waits for deferred responders when necessary, and returns structured results. The agent then appends the original assistant message and all results to history as one tool turn.
6. The tool executor enforces `MAX_TOOL_CALLS`; further calls return a failure without executing.

When a run has a destination, the agent also publishes typing and progress events back to that channel.

`raw_inference` is both the agent's model-call primitive and a model-callable tool. This keeps provider formatting and credential handling centralized while still allowing higher-level model-to-model delegation.

---

## History

Ironpaw stores durable conversation and automation history in SQLite:

```text
<runtime-root>/data/history.db
```

History is partitioned by an explicit scope: either a `ConversationKey` or an automation name. The history module exposes scoped load, append, and compact operations, along with conversation convenience methods:

- `load(conversation) -> Vec<HistoryEntry>` - load active conversation history.
- `append(conversation, entries)`
- `delete_external_message(message_ref)`
- `compact(conversation, summary)` - replace active items with a summary while retaining originals for search.
- `search(query) -> Vec<HistoryEntry>`

Each history entry stores a Ironpaw message plus metadata: a Ironpaw id, its conversation or automation owner, and an optional provider-owned external message id.

Inbound user messages and delivered outbound messages are persisted once, by the bridge. The agent owns its internal transcript: it stores a model response containing multiple calls as one assistant message followed by all tool results in one history operation. The tool handler only executes calls, serially and in their original order. Direct automation tool calls use the same request path but do not create an agent transcript. Final scheduled-task text is persisted by the agent because it does not cross a channel boundary. Progress updates are ephemeral channel actions, not conversation messages.

Scheduled agent tasks persist their rendered prompt, tool transcript, and final assistant text under the automation's name. On later runs, the agent loads that history before inference. Tool results are bounded before durable storage so a fetched page or document extraction cannot consume the whole future context. Compaction runs before a later inference call, retains the current user request and recent tool transcript, and replaces only older context with an explicitly non-authoritative system summary. Automation history is isolated from other automations and conversations.

---

## Memory

History is Ironpaw's complete record of what happened. Memory is a smaller,
derived collection of facts that may be useful again across conversations. It
does not replace history and can be rebuilt from it.

After a conversation turn finishes, Ironpaw gives the recent messages and the
ten most relevant existing memories to the text model. The model extracts
self-contained memories such as preferences, relationships, exact identifiers,
decisions, and outcomes. Ironpaw embeds the extracted text with NEAR AI, removes
exact duplicates, and stores the memories and their embeddings in
`<runtime-root>/data/memory.db`.

Before a conversation run, Ironpaw searches the latest verbatim user message
with both dense similarity and SQLite FTS5/BM25. Reciprocal rank fusion combines
the two rankings without mixing their incompatible raw scores, and the ten best
memories are added to the model input as recalled context. Recall is automatic:
the model does not need to decide to call a tool. The existing history search
tool remains available when the model needs the original transcript or exact
evidence.

The first implementation deliberately has no vector server. Embeddings are
stored as SQLite blobs and compared in process because a personal agent is
expected to have a modest number of memories. Memory failures do not prevent
Ironpaw from answering; durable history remains the fallback and source of truth.

---

## Tools

In Ironpaw we currently distinguish between internal tools, external WASM tools, and dynamic ones.

### Internal Tools

Internal tools are async and model-callable. Every internal tool exposes:

- `name()`
- `tool_definition()`
- `execute(args, ctx, responder).await`

Each tool receives an `ExecutionContext` with a host-selected workspace, tool-call depth, and the shared call budget. Conversations and scheduled automations select their workspace before invoking a tool; tools do not branch on the invocation source. Internal tools share that workspace. Dynamic and shell tools receive a tool-specific directory beneath it. Result delivery is correlated by request id and remains separate through `ToolResponder`. Conversation and message destinations are action input, so tools such as `request_user_input` and `delete_message` require them explicitly.

Implemented internal tools include:

- `raw_inference`: calls `/v1/chat/completions` using the configured NEAR API key.
- `image_generation`: calls an OpenAI-compatible image generation endpoint, stores the generated image under the requested sanitized filename, and returns that filename for later delivery.
- `inspect_file`: inspects a locally stored file by filename, using text extraction for documents and a vision model for images.
- `download_file`: downloads a public URL into the current execution's file directory under its sanitized original filename for bounded inspection with `inspect_file`.
- `read_file`, `write_file`, `edit`, `search_files`: access the execution-scoped agent workspace.
- `web_fetch`: fetches a public URL with no credentials.
- `send_message`: publishes a message to a user through a channel.
- `delete_message`: deletes a channel message when the channel supports it.
- `request_user_input`: asks the current user to choose from specific answers. Telegram renders inline callback buttons; the selected value returns as a normal inbound user message.
- `create_tool`, `edit_tool`, `delete_tool`: create, replace, or delete dynamic JavaScript tools. The host validates the complete script, writes the tool files, and updates the registry as one operation.
- `create_automation`, `edit_automation`, `delete_automation`, `inspect_automation`, `list_automations`: validate, persist, and manage automations.
- `list_channels`, `install_channel`, `setup_channel`: inspect and install local channel packages. `install_channel` starts the install flow and registers a temporary automation; `setup_channel` handles deterministic follow-up replies from that automation.
- `list_tools`, `install_tool`, `setup_tool`: inspect and install local external WASM tool packages. `install_tool` starts the install flow and registers a temporary scoped automation; `setup_tool` handles the deterministic follow-up replies from that automation.
- `onboarding`: collects required setup secrets and installs the normal answer automation.
- `todo`: a small scratchpad-style tool.

### Shell Tools

We want `ironpaw` to be able to run shell commands as tools, but we do not want to expose a tool for arbitrary shell execution, instead we want to have different tools, and each tool will have a specific set of commands that it can run.


### External WASM Tools

External WASM tools are provider integrations packaged as WASM components. They are request/response tools, not channels: the agent calls them with JSON arguments, and they return JSON output.

They live in:

```text
<runtime-root>/tools/<name>.wasm
<runtime-root>/tools/<name>.config.json
```

At startup, Ironpaw discovers installed WASM tools from the runtime tools directory and registers them in the same tool registry as internal and dynamic tools. The WASM component exports its name, description, parameter schema, and execute function through `wit/tool.wit`.

External tools do not receive raw secrets directly. They use host-provided imports for external effects. HTTP tools call `http_request`, which the host wraps with `NetworkGate<ToolSecrets>` so the installed tool config decides which hosts are allowed and whether a header secret, URL placeholder, or OAuth grant is injected. Email tools call IMAP/SMTP-specific host imports, so the host owns mail-server connections without understanding Gmail or Outlook APIs.

The install flow is implemented through `list_tools`, `install_tool`, `configure_tool`, and `setup_tool`. `install_tool` discovers packages only from `<runtime-root>/tool-store`, shows the package's network hosts, OAuth endpoints, and maximum scopes for approval, then registers a temporary automation scoped to the starting conversation. `configure_tool` uses the same flow for an installed tool and asks before replacing every saved value; secret values are never displayed. That automation calls `setup_tool` for follow-up replies. `setup_tool` asks deterministic setup questions, stores required secrets in the encrypted secret store, writes the final tool config, copies the WASM component into `<runtime-root>/tools`, and registers or reloads the tool immediately.

The first external WASM tool is `web_search`. It searches Brave Search through `https://api.search.brave.com/res/v1/web/search`, while the `BRAVE_SEARCH_API` key is injected by the host as `X-Subscription-Token`. The `email` package owns its Gmail and Microsoft Graph request formats and declares OAuth endpoints, public client IDs (literal or operator-configured), confidential-client secret handles, scopes, refresh-token handles, and resource hosts in package files. Ironpaw generically owns PKCE, callbacks, token exchange, encrypted token storage, refresh, and bearer injection. Its IMAP/SMTP path uses provider-neutral host imports because WASM has no raw socket capability.

### Dynamic Tools

Dynamic tools are JavaScript files executed in a QuickJS sandbox.

They live in:

```text
<runtime-root>/tools/<name>/tool.js
<runtime-root>/tools/<name>/config.json
```

At startup, Ironpaw discovers tool directories in the tools directory and registers them in the same tool registry as internal tools.

Scripts have no ambient filesystem or network access. They receive:

- `args`: the tool call arguments.
- registered tools as global functions named after the tool, e.g. `web_fetch({ url })`.
- `console.log(...)`: debug logging.

Scripts use top-level code and `return` their result; defining an uncalled `main` function does nothing. They have no ambient network access: `fetch` is unavailable and `web_fetch({ url })` is the network tool. Creation and editing parse the script and reject calls to unavailable functions before activation. The agent supplies the complete script, description, and optional parameter schema to `create_tool` or `edit_tool`; Ironpaw owns the resulting files.

---

## Example Flow: TUI Asks For A Telegram Message

This example assumes the Telegram chat id is known.

1. The user sends a message in the TUI:

   ```text
   send me a message on telegram
   ```

2. `ironpaw-tui` posts the message to the local channel bridge using bearer auth.

3. The bridge routes the HTTP request to the TUI WASM channel:

   ```text
   message_channel_to_ironpaw
   -> tui.handle_http(...)
   ```

4. The TUI channel emits a channel inbound message. The bridge converts it to:

   ```text
   Event::MessageIn {
       channel: "tui",
       sender: User,
       conversation: ConversationKey { channel: "tui", chat_id: <callback>, thread_id: None },
       message_id: None,
       message: Message::User { ... }
   }
   ```

5. The bridge stores the inbound message in history, then the automation runtime matches the default `handle-conversation` automation.

6. `handle-conversation` dispatches a conversation agent request, which starts the agent tool loop.

7. The agent calls `raw_inference.execute(RawInferenceRequest { ... })`. The model receives schemas for the tools relevant to this flow, including:

   ```text
   raw_inference
   send_message
   delete_message
   create_tool
   edit_tool
   delete_tool
   create_automation
   install_channel
   install_tool
   ```

8. The model decides to call:

   ```json
   {
     "name": "send_message",
     "arguments": {
       "channel": "telegram",
       "chat_id": "telegram-chat-id",
       "content": "hi"
     }
   }
   ```

9. The agent executes the `send_message` tool with an explicit destination and publishes:

   ```text
   Event::MessageOut {
       channel: "telegram",
       chat_id: Some("telegram-chat-id"),
       message: Message::Assistant { ... }
   }
   ```

10. The channel bridge receives `MessageOut` and routes it to the explicit Telegram chat.

11. The bridge calls:

    ```text
    telegram.on_outbound_event(event_json, config_json)
    ```

12. The Telegram WASM channel sends the message to Telegram using its configured HTTP credential injection.

13. The model loop finishes. `answer_the_user` sends the final response back to the original TUI conversation, usually something like:

    ```text
    ok
    ```

This flow is the intended uniform shape: TUI and Telegram are both channels, and cross-channel effects happen through tools and bus events rather than special-case code paths.

---

## Appendix: ToDo

- make sure code does not panic instead we need to return errors for the agent to handle or tell the user
- Decide whether dynamic tools should declare allowed tool calls and outbound HTTPS endpoints beyond their current description and parameter schema.
- Decide whether runtime reload/file watching is needed.
- Decide whether some internal tools should eventually move to packaged WASM tools with declared capabilities and secret requirements.
- Add durable tool execution audit logs.
- Let a running automation report that its definition needs repair—such as a missing required tool—and provide a controlled path for the agent to inspect and update that automation.
- I asked ironpaw to install a channel, and it said "ok, I need your token to do that" instead of starting the install flow!

### Better Config UX

Channel config files currently work, and channel installation can handle some setup flows, but the user still sometimes has to know the exact JSON shape. We need broader validation, friendlier errors, and setup flows for more channels.

### Web Channel

The current architecture can support a web UI as another channel, but there is no web channel implementation yet.

### Tool Permissions

The model can call the tools exposed by `InternalTools`. There is not yet a permission layer for:

- allow/deny by tool.
- per-channel tool restrictions.
- confirmation before sensitive actions.
- durable audit logs of tool execution.

### Outbound Documents In Telegram

The bus and `send_message` tool support outbound attachments, `image_generation` stores generated images as files, and Telegram can deliver images with `sendPhoto`. Telegram still needs document delivery for non-image files.

---

## Appendix: Things We Tried But Didn't Work

### Processing images through the model

We ended up having multiple "if" branches to handle images vs other types of media, and the engine ended up too complex without clear benefits.

The model seems to take well to using an `inspect_file` tool to "see" the image, so we went with that simpler approach.

### Responses API

Tried using the `/responses` API, but NEAR AI's `previous_response_id` does not load conversation history into the model.

The agent therefore does not rely on `previous_response_id` or `function_call_output` for its main loop.



---

## Changing the AI API Key

Inference returns authentication failures normally and has no bus or conversation dependency. Key changes are initiated explicitly by the owner with `/change_api_key`, which publishes `ApiKeyChangeRequested`. The onboarding automation handles the replacement without relying on the agent, and failed inference requests are not retried automatically.
