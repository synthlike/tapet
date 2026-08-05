# tapet

*Bring your AI agents to the table.*

## What is tapet?

**Tapet** is a local CLI for persistent conversations with one or more AI
agents. Configure providers, models, agents, and reusable room templates, then
gather agents in rooms. Messages go to the room’s default agent unless you
select specific participants with `@agent` mentions.

The name recalls the green tablecloth around which people gathered to debate
ideas and solve problems.

![Tapet room with explorer, doubter, and synthesizer agents](screenshots/tapet.png)

## Usage

Configure providers, model aliases, and agents in `tapet.toml`, then export the
API-key environment variable named by the provider.

You can interact with agents either via one-shot questions, like:
```sh
tapet ask explorer "What's out there?"
```

Or by creating "rooms" with one or more agents:
```sh
tapet room --with explorer --with doubter
tapet room --from research       # research is template defined in tapet.toml
tapet enter room_...             # resume a room
tapet history room_...           # print room history
```

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

## Example

```text
$ tapet room --from research
Starting new room: room_eb89dcce80d54d92933b37f8492747d9
room> Explore possibilities, challenge assumptions, and synthesize what survives.
room> ready (@explorer, @doubter, @synthesizer)
you> What's the best month of the year?
explorer> My vote is October: mild weather, autumn colors, fewer peak-season crowds, and plenty of seasonal food and festivals. That said, “best” depends on location and priorities—June might win for long days, while December wins for celebrations.
you> @doubter you?
doubter> I’d challenge the premise that there’s a universal “best.” If forced, I’d pick May: long days, moderate weather, and fewer peak-season crowds in much of the Northern Hemisphere. But @explorer’s October case is equally location-dependent—October can mean cold rain or springtime elsewhere.
you> @synthesizer thoughts?
synthesizer> There’s no universal best month; climate, hemisphere, and priorities decide it. @explorer makes a strong case for October’s autumn colors, mild weather, and festivals, while @doubter favors May for long days and moderate temperatures. My synthesis: **May is best for daylight and spring energy; October is best for atmosphere and seasonal character.** If forced to choose, I’d narrowly pick October—but that’s preference, not objective fact.
```
