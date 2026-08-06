# tapet

*Bring your AI agents to the table.*

## What is tapet?

**Tapet** is a local CLI for persistent conversations with one or more AI
agents. Configure providers, models, agents, and reusable room templates, then
gather agents in rooms. Messages go to the room’s default agent unless you
select specific participants with `@agent` mentions, or `@all` to address
every participant at once.

The name recalls the green tablecloth around which people gathered to debate
ideas and solve problems.

![Tapet room with explorer, doubter, and synthesizer agents](screenshots/tapet.png)

## Install

Requires Rust ([rustup.rs](https://rustup.rs)).

```sh
cargo install --git https://github.com/synthlike/tapet
```

Or from a local checkout:

```sh
git clone https://github.com/synthlike/tapet
cd tapet
cargo install --path .
```

## Usage

Generate a starter `tapet.toml` (or write your own — see Configuration below),
then export the API-key environment variable named by the provider.

```sh
tapet config init > tapet.toml                  # generate a starter config

tapet agents                                    # list configured agents
tapet templates                                 # list tapet.toml room templates
tapet rooms                                     # list rooms you can enter, most recent first

tapet ask explorer "What's out there?"          # one-shot question

tapet room --with explorer --with doubter       # ad-hoc room
tapet room --from research --name moon-lab      # from a tapet.toml template
tapet enter moon-lab                            # resume a room
tapet history moon-lab                          # print room history

tapet --config other.toml agents                # use a config other than tapet.toml
```

Rooms default to their first (or default) agent; address others with
`@agent`, or `@all` to broadcast. Interactive rooms support scrollback, input
history, Tab completion for `@agent` mentions and `/` commands, and slash
commands (`/agents`, `/add <agent>`, `/help`, `/exit`) to manage the room
without leaving it.

Agents may also request the `read_file`, `list_files`, `write_file`, and
`search_files` tools to inspect, search, or change the workspace —
`write_file` shows a diff to approve before anything is written, and
`search_files` does a literal, case-sensitive substring search across a
directory (skipping `.git`, build/dependency directories, and binary files).
Every use pauses for a `[y]/[n]` approval and is logged to SQLite for
auditing; nothing runs when input or output is redirected — unless the room
grants that agent the tool's category (see `permissions` below), in which
case it runs immediately with a notice instead of a prompt.

## Configuration

Reusable room templates live alongside providers, models, and agents in
`tapet.toml`:

```toml
version = 1

[providers.openai]
type = "openai"
api_key_env = "OPENAI_API_KEY"

[models.gpt-sol]
provider = "openai"
model = "gpt-5.6-sol"

[agents.explorer]
model = "gpt-sol"
prompt = "Generate promising possibilities, uncover useful context, and clearly mark uncertain claims."

[agents.doubter]
model = "gpt-sol"
prompt = "Stress-test claims, expose hidden assumptions, and present the strongest reasonable counterargument."

[agents.synthesizer]
model = "gpt-sol"
prompt = "Combine the strongest surviving ideas into a clear conclusion, preserving important disagreements and open questions."

[rooms.research]
agents = ["explorer", "doubter", "synthesizer"]
default = "explorer"
description = "Explore possibilities, challenge assumptions, and synthesize what survives."
prompt = """
Treat every contribution as a proposal that may be challenged.
Distinguish evidence from speculation.
Refer to other participants by name when responding to their arguments.
Keep unresolved disagreements and open questions visible.
"""
```

A room template can optionally grant each participant a set of tool
categories — `read` (`read_file`, `list_files`, `search_files`) and `write`
(`write_file`); `call` and `exec` are reserved for future tools. A granted
category runs without an approval prompt (just a notice); an agent omitted
from the table gets no tools at all. Omitting `permissions` entirely keeps
today's behavior — every tool offered, every call still prompts:

```toml
[rooms.build]
agents = ["architect", "dev"]
default = "architect"
description = "Ship a feature end to end."
prompt = "Architect plans, dev builds."

[rooms.build.permissions]
architect = ["read"]
dev = ["read", "write"]
```
