# Project [Ironpaw]

[Ironpaw] is a small, security-minded personal AI agent that runs on your
machine. It talks to you through a web UI, or Telegram; uses tools to
get work done; and can trigger automations on a schedule or when messages
arrive. 

## How it works

- **Channels** translate external messages into Ironpaw events and deliver responses back to the user
- **Automations** decide what should happen when a message arrives or a schedule triggers
- **The agent** is a bounded LLM loop. It reasons, calls allowed tools, and returns control when the turn is complete
- **Tools** perform side effects. Network access, secrets, and workspaces stay behind host-owned boundaries instead of being handed directly to the model
- Ironpaw can keep **Secrets**, which are encrypted locally and bounded to specific tools

The components communicate through a typed publish/subscribe bus, so channels,
automations, tools, and the agent remain independent of one another.

## Features

- Local TUI and web clients, plus installable WASM channels
- Scheduled and inbound-message automations
- Built-in, WASM, shell, and runtime-created JavaScript tools
- File reading, writing, editing, searching, inspection, and attachments
- Durable conversation history, search, and long-term memory
- Encrypted secrets and mediated outbound network access
- Isolated workspaces for conversations, automations, and external tools

Ironpaw is still young and changing quickly. For the reasoning behind its
design, read [the architecture guide](docs/architecture.md).
