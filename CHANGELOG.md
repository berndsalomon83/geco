# Changelog

Notable changes per release. Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versioning is [semver](https://semver.org/), with the pre-1.0 caveat that a
minor bump may break.

## [Unreleased]

## [0.9.1] — 2026-10-10

### Changed

- Dependency updates: `tokio` 1.53.2, `clap` 4.6.7, `schemars` 1.2.2 (which
  moves `serde_derive_internals` to 0.30). No behaviour change in geco.

## [0.9.0] — 2026-10-10

First public release.

geco is an MCP and skill gateway: it sits between your agent and your MCP
servers and skills, and keeps their schemas out of the model's context until
they are asked for.

- **Three meta-tools, ~2.5 KB of context**, whatever is behind the gateway:
  `search` (tools and skills by keyword, `"*"` for the catalogue, schemas
  inline on request), `execute` (1–10 calls per round-trip, cross-backend
  calls in parallel; images, audio and resources come back as real content
  blocks, structured output under `structured`) and `read_skill`.
  `proxy.surface: full` adds `list_mcp_servers`, `list_tools` and
  `list_skills`.
- **Lexical search** (BM25F with identifier splitting, conservative stemming
  and a small measured synonym table), extendable with
  `proxy.search_synonyms`.
- **Optional LLM picker** for requests that share no words with the tool they
  need: MCP sampling or any command, the whole catalogue when it fits,
  falling back to the lexical order on any failure.
- **Skills on demand**: a skill library served through `search` and
  `read_skill`, with `skills.absorb` sweeping skills out of an agent's
  auto-loaded directory.
- **Nothing restarts**: backends start on first use, the config and skill
  directories are watched, `tools/list_changed` from backends is followed,
  and geco notifies its client when its own tool list changes.
- **Isolation and secrets**: each backend gets a clean environment plus what
  its `env` / `pass_env` declare; `${keychain:…}` resolves from the OS
  keychain at spawn time; `allow` / `deny` globs are enforced at listing and
  at execution.
- **Self-healing backends** with active health probes; `geco health`,
  `geco stats`, `geco inspect`, `geco validate` and `geco refresh` on the
  CLI.
- **Prebuilt binaries** for macOS (arm64, x86_64), Linux (x86_64 static,
  aarch64) and Windows (x86_64), with SHA256SUMS and a CycloneDX SBOM, and
  one-line installers: `curl … | bash` and `irm … | iex`.

[Unreleased]: https://github.com/berndsalomon83/geco/compare/v0.9.1...HEAD
[0.9.1]: https://github.com/berndsalomon83/geco/compare/v0.9.0...v0.9.1
[0.9.0]: https://github.com/berndsalomon83/geco/releases/tag/v0.9.0
