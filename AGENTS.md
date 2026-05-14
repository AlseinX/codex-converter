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
