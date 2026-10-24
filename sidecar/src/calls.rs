//! Call records and the conversation bridge: WhatsApp call audio <-> Gemini Live, with Hermes as
//! the voice agent's tool.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use whatsapp_rust::async_channel;
use whatsapp_rust::prelude::*;
use whatsapp_rust::voip::{CallHandle, CallTermination};
use whatsapp_rust::wacore::types::call::IncomingCall;

use crate::audio::{Downsampler, Playout};
use crate::config::Config;
use crate::gemini::{Live as GeminiLive, LiveEvent, Setup};
use crate::grok::Live as GrokLive;
use crate::hermes::Hermes;

const KEEP_RECORDS: usize = 50;
const FRAME_INTERVAL: Duration = Duration::from_millis(60);
const HERMES_TIMEOUT: Duration = Duration::from_secs(180);
const STILL_CONNECTED: &str = "(The call is still connected: the owner spoke after your goodbye. \
    Answer them, and call end_call again once they are done.)";

enum Voice {
    Gemini(GeminiLive),
    Grok(GrokLive),
}

impl Voice {
    fn send_audio(&self, pcm: &[i16]) {
        match self {
            Voice::Gemini(live) => live.send_audio(pcm),
            Voice::Grok(live) => live.send_audio(pcm),
        }
    }

    fn send_text(&self, text: &str) {
        match self {
            Voice::Gemini(live) => live.send_text(text),
            Voice::Grok(live) => live.send_text(text),
        }
    }

    fn send_tool_response(&self, id: &str, name: &str, response: Value, scheduling: &str) {
        match self {
            Voice::Gemini(live) => live.send_tool_response(id, name, response, scheduling),
            Voice::Grok(live) => live.send_tool_response(id, name, response, scheduling),
        }
    }

    fn events_mut(&mut self) -> &mut mpsc::UnboundedReceiver<LiveEvent> {
        match self {
            Voice::Gemini(live) => &mut live.events,
            Voice::Grok(live) => &mut live.events,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Dialing,
    Ringing,
    InProgress,
    Completed,
    NoAnswer,
    Rejected,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
pub struct Turn {
    pub role: &'static str,
    pub text: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct CallRecord {
    pub id: String,
    pub direction: &'static str,
    pub status: Status,
    pub reason: String,
    pub started_at: u64,
    pub answered_at: Option<u64>,
    pub ended_at: Option<u64>,
    pub end_reason: Option<String>,
    pub transcript: Vec<Turn>,
    pub summary: Option<String>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn new_id() -> String {
    format!("{:012x}", rand::random::<u64>() & 0xffff_ffff_ffff)
}

impl CallRecord {
    fn new(direction: &'static str, reason: &str) -> Self {
        Self {
            id: new_id(),
            direction,
            status: Status::Dialing,
            reason: reason.to_string(),
            started_at: now(),
            answered_at: None,
            ended_at: None,
            end_reason: None,
            transcript: Vec::new(),
            summary: None,
        }
    }

    /// Transcription arrives in fragments; consecutive fragments from one speaker form a turn.
    fn append(&mut self, role: &'static str, text: &str) {
        match self.transcript.last_mut() {
            Some(last) if last.role == role => last.text.push_str(text),
            _ => self.transcript.push(Turn {
                role,
                text: text.to_string(),
            }),
        }
    }

    /// Whether the assistant spoke last and ended on a question.
    fn ends_with_question(&self) -> bool {
        self.transcript
            .last()
            .is_some_and(|t| t.role == "assistant" && t.text.trim_end().ends_with('?'))
    }

    pub fn transcript_text(&self) -> String {
        self.transcript
            .iter()
            .map(|t| format!("{}: {}", t.role, t.text.trim()))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Recent calls, newest last, and the single active call slot.
#[derive(Default)]
pub struct Calls {
    records: Mutex<VecDeque<CallRecord>>,
    active: Mutex<Option<String>>,
}

impl Calls {
    /// Claims the active slot for a new call; fails when one is already running.
    fn begin(&self, direction: &'static str, reason: &str) -> Result<CallRecord> {
        let mut active = self.active.lock().unwrap();
        if let Some(id) = active.as_ref() {
            bail!("a call is already in progress ({id})");
        }
        let record = CallRecord::new(direction, reason);
        *active = Some(record.id.clone());
        let mut records = self.records.lock().unwrap();
        records.push_back(record.clone());
        if records.len() > KEEP_RECORDS {
            records.pop_front();
        }
        Ok(record)
    }

    fn finish(&self, id: &str, status: Status, end_reason: impl Into<String>) {
        self.update(id, |r| {
            r.status = status;
            r.ended_at = Some(now());
            r.end_reason = Some(end_reason.into());
        });
        let mut active = self.active.lock().unwrap();
        if active.as_deref() == Some(id) {
            *active = None;
        }
    }

    fn update(&self, id: &str, f: impl FnOnce(&mut CallRecord)) {
        if let Some(r) = self.records.lock().unwrap().iter_mut().find(|r| r.id == id) {
            f(r);
        }
    }

    pub fn get(&self, id: &str) -> Option<CallRecord> {
        self.records
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.id == id)
            .cloned()
    }

    pub fn recent(&self, limit: usize) -> Vec<CallRecord> {
        self.records
            .lock()
            .unwrap()
            .iter()
            .rev()
            .take(limit)
            .cloned()
            .collect()
    }

    pub fn active(&self) -> Option<String> {
        self.active.lock().unwrap().clone()
    }
}

/// What the callee's devices have said about an outbound call. whatsapp-rust leaves an outgoing
/// 1:1 call's phase at `Calling`, so answering is learned from the peer's call signaling.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Signal {
    Pending,
    Ringing,
    Accepted,
    Declined,
}

/// Peer signals for the outbound call that is ringing, keyed by WhatsApp call id. Collected only
/// while a call rings. A signal can arrive before `start()` returns the id, so whichever side
/// comes first creates the entry.
#[derive(Default)]
pub struct RingSignals(Mutex<Option<HashMap<String, watch::Sender<Signal>>>>);

impl RingSignals {
    fn start(&self) {
        *self.0.lock().unwrap() = Some(HashMap::new());
    }

    fn stop(&self) {
        *self.0.lock().unwrap() = None;
    }

    /// Called for the peer's preaccept/accept/reject stanzas.
    pub fn send(&self, call_id: &str, signal: Signal) {
        if let Some(map) = self.0.lock().unwrap().as_mut() {
            map.entry(call_id.to_string())
                .or_insert_with(|| watch::channel(Signal::Pending).0)
                .send_replace(signal);
        }
    }

    fn subscribe(&self, call_id: &str) -> Option<watch::Receiver<Signal>> {
        let mut guard = self.0.lock().unwrap();
        let map = guard.as_mut()?;
        Some(
            map.entry(call_id.to_string())
                .or_insert_with(|| watch::channel(Signal::Pending).0)
                .subscribe(),
        )
    }
}

/// Decides when a call is over, without reading transcripts (they lag, and come out in the wrong
/// language). The model ends the call itself, after the owner's goodbye; the sidecar hangs up once
/// that goodbye has finished playing and a short quiet window passes. Any owner speech in the window
/// calls the hangup off. A long silence ends a call nobody closed (or left on hold too long).
struct Ending {
    silence: Duration,
    requested: bool,
    /// When playback went quiet after end_call.
    quiet_since: Option<Instant>,
    last_activity: Option<Instant>,
}

impl Ending {
    const GRACE: Duration = Duration::from_secs(3);

    fn new(silence: Duration) -> Self {
        Self {
            silence,
            requested: false,
            quiet_since: None,
            last_activity: None,
        }
    }

    fn request(&mut self) {
        self.requested = true;
        self.quiet_since = None;
    }

    /// Returns true when this called off a pending hangup.
    fn owner_spoke(&mut self, now: Instant) -> bool {
        self.last_activity = Some(now);
        self.quiet_since = None;
        std::mem::take(&mut self.requested)
    }

    /// Why the call should end now, if it should. `playing`: model speech is still being played;
    /// `working`: a request to Hermes is pending, so silence is expected.
    fn check(&mut self, now: Instant, playing: bool, working: bool) -> Option<&'static str> {
        if playing {
            self.last_activity = Some(now);
            self.quiet_since = None;
            return None;
        }
        if self.requested && now.duration_since(*self.quiet_since.get_or_insert(now)) >= Self::GRACE
        {
            return Some("assistant ended the call");
        }
        let last = *self.last_activity.get_or_insert(now);
        (!working && now.duration_since(last) >= self.silence)
            .then_some("hung up after a long silence")
    }
}

pub struct Bridge {
    pub cfg: Config,
    pub client: Arc<Client>,
    pub calls: Calls,
    pub ring_signals: RingSignals,
    hermes: Option<Hermes>,
    /// Hermes's own briefing on who it is and who the owner is; empty until fetched.
    persona: Mutex<String>,
}

impl Bridge {
    pub fn new(cfg: Config, client: Arc<Client>) -> Self {
        let hermes = (!cfg.hermes_api_key.is_empty())
            .then(|| Hermes::new(&cfg.hermes_api_url, &cfg.hermes_api_key));
        Self {
            cfg,
            client,
            calls: Calls::default(),
            hermes,
            ring_signals: RingSignals::default(),
            persona: Mutex::default(),
        }
    }

    /// Rings until the owner answers (true) or the call is declined, ends, or times out (false,
    /// with the call record finished).
    async fn ring(&self, id: &str, handle: &CallHandle) -> bool {
        let Some(mut rx) = self.ring_signals.subscribe(handle.call_id()) else {
            return false;
        };
        let ended = handle.wait_ended();
        tokio::pin!(ended);
        let outcome = tokio::time::timeout(self.cfg.ring_timeout, async {
            loop {
                match *rx.borrow_and_update() {
                    Signal::Accepted => return Some(true),
                    Signal::Declined => return Some(false),
                    Signal::Ringing => self.calls.update(id, |r| r.status = Status::Ringing),
                    Signal::Pending => {}
                }
                tokio::select! {
                    _ = &mut ended => return None,
                    changed = rx.changed() => if changed.is_err() { return None },
                }
            }
        })
        .await;
        match outcome {
            Ok(Some(true)) => true,
            Ok(Some(false)) => {
                terminate(handle).await;
                self.calls.finish(id, Status::Rejected, "declined");
                false
            }
            Ok(None) => {
                self.calls
                    .finish(id, Status::Rejected, "ended before answer");
                false
            }
            Err(_) => {
                terminate(handle).await;
                self.calls.finish(id, Status::NoAnswer, "no answer");
                false
            }
        }
    }

    fn owner_jid(&self) -> Jid {
        Jid::pn(self.cfg.owner.as_str())
    }

    /// Whether an incoming offer comes from the owner's number.
    pub async fn is_owner(
        &self,
        call: &IncomingCall,
        creator: &Jid,
        caller_pn: Option<&Jid>,
    ) -> bool {
        for jid in caller_pn.into_iter().chain([creator, &call.from]) {
            if jid.is_pn() {
                return jid.user == self.cfg.owner;
            }
        }
        for jid in [creator, &call.from] {
            if jid.is_lid()
                && let Ok(Some(entry)) = self.client.get_lid_pn_entry(jid).await
            {
                return *entry.phone_number == *self.cfg.owner;
            }
        }
        log::warn!(
            "call from {creator}: no phone number known for this LID, so it cannot be matched to the owner"
        );
        false
    }

    /// Validates and registers an outbound call; the caller spawns `run_outbound`.
    pub fn begin_outbound(&self, reason: &str) -> Result<CallRecord> {
        self.calls.begin("outbound", reason)
    }

    pub async fn run_outbound(self: Arc<Self>, id: String, reason: String, details: String) {
        let (mic_tx, mic_rx) = async_channel::bounded::<Vec<i16>>(8);
        let (spk_tx, spk_rx) = async_channel::bounded::<Vec<i16>>(64);
        self.ring_signals.start();
        let started = self
            .client
            .voip()
            .call(&self.owner_jid())
            .audio(mic_rx, spk_tx)
            .start()
            .await;
        let answered = match &started {
            Ok(handle) => {
                log::info!(
                    "call {id}: dialing owner (whatsapp call {})",
                    handle.call_id()
                );
                self.ring(&id, handle).await
            }
            Err(e) => {
                self.calls
                    .finish(&id, Status::Failed, format!("could not place call: {e}"));
                false
            }
        };
        self.ring_signals.stop();
        let (Ok(handle), true) = (started, answered) else {
            return;
        };

        let opening = format!(
            "(The owner just answered the call you placed. Greet them and get to the point: {reason})"
        );
        let mut context = format!("You placed this call. Reason: {reason}");
        if !details.is_empty() {
            context.push_str(&format!("\nDetails: {details}"));
        }
        self.converse(&id, handle, mic_tx, spk_rx, &opening, &context)
            .await;
    }

    pub async fn run_inbound(self: Arc<Self>, call: IncomingCall) {
        let record = match self.calls.begin("inbound", "owner called") {
            Ok(r) => r,
            Err(e) => return log::info!("ignoring owner call: {e}"),
        };
        let (mic_tx, mic_rx) = async_channel::bounded::<Vec<i16>>(8);
        let (spk_tx, spk_rx) = async_channel::bounded::<Vec<i16>>(64);
        let handle = match self
            .client
            .voip()
            .accept(&call)
            .audio(mic_rx, spk_tx)
            .start()
            .await
        {
            Ok(h) => h,
            Err(e) => {
                return self.calls.finish(
                    &record.id,
                    Status::Failed,
                    format!("could not answer: {e}"),
                );
            }
        };
        log::info!(
            "call {}: answered owner (whatsapp call {})",
            record.id,
            handle.call_id()
        );
        let opening = "(The owner is calling you and is now connected. Greet them briefly and ask what they need.)";
        self.converse(
            &record.id,
            handle,
            mic_tx,
            spk_rx,
            opening,
            "The owner called you.",
        )
        .await;
    }

    /// Asks Hermes for a short briefing so the voice model shares its name, persona and memory.
    /// Fetched ahead of time so answering a call never waits on it.
    pub async fn refresh_persona(&self) {
        let Some(h) = &self.hermes else { return };
        let prompt = "A voice version of you is about to talk with the owner on a WhatsApp call, and it \
            knows only what you write here. In under 150 words of plain sentences, tell it: your name and \
            personality as you present yourself, how the owner likes to be spoken to (including \
            language), and the few facts about the owner and their current priorities it should know. \
            Write it as instructions to that voice (\"You are ...\").";
        match h
            .run(
                prompt,
                "Reply in plain text only.",
                "whatsapp-voice-briefing",
                HERMES_TIMEOUT,
            )
            .await
        {
            Ok(text) if !text.trim().is_empty() => {
                log::info!(
                    "persona briefing from Hermes loaded ({} chars)",
                    text.trim().len()
                );
                *self.persona.lock().unwrap() = text.trim().to_string()
            }
            Ok(_) => {}
            Err(e) => log::warn!("could not fetch persona briefing from Hermes: {e:#}"),
        }
    }

    fn setup(&self, context: &str) -> Setup {
        let persona = self.persona.lock().unwrap().clone();
        let tools_note = if self.hermes.is_some() {
            // Never name the voice "Hermes" here: the persona briefing supplies the name, and this
            // line used to override it ("this is a test call from Hermes").
            "The ask_hermes tool reaches the owner's full agent system, which has their memory, files, \
             calendar, reminders, messages and tools. For any action, or any fact you do not already \
             know, call ask_hermes with a clear, self-contained request. Say a few words first, like \"one moment, \
             let me check\"; the answer arrives a little later and you will be told when it does. Never \
             make up results."
        } else {
            "You cannot take actions or look things up during this call; say so if asked."
        };
        let mut owner_instructions = self.cfg.voice_instructions.clone();
        if let Some(first) = self.cfg.languages.first() {
            owner_instructions.push_str(&format!(
                "\n- Speak only these languages: {}. If the owner uses any other language, reply in {first}.",
                self.cfg.languages.join(", ")
            ));
        }
        let system_instruction = system_instruction(
            &self.cfg.assistant_name,
            &persona,
            &owner_instructions,
            tools_note,
            context,
        );
        let mut functions = vec![json!({
            "name": "end_call",
            "description": "Hang up the call. Call it only after you have said goodbye.",
            "behavior": "NON_BLOCKING",
        })];
        if self.hermes.is_some() {
            functions.push(json!({
                "name": "ask_hermes",
                "description": "Hand a request to Hermes, the owner's full agent with memory and tools \
                    (calendar, reminders, messages, notes, web, home). Use it for any action or any \
                    information you do not have.",
                "parameters": {
                    "type": "object",
                    "properties": {"request": {
                        "type": "string",
                        "description": "A complete, self-contained instruction or question for Hermes.",
                    }},
                    "required": ["request"],
                },
                "behavior": "NON_BLOCKING",
            }));
        }
        Setup {
            model: self.cfg.model.clone(),
            voice: self.cfg.voice.clone(),
            system_instruction,
            functions,
            languages: self.cfg.languages.clone(),
            overrides: self.cfg.live_config.clone(),
        }
    }

    async fn connect_voice(&self, context: &str) -> Result<(Voice, &'static str)> {
        let setup = self.setup(context);
        if self.cfg.voice_provider == "grok" {
            match GrokLive::connect(&self.cfg, setup.clone()).await {
                Ok(live) => return Ok((Voice::Grok(live), "grok")),
                Err(e) => {
                    log::error!("provider=grok connect failed, falling back to gemini: {e:#}");
                }
            }
        }
        let live = GeminiLive::connect(&self.cfg.gemini_api_key, setup).await?;
        Ok((Voice::Gemini(live), "gemini"))
    }

    fn spawn_ask_hermes(
        &self,
        id: &str,
        call_id: &str,
        args: &Value,
        done: mpsc::UnboundedSender<(String, Value)>,
    ) -> JoinHandle<()> {
        let hermes = self.hermes.clone();
        let request = args["request"].as_str().unwrap_or_default().to_string();
        let (tool_id, session) = (id.to_string(), format!("whatsapp-voice-{call_id}"));
        tokio::spawn(async move {
            let result = match hermes {
                None => json!({"error": "Hermes is not connected"}),
                Some(h) => {
                    let instructions = "This request comes from the owner, live on a WhatsApp voice call \
                        with your voice agent. Do what is asked. Reply in one to three short, plain \
                        sentences that can be read aloud: no markdown, lists, links or emoji.";
                    match h
                        .run(&request, instructions, &session, HERMES_TIMEOUT)
                        .await
                    {
                        Ok(answer) => json!({"result": answer}),
                        Err(e) => json!({"error": format!("{e:#}")}),
                    }
                }
            };
            let _ = done.send((tool_id, result));
        })
    }

    async fn converse(
        &self,
        id: &str,
        handle: CallHandle,
        mic_tx: async_channel::Sender<Vec<i16>>,
        spk_rx: async_channel::Receiver<Vec<i16>>,
        opening: &str,
        context: &str,
    ) {
        self.calls.update(id, |r| {
            r.status = Status::InProgress;
            r.answered_at = Some(now());
        });
        let (mut live, provider) = match self.connect_voice(context).await {
            Ok(connected) => connected,
            Err(e) => {
                log::error!("call {id}: voice model unavailable: {e:#}");
                terminate(&handle).await;
                return self.calls.finish(
                    id,
                    Status::Failed,
                    format!("voice model unavailable: {e:#}"),
                );
            }
        };
        log::info!("call {id}: provider={provider}");
        live.send_text(opening);

        // Model speech goes out on a fixed 60 ms clock; the library does not pace for us.
        let playout = Arc::new(Playout::default());
        let pacer = tokio::spawn({
            let playout = playout.clone();
            async move {
                let mut tick = tokio::time::interval(FRAME_INTERVAL);
                tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
                loop {
                    tick.tick().await;
                    if mic_tx.send(playout.next_frame()).await.is_err() {
                        break;
                    }
                }
            }
        });

        let mut down = Downsampler::new();
        let (tool_tx, mut tool_rx) = mpsc::unbounded_channel::<(String, Value)>();
        let mut tool_tasks: HashMap<String, JoinHandle<()>> = HashMap::new();
        let mut ending = Ending::new(self.cfg.silence_timeout);
        let mut check = tokio::time::interval(Duration::from_millis(200));
        let max_duration = tokio::time::sleep(self.cfg.max_duration);
        let ended = handle.wait_ended();
        tokio::pin!(max_duration, ended);

        let mut peer_ended = false;
        let end_reason = loop {
            tokio::select! {
                _ = &mut ended => {
                    peer_ended = true;
                    break "call ended".to_string();
                }
                _ = &mut max_duration => break "maximum call length reached".to_string(),
                frame = spk_rx.recv() => match frame {
                    Ok(f) => live.send_audio(&f),
                    Err(_) => {
                        peer_ended = true;
                        break "call audio closed".to_string();
                    }
                },
                Some((tool_id, result)) = tool_rx.recv() => {
                    tool_tasks.remove(&tool_id);
                    live.send_tool_response(&tool_id, "ask_hermes", result, "WHEN_IDLE");
                }
                _ = check.tick() => {
                    if let Some(why) = ending.check(Instant::now(), playout.buffered() > 0, !tool_tasks.is_empty()) {
                        break why.to_string();
                    }
                }
                ev = live.events_mut().recv() => match ev {
                    Some(LiveEvent::Audio(pcm)) => playout.push(&down.process(&pcm)),
                    Some(LiveEvent::Interrupted) => {
                        playout.clear();
                        if ending.owner_spoke(Instant::now()) {
                            live.send_text(STILL_CONNECTED);
                        }
                    }
                    Some(LiveEvent::InputTranscript(t)) => {
                        if ending.owner_spoke(Instant::now()) {
                            live.send_text(STILL_CONNECTED);
                        }
                        self.calls.update(id, |r| r.append("owner", &t));
                    }
                    Some(LiveEvent::OutputTranscript(t)) => self.calls.update(id, |r| r.append("assistant", &t)),
                    Some(LiveEvent::ToolCall { id: tool_id, name, args }) => match name.as_str() {
                        "ask_hermes" => {
                            log::info!("call {id}: ask_hermes {}", args["request"]);
                            let task = self.spawn_ask_hermes(&tool_id, id, &args, tool_tx.clone());
                            tool_tasks.insert(tool_id, task);
                        }
                        "end_call" => {
                            // The model sometimes asks a question and hangs up in the same turn.
                            if self.calls.get(id).is_some_and(|r| r.ends_with_question()) {
                                live.send_tool_response(&tool_id, &name, json!({"error":
                                    "You just asked the owner a question. Wait for their answer, and end the call only after they say bye."}), "SILENT");
                            } else {
                                live.send_tool_response(&tool_id, &name, json!({"ok": true}), "SILENT");
                                ending.request();
                            }
                        }
                        _ => live.send_tool_response(&tool_id, &name, json!({"error": "unknown function"}), "WHEN_IDLE"),
                    },
                    Some(LiveEvent::ToolCancelled(ids)) => {
                        for tool_id in ids {
                            if let Some(task) = tool_tasks.remove(&tool_id) {
                                task.abort();
                            }
                        }
                    }
                    Some(LiveEvent::Closed(why)) => break format!("voice model disconnected: {why}"),
                    Some(LiveEvent::Fallback(why)) => {
                        log::error!("call {id}: provider=grok falling back to gemini: {why}");
                        match GeminiLive::connect(&self.cfg.gemini_api_key, self.setup(context)).await {
                            Ok(next) => {
                                live = Voice::Gemini(next);
                                live.send_text("(The call is still connected. Continue from where you left off. Speak English only.)");
                                log::info!("call {id}: provider=gemini fallback=grok");
                            }
                            Err(_) => {
                                log::error!("call {id}: gemini fallback failed");
                                break format!("voice model disconnected: {why}");
                            }
                        }
                    }
                    None => break "voice model stopped".to_string(),
                },
            }
        };

        pacer.abort();
        tool_tasks.values().for_each(JoinHandle::abort);
        drop(live);
        if !peer_ended {
            terminate(&handle).await;
        }
        log::info!("call {id}: {end_reason}");
        self.calls.finish(id, Status::Completed, end_reason);
        self.send_summary(id).await;
        // Memory may have changed during the call (e.g. a new preference); the next call should know.
        self.refresh_persona().await;
    }

    /// After a call: let Hermes digest the transcript, then message the owner a summary.
    async fn send_summary(&self, id: &str) {
        let Some(record) = self.calls.get(id) else {
            return;
        };
        if record.transcript.is_empty() {
            return;
        }
        let transcript = record.transcript_text();
        let summary = match &self.hermes {
            Some(h) => {
                let input = format!(
                    "A WhatsApp voice call between the owner and your voice agent just ended. \
                     Transcript:\n\n{transcript}\n\nNote anything worth remembering. Do not redo \
                     actions that were already done during the call. Reply with only a two or three \
                     sentence summary of the call for the owner."
                );
                match h
                    .run(
                        &input,
                        "Reply in plain text only.",
                        &format!("whatsapp-voice-{id}"),
                        HERMES_TIMEOUT,
                    )
                    .await
                {
                    Ok(s) if !s.trim().is_empty() => s,
                    Ok(_) => transcript.clone(),
                    Err(e) => {
                        log::warn!("call {id}: summary run failed: {e:#}");
                        transcript.clone()
                    }
                }
            }
            None => transcript.clone(),
        };
        self.calls.update(id, |r| r.summary = Some(summary.clone()));
        if !self.cfg.send_summary {
            return;
        }
        let minutes = record
            .answered_at
            .zip(record.ended_at)
            .map(|(a, e)| e.saturating_sub(a).div_ceil(60))
            .unwrap_or(0);
        let text = format!(
            "📞 Call summary ({minutes} min):\n{}",
            truncate(&summary, 3500)
        );
        if let Err(e) = self
            .client
            .send_message(&self.owner_jid(), wa::Message::text(text))
            .await
        {
            log::warn!("call {id}: could not send summary: {e}");
        }
    }
}

fn system_instruction(
    name: &str,
    persona: &str,
    owner_instructions: &str,
    tools_note: &str,
    context: &str,
) -> String {
    let identity = if persona.is_empty() {
        format!("You are {name}, the owner's personal assistant.")
    } else {
        format!(
            "Hermes describes who you are and who the owner is; stay in that character:\n{persona}"
        )
    };
    format!(
        "{identity}\n\nYou are talking with the owner on a live WhatsApp voice call.\n\
         - This is a phone call: speak naturally and keep replies short. No lists, markdown, links or emoji.\n\
         - {tools_note}\n\
         - End the call when the owner says goodbye, or once the purpose is done and they have nothing \
         more: give a brief goodbye and call end_call in that same reply. Never call end_call while \
         asking a question.\n\
         {owner_instructions}\n\n\
         {context}"
    )
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

async fn terminate(handle: &CallHandle) {
    match handle.terminate().await {
        CallTermination::LocalOnly(e) => log::warn!("hangup not confirmed by peer: {e}"),
        CallTermination::PartlyNotified { unconfirmed, .. } => {
            log::warn!("hangup unconfirmed on {unconfirmed} device(s)")
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_fragments_merge_by_speaker() {
        let mut r = CallRecord::new("inbound", "");
        r.append("owner", "Hey, ");
        r.append("owner", "what's up?");
        r.append("assistant", "Not much.");
        r.append("owner", "Ok");
        assert_eq!(r.transcript.len(), 3);
        assert_eq!(
            r.transcript_text(),
            "owner: Hey, what's up?\nassistant: Not much.\nowner: Ok"
        );
    }

    #[test]
    fn detects_a_trailing_question() {
        let mut r = CallRecord::new("outbound", "");
        assert!(!r.ends_with_question());
        r.append("assistant", "Sure. Is that better? ");
        assert!(r.ends_with_question());
        r.append("owner", "Yes.");
        assert!(!r.ends_with_question(), "the owner answered");
        r.append("assistant", "Great, talk soon!");
        assert!(!r.ends_with_question());
    }

    #[test]
    fn one_call_at_a_time() {
        let calls = Calls::default();
        let first = calls.begin("outbound", "a").unwrap();
        assert!(calls.begin("inbound", "b").is_err());
        calls.finish(&first.id, Status::Completed, "done");
        assert_eq!(calls.get(&first.id).unwrap().status, Status::Completed);
        assert!(calls.active().is_none());
        assert!(calls.begin("inbound", "b").is_ok());
        assert_eq!(calls.recent(1)[0].direction, "inbound");
    }

    #[test]
    fn ring_signals_reach_a_late_subscriber_only_while_ringing() {
        let signals = RingSignals::default();
        signals.send("C1", Signal::Accepted);
        assert!(
            signals.subscribe("C1").is_none(),
            "nothing is collected before a call rings"
        );

        signals.start();
        signals.send("C1", Signal::Accepted); // the accept can beat start() returning the id
        assert_eq!(*signals.subscribe("C1").unwrap().borrow(), Signal::Accepted);
        assert_eq!(*signals.subscribe("C2").unwrap().borrow(), Signal::Pending);

        signals.stop();
        assert!(signals.subscribe("C1").is_none());
    }

    #[test]
    fn persona_replaces_the_default_identity() {
        let plain = system_instruction("Hermes", "", "", "tools", "ctx");
        assert!(plain.starts_with("You are Hermes, the owner's personal assistant."));
        let briefed =
            system_instruction("Hermes", "You are Nova.", "Speak Spanish.", "tools", "ctx");
        assert!(briefed.contains("You are Nova."));
        assert!(briefed.contains("Speak Spanish."));
        assert!(!briefed.contains("You are Hermes"));
        assert!(briefed.ends_with("ctx"));
    }

    #[test]
    fn ending_waits_for_the_goodbye_to_play_then_a_quiet_window() {
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let mut e = Ending::new(Duration::from_secs(30));
        assert_eq!(e.check(t0, false, false), None);

        e.request();
        assert_eq!(
            e.check(at(1_000), true, false),
            None,
            "goodbye still playing"
        );
        assert_eq!(
            e.check(at(2_000), false, false),
            None,
            "quiet window starts"
        );
        assert_eq!(e.check(at(4_900), false, false), None);
        assert_eq!(
            e.check(at(5_000), false, false),
            Some("assistant ended the call")
        );
    }

    #[test]
    fn owner_speech_calls_the_hangup_off() {
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let mut e = Ending::new(Duration::from_secs(30));
        assert!(!e.owner_spoke(t0), "nothing to call off before end_call");

        e.request();
        assert_eq!(e.check(at(1_000), false, false), None);
        assert!(
            e.owner_spoke(at(2_000)),
            "\"I have another question\", in any language"
        );
        assert_eq!(e.check(at(20_000), false, false), None, "call stays up");
    }

    #[test]
    fn silence_ends_an_unclosed_call_unless_hermes_is_working() {
        let t0 = Instant::now();
        let at = |s| t0 + Duration::from_secs(s);
        let mut e = Ending::new(Duration::from_secs(30));
        e.owner_spoke(t0);
        assert_eq!(e.check(at(29), false, false), None);
        assert_eq!(e.check(at(31), false, true), None, "waiting on ask_hermes");
        assert_eq!(
            e.check(at(31), false, false),
            Some("hung up after a long silence")
        );
        assert_eq!(
            e.check(at(32), true, false),
            None,
            "model speech is activity"
        );
        assert_eq!(e.check(at(61), false, false), None);
        assert_eq!(
            e.check(at(62), false, false),
            Some("hung up after a long silence")
        );
    }

    #[test]
    fn truncates_on_char_boundary() {
        assert_eq!(truncate("héllo", 2), "hé…");
        assert_eq!(truncate("hi", 5), "hi");
    }
}
