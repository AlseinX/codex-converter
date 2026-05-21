# AGENTS.md

## Protocol Research Requirements

All protocol and conversion behavior research must be verified against the **latest official documentation** for both APIs:

- **Anthropic Messages API:** https://platform.claude.com/docs/en/api
- **OpenAI Responses API:** https://developers.openai.com/api/reference/responses/
- **Codex CLI source code:** https://github.com/openai/codex (primary source for Codex behavior — prefer over third-party articles)

### Mandatory Process

1. **Verify against primary sources.** All understanding of protocol definitions must come from official documentation or official repository source code. Every factual claim about API behavior, field names, event types, or value mappings must cite official documentation or source code. Third-party articles (blog posts, tutorials) are not authoritative for protocol definitions — check publication dates and prefer current official docs.

2. **Record in `docs/protocol_research.md`.** Every research finding must be documented using the following format:

```markdown
## N. Descriptive title

### Description
What question is being investigated and why.

### Result
The verified conclusion with inline reference numbers [1][2] citing the Reference section.

### Reference
Numbered list with source descriptions and URLs.
```

3. **Cross-protocol conversion requires comparative research.** When the two protocols cannot be directly mapped (e.g., one side has a field or behavior with no straightforward equivalent on the other side), the Result must follow this structure:
   - **First:** Describe and cite how other existing conversion implementations handle the same scenario (e.g., CLIProxyAPI, LiteLLM, open-source proxies). Each cited implementation's approach must include a reference link.
   - **Then:** Evaluate whether any existing approach solves the problem completely. If none do, design a new solution with explicit reasoning for why it is superior, and document any known limitations or trade-offs. The goal is the most correct solution, not merely the most popular one.

4. **Spec references research.** Every conversion mapping in the design spec (`docs/superpowers/specs/2026-05-13-codex-conv-design.md`) should trace back to an entry in `docs/protocol_research.md`.

### Information Freshness

Both APIs and Codex CLI evolve rapidly. Before relying on any research entry, verify it is still current. If an entry is outdated, update it with new references and note the change.

## Integration Testing Requirement

Integration tests are the **sole acceptance criterion** for all features. Unit tests may supplement but can never replace integration tests.

- All integration tests must run without `#[ignore]`. Adding `#[ignore]` to an integration test is **strictly forbidden**.
- Integration tests must exercise the full system end-to-end: proxy receives request, converts protocol, communicates with upstream, and returns correct response to the client.
- A feature is not done until its integration tests pass against the real or mock upstream.

## Code Quality Gates

All code must pass `cargo clippy` and `cargo fmt` checks with zero warnings. Run both before committing:

```
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Do not suppress clippy warnings with `#[allow(...)]` unless the reason is documented inline.

## Coding Behavior Contract (12 Rules)

### Core (Karpathy via Forrest Chang)

1. Think before coding. State your assumptions. Surface tradeoffs.
   Ask before guessing. Push back when a simpler approach exists.
2. Simplicity first. Minimum code that solves the problem. No
   speculative features. No abstractions for single-use code.
3. Surgical changes. Touch only what is asked. Do not "improve"
   adjacent code, comments, or formatting. Match existing style.
4. Goal-driven execution. Define success criteria. Loop until
   verified. Do not narrate steps; tell me what success looks like.

### Extended (Mnimiy, May 2026)

5. Do not make the model do non-language work. Retry policies,
   routing, escalation thresholds belong in deterministic code.
6. Hard token budgets, no exceptions. Stop and ask if a task is
   trending past its budget.
7. Surface conflicts, do not average them. If two parts of the
   codebase disagree, flag the disagreement and ask which to follow.
8. Read before you write. Understand adjacent code (the file and
   nearby siblings) before adding new code.
9. Tests are required but are not the goal. A passing test that
   tests nothing useful is a failure. Tests must check behavior.
10. Long-running operations require checkpoints. After every
    significant step, summarize what was done and confirm before
    proceeding.
11. Convention beats novelty. In an established codebase, match
    the existing pattern even if a "better" one exists.
12. Fail visibly, not silently. Surface every skipped record,
    every rolled-back transaction, every constraint violation.
    Never report success when something was bypassed.
