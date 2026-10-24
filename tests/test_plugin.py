from __future__ import annotations

import json

import pytest
from conftest import FakeCtx, FakeSidecar


@pytest.fixture
def no_sleep(monkeypatch, plugin):
    monkeypatch.setattr(plugin.tools.time, "sleep", lambda _s: None)


def _setup(plugin, sidecar, **settings):
    (plugin.sidecar_client.data_dir() / "sidecar.token").write_text(sidecar.token)
    ctx = FakeCtx(owner_number="+15551234567", listen=sidecar.listen, ring_timeout_seconds=1, **settings)
    plugin.register(ctx)
    return ctx


def _call(ctx, name, args):
    return json.loads(ctx.tools[name]["handler"](args))


def test_registers_declared_tools_and_cli(plugin):
    ctx = FakeCtx()
    plugin.register(ctx)
    assert set(ctx.tools) == {"whatsapp_call_owner", "whatsapp_call_status"}
    assert "whatsapp-voice" in ctx.cli
    assert "setup" in ctx.skills
    # Unconfigured: tools stay hidden from the model.
    assert ctx.tools["whatsapp_call_owner"]["check_fn"]() is False
    # The call tool can only ever reach the owner.
    assert set(ctx.tools["whatsapp_call_owner"]["schema"]["parameters"]["properties"]) == {"reason", "details"}


def test_answered_call(plugin, no_sleep):
    sidecar = FakeSidecar(["ringing", "ringing", "in_progress"])
    try:
        ctx = _setup(plugin, sidecar)
        assert ctx.tools["whatsapp_call_owner"]["check_fn"]() is True
        out = _call(ctx, "whatsapp_call_owner", {"reason": "Rent is due today", "details": "  $1200 "})
        assert out["status"] == "in_progress"
        assert "answered" in out["note"]
        assert sidecar.requests[0] == ("POST", "/calls", {"reason": "Rent is due today", "details": "$1200"})
    finally:
        sidecar.close()


@pytest.mark.parametrize("status", ["no_answer", "rejected", "failed"])
def test_unanswered_call_suggests_fallback(plugin, no_sleep, status):
    sidecar = FakeSidecar(["ringing", status])
    try:
        out = _call(_setup(plugin, sidecar), "whatsapp_call_owner", {"reason": "x"})
        assert out["status"] == status
        assert "another channel" in out["note"]
    finally:
        sidecar.close()


def test_sidecar_down(plugin):
    sidecar = FakeSidecar(["ringing"])
    ctx = _setup(plugin, sidecar)
    sidecar.close()
    sidecar.server.server_close()
    out = _call(ctx, "whatsapp_call_owner", {"reason": "x"})
    assert "not reachable" in out["error"]


def test_reason_required(plugin):
    sidecar = FakeSidecar(["ringing"])
    try:
        assert "error" in _call(_setup(plugin, sidecar), "whatsapp_call_owner", {"reason": "  "})
    finally:
        sidecar.close()


def test_call_status(plugin):
    sidecar = FakeSidecar(["completed"])
    try:
        ctx = _setup(plugin, sidecar)
        assert _call(ctx, "whatsapp_call_status", {})["calls"][0]["id"] == "c1"
        assert _call(ctx, "whatsapp_call_status", {"call_id": "c1"})["call"]["status"] == "completed"
    finally:
        sidecar.close()


def test_bad_token_is_reported(plugin):
    sidecar = FakeSidecar(["completed"], token="right")
    try:
        ctx = _setup(plugin, sidecar)
        (plugin.sidecar_client.data_dir() / "sidecar.token").write_text("wrong")
        assert "401" in _call(ctx, "whatsapp_call_status", {})["error"]
    finally:
        sidecar.close()


def test_sidecar_env(plugin):
    ctx = FakeCtx(owner_number="+1 555 123 4567", answer_inbound=False, max_call_minutes=5)
    env = plugin.sidecar_client.sidecar_env(ctx)
    assert env["HWV_OWNER"] == "+1 555 123 4567"
    assert env["HWV_ANSWER_INBOUND"] == "false"
    assert env["HWV_SEND_SUMMARY"] == "true"
    assert env["HWV_MAX_DURATION"] == "5m"
    assert env["HWV_RING_TIMEOUT"] == "45s"
    assert env["HWV_LIVE_CONFIG"] == "{}"
    assert env["HWV_LANGUAGES"] == ""
    assert env["HWV_VOICE_PROVIDER"] == "grok"
    assert env["HWV_GROK_MODEL"] == "grok-voice-latest"
    assert env["HWV_GROK_VOICE"] == "ara"
    assert env["HWV_SILENCE_TIMEOUT"] == "180s"
    assert env["GEMINI_API_KEY"] == "gemini-test"
    assert env["HERMES_API_KEY"] == "hermes-test"
    # A token is minted once and reused.
    assert env["HWV_TOKEN"] == plugin.sidecar_client.sidecar_env(ctx)["HWV_TOKEN"]
    token_file = plugin.sidecar_client.data_dir() / "sidecar.token"
    assert oct(token_file.stat().st_mode & 0o777) == "0o600"


def test_gemini_key_follows_hermes_provider_order(plugin, monkeypatch):
    import conftest

    monkeypatch.setitem(conftest.SECRETS, "GOOGLE_API_KEY", "google-test")
    assert plugin.sidecar_client.gemini_key() == "google-test"
    monkeypatch.delitem(conftest.SECRETS, "GOOGLE_API_KEY")
    monkeypatch.delitem(conftest.SECRETS, "GEMINI_API_KEY")
    assert plugin.sidecar_client.gemini_key() == ""


def test_download_rejects_binary_not_matching_pinned_checksum(plugin, monkeypatch, tmp_path):
    class FakeResponse:
        def __enter__(self):
            return self

        def __exit__(self, *exc):
            return False

        def read(self):
            return b"not the real binary"

    monkeypatch.setattr(plugin.sidecar_client, "_release_asset", lambda: "hermes-wa-voice_linux_arm64")
    monkeypatch.setattr(plugin.sidecar_client.urllib.request, "urlopen", lambda *a, **k: FakeResponse())
    dest = tmp_path / "hermes-wa-voice"
    with pytest.raises(SystemExit, match="Checksum mismatch"):
        plugin.cli._download_release(dest)
    assert not dest.exists()


def test_settings_match_manifest_and_hermes_key_rules(plugin):
    import yaml

    manifest = yaml.safe_load((plugin.cli.PLUGIN_DIR / "plugin.yaml").read_text())
    assert set(manifest["config_schema"]) == set(plugin.sidecar_client.SETTINGS)
    for key, spec in manifest["config_schema"].items():
        assert spec.get("default") == plugin.sidecar_client.SETTINGS[key] or key == "owner_number"
    plugin.sidecar_client.settings(FakeCtx())  # every key must be readable through Hermes


def test_dashboard_settings_reject_unknown_keys_and_wrong_types(plugin, monkeypatch):
    import importlib.util
    import subprocess

    calls = []

    def fake_run(argv, **kwargs):
        calls.append(argv[-2:])
        return subprocess.CompletedProcess(argv, 0, "", "")

    spec = importlib.util.spec_from_file_location(
        "hwv_dashboard_api", plugin.cli.PLUGIN_DIR / "dashboard" / "plugin_api.py"
    )
    api = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(api)
    monkeypatch.setattr(api.subprocess, "run", fake_run)

    from fastapi import HTTPException

    for bad in (
        {"not_a_setting": 1},
        {"ring_timeout_seconds": "45"},
        {"answer_inbound": "yes"},
        {"ring_timeout_seconds": True},
        {"max_call_minutes": 0},
    ):
        with pytest.raises(HTTPException):
            api.save_settings(bad)
    assert calls == [], "nothing is written when any value is rejected"

    api.save_settings({"voice": "Puck", "ring_timeout_seconds": 30, "answer_inbound": False, "live_config": {"a": {}}})
    prefix = "plugins.entries.hermes-whatsapp-voice.settings."
    assert calls == [
        [prefix + "voice", "Puck"],
        [prefix + "ring_timeout_seconds", "30"],
        [prefix + "answer_inbound", "false"],
        [prefix + "live_config", '{"a": {}}'],
    ]
