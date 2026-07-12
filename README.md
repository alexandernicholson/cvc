# cvc

`cvc` is a single-replica, multi-user Claude Code gateway to the ChatGPT-backed Codex Responses transport. This transport is an internal compatibility boundary and is more change-sensitive than OpenAI's supported public API. Anthropic does not support routing Claude Code to non-Claude models. TLS belongs at a trusted reverse proxy.

## Configure

Required server variables:

```sh
export CVC_MASTER_KEY="$(openssl rand -base64 32)"
export CVC_MODELS='[{"alias":"claude-gpt-5-6-sol","display_name":"GPT-5.6-Sol","upstream":"gpt-5.6-sol","efforts":["low","medium","high","xhigh","max"],"context_limit":372000,"output_limit":32000,"structured_output":true}]'
export CVC_DEFAULT_MODEL=claude-gpt-5-6-sol
export CVC_DATABASE_URL='sqlite://cvc.db?mode=rwc'
cargo run -- serve
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
`claude-` or `anthropic-`. Use a Claude-compatible alias while keeping the real
Codex name as its display name and upstream target:

```text
claude-gpt-5-6-sol → GPT-5.6-Sol → gpt-5.6-sol
```

### CVM plugin

The repository is also a [CVM](https://github.com/alexandernicholson/cvm)
plugin. It requires the
[cvp profile manager](https://github.com/alexandernicholson/cvp):

```sh
cvm plugin install alexandernicholson/cvp
cvm plugin install DragonStuff/cvc
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
export CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1
export CLAUDE_CODE_EFFORT_LEVEL='high'
export CLAUDE_CODE_MAX_TOOL_USE_CONCURRENCY=3
export ENABLE_TOOL_SEARCH=false
claude
```

`/v1/messages/count_tokens` intentionally returns 404 so Claude Code uses its
local estimate. The service does not store conversations. Prometheus metrics
contain counts and latency only. Application logs never contain authorization,
OAuth codes, prompts, tool results, tokens, or response bodies. Real Codex
subscription tests remain opt-in and are never run in ordinary CI.

## Attribution

Device authorization paths, the 15-minute expiry, PKCE token exchange behavior, refresh semantics, ChatGPT account claim, Codex Responses endpoint, and required request headers are derived from the Apache-2.0 licensed [OpenAI Codex](https://github.com/openai/codex) implementation. See `NOTICE`.
