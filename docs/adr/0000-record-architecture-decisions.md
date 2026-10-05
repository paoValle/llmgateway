# ADR 0000 — How we write decisions

The technical decisions that are *not* obvious are recorded here, one per file.
Goal: in two years, understand **why** the code is the way it is, not **what** it does.

The format is Michael Nygard's, lightweight: the rest of the document exists only if it is
needed.

## When an ADR is needed

Write an ADR if, rereading the diff, someone could ask you *"but why didn't you do X?"*.
In practice:

- choosing a dependency (or refusing one)
- an algorithm, a data structure, a serialization format
- a module boundary, where a responsibility ends
- a choice that binds the project to an external service
- performance: a deliberate trade-off (memory vs CPU, latency vs cost)

**No** ADR is needed for: function names, formatting, obvious bug fixes, choices a senior
would make the same way.

## File format

`docs/adr/NNNN-title-in-kebab-case.md`, progressive numbering never reused.
ADRs are immutable: if the decision changes, you write a new one that supersedes it.

```markdown
# ADR NNNN — Title

- **Status:** proposed | accepted | superseded by [NNNN]
- **Date:** YYYY-MM-DD
- **Decides:** Paolo Valletta

## Context
Which forces are at play. Facts, not opinions. If there are numbers, they go here.

## Decision
What we decide, in one active sentence. "We will use X", not "X has been chosen".

## Alternatives
| Option | Pros | Cons | Why not |
|---|---|---|---|
| A | | | |
| B | | | |

## Consequences
What becomes possible, what becomes impossible, what we exposed ourselves to.
The negative things matter more than the positive ones.

## Verification
How do we know whether the decision was right? Which measure, by which date.
```

## The number 0000

This file is not a decision, it is the rule. Do not renumber it.
