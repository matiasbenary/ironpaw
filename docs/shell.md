## Shell tools

A shell tool is a native executable running as a child process, invoked through
the restricted `shell` interface:

```text
shell(tool, command, args, cwd)
```

Ironpaw does not concatenate the arguments into a shell command. It resolves a
declared executable and passes each argument directly as `argv`. Shell syntax,
pipes, command substitution, and redirection therefore have no meaning unless
the executable itself chooses to interpret them.

The sandbox limits what that code can observe and reach; it does not make the code trustworthy.

**Installing a shell tool:**

- A package is described by `shell.json` and may include a `SKILL.md` teaching
  the agent how to use it.
- The manifest declares its commands, allowed argument shapes, static
  environment variables, network hosts, and secrets.
- An installer may use a preinstalled command, download a binary with `curl`
  or `wget`, download and execute an installation script, or install an npm
  package. Installation runs inside the same per-tool sandbox as commands and
  has a five-minute timeout.
- The installer places or links every declared command under the package's
  private `bin` directory. Later execution resolves the fixed binary path
  rather than looking up the command through `$PATH`.
- After installation, Ironpaw removes write permissions from the complete
  package tree. Linux additionally enforces this with a read-only Bubblewrap
  mount during every command.
- Ironpaw shows the user the installer, commands, network hosts, and secrets
  before installation. The agent may start the setup flow, but installation
  pauses until the user explicitly confirms it.
- A failed installation removes the incomplete tool directory.

The installed manifest is validated again whenever the tool runs. Unknown
manifest fields, unsafe names and paths, malformed hosts, duplicate entries,
and references to undeclared secrets are rejected.

**Declaring command access:**

Every command exposes one or more invocation shapes. A shape may declare:

- A required argument prefix, such as `issue list`.
- The permitted options and whether they take values or may repeat.
- Minimum and maximum positional argument counts.
- Which of the package's secrets that particular invocation receives.

Arguments that match no declared shape are rejected before the process starts.
The built-in inspection commands (`cat`, `find`, `head`, `jq`, `ls`, `rg`,
`sed`, `tail`, and `wc`) accept arbitrary arguments, but always run inside the
selected package's current workspace and never receive secrets.

**Spawning a shell command:**

- Commands execute serially through one queue. Two shell commands never run at
  the same time.
- Production execution requires Linux and Bubblewrap. If either is unavailable,
  shell execution fails closed.
- Bubblewrap creates new namespaces, including an empty network namespace. The
  installed package appears read-only under `/tool`; its data directory is the
  only writable directory and appears as `/workspace`.
- That data directory is selected from the current conversation or automation,
  then namespaced by shell-tool package. Different tools and different owners
  do not share writable files.
- `/usr`, `/bin`, `/lib`, and `/lib64` are mounted read-only when present.
  `/tmp` is temporary, `/proc` and `/dev` are recreated inside the sandbox, and
  only the system TLS certificate directories are exposed from `/etc`.
- `cwd` must be a relative directory which resolves inside the tool's private
  workspace.
- The environment is cleared before execution. Ironpaw adds only `HOME`,
  `TMPDIR`, a fixed `PATH`, `IRONPAW_TOOL_BIN`, proxy variables, the command's
  declared static environment, and the secrets selected by the matched
  invocation.
- `IRONPAW_TOOL_BIN` points to `/tool/bin`. Ironpaw
  does not expand it inside manifest values; a program which later interprets
  a value through a shell may expand it itself.
- Standard input is closed. Standard output and error are captured separately
  and limited to 1 MiB each.
- A command has a two-minute timeout. When it returns or times out, Ironpaw
  kills its process group and drops the sandbox and network proxy.

Debug builds also contain a portable runner for tests. It retains the private
working directory, scrubbed environment, timeout, and network proxy, but does
not make the package read-only and does not provide Bubblewrap filesystem or
namespace isolation. It must not be treated as a production sandbox.

**Network access:**

- Direct network access is unavailable inside the Linux namespace.
- Ironpaw starts a CONNECT proxy outside the sandbox and mounts only its Unix
  socket into the sandbox.
- A small network-owned bridge exposes that socket as a conventional localhost
  HTTP proxy because command-line programs generally do not understand Unix
  proxy sockets.
- The proxy accepts only exact hostnames declared in `allowed_hosts`, resolves
  them itself, and rejects private or internal addresses. An empty list means
  no outbound destination is allowed.
- The same restrictions apply while installing the tool. An installer URL's
  hostname must appear in `allowed_hosts`.

**Secrets:**

- A package declares secret names and their environment variable names.
- Each invocation separately declares which of those secrets it needs. Other
  stored secrets are not copied into that process's environment.
- Secrets are available to the native executable itself. Once injected, the
  sandbox cannot stop that executable from sending them to an allowed host.
- Secret storage is namespaced by package. Two packages declaring the same
  manifest secret name receive different stored values.

Static environment values also deserve the same scrutiny as executable code.
Some programs use environment variables as extension points. Git, for example,
can execute a `credential.helper` supplied through `GIT_CONFIG_*`. Argument
validation constrains the initial command, not child programs deliberately
started by that command.

**What this gets us:**

- ✓ Native utilities can be installed without granting them the host
  filesystem or environment.
- ✓ Installed binaries, the manifest, and package instructions are immutable
  during normal execution.
- ✓ Every package gets an independent writable workspace for each conversation
  or automation.
- ✓ The model can invoke only declared commands and argument shapes.
- ✓ Secrets are absent unless the matched invocation explicitly requests them.
- ✓ Direct networking is blocked; permitted traffic must cross the
  network-owned hostname allowlist.
- ✓ Commands run serially and their sandbox is torn down after completion.

**What this doesn't get us:**

- A malicious tool can read and transmit its arguments, its own workspace, and
  any secret deliberately injected into it.
- An allowed host is a destination boundary, not an intent boundary. A tool can
  upload data to an attacker-controlled account hosted by an otherwise
  legitimate service.
- Native code still shares the host kernel. The sandbox is not a substitute for
  keeping the host patched or for stronger VM isolation when running actively
  hostile binaries.
- Manifest review cannot prove that a downloaded binary or installation script
  behaves as described.

WASM remains the preferred format for third-party tools that need credentials:
the host can grant narrow network operations without exposing the credential or
filesystem to the tool. Shell tools exist for programs whose native execution
is itself the required capability.
