# Concepts

## GoalContract

Shown before broad work starts: acceptance criteria, expected artifacts, verify plan,
and assumptions. Low-confidence goals surface clarification questions instead of
burning provider tokens on vague placeholders.

## HandoffPacket (receipt)

The merge artifact: changed files, verification findings, known gaps, token usage,
and suggested review actions. Exportable for PR description — not a chat summary.

## Verify gate

Workers cannot mark a subtask done without passing configured verification layers
(test, build, syntax, etc.). Failures feed back into retries or escalation.

## Provider-only vs local-template

| Mode | When | Receipt `execution` label |
| --- | --- | --- |
| Provider-only | `PHONTON_DISABLE_LOCAL_SEEDS=1` or no matching template | `provider` |
| Product-mode | Known benchmark/workspace slices match a template | `local-template` or `mixed` |

Only provider-reported token lines are eligible for public efficiency claims
(`token_claim_eligible` in benchmark artifacts).

## Focus panels (TUI)

Plan, Code, Problems, Receipt, Flight Log, Memory. After a goal completes, inspect
**Receipt** first for verify status and gaps before re-running.

## Stack workflow

Use Claude Code, Cursor, or Codex for exploration; use Phonton for **merge-bound**
goals on the same branch when you need a verified diff and receipt before review.
