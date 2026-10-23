# Hermes core: patches and internals

## Core patches

None. Core-session voice (`core_session.py`) needs no change to Hermes Agent.

## Hermes internals it relies on

`core_session.py` runs inside `hermes whatsapp-voice serve` (Hermes's own interpreter) and uses these
Hermes modules directly. None are part of the public plugin API, so a Hermes update can rename them;
everything that touches them is in `HermesSession`, and if `/voice/start` fails the sidecar falls back
to the separate voice agent (`context_mode: separate` behaviour) for that call.

| What | Used for |
|---|---|
| `hermes_state.SessionDB`: `load_gateway_routing_entries`, `get_compression_tip`, `get_session`, `get_messages_as_conversation`, `acquire_session_turn_lease` / `release_session_turn_lease`, `append_messages_batch` | Find the owner's DM session, read its stored system prompt, tool pin and history; write the call back under the cross-process turn lease |
| `gateway.whatsapp_identity.canonical_whatsapp_identifier` | Match the owner's number to the session key and to texts (LID/phone aliases) |
| `pre_gateway_dispatch` hook (public plugin hook) returning `{"action": "skip"}` | During a call, the owner's texts go into the call instead of a second text reply |
| `gateway.session_context.set_session_vars` / `clear_session_vars`, `tools.approval_context.set_current_session_key` | Bind the session for tool calls, as a gateway turn does |
| `tools.approval.register_gateway_notify` / `resolve_gateway_approval` / `unregister_gateway_notify` | Ask the owner on the call to approve dangerous commands, with the gateway's approval rules |
| `gateway.run`: `_resolve_runtime_agent_kwargs`, `_resolve_gateway_model`, `_load_gateway_config`, `_checkpoint_agent_kwargs`, `_current_max_iterations`, `GatewayRunner._load_fallback_model`, `GatewayRunner._load_reasoning_config`; `hermes_cli.tools_config._get_platform_tools` | Build an `AIAgent` the way the gateway does for a WhatsApp turn (same model, toolsets, fallback) |
| `run_agent.AIAgent`: `_invoke_tool`, `_build_system_prompt` | Run a tool with hooks, middleware and inline tools (memory, session_search, delegate_task, memory providers); build a prompt for a session that has none stored yet |

Checked against Hermes Agent 0.22.x. This is why core-session mode is not in the plugin-catalog
build (catalog rule 9 forbids private core imports); `context_mode: separate` needs none of it.
