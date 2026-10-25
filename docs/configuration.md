# Configuration

There are three places to change settings, and all of them write the same values:
- the **WhatsApp Voice** tab in the Hermes web dashboard;
- the plugin's settings page in the Hermes Desktop app (Settings > Plugins);
- the command line: `hermes config set plugins.entries.hermes-whatsapp-voice.settings.<key> <value>`.

Restart the sidecar after a change: the dashboard's **Restart sidecar** button, or
`systemctl --user restart hermes-whatsapp-voice` (Linux), or
`launchctl kickstart -k gui/$(id -u)/com.hermes.whatsapp-voice` (macOS). If something else
supervises the sidecar (s6, supervisord, a container's init), set `HWV_RESTART_COMMAND` in the
dashboard's environment to that restart command, e.g. `s6-svc -r /run/service/whatsapp-voice`, and
the dashboard's restart uses it.

## Settings

| key | default | |
|---|---|---|
| `owner_number` | (required) | Your number, international format |
| `voice_model` | `gemini-3.8-live` | Gemini Live model |
| `voice` | `Kore` | Gemini prebuilt voice |
| `voice_provider` | `grok` | `grok`, `openai` or `gemini`; grok and openai fall back to Gemini if they cannot connect |
| `openai_model` | `gpt-realtime-2.1` | OpenAI Realtime model, used when `voice_provider` is `openai` |
| `openai_voice` | `cedar` | OpenAI voice: `alloy`, `ash`, `ballad`, `coral`, `echo`, `sage`, `shimmer`, `verse`, `marin`, `cedar` |
| `assistant_name` | `Hermes` | Fallback name when Hermes gives no persona briefing |
| `languages` | (empty) | Language codes the voice may speak and listen for, e.g. `en-US, ta-IN`; empty allows any |
| `voice_instructions` | (empty) | Your own extra rules for the voice: language, tone, call length |
| `live_config` | `{}` | Advanced: any Gemini Live setup field, merged over the defaults (see below) |
| `ring_timeout_seconds` | `45` | How long an outbound call rings |
| `max_call_minutes` | `15` | Hard cap per call |
| `silence_hangup_seconds` | `180` | End a call when nobody has spoken for this long (e.g. left on hold) |
| `answer_inbound` | `true` | Answer calls from `owner_number` |
| `send_summary` | `true` | Message a summary after each call |
| `listen` | `127.0.0.1:8796` | Sidecar API address (loopback) |
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

## Credentials

The plugin never asks for its own keys. It reuses what Hermes already has:
- **Gemini:** the key of Hermes's Gemini provider, `GOOGLE_API_KEY`, else `GEMINI_API_KEY`, from
  Hermes's `.env` (`~/.hermes/.env` by default). Add one with `hermes model` (choose Gemini). Google login (Cloud Code OAuth)
  doesn't work for the Live API.
- **OpenAI (`voice_provider: openai`):** the ChatGPT login Hermes already holds
  (`hermes auth login openai-codex`), else the Codex CLI login (`$CODEX_HOME/auth.json`). It mints a
  short-lived Realtime client secret per call, so the voice runs on the ChatGPT subscription, not
  on a metered API key. The login is only read: Hermes refreshes it, the sidecar never does. Check
  it without ringing anyone: `hermes-wa-voice openai-check`.
- **Hermes API server:** `API_SERVER_KEY` from Hermes's `.env`. Without it, calls still work but
  the voice can't hand tasks to Hermes, and summaries are raw transcripts.

## Troubleshooting

See the table at the end of the [setup runbook](../skills/setup/SKILL.md#troubleshooting).
