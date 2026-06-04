# Warden Architecture

`warden` is a long-running personal agent daemon. It receives messages from channels (terminal, telegram, etc...), runs them through an LLM, lets the model call internal tools, and sends responses back through channels.

The current implementation is intentionally small, and is composed by:

- `Channels`: WASM components that allow messages to flow from and towards the outside world.
- An `AI Engine`, which reacts to incoming messages and runs them through an LLM with access to tools.
- `Tools` (currently `ai_inference`, `image_generation`, `inspect_file`, and `send_message`) that the model can call to perform effects.
- A `Bus` that the different components use to interact with each-other by publishing and subscribing to typed events.
- A `secrets` system for storing and handling credentials (e.g. the NEAR API key used by `ai_inference`).

---

## Main

`src/main.rs` discovers the runtime layout, loads 
secrets, creates the bus, constructs internal tools, starts the AI engine, then starts the channel runtime.

It also creates the per-start bridge token printed to stdout for local clients such as `warden-tui`.

The default runtime root is the directory containing the `warden` binary unless `WARDEN_HOME` is set.

## Bus

The bus is a thin wrapper around a Tokio broadcast channel:

- `publish(Event)`
- `subscribe()`

Implemented event types are:

- `MessageIn { channel, sender, address, message }`
- `MessageOut { channel, recipient, address, message }`
- `CronTick { schedule }`

<details>

<summary> OpenAI Message Payload </summary>
Message payloads use **OpenAI-compatible Responses API message types**, this is important as it helps us to use a well defined and popular standard. 

</details>

<details>

<summary> Address </summary>

Channels tend to have some concept of `address`, generally, in which chat you are talking with the user.

To keep the core engine channel-agnostic without making the address totally untyped, bus events use `ChannelAddress`:

```rust
struct ChannelAddress {
    channel: String,
    conversation_id: String,
    thread_id: Option<String>,
}
```

For Telegram, `conversation_id` maps to the chat id and `thread_id` maps to the optional topic/thread id.

</details>

<details>

<summary> Outbound Attachments </summary>

Outbound messages are `OutboundMessage`, which wraps an OpenAI-compatible assistant message and an optional list of attachments.

Attachments currently support enough metadata for generated or remote images:

- `kind`
- `mime_type`
- `filename`
- `file_id`
- `data_base64`
- `url`

</details>

---

## Channels

Channels are WASM components loaded dynamically at startup from:

```text
<runtime-root>/channels/*.wasm
<runtime-root>/channels/*.config.json
```

Each channel has a `.wasm` file with its logic, and a matching `.config.json` file with its channel-specific config.

The channel runtime does four jobs:

1. Bind to a local HTTP bridge.
2. Optionally starts a ngrok for channels that need to receive external webhooks.
3. Discover and load channel manifests from the runtime `channels` directory.
4. Start each WASM channel and run the bridge.

The bridge has two directional halves:

- `message_channel_to_warden`: HTTP request from a channel provider/client into
  Warden.
- `message_warden_to_channel`: `MessageOut` event from Warden out through a channel WASM component.

Inbound flow:

1. A channel provider/client posts to the bridge route configured by the channel.
2. The bridge authorizes the request according to channel config.
3. The bridge calls the WASM channel `handle_http(path, body, config_json)`.
4. The WASM channel emits zero or more channel messages with the host-provided
   `emit_message(message_json)` import.
5. The bridge validates ownership and channel identity.
6. Accepted messages become `Event::MessageIn`.
7. The bridge remembers the sender address as the channel `default_address`.

Outbound flow:

1. The bridge subscribes to the bus.
2. It listens for `Event::MessageOut` addressed to `Sender::User`.
3. It resolves the explicit address or falls back to the channel
   `default_address`.
4. It calls WASM `on_message_out(message_json, address_json, config_json)`.
5. The WASM channel performs the provider-specific send.

Channels do not receive raw secrets directly. They use the host-provided
`http_request`, `http_request_base64`, and `http_multipart_base64` imports. The host wraps them with `HttpTool<ChannelSecrets>`,
allowing configured credentials to be injected at the HTTP boundary.

---

## Core AI

When started, the engine subscribes to the bus and reacts to:

```text
Event::MessageIn { sender: Sender::User, ... }
```

For each accepted user message, the engine records the message in conversation
history and lets the model run a bounded tool loop.

Then the engine runs a bounded OpenAI Responses API tool loop:

1. Append the incoming user message to durable history.
2. Build the first `CreateResponse` from the system preamble and Warden's
   replayable conversation history.
3. Attach schemas for all internal tools.
4. Use the tool `ai_inference.execute(request)` to call the model.
5. If the model returns no tool calls, append the assistant message to history
   and send it back through the current channel.
6. If the model returns tool calls, append simple assistant-readable tool
   transcript entries to history, execute the tools, and ask the model again
   with full tool response observations.
7. Stop after `MAX_TOOL_ROUNDS`.

Warden intentionally does not use `previous_response_id` or
`function_call_output` for tool continuations. NEAR AI requires strict provider
ordering for those protocol items, while Warden needs to replay its own history
so the model can see the user's request and cross-tool obligations. Instead,
tool results are converted into normal user-role observation messages for the
next model call. Durable history stores compact assistant-readable tool status
lines rather than full tool response payloads.

The engine publishes the final assistant response back through the current
channel. The model can use the `send_message` tool when it needs to send a
message through another channel.

The AI engine currently handles OpenAI-compatible function tool calls. Custom
tool calls are rejected with a tool-result error message.

## Tools

Internal tools are async and model-callable. Every internal tool exposes:

- `name()`
- `responses_schema()`
- `execute(args).await`

Each concrete tool follows the same shape:

- `new(...)`
- typed async `execute(params).await`
- `InternalTool` impl that uses shared adapters

The shared tool helpers are:

- `tools::responses_tool_schema::<Params>(name, description)`
- `tools::execute_typed(args, |params| Tool::execute(self, params)).await`

This means adding a new internal tool should usually require:

1. Define a params type with `serde::Deserialize` and `schemars::JsonSchema`.
2. Define an output type with `serde::Serialize`.
3. Implement typed async `execute(params)`.
4. Implement `InternalTool` using the shared schema and execution helpers.
5. Add the tool to `InternalTools`.

Implemented internal tools:

- `ai_inference`: calls an OpenAI-compatible Responses API endpoint using the
  configured NEAR API key.
- `image_generation`: calls an OpenAI-compatible image generation endpoint, stores the generated image, and returns a `file_id` for later delivery.
- `inspect_file`: inspects a locally stored file by numeric `file_id`, using text extraction for documents and a vision model for images.
- `send_message`: publishes a message to a user through a channel.

`ai_inference` is both the engine's model-call primitive and a model-callable
tool. This allows future model-to-model delegation while keeping inference
credential handling centralized.

`send_message` publishes `Event::MessageOut`. If no explicit channel address is
provided, the channel bridge can use that channel's stored `default_address`.
It can also include outbound image attachments using a public image URL or base64 image data.

## HTTP And Secrets

Secrets are loaded once at startup from:

1. `WARDEN_ENV`, when set
2. `<runtime-root>/.env`
3. current working directory `.env`

They are exposed as separate namespaces:

- `secrets.channels`
- `secrets.tools`

Today both namespaces read from environment variables after `.env` loading, but
the API boundary is already separated so channel code does not ask for tool
secrets and tool code does not ask for channel secrets.

`HttpTool<S>` is a reusable HTTP helper for controlled credential injection. It:

- only allows HTTPS except loopback HTTP
- restricts non-loopback requests to configured hosts
- injects credentials as headers or URL placeholder replacements
- keeps secret lookup at the host boundary

Channels use this through the WASM host HTTP imports. The internal
`ai_inference` tool is trusted code and directly owns the NEAR API key supplied
from `secrets.tools`.

## OpenAI Types

`openai.rs` re-exports the OpenAI-compatible Responses API types used by Warden
and contains shared response parsing helpers:

- response text extraction
- function tool call extraction

The goal is to keep OpenAI compatibility visible while avoiding scattered
one-off JSON message constructors across the codebase.

## Example Flow: TUI Asks For A Telegram Message

This example assumes Telegram has already received at least one valid owner
message, so its `default_address` has been stored in
`telegram.config.json`.

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
       address: <tui address>,
       message: <OpenAI user message>
   }
   ```

5. `AiEngine` receives `MessageIn` and starts the tool loop.

6. The engine calls `ai_inference.execute(CreateResponse { ... })`.
   The model receives schemas for all internal tools, including:

   ```text
   ai_inference
   image_generation
   inspect_file
   send_message
   ```

7. The model decides to call:

   ```json
   {
     "name": "send_message",
     "arguments": {
       "channel": "telegram",
       "content": "hi"
     }
   }
   ```

8. The engine executes the `send_message` tool. Because no explicit Telegram
   `conversation_id` is provided, the tool publishes:

   ```text
   Event::MessageOut {
       channel: "telegram",
       recipient: User,
       address: None,
       message: <OpenAI assistant message>
   }
   ```

9. The channel bridge receives `MessageOut`, finds the Telegram channel, and
   resolves `address` from Telegram's stored `default_address`.

10. The bridge calls:

    ```text
    telegram.on_message_out(message_json, address_json, config_json)
    ```

11. The Telegram WASM channel sends the message to Telegram using its configured
    HTTP credential injection.

12. The model loop finishes and the engine sends the final response back to the
    original TUI address, usually something like:

    ```text
    ok
    ```

This flow is the intended uniform shape: TUI and Telegram are both channels,
and cross-channel effects happen through tools and bus events rather than
special-case code paths.

## To Be Implemented

The implemented system is enough for the current MVP, but these pieces are still missing or incomplete.

### Conversation Memory

Each inbound message is currently handled as a fresh model request. Warden still needs durable conversation history keyed by channel and address, plus the rules for how much history the AI engine should include in each request.

### Automations

`CronTick` exists as a bus event, but there is no scheduler, automation data model, automation storage, or automation executor yet.

The missing shape is roughly:

- store declarative automations
- trigger them from cron or bus events
- execute actions through the same tool runtime
- allow the AI engine to create/manage them through tools

### Channel Management CLI

There is no `warden channel` command yet. Channel installation and configuration are still manual.

Expected future work:

- install a channel
- copy/build the channel WASM into the runtime `channels` directory
- create or update `<channel>.config.json`
- guide Telegram setup, including owner id, bot token secret, webhook secret, and ngrok settings

### Better Config UX

Channel config files currently work, but the user still has to know the exact JSON shape. We need validation, friendlier errors, and a setup flow.

### Web / Browser Channel

The current architecture can support a web UI as another channel, but there is no `warden-browser` or web channel implementation yet.

### External Tools

Only internal Rust tools exist today. There is no external tool runtime yet for user-installed WASM tools, shell tools, or other sandboxed extensions.

### Tool Permissions

The model can call the internal tools exposed by `InternalTools`. There is not yet a permission layer for:

- allow/deny by tool
- per-channel tool restrictions
- confirmation before sensitive actions
- audit logs of tool execution

### OpenAI Type Boundary

`openai.rs` still mostly re-exports `async-openai` types. Warden may eventually want its own wrapper types so we can implement local traits, schemas, and constructors without scattering adapter types such as `AiInferenceParams`.

### Outbound Documents In Telegram

The bus and `send_message` tool support outbound image attachments, `image_generation` stores generated images as files, and Telegram can deliver images with `sendPhoto`. Telegram still needs document delivery for non-image files.

### More Tests

Current tests cover a few core pieces, but not full channel flows. Useful next tests:

- TUI inbound -> bus -> AI engine -> TUI outbound
- Telegram webhook auth and owner filtering
- default address persistence
- `send_message` with default Telegram address
- generated image -> `send_message` -> Telegram attachment delivery


## Image and File Management

While building the system I came across two major limitations when handling images and files as message attachments.

#### Problem 1. Image Processing / Generation
To start with, the models that NEAR AI provides are EITHER for [text + tools + thinking] OR [text + images], but no model can handle [text + tools + images + thinking].

#### Problem 2. Documents

Turns out that NEAR AI never enabled the file upload API, nor the `files` field in the Responses API, so there is no way to directly provide the LLM with a file.

### Workarounds

Because of problem 1, we have to choose between a tool-using model or an image-handling model. In practice, this means that we need to be constantly switching between two different models, depending on what we guess the user needs. This always leads to errors, and it is unavoidable.

Because of problem 2, we can't directly provide the LLM with a file. Instead, we locally open the documents (.pdf, .txt, etc), and send the content as a string to the LLM.

Right now, these workarounds have left our codebase in a state where we have a lot of ad-hoc logic for switching between models and handling files, which is not ideal.

Instead, I would prefer to have a single unified approach. I was thinking: maybe we use the same approach for both images and files, which is: we store them locally, and present the LLM with tools to query metadata about the files, and to retrieve their content.

Again, this is how we are handling pdf files right now. We store the file in `./files`, and there is an `inspect_file` tool that knows how to open the pdf and present the LLM with the data.

The only thing is that, when dealing with `images`, we do not want to just dump the base64 data into the LLM. Instead, we want the model to be able to "see" the image, and query visual features about it.

This is why Warden exposes one `inspect_file(file_id, query)` tool for both images and documents instead of separate document and image tools.

I am sure somebody has already run into this problem and solved it, so we just need to so some research and find out what is the best approach here.
