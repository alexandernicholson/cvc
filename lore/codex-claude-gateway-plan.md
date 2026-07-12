# Rust Codex-to-Claude Code Gateway

## Summary

Build `cvc`, a lean Rust service that exposes the Anthropic Messages protocol expected by Claude Code and translates requests to the ChatGPT-backed Codex Responses endpoint. Each gateway user supplies their own Codex subscription through device-code OAuth; no OpenAI API key is required.

The design targets Claude Code compatibility, not universal Anthropic API emulation. Anthropic requires `/v1/messages?beta=true`, real-time SSE, open-ended beta fields, and evolving gateway behavior, so compatibility fixtures and release testing are first-class requirements. [Anthropic gateway protocol](https://code.claude.com/docs/en/llm-gateway-protocol)

## Core Implementation

- Create one Rust workspace and binary using Tokio, Axum, Reqwest, Serde, Clap, SQLx, Tower, and tracing.
- Organize the application around protocol, translator, OpenAI client, authentication, persistence, HTTP server, and CLI modules.
- Follow the Apache-2.0 OpenAI Codex implementation for device OAuth, token refresh, Codex request headers, and ChatGPT endpoint behavior, retaining required attribution. Do not depend on an installed Codex binary. [OpenAI Codex source](https://github.com/openai/codex)
- Bind plain HTTP internally; production deployment terminates TLS at Caddy, nginx, or another trusted reverse proxy. Refuse unsafe public-bind configurations unless trusted proxy and public URL settings are explicit.
- Provide Dockerfile, Compose example with Caddy, health checks, structured configuration, graceful shutdown, and database migrations.

### Authentication and tenancy

- Admins manage users locally with:
  - `cvc admin user create <name>`: creates a user and prints the gateway bearer key once.
  - `cvc admin user revoke|rotate|list`.
  - `cvc admin openai disconnect <user>`.
- Hash gateway keys with Argon2id; never store recoverable key material.
- Users run `cvc login --server <url> --token <gateway-key>`.
- The server initiates OpenAI device authorization, returns the verification URL and one-time code, polls at the prescribed interval, validates the resulting account/workspace claims, and stores the refreshable credential for that user.
- Device-login operations expire after 15 minutes, are single-use, rate-limited, and may be cancelled.
- Refresh OpenAI access tokens under a per-user lock so concurrent Claude Code requests cannot race token rotation.
- Encrypt OpenAI access, refresh, and ID tokens with AES-256-GCM using a required deployment master key; include user ID and credential version as authenticated data.
- Never log authorization headers, OAuth codes, tokens, complete prompts, tool results, or response bodies.

### Persistence

- Define repository traits for users, gateway credentials, encrypted OpenAI credentials, OAuth attempts, and migrations.
- Ship a SQLx SQLite implementation using WAL mode and bounded connection settings.
- Keep SQL and domain types separated so PostgreSQL can be added without changing protocol or business logic; PostgreSQL itself is not part of v1.
- Support one service replica in v1 and document this explicitly.

## Public Interfaces and Translation

### HTTP endpoints

- `HEAD /`: connectivity probe.
- `GET /healthz` and `GET /readyz`: liveness and database/config readiness.
- `POST /auth/device/start`, `GET /auth/device/{id}`, and `DELETE /auth/device/{id}`: gateway-authenticated device login.
- `DELETE /auth/openai`: disconnect the caller’s OpenAI account.
- `GET /v1/models?limit=1000`: Claude Code model discovery.
- `POST /v1/messages` and `/v1/messages?beta=true`: streaming and non-streaming Messages responses.
- Do not implement `/v1/messages/count_tokens` in v1; return a normal 404 so Claude Code uses its documented local estimate rather than presenting inaccurate counts.

Accept gateway credentials through `Authorization: Bearer`; return Anthropic-shaped JSON errors with stable request IDs. Preserve `Retry-After` where applicable and map upstream authentication, rate-limit, validation, overload, and server failures to appropriate Anthropic status/error categories.

### Model discovery and effort

- Configure a global model catalog containing:
  - Anthropic-compatible alias beginning with `claude-`.
  - Display name.
  - Actual Codex model ID.
  - Supported reasoning efforts.
  - Context and output limits used for validation/documentation.
- Include a required default alias such as `claude-codex-default`; administrators explicitly choose its upstream model rather than relying on a stale hard-coded model.
- Return aliases and display names from `/v1/models`; document `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`.
- Supply a generated Claude Code configuration snippet with `ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN`, default model aliases, and discovery enabled.
- Translate `output_config.effort` from Claude Code into OpenAI `reasoning.effort`.
- Preserve supported `low`, `medium`, `high`, `xhigh`, and `max` values; reject efforts unsupported by the selected upstream model with an actionable validation error rather than silently changing quality.

Claude Code only accepts discovered IDs beginning with `claude` or `anthropic`, so aliases are necessary even though their display names identify the real Codex models. [Claude Code model-discovery contract](https://code.claude.com/docs/en/llm-gateway-protocol#model-discovery)

### Request translation

- Convert system blocks into OpenAI instructions without merging away meaningful block order.
- Convert user/assistant text blocks into Responses input messages.
- Convert assistant `tool_use` blocks into `function_call` items and user `tool_result` blocks into matching `function_call_output` items, preserving call IDs and parallel calls.
- Convert Anthropic tool definitions and tool choice into OpenAI function tools, preserving JSON Schema while removing only known Anthropic-only annotations.
- Support Claude Code image attachments by converting base64 and URL image blocks into Responses image inputs.
- Preserve prior Codex reasoning items through Anthropic thinking blocks by placing upstream encrypted reasoning content in the thinking signature and restoring it on the next request.
- Translate `output_config.format` to Responses structured output when the chosen model supports it; otherwise return a clear unsupported-feature error.
- Treat prompt-cache hints, Anthropic context-management requests, attribution blocks, and known Claude-only beta fields as compatibility metadata: consume them without forwarding invalid fields upstream.
- Deserialize unknown headers and body fields permissively, record only field names in debug telemetry, and fail only when an unknown field changes semantics that cannot safely be represented.
- Use stateless, full-history requests with upstream storage disabled; do not maintain a second conversation database.

### Response translation

- Implement an explicit per-request streaming state machine.
- Translate Responses events into the exact Anthropic SSE sequence:
  - `message_start`
  - ordered `content_block_start`
  - incremental text, thinking, and `input_json_delta` events
  - corresponding `content_block_stop`
  - cumulative `message_delta` usage and stop reason
  - `message_stop`
- Stream tool arguments incrementally and validate that the accumulated value is a JSON object at block completion.
- Support interleaved reasoning, text, and multiple parallel function calls without reusing block indexes.
- Map completion states to `end_turn`, `tool_use`, `max_tokens`, `stop_sequence`, or `refusal` as applicable.
- Map input, output, cached-input, and reasoning usage without fabricating unavailable Anthropic cache fields.
- Forward mid-stream upstream failures as Anthropic `event: error`; never emit `message_stop` after a failed stream.
- For `stream: false`, run the same event/state machinery into an accumulator to ensure streaming and non-streaming results remain equivalent.
- Disable proxy buffering and flush every completed SSE frame immediately. Anthropic explicitly requires inference responses to stream. [Anthropic streaming format](https://platform.claude.com/docs/en/build-with-claude/streaming)

## Reliability and Security

- Apply request-body limits, header limits, maximum tool count/schema size, bounded upstream timeouts, SSE idle timeout, and per-user concurrency limits.
- Cancel the upstream request when the Claude Code connection closes.
- Do not automatically retry inference after any response bytes have been emitted; allow Claude Code’s retry policy to act. Before streaming starts, retry only safe transient connection failures with bounded jitter.
- Isolate all authentication, rate limits, concurrency, and refresh state by gateway user.
- Provide configurable request-ID and Claude session/agent ID telemetry while treating those IDs as attribution metadata rather than identity.
- Expose Prometheus-compatible counts and latency histograms without prompt contents or model output.
- Mark the ChatGPT-backed Codex transport as an internal compatibility boundary: pin tested behavior, surface upstream protocol changes clearly, and keep an adapter interface so the supported public OpenAI Responses API can be added later.
- Document that Anthropic does not support routing Claude Code to non-Claude models and that ChatGPT-backed transport is more change-sensitive than the public OpenAI API. [Anthropic gateway guidance](https://code.claude.com/docs/en/llm-gateway)

## Test Plan

- Unit-test every request content block, tool schema/choice, reasoning-effort mapping, stop reason, usage mapping, error mapping, and credential transition.
- Use table-driven streaming tests for fragmented SSE frames, UTF-8 boundaries, delayed function names, partial tool JSON, multiple calls, reasoning/text interleaving, upstream errors, disconnects, and incomplete responses.
- Validate all emitted streams with an Anthropic event accumulator and assert legal event ordering and contiguous block indexes.
- Add sanitized fixtures derived from current Claude Code requests and OpenAI Responses streams, including the attribution block and evolving beta fields.
- Add mock-upstream integration tests covering OAuth refresh races, 401 refresh-and-retry, 429 propagation, cancellation, non-streaming accumulation, and user isolation.
- Add migration, encryption round-trip, wrong-master-key, key rotation, revoked-user, and database-lock tests.
- Run Claude Code black-box tests for:
  - Initial prompt and streamed text.
  - `/model` discovery and model switching.
  - `/effort` at every supported level.
  - Single and parallel tools.
  - Tool failure output.
  - Images.
  - Long multi-turn conversations with reasoning preservation.
  - Compaction and subagents.
  - Stream interruption and retry without duplicate tool execution.
- Gate real Codex subscription tests behind explicit environment variables; never run them in ordinary CI.
- Maintain a tested Claude Code version matrix with latest stable plus one previous release, because gateway fields evolve with Claude Code releases.

## Assumptions

- V1 is a single-replica, multi-user service where every user brings their own eligible ChatGPT/Codex subscription.
- User registration is closed; administrators issue access keys through local CLI commands.
- Device-code OAuth is the only OpenAI login flow in v1.
- Model aliases are globally administered and visible through opt-in Claude Code discovery; per-user mappings are deferred.
- SQLite is the shipped backend, behind repository interfaces prepared for a later PostgreSQL implementation.
- TLS, certificate renewal, and public ingress are owned by a reverse proxy.
- Broad Anthropic SDK compatibility, token counting, native TLS, web administration, OIDC, OpenAI API-key transport, horizontal scaling, and server-side conversation storage are out of scope for v1.
