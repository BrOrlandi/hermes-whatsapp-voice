"""Core-session voice: the live voice model acts as the model of the owner's WhatsApp DM session.

Runs inside `hermes whatsapp-voice serve` (Hermes's own interpreter, so Hermes modules import and
state.db is opened with Hermes's SQLite). The sidecar calls this loopback service to:

- start a call: get the session's own system prompt, recent history and tool schemas (/voice/start)
- run a tool through Hermes's dispatcher, in that session (/voice/tool, /voice/approve)
- write the call back into the session's history as it happens (/voice/events, /voice/end)

Everything that touches Hermes internals is in `HermesSession`; the rest is plain data shaping, so
it is unit-tested without Hermes. Hermes APIs used here are listed in CORE_PATCHES.md.
"""

from __future__ import annotations

import hmac
import json
import logging
import os
import re
import threading
import time
from concurrent.futures import Future, ThreadPoolExecutor
from dataclasses import dataclass, field
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any

log = logging.getLogger("hermes_whatsapp_voice.core_session")

APPROVE_TOOL = "approve_command"
VOICE_KIND = "voice_call"
USER_PREFIX = "[Voice call] "
TEXT_PREFIX = "[Texted during the voice call] "
TOOL_TIMEOUT = 600.0  # long tools (delegate_task, execute_code) run while the call goes on


# --- pure helpers -------------------------------------------------------------------------------


def digits(number: str) -> str:
    return re.sub(r"\D", "", number or "")


def pick_owner_entry(entries: dict[str, dict], owner: str, canonical=digits) -> tuple[str, dict] | None:
    """The gateway routing entry of the owner's WhatsApp DM. Matched on the owner's number (as the
    session key or the origin's user id), never on a stored chat id: sessions and LIDs change."""
    for key, entry in entries.items():
        origin = entry.get("origin") or {}
        if entry.get("platform", origin.get("platform")) != "whatsapp":
            continue
        if entry.get("chat_type", origin.get("chat_type")) != "dm":
            continue
        ids = [key.rsplit(":", 1)[-1], origin.get("user_id") or "", origin.get("chat_id") or ""]
        if is_owner(ids, owner, canonical):
            return key, entry
    return None


def is_owner(ids: list[str], owner: str, canonical=digits) -> bool:
    """Whether any WhatsApp id (JID, LID or number) is the owner's number."""
    want = canonical(digits(owner))
    return any(i and canonical(digits(i.split("@")[0].split(":")[0])) == want for i in ids)


def _clip(text: str, limit: int) -> str:
    text = " ".join(str(text or "").split())
    return text if len(text) <= limit else text[: limit - 1] + "…"


def history_to_contents(messages: list[dict], budget: int) -> list[dict]:
    """Hermes conversation rows -> Gemini `clientContent` turns, newest kept within `budget` chars.

    Tool calls and results are folded into the model's turns as short notes: the voice model needs
    to know what happened, not the raw payloads. A compaction summary at the start is kept first.
    """
    turns: list[tuple[str, str]] = []
    summary = ""
    for m in messages:
        role, content = m.get("role"), m.get("content")
        text = content if isinstance(content, str) else _content_text(content)
        if m.get("_compressed_summary") and not turns:
            summary = _clip(text, min(budget // 3, 6000))
            continue
        if role == "user":
            turns.append(("user", text.strip()))
        elif role == "assistant":
            parts = [text.strip()] if text and text.strip() else []
            for call in m.get("tool_calls") or []:
                fn = call.get("function", call)
                parts.append(f"[used {fn.get('name')}({_clip(fn.get('arguments', ''), 160)})]")
            turns.append(("model", "\n".join(parts)))
        elif role == "tool":
            turns.append(("model", f"[{m.get('tool_name') or 'tool'} result: {_clip(text, 300)}]"))
    merged: list[list[str]] = []
    for role, text in turns:
        if not text:
            continue
        if merged and merged[-1][0] == role:
            merged[-1][1] += "\n" + text
        else:
            merged.append([role, text])
    kept: list[list[str]] = []
    used = len(summary)
    for role, text in reversed(merged):
        if used + len(text) > budget:
            if not kept:  # always keep the latest turn, clipped
                kept.append([role, text[-(budget - used) :]])
            break
        kept.append([role, text])
        used += len(text)
    kept.reverse()
    while kept and kept[0][0] != "user":
        kept.pop(0)
    if summary:
        note = f"(Summary of the earlier conversation)\n{summary}"
        if kept:
            kept[0][1] = f"{note}\n\n{kept[0][1]}"
        else:
            kept.append(["user", note])
    return [{"role": r, "parts": [{"text": t}]} for r, t in kept]


def _content_text(content) -> str:
    if isinstance(content, list):
        return "\n".join(p.get("text", "") for p in content if isinstance(p, dict))
    return str(content or "")


def call_instructions(*, owner_name: str, reason: str, details: str, direction: str,
                      voice_instructions: str, languages: list[str]) -> str:  # fmt: skip
    """The part of the system prompt that is specific to a live call; appended to the session's own."""
    owner = owner_name or "the owner"
    lines = [
        "## Live voice call",
        (
            f"You are now speaking with {owner} on a live WhatsApp voice call. This call is part of the "
            "same WhatsApp conversation as the text chat above: the history you were given is that chat, "
            "and everything said on this call is saved back into it. You are the same assistant, with the "
            "same name and persona as described above. Never say you are a separate agent or a different "
            "conversation."
        ),
        (
            "- This is a phone call: talk like a person on the phone, in your usual voice with them, and keep "
            "replies short. No lists, markdown, links or emoji."
        ),
        (
            '- Skip stock assistant phrases ("How can I help you today?", "I understand.", "I apologize '
            'for the interruption", "Let me know if there is anything else"). Vary how you say things.'
        ),
        (
            f"- Use {owner}'s name at most once, when you greet them; after that, don't say it. If you hear a "
            "fragment or noise that makes no sense, don't act on it; ask briefly or let it go."
        ),
        "- You have all of your usual tools on this call, and use them as you would in the text chat.",
        (
            "- Before a tool, say a few words first. Only say you will check something if you call a tool in "
            "that same reply. Results arrive a little later and you will be told when they do; keep talking "
            "with the owner meanwhile. Never make up results or claim a tool ran when it did not."
        ),
        (
            f"- If a tool result says a command needs approval, tell the owner briefly what it would do, ask "
            f"them, and call {APPROVE_TOOL} with their answer."
        ),
        (
            "- End the call when the owner says goodbye, or once the purpose is done and they have nothing "
            "more: give a brief goodbye and call end_call in that same reply. Never call end_call while "
            "asking a question."
        ),
    ]
    if languages:
        lines.append(
            f"- Speak only these languages: {', '.join(languages)}. If the owner uses any other "
            f"language, reply in {languages[0]}."
        )
    if voice_instructions.strip():
        lines.append(voice_instructions.strip())
    if direction == "outbound":
        lines.append(f"\nYou placed this call. Reason: {reason}" + (f"\nDetails: {details}" if details else ""))
    else:
        lines.append(f"\n{owner} called you.")
    return "\n".join(lines)


@dataclass
class EventLog:
    """The call as ordered events from the sidecar, turned into Hermes messages for write-back.

    Only a prefix whose tool calls all have results is written, so a flushed transcript is always a
    valid conversation (no user turn between an assistant's tool call and its result).
    """

    call_id: str
    events: list[dict] = field(default_factory=list)
    flushed: int = 0  # messages already written

    def add(self, events: list[dict]) -> None:
        self.events.extend(events)

    def messages(self, *, final: bool = False) -> list[dict]:
        """All messages so far; unless final, only up to the first tool call still awaiting its result."""
        return self._build(final)[0]

    def _build(self, final: bool) -> tuple[list[dict], bool]:
        meta = {"source": "whatsapp_voice", "call_id": self.call_id}
        out: list[dict] = []
        owner_of_call: dict[str, int] = {}  # tool call id -> index of its assistant message
        open_calls: dict[int, set[str]] = {}

        def stamp(msg: dict) -> dict:
            msg["display_kind"] = VOICE_KIND
            msg["display_metadata"] = dict(meta)
            return msg

        for ev in self.events:
            kind = ev.get("type")
            text = ev.get("text") or ""
            if kind == "owner_text" and text:
                out.append(stamp({"role": "user", "content": TEXT_PREFIX + text}))
            elif kind == "owner" and text:
                last = out[-1] if out else None
                if last and last["role"] == "user":
                    last["content"] += text
                else:
                    out.append(stamp({"role": "user", "content": USER_PREFIX + text}))
            elif kind == "assistant" and text:
                last = out[-1] if out else None
                if (
                    last
                    and last["role"] == "assistant"
                    and (open_calls.get(len(out) - 1) or not last.get("tool_calls"))
                ):
                    last["content"] = (last.get("content") or "") + text
                else:
                    out.append(stamp({"role": "assistant", "content": text}))
            elif kind == "tool_call":
                last = out[-1] if out else None
                if not (
                    last
                    and last["role"] == "assistant"
                    and (open_calls.get(len(out) - 1) or not last.get("tool_calls"))
                ):
                    out.append(stamp({"role": "assistant", "content": ""}))
                idx = len(out) - 1
                out[idx].setdefault("tool_calls", []).append({
                    "id": ev["id"], "type": "function",
                    "function": {"name": ev["name"], "arguments": json.dumps(ev.get("args") or {})},
                })  # fmt: skip
                owner_of_call[ev["id"]] = idx
                open_calls.setdefault(idx, set()).add(ev["id"])
            elif kind == "tool_result" and ev.get("id") in owner_of_call:
                idx = owner_of_call[ev["id"]]
                open_calls[idx].discard(ev["id"])
                pos = idx + 1
                while pos < len(out) and out[pos]["role"] == "tool":
                    pos += 1
                out.insert(pos, stamp({
                    "role": "tool", "tool_call_id": ev["id"], "tool_name": ev.get("name") or "",
                    "content": str(ev.get("result") or ""),
                }))  # fmt: skip
                # Indexes after the insert shift by one.
                owner_of_call = {k: (v + 1 if v >= pos else v) for k, v in owner_of_call.items()}
                open_calls = {(k + 1 if k >= pos else k): v for k, v in open_calls.items()}
        if final:
            for idx, ids in sorted(open_calls.items(), reverse=True):
                for call_id in ids:
                    out.insert(idx + 1, stamp({
                        "role": "tool", "tool_call_id": call_id, "tool_name": "",
                        "content": "(No result: the call ended before this finished.)",
                    }))  # fmt: skip
            return out, False
        first_open = min((i for i, ids in open_calls.items() if ids), default=len(out))
        return out[:first_open], first_open < len(out)

    def pending(self, *, final: bool = False) -> list[dict]:
        """Messages not written yet. The last one is held back while it may still grow, unless final."""
        msgs, cut = self._build(final)
        if not final and not cut and msgs:
            msgs = msgs[:-1]
        return msgs[self.flushed :]


# --- Hermes side --------------------------------------------------------------------------------


class Job:
    """A tool run, which may stop half-way to ask for an approval."""

    def __init__(self):
        self.done: Future = Future()
        self.approval: dict | None = None
        self.changed = threading.Event()


class HermesSession:
    """One call's view of the owner's WhatsApp DM session in Hermes."""

    def __init__(self, service: Service, call_id: str):
        from hermes_state import SessionDB

        self.service = service
        self.call_id = call_id
        self.db = SessionDB()
        self.log = EventLog(call_id)
        self.lock = threading.Lock()
        self.holder = f"pid={os.getpid()}:turn=voice-{call_id}:platform=whatsapp"
        self.key, self.entry = self._resolve()
        self.session_id = self._tip(self.entry["session_id"])
        self.origin = self.entry.get("origin") or {}
        self.agent: Future = service.pool.submit(self._build_agent)
        self.running: list[Job] = []
        self.local = threading.local()  # which job the current thread works for
        self.seen_row = self._max_row()

    # session ---------------------------------------------------------------------------------

    def _resolve(self) -> tuple[str, dict]:
        from gateway.whatsapp_identity import canonical_whatsapp_identifier
        from hermes_constants import get_hermes_home

        entries: dict[str, dict] = {}
        # API-bound routes use scope ""; the gateway's own scope (its sessions dir) wins.
        for scope in ("", str((get_hermes_home() / "sessions").resolve())):
            for key, raw in self.db.load_gateway_routing_entries(scope=scope).items():
                try:
                    entries[key] = json.loads(raw)
                except ValueError:
                    continue
        found = pick_owner_entry(entries, self.service.owner, canonical_whatsapp_identifier)
        if not found:
            raise LookupError("No WhatsApp DM session with the owner yet: send the bot a text message first.")
        return found

    def _tip(self, session_id: str) -> str:
        return self.db.get_compression_tip(session_id) or session_id

    def _history(self) -> list[dict]:
        return self.db.get_messages_as_conversation(self.session_id, repair_alternation=True, include_row_ids=True)

    def _max_row(self) -> int:
        return max((m.get("_row_id") or 0 for m in self._history()), default=0)

    def _pin(self) -> str:
        pin = self.entry.get("prompt_pin") or {}
        parts = [pin.get("context_prompt"), pin.get("channel_prompt")]
        return "\n\n".join(p for p in parts if p and p != "None")

    def _env(self):
        """Bind the session the way the gateway does for a turn; returns an undo function."""
        from gateway.session_context import clear_session_vars, set_session_vars
        from tools.approval import register_gateway_notify
        from tools.approval_context import reset_current_session_key, set_current_session_key

        o = self.origin
        tokens = set_session_vars(
            platform="whatsapp", chat_id=o.get("chat_id") or "", chat_type="dm", chat_name=o.get("chat_name") or "",
            user_id=o.get("user_id") or "", user_name=o.get("user_name") or "", session_key=self.key,
            session_id=self.session_id, cron_session="",
        )  # fmt: skip
        key_token = set_current_session_key(self.key)
        register_gateway_notify(self.key, self._on_approval)

        def undo():
            reset_current_session_key(key_token)
            clear_session_vars(tokens)

        return undo

    def _on_approval(self, data: dict) -> None:
        # Called in the thread that hit the approval; nested tools (e.g. delegate_task) use their own threads.
        job = getattr(self.local, "job", None) or (self.running[-1] if self.running else None)
        if job is not None:
            job.approval = {"command": data.get("command", ""), "description": data.get("description", "")}
            job.changed.set()

    def _agent_kwargs(self, ephemeral: str) -> dict:
        from gateway.run import (
            GatewayRunner,
            _checkpoint_agent_kwargs,
            _current_max_iterations,
            _load_gateway_config,
            _resolve_gateway_model,
            _resolve_runtime_agent_kwargs,
        )
        from hermes_cli.tools_config import _get_platform_tools

        runtime = _resolve_runtime_agent_kwargs()
        model = runtime.pop("model", None) or _resolve_gateway_model()
        runtime.pop("_fallback_notice", None)
        cfg = _load_gateway_config()
        o = self.origin
        return {
            "model": model, **runtime, **_checkpoint_agent_kwargs(cfg),
            "max_iterations": _current_max_iterations(), "quiet_mode": True, "verbose_logging": False,
            "enabled_toolsets": sorted(_get_platform_tools(cfg, "whatsapp")),
            "disabled_toolsets": (cfg.get("agent") or {}).get("disabled_toolsets") or None,
            "ephemeral_system_prompt": ephemeral or None,
            "session_id": self.session_id, "platform": "whatsapp",
            "user_id": o.get("user_id"), "user_name": o.get("user_name"), "chat_id": o.get("chat_id"),
            "chat_name": o.get("chat_name"), "chat_type": "dm", "gateway_session_key": self.key,
            "session_db": self.db, "fallback_model": GatewayRunner._load_fallback_model(),
            "reasoning_config": GatewayRunner._load_reasoning_config(model),
        }  # fmt: skip

    def _build_agent(self):
        from run_agent import AIAgent

        started = time.monotonic()
        agent = AIAgent(**self._agent_kwargs(self._pin()))
        log.info("call %s: agent for %s ready in %.1fs", self.call_id, self.session_id, time.monotonic() - started)
        return agent

    # context ---------------------------------------------------------------------------------

    def context(self, *, direction: str, reason: str, details: str) -> dict:
        from .gemini_schema import function_declaration

        s = self.service
        row = self.db.get_session(self.session_id) or {}
        system_prompt = row.get("system_prompt") or ""
        if not system_prompt:  # a session that never ran a turn has no stored prompt yet
            system_prompt = self.agent.result(timeout=60)._build_system_prompt(None)
        schemas = self._tool_schemas(row)
        owner_name = self.origin.get("user_name") or self.entry.get("display_name") or ""
        instruction = "\n\n".join(p for p in [system_prompt, self._pin(), call_instructions(
            owner_name=owner_name, reason=reason, details=details, direction=direction,
            voice_instructions=s.voice_instructions, languages=s.languages,
        )] if p)  # fmt: skip
        history = history_to_contents(self._history(), s.history_chars)
        return {
            "session_id": self.session_id,
            "system_instruction": instruction,
            "history": history,
            "functions": [function_declaration(t) for t in schemas.values()],
            "vocabulary": sorted({w for w in re.split(r"\s+", owner_name) if len(w) > 2} | set(s.vocabulary)),
        }

    def _tool_schemas(self, row: dict) -> dict[str, dict]:
        """The session's own pinned tool schemas (exact bytes the model saw), else the registry's."""
        tools: list = []
        try:
            pinned = json.loads(row.get("tool_names") or "null")
            tools = pinned.get("tools", []) if isinstance(pinned, dict) else (pinned or [])
        except (TypeError, ValueError):
            pass
        if not tools or not isinstance(tools[0], dict):
            tools = self.agent.result(timeout=60).tools or []
        return {t["function"]["name"]: t for t in tools if isinstance(t, dict) and "function" in t}

    # tools -----------------------------------------------------------------------------------

    def _run_job(self, work, timeout: float) -> dict:
        """Run `work` in a thread; return its result, or an approval request if it stops for one."""
        job = Job()
        self.running.append(job)

        def run():
            self.local.job = job
            undo = self._env()
            try:
                job.done.set_result(work())
            except Exception as e:
                log.exception("call %s: job failed", self.call_id)
                job.done.set_exception(e)
            finally:
                undo()
                self.running.remove(job)
                job.changed.set()

        self.service.pool.submit(run)
        return self._wait(job, timeout)

    def _wait(self, job: Job, timeout: float) -> dict:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            job.changed.wait(min(1.0, max(0.0, deadline - time.monotonic())))
            job.changed.clear()
            if job.done.done():
                try:
                    return {"result": _result_text(job.done.result())}
                except Exception as e:  # noqa: BLE001
                    return {"error": f"{type(e).__name__}: {e}"}
            if job.approval is not None:
                return {"approval_required": job.approval,
                        "instruction": f"Ask the owner, then call {APPROVE_TOOL} with their answer."}  # fmt: skip
        return {"error": f"Still running after {int(timeout)} seconds; the result will not reach this call."}

    def tool(self, name: str, args: dict, tool_call_id: str) -> dict:
        def work():
            agent = self.agent.result(timeout=90)
            return agent._invoke_tool(name, args, self.session_id, tool_call_id=tool_call_id)

        return self._run_job(work, TOOL_TIMEOUT)

    def approve(self, choice: str) -> dict:
        from tools.approval import resolve_gateway_approval

        job = next((j for j in self.running if j.approval is not None), None)
        if job is None:
            return {"error": "Nothing is waiting for approval."}
        verdict = {"once": "once", "session": "session", "always": "always"}.get(choice, "deny")
        job.approval = None
        resolve_gateway_approval(self.key, verdict)
        return self._wait(job, TOOL_TIMEOUT)

    # write-back ------------------------------------------------------------------------------

    def events(self, events: list[dict]) -> dict:
        self.log.add(events)
        if any(e.get("type") == "turn_complete" for e in events):
            self.service.pool.submit(self.flush)
        return {"chat_updates": self._chat_updates()}

    def flush(self, final: bool = False) -> int:
        with self.lock:
            msgs = self.log.pending(final=final)
            if not msgs:
                return 0
            self.session_id = self._tip(self.session_id)
            if not self.db.acquire_session_turn_lease(self.session_id, self.holder, ttl_seconds=60, wait_seconds=120):
                log.warning("call %s: could not lock the session to save %d messages", self.call_id, len(msgs))
                return 0
            try:
                n = self.db.append_messages_batch(
                    self.session_id, [dict(m) for m in msgs], turn_lease_holder=self.holder
                )
                self.log.flushed += len(msgs)
                self.seen_row = self._max_row()
                return n
            finally:
                self.db.release_session_turn_lease(self.session_id, self.holder)

    def _chat_updates(self) -> str:
        """Text-chat messages that arrived during the call, so the voice knows about them."""
        news = [m for m in self._history() if (m.get("_row_id") or 0) > self.seen_row]
        news = [m for m in news if m.get("display_kind") != VOICE_KIND and m.get("role") in ("user", "assistant")]
        if not news:
            return ""
        self.seen_row = max(m.get("_row_id") or 0 for m in news)
        lines = [f"{'Owner' if m['role'] == 'user' else 'You'}: {_clip(_content_text(m.get('content')), 500)}"
                 for m in news if _content_text(m.get("content")).strip()]  # fmt: skip
        return "(Meanwhile, in the WhatsApp text chat:\n" + "\n".join(lines) + ")" if lines else ""

    def end(self) -> int:
        n = self.flush(final=True)
        from tools.approval import unregister_gateway_notify

        unregister_gateway_notify(self.key)
        return n


def _result_text(result: Any) -> str:
    # Hermes already limits tool output, as in a text turn; nothing is cut here.
    return result if isinstance(result, str) else json.dumps(result, default=str)


def text_into_call(ctx, event) -> dict | None:
    """`pre_gateway_dispatch` hook (gateway process): while a core-session call is active, a plain
    text from the owner goes into the call and the text agent does not answer it, so one assistant
    handles the conversation. Anything else, or no active call, dispatches as usual."""
    from .sidecar_client import Client, settings, token

    source = getattr(event, "source", None)
    if source is None or getattr(event, "internal", False):
        return None
    if getattr(getattr(event, "message_type", None), "value", "") != "text":
        return None
    if getattr(getattr(source, "platform", None), "value", "") != "whatsapp" or source.chat_type != "dm":
        return None
    text = (event.text or "").strip()
    s = settings(ctx)
    if not text or text.startswith("/") or s["context_mode"] != "core_session":
        return None
    from gateway.whatsapp_identity import canonical_whatsapp_identifier

    ids = [source.user_id or "", getattr(source, "user_id_alt", None) or "", source.chat_id or ""]
    if not is_owner(ids, str(s["owner_number"]), canonical_whatsapp_identifier):
        return None
    if not Client(str(s["listen"]), token(), timeout=2.0).text_into_call(text):
        return None
    return {"action": "skip", "reason": "delivered to the active WhatsApp voice call"}


# --- HTTP service -------------------------------------------------------------------------------


class Service:
    def __init__(self, *, owner: str, token: str, history_chars: int = 30000,
                 voice_instructions: str = "", languages: list[str] | None = None,
                 vocabulary: list[str] | None = None):  # fmt: skip
        self.owner = owner
        self.token = token
        self.history_chars = history_chars
        self.voice_instructions = voice_instructions
        self.languages = languages or []
        self.vocabulary = vocabulary or []
        self.pool = ThreadPoolExecutor(max_workers=16, thread_name_prefix="hwv-core")
        self.calls: dict[str, HermesSession] = {}

    def handle(self, path: str, body: dict) -> dict:
        call_id = str(body.get("call") or "")
        if path == "/voice/start":
            started = time.monotonic()
            session = HermesSession(self, call_id)
            self.calls[call_id] = session
            ctx = session.context(direction=body.get("direction", "inbound"), reason=body.get("reason", ""),
                                  details=body.get("details", ""))  # fmt: skip
            log.info(
                "call %s: core session %s context in %.2fs", call_id, session.session_id, time.monotonic() - started
            )
            return ctx
        session = self.calls.get(call_id)
        if session is None:
            raise LookupError(f"unknown call {call_id}")
        if path == "/voice/tool":
            return session.tool(body["name"], body.get("args") or {}, str(body.get("id") or ""))
        if path == "/voice/approve":
            return session.approve(str(body.get("choice") or "deny"))
        if path == "/voice/events":
            return session.events(body.get("events") or [])
        if path == "/voice/end":
            self.calls.pop(call_id, None)
            return {"written": session.end()}
        raise LookupError(f"no route {path}")

    def serve(self, listen: str) -> ThreadingHTTPServer:
        host, _, port = listen.rpartition(":")
        if host not in ("127.0.0.1", "localhost", "::1", "[::1]"):
            raise ValueError(f"context_listen must be a loopback address, not {listen}")
        service = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, fmt, *args):  # quiet: the sidecar logs calls
                pass

            def do_POST(self):
                auth = self.headers.get("Authorization", "")
                if not hmac.compare_digest(auth.encode(), f"Bearer {service.token}".encode()):
                    return self._send(401, {"error": "unauthorized"})
                try:
                    body = json.loads(self.rfile.read(int(self.headers.get("Content-Length") or 0)) or b"{}")
                    self._send(200, service.handle(self.path, body))
                except LookupError as e:
                    self._send(404, {"error": str(e)})
                except Exception as e:
                    log.exception("core session %s failed", self.path)
                    self._send(500, {"error": f"{type(e).__name__}: {e}"})

            def _send(self, status: int, payload: dict):
                data = json.dumps(payload).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

        server = ThreadingHTTPServer((host.strip("[]"), int(port)), Handler)
        server.daemon_threads = True
        threading.Thread(target=server.serve_forever, name="hwv-core-http", daemon=True).start()
        return server
