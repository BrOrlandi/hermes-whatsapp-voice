//! Grok realtime voice client for WhatsApp calls.
//!
//! Wire and auth follow hermes-talk `talk_grok_realtime.py` and `talk_grok_auth.py`:
//! the host `xai-oauth` login is read and never refreshed or rewritten. A metered
//! xAI API key is never used. Call audio is 16 kHz; Grok PCM is 24 kHz.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::audio::{Upsampler, i16_to_le_bytes, le_bytes_to_i16};
use crate::config::Config;
use crate::gemini::{LiveEvent, Setup};

const WS_URL: &str = "wss://api.x.ai/v1/realtime";
const DEFAULT_MODEL: &str = "grok-voice-latest";
const DEFAULT_VOICE: &str = "ara";
const MAX_RECONNECTS: u32 = 3;
/// Reconnect before a long call outlives a single socket. Resumption replays context.
const RENEW_AFTER: Duration = Duration::from_secs(12 * 60);

const GROK_VOICES: &[&str] = &[
    "ara", "rex", "sal", "eve", "leo", "carina", "zagan", "helix", "orion", "luna", "iris",
    "altair", "zenith", "perseus", "helios", "lux", "kepler", "rigel", "cosmo", "celeste",
    "ursa", "sirius", "lumen", "castor", "naksh", "atlas", "aurora", "liora",
];

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

enum Outgoing {
    Audio(String),
    Control(String),
}

pub struct Live {
    outbox: mpsc::UnboundedSender<Outgoing>,
    pub events: mpsc::UnboundedReceiver<LiveEvent>,
    upsampler: Mutex<Upsampler>,
    task: JoinHandle<()>,
}

impl Live {
    pub async fn connect(cfg: &Config, setup: Setup) -> Result<Self> {
        let model = grok_model(cfg);
        let voice = grok_voice(cfg);
        let ws = match open_session(
            &cfg.hermes_home,
            &model,
            None,
            &session_update(&setup, &voice, true),
        )
        .await
        {
            Ok(ws) => ws,
            Err(e) => {
                log::warn!("provider=grok full session.update failed, retrying minimal shape: {e:#}");
                open_session(
                    &cfg.hermes_home,
                    &model,
                    None,
                    &session_update(&setup, &voice, false),
                )
                .await?
            }
        };
        log::info!("provider=grok voice={voice} model={model}");
        let (outbox, out_rx) = mpsc::unbounded_channel();
        let (ev_tx, events) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(
            cfg.hermes_home.clone(),
            model,
            setup,
            voice,
            ws,
            out_rx,
            ev_tx,
        ));
        Ok(Self {
            outbox,
            events,
            upsampler: Mutex::new(Upsampler::new()),
            task,
        })
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
        let _ = self
            .outbox
            .send(Outgoing::Control(json!({"type": "response.create"}).to_string()));
    }

    /// `scheduling` "SILENT" submits the tool result without asking Grok to speak.
    pub fn send_tool_response(&self, id: &str, _name: &str, response: Value, scheduling: &str) {
        let output = response.to_string();
        let item = json!({
            "type": "conversation.item.create",
            "item": {
                "type": "function_call_output",
                "call_id": id,
                "output": output,
            },
        });
        let _ = self.outbox.send(Outgoing::Control(item.to_string()));
        if scheduling != "SILENT" {
            let _ = self
                .outbox
                .send(Outgoing::Control(json!({"type": "response.create"}).to_string()));
        }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn grok_model(cfg: &Config) -> String {
    let model = cfg.grok_model.trim();
    if model.is_empty() {
        DEFAULT_MODEL.to_string()
    } else {
        model.to_string()
    }
}

pub fn grok_voice_name(configured: &str, dedicated: &str) -> String {
    fn known(name: &str) -> Option<String> {
        let name = name.trim().trim_start_matches("xai_").to_ascii_lowercase();
        GROK_VOICES
            .iter()
            .find(|v| **v == name)
            .map(|v| (*v).to_string())
    }
    known(dedicated)
        .or_else(|| known(configured))
        .unwrap_or_else(|| DEFAULT_VOICE.to_string())
}

fn grok_voice(cfg: &Config) -> String {
    grok_voice_name(&cfg.voice, &cfg.grok_voice)
}

fn wire_voice(voice: &str) -> String {
    if voice.starts_with("xai_") {
        voice.to_string()
    } else {
        format!("xai_{voice}")
    }
}

fn language_hint(languages: &[String]) -> String {
    languages
        .iter()
        .find(|l| l.to_ascii_lowercase().starts_with("en"))
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| "en".to_string())
}

fn english_instructions(instructions: &str, languages: &[String]) -> String {
    let other = languages.iter().any(|l| {
        let l = l.to_ascii_lowercase();
        !l.is_empty() && !l.starts_with("en")
    });
    if languages.is_empty() || !other {
        format!("{instructions}\n- Speak English only.")
    } else {
        instructions.to_string()
    }
}

fn tool_wire(tool: &Value) -> Value {
    let mut parameters = tool.get("parameters").cloned().unwrap_or_else(|| {
        json!({"type": "object", "properties": {}})
    });
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

/// OpenAI-GA session update that xAI accepts, plus resumption and an English hint.
pub fn session_update(setup: &Setup, voice: &str, full: bool) -> Value {
    let mut input = json!({
        "format": {"type": "audio/pcm", "rate": 24000},
        "turn_detection": {
            "type": "server_vad",
            "create_response": true,
            "interrupt_response": true,
        },
        "transcription": {"model": "grok-transcribe"},
    });
    if full {
        input["transcription"]["language_hint"] = json!(language_hint(&setup.languages));
    }
    let mut session = json!({
        "type": "realtime",
        "instructions": english_instructions(&setup.system_instruction, &setup.languages),
        "output_modalities": ["audio"],
        "audio": {
            "input": input,
            "output": {
                "format": {"type": "audio/pcm", "rate": 24000},
                "voice": wire_voice(voice),
            },
        },
    });
    if full {
        session["reasoning"] = json!({"effort": "none"});
        session["resumption"] = json!({"enabled": true});
    }
    let tools: Vec<Value> = setup.functions.iter().map(tool_wire).collect();
    if !tools.is_empty() {
        session["tools"] = json!(tools);
        session["tool_choice"] = json!("auto");
    }
    json!({"type": "session.update", "session": session})
}

#[derive(Default)]
struct Parsed {
    events: Vec<LiveEvent>,
    conversation_id: Option<String>,
    response_started: bool,
    response_done: bool,
    barge_in: bool,
    fatal: Option<String>,
    session_ready: bool,
    session_updated: bool,
    update_rejected: bool,
}

fn parse_event(msg: &Value) -> Parsed {
    let mut p = Parsed::default();
    let kind = msg["type"].as_str().unwrap_or("");
    match kind {
        "session.created" => p.session_ready = true,
        "session.updated" => p.session_updated = true,
        "conversation.created" => {
            p.conversation_id = msg["conversation"]["id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string);
        }
        "input_audio_buffer.speech_started" => p.barge_in = true,
        "response.created" => p.response_started = true,
        "response.done" => p.response_done = true,
        "response.output_audio.delta" | "response.audio.delta" => {
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
            if let Some(t) = msg["transcript"].as_str().filter(|t| !t.is_empty()) {
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
            let detail = msg["error"]["message"]
                .as_str()
                .or_else(|| msg["error"]["type"].as_str())
                .unwrap_or("provider error");
            let lowered = detail.to_ascii_lowercase();
            if lowered.contains("session.update")
                || lowered.contains("unknown")
                || lowered.contains("unsupported")
                || lowered.contains("invalid")
            {
                p.update_rejected = true;
            }
            p.fatal = Some(detail.to_string());
        }
        _ => {}
    }
    // Binary frames are handled by the socket loop, not here.
    p
}

fn decode_pcm(data: &str) -> Option<Vec<i16>> {
    if data.is_empty() {
        return None;
    }
    let bytes = B64.decode(data).ok()?;
    Some(le_bytes_to_i16(&bytes))
}

fn hermes_home_default() -> PathBuf {
    std::env::var_os("HERMES_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".hermes")))
        .unwrap_or_else(|| PathBuf::from("/opt/data"))
}

fn b64url(data: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD
        .decode(data.trim_end_matches('='))
        .ok()
}

fn jwt_expired(token: &str, now: i64) -> bool {
    let mut parts = token.split('.');
    let Some(_header) = parts.next() else {
        return true;
    };
    let Some(payload) = parts.next() else {
        return true;
    };
    let Some(bytes) = b64url(payload) else {
        return true;
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return true;
    };
    let Some(exp) = value.get("exp").and_then(|v| v.as_i64()) else {
        return true;
    };
    exp <= now + 60
}

fn token_pair(tokens: &Value) -> Option<String> {
    let access = tokens.get("access_token")?.as_str()?.trim();
    let refresh = tokens.get("refresh_token")?.as_str()?.trim();
    if access.is_empty() || refresh.is_empty() {
        return None;
    }
    Some(access.to_string())
}

/// Read-only. Never refreshes and never writes auth.json. Never returns an API key.
pub fn read_oauth_access_token(home: &Path, now: i64) -> Result<String> {
    let path = home.join("auth.json");
    let raw = std::fs::read_to_string(&path).context("xai-oauth store unreadable")?;
    let data: Value = serde_json::from_str(&raw).context("xai-oauth store is not JSON")?;
    let token = token_pair(&data["providers"]["xai-oauth"]["tokens"])
        .or_else(|| {
            data["credential_pool"]["xai-oauth"]
                .as_array()
                .and_then(|rows| {
                    rows.iter().find_map(|row| {
                        token_pair(row).or_else(|| token_pair(&row["tokens"]))
                    })
                })
        })
        .context("no usable xai-oauth login")?;
    if jwt_expired(&token, now) {
        bail!("xai-oauth access token is expired; not refreshing and not using an API key");
    }
    Ok(token)
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn redact(msg: impl AsRef<str>, secret: &str) -> String {
    let msg = msg.as_ref();
    if secret.len() < 8 {
        return msg.to_string();
    }
    msg.replace(secret, "[redacted]")
}

/// Log text only. Never a token. Caps length so a provider payload cannot flood the log.
pub fn sanitize_log(detail: &str) -> String {
    let mut out: String = detail
        .chars()
        .filter(|c| !c.is_control())
        .take(240)
        .collect();
    if out.contains("eyJ") || out.to_ascii_lowercase().contains("bearer ") {
        out = "[redacted]".into();
    }
    if out.is_empty() {
        "provider error".into()
    } else {
        out
    }
}

/// xAI returns this when we cancel a response that already ended. It is not a dead session.
pub fn provider_error_is_benign(detail: &str) -> bool {
    let lowered = detail.to_ascii_lowercase();
    lowered.contains("no active response") || lowered.contains("already has an active response")
}

fn endpoint(model: &str, conversation_id: Option<&str>) -> String {
    let mut url = format!("{WS_URL}?model={}", query_escape(model));
    if let Some(id) = conversation_id.filter(|s| !s.is_empty()) {
        url.push_str("&conversation_id=");
        url.push_str(&query_escape(id));
    }
    url
}

fn query_escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

async fn open_session(
    home: &Path,
    model: &str,
    conversation_id: Option<&str>,
    update: &Value,
) -> Result<Ws> {
    let token = read_oauth_access_token(home, now_secs())?;
    let result = connect_ws(&token, model, conversation_id, update).await;
    result.map_err(|e| anyhow!(redact(format!("{e:#}"), &token)))
}

async fn connect_ws(
    token: &str,
    model: &str,
    conversation_id: Option<&str>,
    update: &Value,
) -> Result<Ws> {
    let mut req = endpoint(model, conversation_id).into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {token}").parse()?);
    let (mut ws, _) = tokio::time::timeout(
        Duration::from_secs(20),
        tokio_tungstenite::connect_async(req),
    )
    .await
    .context("timed out connecting to Grok realtime")?
    .context("connecting to Grok realtime")?;

    let ready = wait_for(&mut ws, "session.created").await;
    ready.context("Grok realtime closed before session.created")?;
    ws.send(Message::text(update.to_string())).await?;
    match wait_for(&mut ws, "session.updated").await {
        Ok(()) => Ok(ws),
        Err(e) => Err(e),
    }
}

async fn wait_for(ws: &mut Ws, want: &str) -> Result<()> {
    let ready = async {
        while let Some(frame) = ws.next().await {
            match frame? {
                Message::Text(t) => {
                    let value: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                    let parsed = parse_event(&value);
                    if parsed.fatal.is_some() && want == "session.updated" {
                        bail!(
                            "Grok realtime rejected session.update: {}",
                            parsed.fatal.unwrap_or_default()
                        );
                    }
                    if (want == "session.created" && parsed.session_ready)
                        || (want == "session.updated" && parsed.session_updated)
                    {
                        return Ok(());
                    }
                }
                Message::Close(c) => bail!("Grok realtime closed during setup: {c:?}"),
                _ => {}
            }
        }
        Err(anyhow!("Grok realtime closed during setup"))
    };
    tokio::time::timeout(Duration::from_secs(20), ready)
        .await
        .context("timed out waiting for Grok realtime")?
}

enum Turnover {
    Reconnect(String),
    Stop,
}

async fn pump(
    ws: &mut Ws,
    out_rx: &mut mpsc::UnboundedReceiver<Outgoing>,
    backlog: &mut VecDeque<String>,
    ev_tx: &mpsc::UnboundedSender<LiveEvent>,
    conversation_id: &mut Option<String>,
    renew_at: Instant,
) -> Turnover {
    while let Some(text) = backlog.pop_front() {
        if let Err(e) = ws.send(Message::text(text.clone())).await {
            backlog.push_front(text);
            return Turnover::Reconnect(format!("send failed: {e}"));
        }
    }
    let mut in_response = false;
    let mut last_input = String::new();
    let mut last_output = String::new();
    loop {
        tokio::select! {
            _ = sleep_until(renew_at) => {
                if in_response {
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    continue;
                }
                return Turnover::Reconnect("session renewal".into());
            }
            out = out_rx.recv() => match out {
                Some(Outgoing::Audio(text)) => {
                    if let Err(e) = ws.send(Message::text(text)).await {
                        return Turnover::Reconnect(format!("send failed: {e}"));
                    }
                }
                Some(Outgoing::Control(text)) => {
                    if let Err(e) = ws.send(Message::text(text.clone())).await {
                        backlog.push_back(text);
                        return Turnover::Reconnect(format!("send failed: {e}"));
                    }
                }
                None => {
                    let _ = ws.close(None).await;
                    return Turnover::Stop;
                }
            },
            frame = ws.next() => {
                let value = match frame {
                    Some(Ok(Message::Text(t))) => serde_json::from_str::<Value>(&t).ok(),
                    Some(Ok(Message::Binary(b))) => {
                        let _ = ev_tx.send(LiveEvent::Audio(le_bytes_to_i16(&b)));
                        None
                    }
                    Some(Ok(Message::Close(c))) => {
                        return Turnover::Reconnect(format!("closed: {c:?}"));
                    }
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => return Turnover::Reconnect(format!("socket error: {e}")),
                    None => return Turnover::Reconnect("socket ended".into()),
                };
                let Some(value) = value else { continue };
                let parsed = parse_event(&value);
                if parsed.conversation_id.is_some() {
                    *conversation_id = parsed.conversation_id;
                }
                if parsed.response_started {
                    in_response = true;
                }
                if parsed.response_done {
                    in_response = false;
                }
                if parsed.barge_in {
                    // The server already has interrupt_response. Cancelling when nothing
                    // is speaking is the xAI error that was tearing the call down.
                    if in_response {
                        let _ = ws
                            .send(Message::text(json!({"type": "response.cancel"}).to_string()))
                            .await;
                        in_response = false;
                    }
                    if ev_tx.send(LiveEvent::Interrupted).is_err() {
                        return Turnover::Stop;
                    }
                }
                for ev in parsed.events {
                    let skip = match &ev {
                        LiveEvent::InputTranscript(t) if t == &last_input => true,
                        LiveEvent::OutputTranscript(t) if t == &last_output => true,
                        LiveEvent::InputTranscript(t) => {
                            last_input = t.clone();
                            false
                        }
                        LiveEvent::OutputTranscript(t) => {
                            last_output = t.clone();
                            false
                        }
                        _ => false,
                    };
                    if skip {
                        continue;
                    }
                    if ev_tx.send(ev).is_err() {
                        return Turnover::Stop;
                    }
                }
                if let Some(detail) = parsed.fatal {
                    let safe = sanitize_log(&detail);
                    if provider_error_is_benign(&detail) {
                        log::warn!("provider=grok xai_error={safe} action=ignore");
                        continue;
                    }
                    log::warn!("provider=grok xai_error={safe} action=reconnect");
                    return Turnover::Reconnect(format!("provider error: {safe}"));
                }
            }
        }
    }
}

async fn run(
    home: PathBuf,
    model: String,
    setup: Setup,
    voice: String,
    mut ws: Ws,
    mut out_rx: mpsc::UnboundedReceiver<Outgoing>,
    ev_tx: mpsc::UnboundedSender<LiveEvent>,
) {
    let mut conversation_id = None;
    let mut backlog = VecDeque::new();
    let mut failures = 0u32;
    let mut provider_errors = 0u32;
    let mut full_shape = true;
    let started = Instant::now();
    loop {
        let renew_at = started + RENEW_AFTER;
        let why = match pump(
            &mut ws,
            &mut out_rx,
            &mut backlog,
            &ev_tx,
            &mut conversation_id,
            renew_at,
        )
        .await
        {
            Turnover::Stop => return,
            Turnover::Reconnect(why) => why,
        };
        let safe = sanitize_log(&why);
        log::info!(
            "provider=grok session renew reason={safe} resumed={}",
            conversation_id.is_some()
        );
        if why.starts_with("provider error") {
            provider_errors += 1;
        }
        // A successful reconnect used to clear the counter, so the cancel-error
        // loop never fell back. Real provider errors accumulate for the call.
        if provider_errors >= MAX_RECONNECTS || failures + 1 >= MAX_RECONNECTS {
            log::error!("provider=grok falling back to gemini: {safe}");
            let _ = ev_tx.send(LiveEvent::Fallback(safe));
            return;
        }
        failures += 1;
        tokio::time::sleep(Duration::from_millis(400 * u64::from(failures))).await;
        let update = session_update(&setup, &voice, full_shape);
        match open_session(&home, &model, conversation_id.as_deref(), &update).await {
            Ok(next) => {
                ws = next;
                failures = 0;
                log::info!("provider=grok session renewed");
            }
            Err(e) if full_shape => {
                full_shape = false;
                log::warn!("provider=grok full session.update rejected, retrying minimal shape");
                match open_session(
                    &home,
                    &model,
                    conversation_id.as_deref(),
                    &session_update(&setup, &voice, false),
                )
                .await
                {
                    Ok(next) => {
                        ws = next;
                        failures = 0;
                        log::info!("provider=grok session renewed shape=minimal");
                    }
                    Err(e2) => {
                        log::warn!("provider=grok renew attempt {failures} failed");
                        let _ = e;
                        let _ = e2;
                    }
                }
            }
            Err(_) => {
                log::warn!("provider=grok renew attempt {failures} failed");
            }
        }
        while let Ok(out) = out_rx.try_recv() {
            if let Outgoing::Control(text) = out {
                backlog.push_back(text);
            }
        }
        if conversation_id.is_none() {
            backlog.push_back(
                json!({
                    "type": "conversation.item.create",
                    "item": {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": "(The call is still connected. Continue from where you left off.)"}],
                    },
                })
                .to_string(),
            );
        }
    }
}

/// Non-ringing wire check. Prints provider=grok and event names. Never prints a token.
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
    let cfg = Config {
        owner: String::new(),
        listen: String::new(),
        token: String::new(),
        gemini_api_key: String::new(),
        model: String::new(),
        voice: "Kore".into(),
        assistant_name: String::new(),
        voice_instructions: String::new(),
        languages: vec![],
        live_config: json!({}),
        hermes_api_url: String::new(),
        hermes_api_key: String::new(),
        ring_timeout: Duration::from_secs(1),
        max_duration: Duration::from_secs(1),
        silence_timeout: Duration::from_secs(1),
        answer_inbound: false,
        send_summary: false,
        voice_provider: "grok".into(),
        grok_model: DEFAULT_MODEL.into(),
        grok_voice: DEFAULT_VOICE.into(),
        hermes_home: home,
    };
    let mut live = Live::connect(&cfg, setup).await?;
    live.send_text("Say the single word ready.");
    let deadline = Instant::now() + Duration::from_secs(25);
    let mut audio_samples = 0usize;
    let mut transcript = String::new();
    let mut tool = String::new();
    println!("provider=grok");
    println!("event=session.created");
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(20), live.events.recv()).await {
            Ok(Some(LiveEvent::Audio(pcm))) => audio_samples += pcm.len(),
            Ok(Some(LiveEvent::OutputTranscript(t))) => {
                transcript = t;
                println!("event=output_transcript chars={}", transcript.chars().count());
            }
            Ok(Some(LiveEvent::ToolCall { name, .. })) => {
                tool = name;
                println!("event=tool_call name={tool}");
            }
            Ok(Some(LiveEvent::Interrupted)) => println!("event=interrupted"),
            Ok(Some(LiveEvent::Closed(why))) => bail!("provider=grok closed: {why}"),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
        if audio_samples > 0 && !transcript.is_empty() {
            break;
        }
    }
    println!("audio_samples={audio_samples}");
    if transcript.is_empty() && audio_samples == 0 {
        bail!("provider=grok produced no audio");
    }
    println!("provider=grok probe=ok");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Setup {
        Setup {
            model: "gemini-3.8-live".into(),
            voice: "Kore".into(),
            system_instruction: "Be brief.".into(),
            functions: vec![json!({
                "name": "end_call",
                "description": "Hang up.",
                "behavior": "NON_BLOCKING",
            })],
            languages: vec![],
            overrides: json!({}),
        }
    }

    #[test]
    fn session_update_maps_tools_and_english() {
        let msg = session_update(&setup(), "ara", true);
        assert_eq!(msg["type"], "session.update");
        assert_eq!(msg["session"]["audio"]["output"]["voice"], "xai_ara");
        assert_eq!(msg["session"]["audio"]["input"]["format"]["rate"], 24000);
        assert_eq!(msg["session"]["resumption"]["enabled"], true);
        assert_eq!(msg["session"]["tools"][0]["name"], "end_call");
        assert!(msg["session"]["tools"][0].get("behavior").is_none());
        assert!(
            msg["session"]["instructions"]
                .as_str()
                .unwrap()
                .contains("Speak English only.")
        );
    }

    #[test]
    fn parses_audio_interrupt_and_tool() {
        let pcm = B64.encode(i16_to_le_bytes(&[1, -2]));
        let audio = parse_event(&json!({"type": "response.output_audio.delta", "delta": pcm}));
        assert_eq!(audio.events, vec![LiveEvent::Audio(vec![1, -2])]);
        let barge = parse_event(&json!({"type": "input_audio_buffer.speech_started"}));
        assert!(barge.barge_in);
        let tool = parse_event(&json!({
            "type": "response.function_call_arguments.done",
            "call_id": "c1",
            "name": "ask_hermes",
            "arguments": "{\"request\":\"ping\"}",
        }));
        assert_eq!(
            tool.events,
            vec![LiveEvent::ToolCall {
                id: "c1".into(),
                name: "ask_hermes".into(),
                args: json!({"request": "ping"}),
            }]
        );
    }

    #[test]
    fn cancel_with_no_active_response_is_not_fatal() {
        let detail = "Cancellation failed: no active response found";
        assert!(provider_error_is_benign(detail));
        assert!(!provider_error_is_benign("invalid audio format"));
        let parsed = parse_event(&json!({
            "type": "error",
            "error": {
                "message": detail,
                "type": "invalid_request_error",
                "code": "invalid_request_error",
            },
        }));
        assert_eq!(parsed.fatal.as_deref(), Some(detail));
        assert_eq!(sanitize_log(detail), detail);
    }

    #[test]
    fn kore_is_not_a_grok_voice() {
        assert_eq!(grok_voice_name("Kore", "ara"), "ara");
        assert_eq!(grok_voice_name("xai_eve", ""), "eve");
    }

    #[test]
    fn oauth_reader_rejects_expired_and_missing_refresh() {
        let header = B64URL_header();
        let future = now_secs() + 3600;
        let past = now_secs() - 120;
        let good = format!("{header}.{}", b64url_json(&json!({"exp": future})));
        let bad = format!("{header}.{}", b64url_json(&json!({"exp": past})));
        let dir = std::env::temp_dir().join(format!("hwv-grok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        std::fs::write(
            &path,
            json!({
                "providers": {"xai-oauth": {"tokens": {
                    "access_token": good,
                    "refresh_token": "refresh-present",
                }}},
            })
            .to_string(),
        )
        .unwrap();
        assert!(read_oauth_access_token(&dir, now_secs()).is_ok());
        std::fs::write(
            &path,
            json!({
                "providers": {"xai-oauth": {"tokens": {
                    "access_token": bad,
                    "refresh_token": "refresh-present",
                }}},
            })
            .to_string(),
        )
        .unwrap();
        let err = read_oauth_access_token(&dir, now_secs()).unwrap_err();
        assert!(err.to_string().contains("expired"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn B64URL_header() -> String {
        URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#)
    }

    fn b64url_json(value: &Value) -> String {
        URL_SAFE_NO_PAD.encode(value.to_string().as_bytes())
    }
}
