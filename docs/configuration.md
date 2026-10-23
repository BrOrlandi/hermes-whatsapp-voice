# Configuration

There are three places to change settings, and all of them write the same values:
- the **WhatsApp Voice** tab in the Hermes web dashboard;
- the plugin's settings page in the Hermes Desktop app (Settings > Plugins);
- the command line: `hermes config set plugins.entries.hermes-whatsapp-voice.settings.<key> <value>`.

Restart the sidecar after a change: the dashboard's **Restart sidecar** button, or
`systemctl --user restart hermes-whatsapp-voice` (Linux), or
`launchctl kickstart -k gui/$(id -u)/com.hermes.whatsapp-voice` (macOS).

## Settings

| key | default | |
|---|---|---|
| `context_mode` | `core_session` | `core_session`: the voice is the model of your WhatsApp chat with Hermes (see below). `separate`: a separate voice agent that asks Hermes through `ask_hermes` |
| `owner_number` | (required) | Your number, international format |
| `voice_model` | `gemini-3.8-live` | Gemini Live model |
| `voice` | `Kore` | Gemini prebuilt voice |
| `assistant_name` | `Hermes` | Fallback name when Hermes gives no persona briefing |
| `languages` | (empty) | Language codes the voice may speak and listen for, e.g. `en-US, ta-IN`; empty allows any |
| `voice_instructions` | (empty) | Your own extra rules for the voice: language, tone, call length |
| `vocabulary` | (empty) | Words transcripts should spell as given, e.g. names; your own name is added automatically |
| `history_chars` | `30000` | `core_session`: recent chat history given to the voice at call start |
| `live_config` | `{}` | Advanced: any Gemini Live setup field, merged over the defaults (see below) |
| `ring_timeout_seconds` | `45` | How long an outbound call rings |
| `max_call_minutes` | `15` | Hard cap per call |
| `silence_hangup_seconds` | `180` | End a call when nobody has spoken for this long (e.g. left on hold) |
| `answer_inbound` | `true` | Answer calls from `owner_number` |
| `send_summary` | `true` | Message a summary after each call |
| `listen` | `127.0.0.1:8796` | Sidecar API address (loopback) |
| `context_listen` | `127.0.0.1:8797` | `core_session` service address (loopback) |
| `hermes_api_url` | `http://127.0.0.1:8642` | Hermes API server |

**Every Gemini Live option.** `live_config` takes any field of the Live API's setup message and
merges it over the plugin's defaults. Objects merge key by key. `tools` is appended to the
plugin's own tools rather than replacing them. For example, to change the voice and temperature, and
let the caller pause longer:

```yaml
# ~/.hermes/config.yaml
plugins:
  entries:
    hermes-whatsapp-voice:
      settings:
        live_config:
          generationConfig:
            temperature: 0.6
            speechConfig: {voiceConfig: {prebuiltVoiceConfig: {voiceName: Puck}}}
          realtimeInputConfig:
            automaticActivityDetection: {silenceDurationMs: 900, endOfSpeechSensitivity: END_SENSITIVITY_LOW}
```

For languages, prefer the `languages` setting. A `languageCodes` value inside `live_config` wins
over it.

What each field does, and which ones a given model supports:
[Live API reference (setup message)](https://ai.google.dev/api/live),
[Live API capabilities guide](https://ai.google.dev/gemini-api/docs/live-guide),
[session management](https://ai.google.dev/gemini-api/docs/live-session),
[tool use](https://ai.google.dev/gemini-api/docs/live-tools),
[best practices](https://ai.google.dev/gemini-api/docs/live-api/best-practices),
[gemini-3.8-live model page](https://ai.google.dev/gemini-api/docs/models/gemini-3.8-live), and
[voices](https://ai.google.dev/gemini-api/docs/speech-generation). There is no speaking-rate
parameter. Ask for pace and tone in `voice_instructions` instead.

**Personality.** The voice doesn't use a built-in persona. At startup and after each call, the
sidecar asks your Hermes for a short briefing: its name, its personality, and what it knows about
you. The voice then sounds like your Hermes, and changes to Hermes's memory carry over. Use
`voice_instructions` for anything else, for example:

```bash
hermes config set plugins.entries.hermes-whatsapp-voice.settings.voice_instructions \
  "Keep calls very short and direct. Never ask if there is anything else."
```

## Core-session mode

With `context_mode: core_session` (the default), `hermes whatsapp-voice serve` also runs a small
loopback service in Hermes's own interpreter, and each call becomes part of your WhatsApp DM
session with Hermes:

- **Context:** the voice gets that session's stored system prompt (persona, memory, skills), its
  session context, and recent history (`history_chars`), loaded before the call starts.
- **Tools:** all of the session's own tools, run through Hermes in that session exactly as in a
  text turn (hooks, memory, memory providers, the same approval rules). When a command needs
  approval, the voice asks you and answers with `approve_command`.
- **Write-back:** what you and the voice say, and its tool calls, are saved into the session as the
  call goes (marked `display_kind: voice_call`), under Hermes's cross-process turn lock, so your next
  text message continues with the call in context.
- **Texts during a call** go into the call: the voice answers them, and the text agent stays quiet
  (a gateway hook; slash commands, media and other chats are unaffected). Needs a gateway restart
  after installing or updating the plugin.

It uses Hermes internals that are not part of the plugin API (listed in `CORE_PATCHES.md`); if the
service is unavailable, a call falls back to the separate voice agent. It needs a WhatsApp DM
session with the bot to exist (send it one text first).

## Credentials

The plugin never asks for its own keys. It reuses what Hermes already has:
- **Gemini:** the key of Hermes's Gemini provider, `GOOGLE_API_KEY`, else `GEMINI_API_KEY`, from
  Hermes's `.env` (`~/.hermes/.env` by default). Add one with `hermes model` (choose Gemini). Google login (Cloud Code OAuth)
  doesn't work for the Live API.
- **Hermes API server:** `API_SERVER_KEY` from Hermes's `.env`. Without it, calls still work but
  the voice can't hand tasks to Hermes, and summaries are raw transcripts.

## Troubleshooting

See the table at the end of the [setup runbook](../skills/setup/SKILL.md#troubleshooting).
