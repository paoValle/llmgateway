# Contributing

Thanks for your time. This is a side project: the bar is "readable in six months",
not "production-grade". But the bar is still high.

## Work loop

1. Open an **issue** describing the problem in 5 lines. If you cannot write it, the
   problem is not clear yet.
2. Branch: `feat/xyz`, `fix/xyz`, `chore/xyz`.
3. One commit = one concept. Messages in [Conventional Commits](https://www.conventionalcommits.org/).
4. Open the PR. Max ~300 lines. The description says **why**, not **what**: the diff says
   the *what*.
5. `make ci` must be green. Red is not merged "I will fix it later".

## Style

- The automatic formatter decides (`.editorconfig` + the language config).
- No secrets, no real data, no binary committed by mistake.
- If you take code or an idea from outside: link it in the file header or in the commit.
- Comments explain the **why**. The code says the **what**.
- Non-obvious decisions go into `docs/adr/`, not in a scattered comment.

## Reporting problems

Open an issue. If it is a bug, the ideal is a minimal reproducible case.
