"""End-to-end smoke test of core-session voice, without a phone call.

Opens a real Gemini Live session with the owner's WhatsApp DM session as context, asks typed
questions (transcribed answers come back as text), runs any tool calls through Hermes in that
session, writes the exchange back, and checks the rows landed in the session. Sends no WhatsApp
messages and places no calls, but it does add a short test exchange to the owner's chat history.

Run with Hermes's interpreter, from the plugin directory's parent:
    hermes --run-module cProfile -o /dev/null scripts/core_session_smoke.py <plugin dir>
"""

from __future__ import annotations

import asyncio
import importlib.util
import json
import logging
import sys
import time
import uuid
from pathlib import Path

ENDPOINT = "wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent"
QUESTIONS = [
    "What did we last talk about in our text chat? One sentence.",
    "Which memory system do we use? One sentence.",
    "Use your skills_list tool and tell me how many skills I have, in one sentence.",
]


def load_plugin(path: Path):
    spec = importlib.util.spec_from_file_location(
        "hwv_dev", path / "__init__.py", submodule_search_locations=[str(path)]
    )
    pkg = importlib.util.module_from_spec(spec)
    sys.modules["hwv_dev"] = pkg
    spec.loader.exec_module(pkg)
    import hwv_dev.core_session as cs
    import hwv_dev.sidecar_client as sc

    return cs, sc


class Ctx:
    def get_config(self, key, default=None):
        from hermes_cli.config import load_config_readonly

        entry = ((load_config_readonly() or {}).get("plugins", {}).get("entries", {}) or {}).get(
            "hermes-whatsapp-voice"
        ) or {}
        return (entry.get("settings") or {}).get(key, default)


async def ask(ws, session, question: str, stats: dict) -> str:
    """One typed question; runs tool calls; returns the transcribed answer."""
    session.events([{"type": "owner", "text": question}])
    await ws.send(json.dumps({"realtimeInput": {"text": question}}))
    sent = time.monotonic()
    answer, pending, first = "", set(), None
    heard_since_tool = True  # the answer to a tool result comes in a later turn
    tools_done = asyncio.Queue()

    async def run_tool(fc):
        started = time.monotonic()
        result = await asyncio.to_thread(session.tool, fc["name"], fc.get("args") or {}, fc["id"])
        stats.setdefault("tool_round_trip_s", []).append(round(time.monotonic() - started, 2))
        await tools_done.put((fc, result))

    deadline = time.monotonic() + 120
    while time.monotonic() < deadline:
        while not tools_done.empty():
            fc, result = tools_done.get_nowait()
            pending.discard(fc["id"])
            heard_since_tool = False
            text = result.get("result") if isinstance(result.get("result"), str) else json.dumps(result)
            session.events([{"type": "tool_result", "id": fc["id"], "name": fc["name"], "result": text}])
            await ws.send(json.dumps({"toolResponse": {"functionResponses": [
                {"id": fc["id"], "name": fc["name"], "response": result, "scheduling": "WHEN_IDLE"}]}}))  # fmt: skip
        try:
            raw = await asyncio.wait_for(ws.recv(), timeout=1.0)
        except TimeoutError:
            continue
        msg = json.loads(raw)
        sc_ = msg.get("serverContent") or {}
        if t := (sc_.get("outputTranscription") or {}).get("text"):
            first = first or time.monotonic()
            answer += t
            heard_since_tool = True
            session.events([{"type": "assistant", "text": t}])
        for fc in (msg.get("toolCall") or {}).get("functionCalls") or []:
            print(f"   tool call: {fc['name']}({json.dumps(fc.get('args'))[:120]})")
            session.events([{"type": "tool_call", "id": fc["id"], "name": fc["name"], "args": fc.get("args") or {}}])
            pending.add(fc["id"])
            asyncio.create_task(run_tool(fc))
        if sc_.get("turnComplete"):
            session.events([{"type": "turn_complete"}])
            if not pending and tools_done.empty() and answer.strip() and heard_since_tool:
                break
    if first:
        stats.setdefault("first_words_s", []).append(round(first - sent, 2))
    return answer.strip()


async def main(plugin_dir: Path) -> int:
    cs, sc = load_plugin(plugin_dir)
    s = sc.settings(Ctx())
    service = cs.Service(
        owner=str(s["owner_number"]), token="smoke", voice_tools=str(s["voice_tools"]),
        history_chars=int(s["history_chars"]), voice_instructions=str(s["voice_instructions"]),
        languages=[c.strip() for c in str(s["languages"]).split(",") if c.strip()],
        vocabulary=[w.strip() for w in str(s["vocabulary"]).split(",") if w.strip()],
    )  # fmt: skip
    call_id = "smoke-" + uuid.uuid4().hex[:6]
    stats: dict = {}
    t0 = time.monotonic()
    session = cs.HermesSession(service, call_id)
    service.calls[call_id] = session
    ctx = session.context(direction="inbound", reason="", details="")
    stats["context_s"] = round(time.monotonic() - t0, 2)
    print(f"session {session.session_id}: prompt {len(ctx['system_instruction'])} chars, "
          f"{len(ctx['history'])} history turns, tools {[f['name'] for f in ctx['functions']]}")  # fmt: skip
    import websockets

    functions = ctx["functions"] + [{"name": "end_call", "description": "Hang up.", "behavior": "NON_BLOCKING"}]
    transcription = {"customVocabulary": ctx["vocabulary"]} if ctx["vocabulary"] else {}
    setup = {
        "model": f"models/{s['voice_model']}",
        "generationConfig": {"responseModalities": ["AUDIO"],
                             "speechConfig": {"voiceConfig": {"prebuiltVoiceConfig": {"voiceName": s["voice"]}}}},
        "systemInstruction": {"parts": [{"text": ctx["system_instruction"]}]},
        "inputAudioTranscription": transcription, "outputAudioTranscription": transcription,
        "tools": [{"functionDeclarations": functions}],
        "historyConfig": {"initialHistoryInClientContent": True},
    }  # fmt: skip
    t0 = time.monotonic()
    async with websockets.connect(
        ENDPOINT, additional_headers={"x-goog-api-key": sc.gemini_key()}, max_size=None
    ) as ws:
        await ws.send(json.dumps({"setup": setup}))
        while "setupComplete" not in json.loads(await ws.recv()):
            pass
        await ws.send(json.dumps({"clientContent": {"turns": ctx["history"], "turnComplete": True}}))
        stats["gemini_setup_s"] = round(time.monotonic() - t0, 2)
        for q in QUESTIONS:
            print(f"Q: {q}")
            print(f"A: {await ask(ws, session, q, stats)}")
    written = session.end()
    rows = [m for m in session.db.get_messages_as_conversation(session.session_id)
            if (m.get("display_metadata") or {}).get("call_id") == call_id]  # fmt: skip
    print(f"written {written}, found {len(rows)} rows for {call_id}: {[m['role'] for m in rows]}")
    print("stats", json.dumps(stats))
    service.pool.shutdown(wait=False, cancel_futures=True)
    return 0 if rows and written == len(rows) else 1


if __name__ == "__main__":
    logging.basicConfig(level=logging.WARNING)
    raise SystemExit(asyncio.run(main(Path(sys.argv[1]))))
