---
name: cadet-workflow
description: Refine, capture, hand off, monitor, and answer Cadet requirements through the repo-local Gas City workflow. Use when discussing a requirement or milestone for Cadet; when asked to add, save, revise, queue, start, or hand work to Cadet or Gas City; when asked for automation status, blockers, feedback, or open questions; or when answering a Gas City question. Do not use for ordinary direct implementation work.
---

# Cadet workflow

## Instructions

### Check the shared feedback channel

At the start of each use, inspect both durable views from the repository root:

```sh
.agents/skills/cadet-workflow/assets/cadetctl status
.agents/skills/cadet-workflow/assets/cadetctl inbox --json
```

Surface any relevant unread question or failed requirement. Do not start another supervisor, Dolt server, watcher, or background process.

### Refine a requirement

Work with the user until the requirement has:

- one observable outcome;
- enough context to explain why it matters;
- explicit scope and useful non-goals;
- verifiable acceptance criteria;
- constraints or standards that differ from the repository defaults;
- dependencies on existing Cadet keys, if any;
- no unresolved product decision hidden inside the acceptance criteria.

Leave implementation ordering, file-level decomposition, parallelism, and ordinary code-review mechanics to Gas City. Its workflow already plans, reviews the plan, fans independent tasks out, runs `code-review`, fixes findings, re-reviews, integrates, and performs the final acceptance gate.

Use `assets/requirement.md` as the body shape. Keep the requirement about desired behavior, not a speculative implementation plan.

### Capture and revise drafts

Do not write to Cadet merely because the user is exploring an idea. Create a task only after an explicit request such as “save this,” “add this to Cadet,” or “hand this off.”

Write the Markdown body to a temporary file with the harness's file-editing tool, then create a draft without interpolating the body through a shell:

```sh
.agents/skills/cadet-workflow/assets/cadetctl draft --title '<short title>' --body-file /tmp/cadet-requirement.md
```

For an existing draft:

```sh
.agents/skills/cadet-workflow/assets/cadetctl revise CADE-12 --body-file /tmp/cadet-requirement.md
```

`revise` accepts `--title` too, but refuses requirements that Gas City has claimed. After either command, inspect the stored task:

```sh
cadet --project cadet show CADE-12 --json
```

Report the key, title, and remaining open questions. “Add this to Cadet” means draft only; it is not permission to start implementation.

### Hand work to Gas City

Before handoff, re-read the full task and make sure every required user decision is resolved. Record declared ordering before readiness:

```sh
cadet --project cadet set CADE-12 automation_dependencies=CADE-8,CADE-9
```

Only run the following when the user explicitly says the requirement may start, be queued, or be handed off:

```sh
.agents/skills/cadet-workflow/assets/cadetctl ready CADE-12
```

Readiness is the control-plane boundary. The intake loop will select work under the configured concurrency and conflict limits; do not sling a second workflow manually.

### Monitor work from one or more sessions

All interactive sessions share the same Cadet state and Gas City human mailbox. Use `status` for the lifecycle and `inbox --json` for unread feedback. For each question, read the complete thread before interpreting it:

```sh
.agents/skills/cadet-workflow/assets/cadetctl thread <message-id> --json
```

Summarize the originating requirement, current stage, exact decision requested, safe options, and your recommendation. Continue monitoring unrelated work while one requirement waits; a waiting task does not block the other active slots.

### Answer safely

Ask the user before answering any product, scope, acceptance, data-loss, or architectural tradeoff unless they explicitly granted answer autonomy for that decision. A fact already stated unambiguously in the approved requirement may be relayed without inventing a new decision.

Immediately before sending, re-read the thread. Then reply through the guarded operator command:

```sh
.agents/skills/cadet-workflow/assets/cadetctl answer <message-id> '<approved answer>'
```

Do not call `gc mail reply` directly. `cadetctl answer` serializes concurrent operators, refuses a conflicting second human answer, treats an identical retry as success, marks the question read, and notifies the waiting session.

### Recover failures

Inspect before changing state:

```sh
.agents/skills/cadet-workflow/assets/cadetctl status
.agents/skills/cadet-workflow/assets/cadetctl retry CADE-12 --dry-run
```

Only remove `--dry-run` after explaining the failed run and confirming that retrying the current requirement is intended. Never edit active Gas City or Beads metadata by hand.

## Examples

- “Let’s work out offline sync.” Refine it conversationally; do not create a task yet.
- “Save that requirement in Cadet.” Create a draft, show the stored result, and stop before readiness.
- “CADE-12 looks good; hand it off.” Re-read it, validate dependencies, then run `ready`.
- “What is automation waiting on?” Run `status`, `inbox --json`, and `thread --json`, then present the decision.
- “Tell the worker to preserve the existing file format.” Re-read the thread and send that exact approved answer with `cadetctl answer`.

## Troubleshooting

- If `draft` or `revise` rejects the body, use `--body-file`; do not flatten Markdown into a one-line description.
- If `ready` rejects a task, check that it is `todo`, has `automation=draft`, has a nonempty body, and has no `gas_city_run`.
- If `answer` says the message was already answered, re-read the thread and report the recorded answer; do not override it.
- If the city is unreachable, report the exact command error and use `.agents/skills/cadet-workflow/assets/cityctl doctor`; do not launch replacement background services.
