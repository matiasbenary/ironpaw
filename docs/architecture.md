# Warden Architecture

`warden` is a long-running personal agent daemon. It receives messages from
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
- An [HTTP and Secrets](#http-and-secrets) system for loading credentials at the host boundary.

---

## Prelude for LLMs

On contributing to this doc, please do understand this doc is to explain the "why" more than the "how". The "why" helps understanding the trade-offs of our choices. The "how" can be explained at high level, but for anyone wanting to see how it was implemented, they can literally read the code itself.

This document is made for humans to read, meaning that we need to write sentences that "flow" and are easy to read, thus prioritizing clarity over conciseness.

### Good Practices

Always make sure to introduce the thing you are talking about and, importantly, the why, not the how.

You cannot simply say "The default `answer-the-user` path is also an automation", because nobody knows what `answer-the-user` is. It is better to be clear and highlight the relevancy of why we are talking about it. In this case, the relevant part is that, by default, warden reacts to inbound messages and sends them to the AI Agent through an automation. This means that automations are not "just a part" of the system, they are the core component that makes `warden` act and react. The fact that it is called `answer-the-user` is irrelevant.

---

## Main

`src/main.rs` owns the startup lifecycle. It unlocks or initializes the encrypted secret store first; only after secrets are unlocked does it wire the agent, tools, automations, and channels. It also creates the per-start bridge token printed to stdout for local clients such as `warden-tui`.

The default runtime root is the directory containing the `warden` binary unless `WARDEN_HOME` is set. Runtime data lives under:

```text
<runtime-root>/channels
<runtime-root>/tools
<runtime-root>/files
<runtime-root>/data
<runtime-root>/secrets
```

On a fresh install, Warden starts the bus, loads the TUI channel, runs the boot unlock automation, generates a six-word unlock phrase, asks the user to verify selected words through normal channel messages, and creates an encrypted local secret store. On later starts, Warden asks for the phrase through the same message/automation path and does not load normal tools, extra channels, or stored automations until the store unlocks. Unlock is handled by an internal tool and asks the active channel to delete sensitive inbound messages after they are consumed.

Once unlocked, Warden installs a default onboarding automation if required setup secrets such as `NEAR_API_KEY` are missing.

After onboarding, it sets `answer-the-user` as the default automation, which routes inbound messages to the agent and sends answers back to the user through the same channel. Web search is implemented as an external WASM tool, so it is available after the `web_search` tool package has been installed into the runtime tools directory.

---

## Bus

The bus is the smallest shared coordination primitive in Warden. It is a thin wrapper around a Tokio broadcast channel, which allows multiple subscribers to receive the same events.

It exposes two main methods:

- `publish(Event)`
- `subscribe()`

Implemented event types are:

- `MessageIn { channel, sender, conversation, message_id, message }`
- `MessageOut { channel, recipient, conversation, message }`
- `DeleteMessage { channel, message }`
- `ChannelAction { channel, recipient, conversation, action }`
- `AgentRequest { request_id, destination, request }`
- `AgentResponse { request_id, response }`
- `Automation { id, action }`
- `AutomationResult { id, result }`
- `ToolRequest { context, tool_call, args }`
- `ToolResult { context, tool_call_id, tool_name, result }`

### Message Types

Warden's bus, history, and agent use Warden-owned message types:

```rust
enum Message {
    System(String),
    User { text, attachments },
    Assistant { text, attachments, tool_calls },
    ToolResult { tool_call_id, tool_name, result },
}
```

Attachments are local files identified by `file_id`, with metadata such as MIME type, optional filename, and local source path. They can appear on user messages received from channels and assistant messages produced for outbound delivery. The bus carries the same `Message` type in both directions; channel-specific payloads are only produced by the channel bridge.

#### Image and File Management

Warden handles files as first-class message attachments: images, PDFs, text files, and eventually other documents. The core agent sees the same shape regardless of whether a file came from Telegram, TUI, image generation, or another channel.

Warden stores files locally and passes around typed attachments containing a numeric `file_id`, MIME type, optional filename, and source path. User messages and assistant messages both use the same attachment type on the bus; channels translate their provider-specific payloads into attachments on the way in, and the bridge translates attachments back into provider-ready outbound payloads on the way out.

The model interacts with stored files through tools. `inspect_file(file_id, query)` can extract text from documents or call a vision model for images. `image_generation` stores the generated image locally and returns a `file_id`; `send_message` can then publish an assistant message with that attachment for channel delivery.

All file ids are `u32` values for now. The current architecture treats that as the stable attachment identifier.

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

Channels do not receive raw secrets directly. They use the host-provided `http_request`, `http_request_base64`, and `http_multipart_base64` imports. The host wraps them with `HttpTool<ChannelSecrets>`, allowing configured credentials to be injected at the HTTP boundary.

<details>

<summary> Adding Channels </summary>

Channel installation is partly implemented inside the agent through `list_channels`, `install_channel`, and `setup_channel`. `install_channel` starts the install flow and registers a temporary automation. That automation calls `setup_channel` for follow-up replies. `setup_channel` discovers local channel packages, asks deterministic setup questions, stores required secrets in the encrypted secret store, copies the WASM/config into the runtime `channels` directory, and activates the channel in the running channel runtime.

</details>

<details>

<summary> Conversations And Messages </summary>

Channels tend to have some concept of a chat, room, DM, or thread where the user talks with Warden. Warden calls that a conversation.

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

- `message_channel_to_warden`: HTTP request from a channel provider/client into Warden.
- `message_warden_to_channel`: outbound bus events from Warden into a channel WASM component.

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

## HTTP and Secrets

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

`HttpTool<S>` is a reusable HTTP helper for controlled credential injection. It:

- only allows HTTPS except loopback HTTP.
- restricts non-loopback requests to configured credential hosts.
- injects credentials as headers or URL placeholder replacements.
- keeps secret lookup at the host boundary.

The generic `web_fetch` tool is read-only and unauthenticated. It rejects plain HTTP except localhost development URLs, rejects private/internal resolved IPs, and does not follow redirects.

---

## Automations

Automations are the cornerstone of Warden's behavior. They are small, declarative rules that explain how `warden` reacts to internal (`cron`) or external (`event`) triggers.

Technically, an automation is defined as a triggering condition plus a tool call (be that an internal or dynamic tool). When the condition is met, the tool is called with pre-defined arguments that can contain information from the triggering event.

Stored automations live in:

```text
<runtime-root>/data/automations.db
```

### Triggers

A trigger can be one of:

- `event`: a bus event, e.g. an inbound user message.
- `cron`: a recurring cron expression, evaluated in local time.
- `at`: one-shot absolute time.

Once a trigger fires, the automation runs a single pre-defined tool call. The tool call can be any registered tool, including internal and dynamic tools.

Inbound-message triggers can match all conversations, or a specific `ConversationKey`. Setup flows that temporarily take over replies should scope their trigger to the conversation that started the flow.

### Examples

When `warden` starts, it installs a default automation that reacts to inbound user messages with an `onboarding` tool. This tool asks the user for the NEAR API key needed for AI inference, then installs the default `answer-the-user` automation, which reacts to inbound user messages by calling the agent and sending the final answer back to the user.

```text answer-the-user
MessageIn(User, any conversation)
-> continue_conversation(current ConversationKey)
```

---

## Agent

The agent is the bounded tool loop: history + raw inference + tool execution. It does not subscribe to the bus directly. Automations or tools call it through the `call_agent` tool.

For each run:

1. Build model input from the system preamble, durable conversation history, and the new transient messages.
2. Attach schemas for all registered tools, internal and dynamic.
3. Use `raw_inference.execute(request)` to call the model.
4. If the model returns no tool calls, append the assistant message to history and return the final text.
5. If the model returns tool calls, hand them to the tool handler, then ask the model again after the tool transcript has been written to history.
6. Stop after `MAX_TOOL_ROUNDS`.

When a run has a destination, the agent also publishes typing and progress events back to that channel.

`raw_inference` is both the agent's model-call primitive and a model-callable tool. This keeps provider formatting and credential handling centralized while still allowing higher-level model-to-model delegation.

---

## History

Warden stores durable conversation history in SQLite:

```text
<runtime-root>/data/history.db
```

The history module exposes:

- `load(conversation) -> Vec<HistoryEntry>` - load active conversation history.
- `append(conversation, entries)`
- `delete_external_message(message_ref)`
- `compact(conversation, summary)` - replace active items with a summary while retaining originals for search.
- `search(query) -> Vec<HistoryEntry>`

Each history entry stores a Warden message plus metadata: Warden id, `ConversationKey`, and optional provider-owned external message id.

Inbound user messages and outbound channel messages are persisted by the bridge. Tool transcript entries, meaning assistant tool calls and tool results, are persisted by the tool handler. Final assistant text is persisted by the agent when it returns directly to the current conversation.

---

## Tools

In Warden we currently distinguish between internal tools, external WASM tools, and dynamic ones.

### Internal Tools

Internal tools are async and model-callable. Every internal tool exposes:

- `name()`
- `tool_definition()`
- `execute(args, ctx, responder).await`

Each tool receives an `ExecutionContext` with invocation metadata and tool-call depth. The invocation is explicit: `InvocationScope::Conversation(ConversationKey)` for conversation-scoped work, or `InvocationScope::ScheduledAutomation { automation_id }` for cron/at automation work. Tools match that scope directly and decide what to do. Result delivery is separate: tools receive a `ToolResponder` alongside the context. Message ids are action input, not execution metadata, so tools that operate on a specific message, such as `delete_message`, require an explicit `message_id`.

Implemented internal tools include:

- `raw_inference`: calls `/v1/chat/completions` using the configured NEAR API key.
- `image_generation`: calls an OpenAI-compatible image generation endpoint, stores the generated image, and returns a `file_id` for later delivery.
- `inspect_file`: inspects a locally stored file by numeric `file_id`, using text extraction for documents and a vision model for images.
- `web_fetch`: fetches a public URL with no credentials.
- `send_message`: publishes a message to a user through a channel.
- `delete_message`: deletes a channel message when the channel supports it.
- `call_agent`: runs a sub-agent and returns its final answer as a tool result.
- `create_tool`: writes and registers a new dynamic tool.
- `create_automation`, `delete_automation`, `inspect_automation`, `list_automations`: manage persisted automations.
- `list_channels`, `install_channel`, `setup_channel`: inspect and install local channel packages. `install_channel` starts the install flow and registers a temporary automation; `setup_channel` handles deterministic follow-up replies from that automation.
- `list_tools`, `install_tool`, `setup_tool`: inspect and install local external WASM tool packages. `install_tool` starts the install flow and registers a temporary scoped automation; `setup_tool` handles the deterministic follow-up replies from that automation.
- `onboarding`: collects required setup secrets and installs the normal answer automation.
- `todo`: a small scratchpad-style tool.

### Shell Tools

We want `warden` to be able to run shell commands as tools, but we do not want to expose a tool for arbitrary shell execution, instead we want to have different tools, and each tool will have a specific set of commands that it can run.


### External WASM Tools

External WASM tools are provider integrations packaged as WASM components. They are request/response tools, not channels: the agent calls them with JSON arguments, and they return JSON output.

They live in:

```text
<runtime-root>/tools/<name>.wasm
<runtime-root>/tools/<name>.config.json
```

At startup, Warden discovers installed WASM tools from the runtime tools directory and registers them in the same tool registry as internal and dynamic tools. The WASM component exports its name, description, parameter schema, and execute function through `wit/tool.wit`.

External tools do not receive raw secrets directly. They use host-provided imports for external effects. HTTP tools call `http_request`, which the host wraps with `HttpTool<ToolSecrets>` so the installed tool config decides which hosts are allowed and where credentials are injected. Email tools call IMAP/SMTP-specific host imports, so the host owns the configured mail server connections and injects credentials without exposing passwords to WASM.

The install flow is implemented through `list_tools`, `install_tool`, and `setup_tool`. `install_tool` discovers local packages from `WARDEN_TOOLS_SRC`, `<runtime-root>/tool-store`, and the built-in `tools-src` directory, then registers a temporary automation scoped to the starting conversation. That automation calls `setup_tool` for follow-up replies. `setup_tool` asks deterministic setup questions, stores required secrets in the encrypted secret store, writes the final tool config, copies the WASM component into `<runtime-root>/tools`, and registers the tool immediately.

The first external WASM tool is `web_search`. It searches Brave Search through `https://api.search.brave.com/res/v1/web/search`, while the `BRAVE_SEARCH_API` key is injected by the host as `X-Subscription-Token`. The `email` tool demonstrates a non-HTTP external tool: it can list and read messages through the configured IMAP server, and send plain-text mail through the configured SMTP server.

### Dynamic Tools

Dynamic tools are JavaScript files executed in a QuickJS sandbox.

They live in:

```text
<runtime-root>/tools/<name>.js
<runtime-root>/tools/<name>.config.json
```

At startup, Warden discovers `.js` files in the tools directory and registers them in the same tool registry as internal tools.

Scripts have no ambient filesystem or network access. They receive:

- `args`: the tool call arguments.
- registered tools as global functions named after the tool, e.g. `web_fetch({ url })`.
- `console.log(...)`: debug logging.

The script's last expression becomes the tool result.

---

## Example Flow: TUI Asks For A Telegram Message

This example assumes Telegram has already received at least one valid owner message, so its `default_conversation` has been stored in `telegram.config.json`.

1. The user sends a message in the TUI:

   ```text
   send me a message on telegram
   ```

2. `warden-tui` posts the message to the local channel bridge using bearer auth.

3. The bridge routes the HTTP request to the TUI WASM channel:

   ```text
   message_channel_to_warden
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

5. The bridge stores the inbound message in history, then the automation runtime matches the default `answer-the-user` automation.

6. `answer_the_user` calls `call_agent`, which starts the agent tool loop.

7. The agent calls `raw_inference.execute(RawInferenceRequest { ... })`. The model receives schemas for the tools relevant to this flow, including:

   ```text
   raw_inference
   send_message
   delete_message
   call_agent
   create_tool
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
       "content": "hi"
     }
   }
   ```

9. The agent executes the `send_message` tool. Because no explicit Telegram `chat_id` is provided, the tool publishes:

   ```text
   Event::MessageOut {
       channel: "telegram",
       recipient: User,
       conversation: None,
       message: Message::Assistant { ... }
   }
   ```

10. The channel bridge receives `MessageOut`, finds the Telegram channel, and resolves `conversation` from Telegram's stored `default_conversation`.

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
- Decide whether dynamic tools should have a richer manifest: version, description, parameter schema, allowed tool calls, and allowed outbound HTTPS endpoints.
- Decide whether runtime reload/file watching is needed.
- Decide whether some internal tools should eventually move to packaged WASM tools with declared capabilities and secret requirements.
- Add durable tool execution audit logs.
- I asked warden to install a channel, and it said "ok, I need your token to do that" instead of starting the install flow!

### Better Config UX

Channel config files currently work, and channel installation can handle some setup flows, but the user still sometimes has to know the exact JSON shape. We need broader validation, friendlier errors, and setup flows for more channels.

### Web / Browser Channel

The current architecture can support a web UI as another channel, but there is no `warden-browser` or web channel implementation yet.

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
