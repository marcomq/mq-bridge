# mq-bridge agent skill

Teaches a coding agent to drive the `mqb` CLI: `list` / `describe` for params, `copy` for
one-off jobs, YAML configs with `--schema`, and the traps. The skill is
[skills/mq-bridge/SKILL.md](skills/mq-bridge/SKILL.md) in the open Agent Skills format, so
the same folder works for Claude Code, Codex and other agents that read `SKILL.md`.

## Install

```bash
npx skills add marcomq/mq-bridge
```

This asks which agents to install for. Without Node, copy or symlink the `skills/mq-bridge` folder into the agent's skills directory:

| Agent | User-wide | Per project |
|---|---|---|
| Claude Code | `~/.claude/skills/mq-bridge` | `.claude/skills/mq-bridge` |
| Codex | `~/.agents/skills/mq-bridge` | `.agents/skills/mq-bridge` |

```bash
git clone --depth 1 https://github.com/marcomq/mq-bridge /tmp/mq-bridge
cp -r /tmp/mq-bridge/apps/mq-bridge-app/agent-skills/skills/mq-bridge ~/.claude/skills/
```

This folder is also a Claude Code plugin; try it in place with
`claude --plugin-dir apps/mq-bridge-app/agent-skills`.
