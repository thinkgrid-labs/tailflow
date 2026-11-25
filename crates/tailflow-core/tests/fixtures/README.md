# Agent hook fixtures

- `claude-code-2.1.331/` — **real** hook inputs captured from Claude Code
  2.1.331 (`claude -p`, Haiku) on 2026-11-25 with a recorder hook beside
  TailFlow's. Paths, prompt and assistant text were replaced; field names,
  types and shapes are exactly as sent.
- `codex-0.172.0-synthetic/` — **synthetic** inputs written from the
  published Codex hook input schemas
  (`codex-rs/hooks/schema/generated/*.input.schema.json`, `rust-v0.172.0`).
  Codex was not available to capture live payloads; the shape of
  `tool_response` for Bash is not documented and is a guess here.
