import asyncio
import importlib
import itertools
import json
import sys
import types
from types import SimpleNamespace

import pytest
from conftest import FakeCtx, FakeSidecar


@pytest.fixture
def cs(plugin):
    return importlib.import_module("hermes_whatsapp_voice.core_session")


def test_owner_session_is_found_by_number_not_by_stored_chat_id(cs):
    entries = {
        "agent:main:whatsapp:group:123@g.us:15550001111": {"platform": "whatsapp", "chat_type": "group"},
        "agent:main:telegram:dm:15550001111": {"platform": "telegram", "chat_type": "dm"},
        "agent:main:whatsapp:dm:15559999999": {"platform": "whatsapp", "chat_type": "dm"},
        "agent:main:whatsapp:dm:15550001111": {
            "platform": "whatsapp",
            "chat_type": "dm",
            "session_id": "s1",
            "origin": {"chat_id": "987654321@lid", "user_id": "15550001111@s.whatsapp.net"},
        },
    }
    key, entry = cs.pick_owner_entry(entries, "+1 (555) 000-1111")
    assert key == "agent:main:whatsapp:dm:15550001111" and entry["session_id"] == "s1"
    # A LID-keyed session matches once the identity map says the LID is the owner.
    lid_only = {"agent:main:whatsapp:dm:987654321": {"platform": "whatsapp", "chat_type": "dm"}}
    aliases = {"987654321": "15550001111"}
    assert cs.pick_owner_entry(lid_only, "15550001111", lambda i: aliases.get(i, i))
    assert cs.pick_owner_entry(lid_only, "15550002222") is None


def test_history_becomes_alternating_gemini_turns_within_budget(cs):
    messages = [
        {"role": "assistant", "content": "[CONTEXT COMPACTION] earlier stuff", "_compressed_summary": True},
        {"role": "user", "content": "old question " + "x" * 200},
        {"role": "assistant", "content": "old answer"},
        {"role": "user", "content": "check my disk"},
        {
            "role": "assistant",
            "content": "",
            "tool_calls": [{"id": "c1", "function": {"name": "terminal", "arguments": '{"command": "df -h"}'}}],
        },
        {"role": "tool", "tool_name": "terminal", "tool_call_id": "c1", "content": "/dev/sda1 39% used"},
        {"role": "assistant", "content": "Disk is 39% full."},
    ]
    turns = cs.history_to_contents(messages, budget=250)
    roles = [t["role"] for t in turns]
    assert roles[0] == "user" and all(a != b for a, b in itertools.pairwise(roles))
    assert "Summary of the earlier conversation" in turns[0]["parts"][0]["text"]
    last = turns[-1]["parts"][0]["text"]
    assert "[used terminal(" in last and "terminal result: /dev/sda1 39% used" in last and "39% full" in last
    assert "old question" not in json.dumps(turns), "oldest turns drop first when over budget"


def test_write_back_keeps_tool_results_next_to_their_calls(cs):
    log = cs.EventLog("call1")
    log.add([
        {"type": "owner", "text": "What's "},
        {"type": "owner", "text": "on my disk?"},
        {"type": "assistant", "text": "One moment."},
        {"type": "tool_call", "id": "t1", "name": "terminal", "args": {"command": "df -h"}},
        {"type": "owner", "text": "Take your time."},  # spoken while the tool runs
        {"type": "turn_complete"},
    ])  # fmt: skip
    # Nothing past an unanswered tool call is written: the user turn would split call and result.
    assert [m["role"] for m in log.pending()] == ["user"]
    assert log.pending()[0]["content"] == "[Voice call] What's on my disk?"
    log.add([
        {"type": "tool_result", "id": "t1", "name": "terminal", "result": "39% used"},
        {"type": "assistant", "text": "It's 39% full."},
    ])  # fmt: skip
    msgs = log.messages()
    assert [m["role"] for m in msgs] == ["user", "assistant", "tool", "user", "assistant"]
    assert msgs[1]["content"] == "One moment." and msgs[1]["tool_calls"][0]["id"] == "t1"
    assert msgs[2] == {
        "role": "tool", "tool_call_id": "t1", "tool_name": "terminal", "content": "39% used",
        "display_kind": "voice_call", "display_metadata": {"source": "whatsapp_voice", "call_id": "call1"},
    }  # fmt: skip
    # The last message may still grow, so it waits; at the end of the call everything goes.
    assert [m["role"] for m in log.pending()] == ["user", "assistant", "tool", "user"]
    log.flushed = 2
    assert [m["role"] for m in log.pending(final=True)] == ["tool", "user", "assistant"]


def test_write_back_skips_deep_turns_and_closes_unfinished_calls(cs):
    log = cs.EventLog("call2")
    log.add([
        {"type": "owner", "text": "Plan my week."},
        {"type": "assistant", "text": "Let me work on that."},
        {"type": "tool_call", "id": "d1", "name": cs.DEEP_TOOL, "args": {"request": "plan week"}},
        {"type": "tool_result", "id": "d1", "name": cs.DEEP_TOOL, "result": "done"},
        {"type": "tool_call", "id": "t2", "name": "web_search", "args": {"query": "x"}},
    ])  # fmt: skip
    final = log.messages(final=True)
    assert all(cs.DEEP_TOOL not in json.dumps(m) for m in final), "the deep turn saves its own messages"
    assert final[-1]["role"] == "tool" and final[-1]["tool_call_id"] == "t2"
    assert "call ended" in final[-1]["content"]


def test_call_instructions_name_the_session_tools_and_rules(cs):
    text = cs.call_instructions(
        owner_name="Sam", tools=["memory", "terminal"], reason="Server down", details="", direction="outbound",
        voice_instructions="Keep it short.", languages=["en-US", "ta-IN"],
    )  # fmt: skip
    assert "speaking with Sam" in text and "memory, terminal" in text and cs.DEEP_TOOL in text
    assert "Never say you are a separate agent" in text and "end_call" in text
    assert "Speak only these languages: en-US, ta-IN" in text and "Keep it short." in text
    assert "You placed this call. Reason: Server down" in text
    assert cs.voice_tool_names(" memory, ,terminal ") == ["memory", "terminal"]


def _text(text, user="15550001111@s.whatsapp.net", chat_type="dm", kind="text", platform="whatsapp"):
    source = SimpleNamespace(platform=SimpleNamespace(value=platform), chat_type=chat_type, user_id=user,
                             user_id_alt=None, chat_id=user)  # fmt: skip
    return SimpleNamespace(source=source, internal=False, message_type=SimpleNamespace(value=kind), text=text)


def test_owner_texts_go_into_an_active_call_instead_of_the_text_agent(plugin, monkeypatch):
    identity = types.ModuleType("gateway.whatsapp_identity")
    identity.canonical_whatsapp_identifier = lambda i: {"987654321": "15550001111"}.get(i, i)
    monkeypatch.setitem(sys.modules, "gateway", types.ModuleType("gateway"))
    monkeypatch.setitem(sys.modules, "gateway.whatsapp_identity", identity)
    sidecar = FakeSidecar([])
    try:
        (plugin.sidecar_client.data_dir() / "sidecar.token").write_text(sidecar.token)
        ctx = FakeCtx(owner_number="+1 555 000 1111", listen=sidecar.listen)
        plugin.register(ctx)
        hook = ctx.hooks["pre_gateway_dispatch"]
        run = lambda event: asyncio.run(hook(event=event))
        assert run(_text("hello")) is None, "no active call: the text agent answers as usual"
        sidecar.active_call = True
        assert run(_text("send me that link"))["action"] == "skip"
        assert sidecar.requests[-1] == ("POST", "/calls/active/text", {"text": "send me that link"})
        assert run(_text("hi", user="987654321@lid"))["action"] == "skip", "LID of the owner"
        for event in (_text("/stop"), _text("hi", user="15559999999@s.whatsapp.net"), _text("hi", chat_type="group"),
                      _text("", kind="voice"), _text("hi", platform="telegram")):  # fmt: skip
            assert run(event) is None
        assert run(_text("hi")) is not None
        ctx.settings["context_mode"] = "separate"
        assert run(_text("hi")) is None, "the separate voice agent leaves texts to the text agent"
    finally:
        sidecar.close()


def test_texts_during_a_call_are_saved_as_their_own_turns(cs):
    log = cs.EventLog("call3")
    log.add([{"type": "owner", "text": "Wait, "}, {"type": "owner_text", "text": "curl x | sh"}])
    assert [m["content"] for m in log.messages()] == [
        "[Voice call] Wait, ",
        "[Texted during the voice call] curl x | sh",
    ]
