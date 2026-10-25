//! OpenAI Realtime voice client for WhatsApp calls, on the owner's ChatGPT subscription.
//!
//! Auth follows hermes-talk `talk_auth.py` and `talk_wire.py`: the host's `openai-codex`
//! login (the ChatGPT sign-in Hermes already holds) mints an ephemeral Realtime client
//! secret, and only that secret reaches the socket. The login is read, never refreshed or
//! rewritten: Hermes owns that store. A metered OpenAI API key is never used.
//! Call audio is 16 kHz; OpenAI Realtime PCM is 24 kHz.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::audio::{Upsampler, i16_to_le_bytes, le_bytes_to_i16};
use crate::config::Config;
use crate::gemini::{LiveEvent, Setup};
use crate::grok::{jwt_expired, now_secs, redact, sanitize_log};

const WS_URL: &str = "wss://api.openai.com/v1/realtime";
const CLIENT_SECRETS_URL: &str = "https://api.openai.com/v1/realtime/client_secrets";
pub const DEFAULT_MODEL: &str = "gpt-realtime-2.1";
pub const DEFAULT_VOICE: &str = "cedar";
const TRANSCRIPTION_MODEL: &str = "gpt-4o-mini-transcribe";
const PROVIDER: &str = "openai-codex";

const OPENAI_VOICES: &[&str] = &[
    "alloy", "ash", "ballad", "coral", "echo", "sage", "shimmer", "verse", "marin", "cedar",
];

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

enum Outgoing {
    Audio(String),
    Control(String),
    /// Ask the model to speak. Held back while a response is already playing.
    Respond,
}

pub struct Live {
    outbox: mpsc::UnboundedSender<Outgoing>,
    pub events: mpsc::UnboundedReceiver<LiveEvent>,
    upsampler: Mutex<Upsampler>,
    task: JoinHandle<()>,
}

impl Live {
    pub async fn connect(cfg: &Config, setup: Setup) -> Result<Self> {
        let model = openai_model(cfg);
        let voice = openai_voice_name(&cfg.openai_voice);
        let session = session_config(&setup, &model, &voice);
        let ws = open_session(&cfg.hermes_home, &model, &session).await?;
        log::info!("provider=openai voice={voice} model={model}");
        Ok(Self::start(ws))
    }

    fn start(ws: Ws) -> Self {
        let (outbox, out_rx) = mpsc::unbounded_channel();
        let (ev_tx, events) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(ws, out_rx, ev_tx));
        Self {
            outbox,
            events,
            upsampler: Mutex::new(Upsampler::new()),
            task,
        }
    }

    /// 16 kHz mono caller audio. Converted to 24 kHz PCM before it is sent.
    pub fn send_audio(&self, pcm: &[i16]) {
        let up = self.upsampler.lock().unwrap().process(pcm);
        if up.is_empty() {
            return;
        }
        let msg = json!({
            "type": "input_audio_buffer.append",
            "audio": B64.encode(i16_to_le_bytes(&up)),
        });
        let _ = self.outbox.send(Outgoing::Audio(msg.to_string()));
    }

    pub fn send_text(&self, text: &str) {
        let item = json!({
            "type": "conversation.item.create",
            "item": {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": text}],
            },
        });
        let _ = self.outbox.send(Outgoing::Control(item.to_string()));
        let _ = self.outbox.send(Outgoing::Respond);
    }

    /// `scheduling` "SILENT" submits the result without asking the model to speak.
    pub fn send_tool_response(&self, id: &str, _name: &str, response: Value, scheduling: &str) {
        let item = json!({
            "type": "conversation.item.create",
            "item": {
                "type": "function_call_output",
                "call_id": id,
                "output": response.to_string(),
            },
        });
        let _ = self.outbox.send(Outgoing::Control(item.to_string()));
        if scheduling != "SILENT" {
            let _ = self.outbox.send(Outgoing::Respond);
        }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn openai_model(cfg: &Config) -> String {
    let model = cfg.openai_model.trim();
    if model.is_empty() {
        DEFAULT_MODEL.to_string()
    } else {
        model.to_string()
    }
}

pub fn openai_voice_name(configured: &str) -> String {
    let name = configured.trim().to_ascii_lowercase();
    OPENAI_VOICES
        .iter()
        .find(|v| **v == name)
        .map(|v| (*v).to_string())
        .unwrap_or_else(|| DEFAULT_VOICE.to_string())
}

/// Pins transcription only when the owner named exactly one language ("pt-BR" -> "pt").
fn transcription_language(languages: &[String]) -> Option<String> {
    match languages {
        [only] => {
            let code: String = only
                .trim()
                .chars()
                .take_while(|c| c.is_ascii_alphabetic())
                .collect::<String>()
                .to_ascii_lowercase();
            (code.len() == 2).then_some(code)
        }
        _ => None,
    }
}

fn tool_wire(tool: &Value) -> Value {
    let mut parameters = tool
        .get("parameters")
        .cloned()
        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
    if !parameters.is_object() {
        parameters = json!({"type": "object", "properties": {}});
    }
    json!({
        "type": "function",
        "name": tool["name"].as_str().unwrap_or(""),
        "description": tool["description"].as_str().unwrap_or(""),
        "parameters": parameters,
    })
}

/// The GA Realtime session, sent with the mint so the socket opens already configured.
pub fn session_config(setup: &Setup, model: &str, voice: &str) -> Value {
    let mut transcription = json!({"model": TRANSCRIPTION_MODEL});
    if let Some(language) = transcription_language(&setup.languages) {
        transcription["language"] = json!(language);
    }
    let mut session = json!({
        "type": "realtime",
        "model": model,
        "instructions": setup.system_instruction,
        "output_modalities": ["audio"],
        "audio": {
            "input": {
                "format": {"type": "audio/pcm", "rate": 24000},
                "noise_reduction": {"type": "near_field"},
                "turn_detection": {
                    "type": "server_vad",
                    "create_response": true,
                    "interrupt_response": true,
                },
                "transcription": transcription,
            },
            "output": {
                "format": {"type": "audio/pcm", "rate": 24000},
                "voice": voice,
            },
        },
    });
    let tools: Vec<Value> = setup.functions.iter().map(tool_wire).collect();
    if !tools.is_empty() {
        session["tools"] = json!(tools);
        session["tool_choice"] = json!("auto");
    }
    session
}

fn access_token(tokens: &Value) -> Option<String> {
    let access = tokens.get("access_token")?.as_str()?.trim();
    (!access.is_empty()).then(|| access.to_string())
}

/// Read-only. Never refreshes and never writes auth.json. Never returns an API key.
///
/// Hermes's own `openai-codex` login comes first: Hermes refreshes it under its own lock.
/// The Codex CLI store (`$CODEX_HOME/auth.json`) is the fallback.
pub fn read_oauth_access_token(home: &Path, now: i64) -> Result<String> {
    let mut found = None;
    if let Ok(raw) = std::fs::read_to_string(home.join("auth.json")) {
        let data: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
        found = access_token(&data["providers"][PROVIDER]["tokens"]).or_else(|| {
            data["credential_pool"][PROVIDER].as_array().and_then(|rows| {
                rows.iter()
                    .find_map(|row| access_token(row).or_else(|| access_token(&row["tokens"])))
            })
        });
    }
    if found.is_none()
        && let Some(path) = codex_auth_path()
        && let Ok(raw) = std::fs::read_to_string(path)
    {
        let data: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
        found = access_token(&data["tokens"]);
    }
    let token = found.context(
        "no ChatGPT login: run `hermes auth login openai-codex` (or `codex login`)",
    )?;
    if jwt_expired(&token, now) {
        bail!("ChatGPT login is expired; not refreshing and not using an API key");
    }
    Ok(token)
}

fn codex_auth_path() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex")))
        .map(|base| base.join("auth.json"))
}

fn hermes_home_default() -> PathBuf {
    std::env::var_os("HERMES_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".hermes")))
        .unwrap_or_else(|| PathBuf::from("/opt/data"))
}

/// Accepts both the flat `{value}` and the nested `{client_secret: {value}}` shapes.
fn parse_client_secret(payload: &Value) -> Option<String> {
    payload["value"]
        .as_str()
        .or_else(|| payload["client_secret"]["value"].as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

async fn mint(token: &str, session: &Value) -> Result<String> {
    let res = reqwest::Client::new()
        .post(CLIENT_SECRETS_URL)
        .bearer_auth(token)
        .json(&json!({"session": session}))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .context("minting the OpenAI Realtime client secret")?;
    let status = res.status();
    let body: Value = res.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let detail = body["error"]["message"].as_str().unwrap_or("no detail");
        bail!("OpenAI Realtime client secret failed ({status}): {}", sanitize_log(detail));
    }
    parse_client_secret(&body).context("OpenAI Realtime client secret response had no value")
}

async fn open_session(home: &Path, model: &str, session: &Value) -> Result<Ws> {
    let token = read_oauth_access_token(home, now_secs())?;
    let secret = mint(&token, session)
        .await
        .map_err(|e| anyhow!(redact(format!("{e:#}"), &token)))?;
    connect_ws(&secret, model)
        .await
        .map_err(|e| anyhow!(redact(format!("{e:#}"), &secret)))
}

async fn connect_ws(secret: &str, model: &str) -> Result<Ws> {
    let mut req = format!("{WS_URL}?model={model}").into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {secret}").parse()?);
    let (mut ws, _) = tokio::time::timeout(
        Duration::from_secs(20),
        tokio_tungstenite::connect_async(req),
    )
    .await
    .context("timed out connecting to OpenAI Realtime")?
    .context("connecting to OpenAI Realtime")?;
    let ready = async {
        while let Some(frame) = ws.next().await {
            if let Message::Text(t) = frame? {
                let value: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                match value["type"].as_str() {
                    Some("session.created") => return Ok(()),
                    Some("error") => bail!(
                        "OpenAI Realtime refused the session: {}",
                        sanitize_log(value["error"]["message"].as_str().unwrap_or("error"))
                    ),
                    _ => {}
                }
            }
        }
        Err(anyhow!("OpenAI Realtime closed during setup"))
    };
    tokio::time::timeout(Duration::from_secs(20), ready)
        .await
        .context("timed out waiting for OpenAI Realtime")??;
    Ok(ws)
}

#[derive(Default, Debug)]
struct Parsed {
    events: Vec<LiveEvent>,
    response_started: bool,
    response_done: bool,
    error: Option<String>,
}

fn parse_event(msg: &Value) -> Parsed {
    let mut p = Parsed::default();
    match msg["type"].as_str().unwrap_or("") {
        "input_audio_buffer.speech_started" => p.events.push(LiveEvent::Interrupted),
        "response.created" => p.response_started = true,
        "response.done" => p.response_done = true,
        "response.output_audio.delta" => {
            if let Some(pcm) = decode_pcm(msg["delta"].as_str().unwrap_or("")) {
                p.events.push(LiveEvent::Audio(pcm));
            }
        }
        "response.output_audio_transcript.done" => {
            if let Some(t) = msg["transcript"].as_str().filter(|t| !t.is_empty()) {
                p.events.push(LiveEvent::OutputTranscript(t.to_string()));
            }
        }
        "conversation.item.input_audio_transcription.completed" => {
            if let Some(t) = msg["transcript"].as_str().filter(|t| !t.trim().is_empty()) {
                p.events.push(LiveEvent::InputTranscript(t.to_string()));
            }
        }
        "response.function_call_arguments.done" => {
            let args = msg["arguments"]
                .as_str()
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .unwrap_or_else(|| json!({}));
            p.events.push(LiveEvent::ToolCall {
                id: msg["call_id"].as_str().unwrap_or_default().to_string(),
                name: msg["name"].as_str().unwrap_or_default().to_string(),
                args,
            });
        }
        "error" => {
            p.error = Some(
                msg["error"]["message"]
                    .as_str()
                    .or_else(|| msg["error"]["code"].as_str())
                    .unwrap_or("provider error")
                    .to_string(),
            )
        }
        _ => {}
    }
    p
}

fn decode_pcm(data: &str) -> Option<Vec<i16>> {
    if data.is_empty() {
        return None;
    }
    Some(le_bytes_to_i16(&B64.decode(data).ok()?))
}

/// One socket for the whole call: Realtime sessions outlast any call this sidecar allows.
/// Errors on the socket are logged and survived; only a closed socket ends the session.
async fn run(
    mut ws: Ws,
    mut out_rx: mpsc::UnboundedReceiver<Outgoing>,
    ev_tx: mpsc::UnboundedSender<LiveEvent>,
) {
    let mut in_response = false;
    let mut respond_pending = false;
    let mut audio_chunks = 0u64;
    let reason = loop {
        tokio::select! {
            out = out_rx.recv() => {
                let text = match out {
                    Some(Outgoing::Audio(text)) => {
                        audio_chunks += 1;
                        if audio_chunks == 1 || audio_chunks % 500 == 0 {
                            log::debug!("provider=openai caller_audio_chunks={audio_chunks}");
                        }
                        text
                    }
                    Some(Outgoing::Control(text)) => text,
                    Some(Outgoing::Respond) if in_response => {
                        respond_pending = true;
                        continue;
                    }
                    Some(Outgoing::Respond) => {
                        in_response = true;
                        json!({"type": "response.create"}).to_string()
                    }
                    None => {
                        let _ = ws.close(None).await;
                        return;
                    }
                };
                if let Err(e) = ws.send(Message::text(text)).await {
                    break format!("send failed: {e}");
                }
            }
            frame = ws.next() => {
                let value = match frame {
                    Some(Ok(Message::Text(t))) => serde_json::from_str::<Value>(&t).unwrap_or(Value::Null),
                    Some(Ok(Message::Close(c))) => break format!("closed: {c:?}"),
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => break format!("socket error: {e}"),
                    None => break "socket ended".to_string(),
                };
                if let Some(kind) = value["type"].as_str()
                    && !kind.ends_with(".delta")
                {
                    log::debug!("provider=openai event={kind}");
                }
                let parsed = parse_event(&value);
                if parsed.response_started {
                    in_response = true;
                }
                if parsed.response_done {
                    in_response = false;
                    if respond_pending {
                        respond_pending = false;
                        in_response = true;
                        let create = json!({"type": "response.create"}).to_string();
                        if let Err(e) = ws.send(Message::text(create)).await {
                            break format!("send failed: {e}");
                        }
                    }
                }
                if let Some(detail) = parsed.error {
                    log::warn!("provider=openai error={}", sanitize_log(&detail));
                }
                for ev in parsed.events {
                    if ev_tx.send(ev).is_err() {
                        let _ = ws.close(None).await;
                        return;
                    }
                }
            }
        }
    };
    let _ = ev_tx.send(LiveEvent::Closed(sanitize_log(&reason)));
}

/// Non-ringing wire check: mints, connects, and asks for one spoken word. Never prints a token.
pub async fn probe() -> Result<()> {
    let home = hermes_home_default();
    let setup = Setup {
        model: DEFAULT_MODEL.into(),
        voice: DEFAULT_VOICE.into(),
        system_instruction: "You are a voice check. Say the single word ready, then stop.".into(),
        functions: vec![json!({
            "name": "ask_hermes",
            "description": "Hand a request to Hermes.",
            "parameters": {
                "type": "object",
                "properties": {"request": {"type": "string"}},
                "required": ["request"],
            },
        })],
        languages: vec![],
        overrides: json!({}),
    };
    let model = std::env::var("HWV_OPENAI_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
    let voice = openai_voice_name(&std::env::var("HWV_OPENAI_VOICE").unwrap_or_default());
    let ws = open_session(&home, &model, &session_config(&setup, &model, &voice)).await?;
    println!("provider=openai model={model} voice={voice}");
    println!("event=session.created");
    let mut live = Live::start(ws);
    live.send_text("Say the single word ready.");
    let mut audio_samples = 0usize;
    let mut transcript = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(20), live.events.recv()).await {
            Ok(Some(LiveEvent::Audio(pcm))) => audio_samples += pcm.len(),
            Ok(Some(LiveEvent::OutputTranscript(t))) => {
                transcript = t;
                println!("event=output_transcript text={transcript:?}");
            }
            Ok(Some(LiveEvent::Closed(why))) => bail!("provider=openai closed: {why}"),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
        if audio_samples > 0 && !transcript.is_empty() {
            break;
        }
    }
    println!("audio_samples={audio_samples}");
    if audio_samples == 0 {
        bail!("provider=openai produced no audio");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(languages: &[&str]) -> Setup {
        Setup {
            model: String::new(),
            voice: String::new(),
            system_instruction: "be brief".into(),
            functions: vec![json!({"name": "end_call", "description": "hang up", "behavior": "NON_BLOCKING"})],
            languages: languages.iter().map(|s| s.to_string()).collect(),
            overrides: json!({}),
        }
    }

    #[test]
    fn voice_falls_back_to_cedar() {
        assert_eq!(openai_voice_name("Marin"), "marin");
        assert_eq!(openai_voice_name("Kore"), "cedar");
        assert_eq!(openai_voice_name(""), "cedar");
    }

    #[test]
    fn session_is_ga_realtime_at_24k() {
        let s = session_config(&setup(&["pt-BR"]), "gpt-realtime-2.1", "cedar");
        assert_eq!(s["type"], "realtime");
        assert_eq!(s["audio"]["input"]["format"]["rate"], 24000);
        assert_eq!(s["audio"]["output"]["voice"], "cedar");
        assert_eq!(s["audio"]["input"]["transcription"]["language"], "pt");
        assert_eq!(s["tools"][0]["type"], "function");
        assert_eq!(s["tools"][0]["parameters"]["type"], "object");
        assert!(s["tools"][0].get("behavior").is_none());
    }

    #[test]
    fn transcription_language_needs_a_single_language() {
        assert_eq!(transcription_language(&["en".into(), "pt".into()]), None);
        assert_eq!(transcription_language(&[]), None);
    }

    #[test]
    fn reads_hermes_codex_login_before_codex_cli() {
        let dir = std::env::temp_dir().join(format!("hwv-openai-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // header.payload.sig with exp far in the future.
        let payload = URL_SAFE.encode(br#"{"exp":4102444800}"#);
        let token = format!("e30.{payload}.sig");
        std::fs::write(
            dir.join("auth.json"),
            json!({"providers": {"openai-codex": {"tokens": {"access_token": token}}}}).to_string(),
        )
        .unwrap();
        assert_eq!(read_oauth_access_token(&dir, 0).unwrap(), token);
        let expired = URL_SAFE.encode(br#"{"exp":1}"#);
        std::fs::write(
            dir.join("auth.json"),
            json!({"providers": {"openai-codex": {"tokens": {"access_token": format!("e30.{expired}.sig")}}}})
                .to_string(),
        )
        .unwrap();
        assert!(read_oauth_access_token(&dir, 100).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parses_audio_tool_calls_and_barge_in() {
        let audio = parse_event(&json!({"type": "response.output_audio.delta", "delta": B64.encode([1u8, 0, 2, 0])}));
        assert!(matches!(&audio.events[..], [LiveEvent::Audio(pcm)] if pcm == &vec![1, 2]));
        let call = parse_event(&json!({
            "type": "response.function_call_arguments.done",
            "call_id": "c1", "name": "ask_hermes", "arguments": "{\"request\":\"x\"}",
        }));
        assert!(matches!(&call.events[..], [LiveEvent::ToolCall { id, name, .. }] if id == "c1" && name == "ask_hermes"));
        let barge = parse_event(&json!({"type": "input_audio_buffer.speech_started"}));
        assert!(matches!(&barge.events[..], [LiveEvent::Interrupted]));
        assert_eq!(
            parse_client_secret(&json!({"client_secret": {"value": "ek_1"}})).as_deref(),
            Some("ek_1")
        );
    }

    use base64::engine::general_purpose::URL_SAFE_NO_PAD as URL_SAFE;
}
