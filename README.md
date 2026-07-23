# cvc

`cvc` is a single-replica, multi-user Claude Code gateway to the ChatGPT-backed Codex Responses transport. This transport is an internal compatibility boundary and is more change-sensitive than OpenAI's supported public API. Anthropic does not support routing Claude Code to non-Claude models. TLS belongs at a trusted reverse proxy.

## Configure

Required server configuration:

```sh
export CVC_MASTER_KEY="$(openssl rand -base64 32)"
export CVC_DATABASE_URL='sqlite://cvc.db?mode=rwc'
cargo run -- serve
```

The authenticated Codex `/models` endpoint is authoritative. Catalogs are
cached in memory per gateway user for five minutes and refreshed lazily.
Expired-cache refresh failures serve the stale catalog and leave it expired so
the next request retries immediately. An inference `404` forces an immediate
catalog refresh and temporarily suppresses the rejected model until the next
cache interval; this handles model-list propagation delays without permanently
removing a temporarily unavailable model.

New upstream slugs receive deterministic Claude-compatible IDs: dots and other
non-alphanumeric characters become hyphens and `claude-` is prepended.
`CVC_MODELS` is an optional JSON array for stable custom aliases, output limits,
structured-output flags, and cold-start fallback entries. Availability still
comes from upstream whenever discovery succeeds.

Optional discovery controls:

```sh
export CVC_MODEL_CACHE_TTL_SECONDS=300
export CVC_UPSTREAM_MODELS_URL=https://chatgpt.com/backend-api/codex/models
export CVC_CODEX_CLIENT_VERSION=0.144.1
```

A non-private `CVC_BIND` is rejected unless both `CVC_TRUSTED_PROXY=true` and `CVC_PUBLIC_URL=https://…` are set. The Compose example supplies Caddy TLS and immediate SSE flushing. V1 supports exactly one `cvc` replica.

## Users and login

```sh
cvc admin user create alice       # key is printed once
cvc admin user list
cvc admin user rotate alice
cvc admin user revoke alice
cvc admin openai disconnect alice
cvc login --server https://cvc.example.com --token cvc_…
```

## Claude Code configuration

Claude Code accepts gateway-discovered model IDs only when they begin with
`claude-` or `anthropic-`. CVC automatically converts discovered Codex slugs:

```text
gpt-5.6-sol → claude-gpt-5-6-sol (display name: GPT-5.6-Sol)
```

An optional `CVC_MODELS` entry with the same `upstream` value preserves its
configured alias instead.

### CVM plugin

The repository is also a [CVM](https://github.com/alexandernicholson/cvm)
plugin. It requires the
[cvp profile manager](https://github.com/alexandernicholson/cvp):

```sh
cvm plugin install alexandernicholson/cvp
cvm plugin install alexandernicholson/cvc
```

To install directly from a source checkout instead:

```sh
./install-cvm-plugin.sh
```

Create a secure `codex` profile and activate it:

```sh
export CVC_GATEWAY_KEY='cvc_…'
cvm cvc configure --url https://cvc.example.com
unset CVC_GATEWAY_KEY
```

The token is requested without echoing when `configure` runs interactively.
For automation, prefer `CVC_GATEWAY_KEY`; `--token` can expose the secret in
process listings and shell history. Profiles are written atomically with mode
`0600`.

Useful commands:

```sh
cvm cvc show codex       # profile with secrets masked
cvm cvc status codex     # health, readiness, auth, and model discovery
cvm cvc test codex       # real Claude Code inference; expects CVC_PLUGIN_OK
cvm profile use default  # return to normal Anthropic configuration
cvm profile use codex    # route Claude Code through cvc
```

Configuration is customizable:

```sh
cvm cvc configure \
  --profile codex \
  --url https://cvc.example.com \
  --default claude-gpt-5-6-sol \
  --opus claude-gpt-5-6-terra \
  --sonnet claude-gpt-5-6-sol \
  --haiku claude-gpt-5-4-mini \
  --subagent claude-gpt-5-6-sol \
  --effort high \
  --concurrency 3
```

Pass `--no-activate` to create or update the profile without changing the
global active profile.

The generated profile includes:

```text
ANTHROPIC_BASE_URL=https://cvc.example.com
ANTHROPIC_AUTH_TOKEN=***
ANTHROPIC_MODEL=claude-gpt-5-6-sol
ANTHROPIC_DEFAULT_OPUS_MODEL=claude-gpt-5-6-terra
ANTHROPIC_DEFAULT_SONNET_MODEL=claude-gpt-5-6-sol
ANTHROPIC_DEFAULT_HAIKU_MODEL=claude-gpt-5-4-mini
CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1
CLAUDE_CODE_SUBAGENT_MODEL=claude-gpt-5-6-sol
CLAUDE_CODE_AUTOCOMPACT_PCT_OVERRIDE=80
CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1
CLAUDE_CODE_EFFORT_LEVEL=high
CLAUDE_CODE_MAX_TOOL_USE_CONCURRENCY=3
ENABLE_TOOL_SEARCH=false
```

`CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1` makes Claude Code send effort for custom
gateway model IDs. `cvc` forwards `low`, `medium`, `high`, `xhigh`, and `max`
unchanged as `reasoning.effort`. `CLAUDE_CODE_SUBAGENT_MODEL` pins subagents to
the selected gateway alias. A client concurrency limit of three stays below
the default `cvc` per-user limit of four.
An 80% auto-compaction threshold leaves headroom for parallel subagent results
and large tool outputs that arrive between Claude Code context checks.

`ENABLE_TOOL_SEARCH=false` is currently required: Claude Code otherwise may
send deferred `tool_reference` blocks, which `cvc` does not yet translate.
Normal tool definitions, parallel tools, and Claude Code subagents are
supported.

### Manual environment

Without CVM/cvp, configure the equivalent environment directly:

```sh
export ANTHROPIC_BASE_URL='https://cvc.example.com'
export ANTHROPIC_AUTH_TOKEN='cvc_…'
export ANTHROPIC_MODEL='claude-gpt-5-6-sol'
export ANTHROPIC_DEFAULT_OPUS_MODEL='claude-gpt-5-6-terra'
export ANTHROPIC_DEFAULT_SONNET_MODEL='claude-gpt-5-6-sol'
export ANTHROPIC_DEFAULT_HAIKU_MODEL='claude-gpt-5-4-mini'
export CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1
export CLAUDE_CODE_SUBAGENT_MODEL='claude-gpt-5-6-sol'
export CLAUDE_CODE_AUTOCOMPACT_PCT_OVERRIDE=80
export CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1
export CLAUDE_CODE_EFFORT_LEVEL='high'
export CLAUDE_CODE_MAX_TOOL_USE_CONCURRENCY=3
export ENABLE_TOOL_SEARCH=false
claude
```

`/v1/messages/count_tokens` accepts the Anthropic request without requiring
`max_tokens`, translates it through the same request path used for inference,
and counts the normalized payload with the GPT-family `o200k` tokenizer. Model
discovery also publishes the upstream `context_window` and the configured
`max_output_tokens`, allowing clients to compact before the upstream rejects
the request.

The service does not store conversations. Prometheus metrics contain counts and
latency only. Application logs never contain authorization, OAuth codes,
prompts, tool results, tokens, or response bodies. Real Codex subscription
tests remain opt-in and are never run in ordinary CI.

An upstream `context_length_exceeded` stream event is returned as Anthropic
`invalid_request_error`, with an instruction to shorten or compact the
conversation. It is not reported as a retryable gateway `502`.

## Attribution

Device authorization paths, the 15-minute expiry, PKCE token exchange behavior, refresh semantics, ChatGPT account claim, Codex Responses endpoint, and required request headers are derived from the Apache-2.0 licensed [OpenAI Codex](https://github.com/openai/codex) implementation. See `NOTICE`.
