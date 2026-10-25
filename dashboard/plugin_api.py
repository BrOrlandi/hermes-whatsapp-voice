"""Dashboard backend for the WhatsApp Voice tab, mounted at /api/plugins/hermes-whatsapp-voice/.

Runs inside the Hermes dashboard process, behind its auth. Settings are written through the public
`hermes config set` CLI, so the dashboard, the Desktop app and the CLI stay in sync.
"""

from __future__ import annotations

import importlib.util
import json
import os
import platform
import shlex
import shutil
import subprocess
import sys
from pathlib import Path

from fastapi import APIRouter, HTTPException
from hermes_yaml import safe_load

PLUGIN_DIR = Path(__file__).resolve().parent.parent
PLUGIN_ID = "hermes-whatsapp-voice"
SERVICE = "hermes-whatsapp-voice"


def _load_sidecar_client():
    # The dashboard loads this file on its own, outside the plugin package; reuse the plugin's module.
    spec = importlib.util.spec_from_file_location("hwv_sidecar_client", PLUGIN_DIR / "sidecar_client.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


sc = _load_sidecar_client()
router = APIRouter()
_pair_proc: subprocess.Popen | None = None


def _schema() -> dict:
    return safe_load((PLUGIN_DIR / "plugin.yaml").read_text())["config_schema"]


class _SettingsCtx:
    """The one ctx method sidecar_client needs, read from config.yaml like PluginContext does."""

    def get_config(self, key, default=None):
        from hermes_cli.config import load_config_readonly

        entry = ((load_config_readonly() or {}).get("plugins", {}).get("entries", {}) or {}).get(PLUGIN_ID) or {}
        return (entry.get("settings") or {}).get(key, default)


def _client():
    return sc.Client.from_ctx(_SettingsCtx())


@router.get("/status")
def status():
    settings = sc.settings(_SettingsCtx())
    out = {
        "owner_number": str(settings["owner_number"] or ""),
        "gemini_key": bool(sc.gemini_key()),
        "hermes_api_key": bool(_secret("API_SERVER_KEY")),
        "installed": sc.binary_path().exists(),
        "sidecar": None,
        "sidecar_error": None,
        "pairing": _pair_state(),
    }
    try:
        client = _client()
        out["sidecar"] = client.health()
        # The setup checklist ends with a test call; remember the first completed call, since the
        # sidecar's call history is lost on restart.
        flag = sc.data_dir() / "test-call-done"
        if not flag.exists() and any(c.get("status") == "completed" for c in client.recent_calls(20)):
            flag.touch()
    except sc.SidecarError as e:
        out["sidecar_error"] = str(e)
    out["tested"] = (sc.data_dir() / "test-call-done").exists()
    return out


def _secret(name: str) -> str:
    from agent.secret_scope import get_secret

    return get_secret(name) or ""


@router.get("/settings")
def get_settings():
    return {"schema": _schema(), "values": sc.settings(_SettingsCtx())}


@router.post("/settings")
def save_settings(body: dict):
    schema = _schema()
    types = {"str": str, "int": int, "bool": bool, "dict": dict}
    for key, value in body.items():
        spec = schema.get(key)
        if spec is None:
            raise HTTPException(400, f"unknown setting {key!r}")
        expected = types.get(spec.get("type", "str"), str)
        if expected is int and isinstance(value, bool) or not isinstance(value, expected):
            raise HTTPException(400, f"{key} must be {spec.get('type', 'str')}")
        if expected is int and value < 1:
            raise HTTPException(400, f"{spec.get('label', key)} must be 1 or more")
    for key, value in body.items():
        # The public `hermes config set` CLI (same interpreter, so PATH doesn't matter): it replaces
        # the value, parses JSON objects, ints and booleans, and keeps other text as a string.
        # shortcut: a str setting whose text looks like a number or boolean is stored as one; fine
        # for these settings, revisit if a free-text setting needs literal "true"/"123".
        text = (
            json.dumps(value)
            if isinstance(value, dict)
            else str(value).lower()
            if isinstance(value, bool)
            else str(value)
        )
        done = subprocess.run(
            [
                sys.executable,
                "-m",
                "hermes_cli.main",
                "config",
                "set",
                f"plugins.entries.{PLUGIN_ID}.settings.{key}",
                text,
            ],
            capture_output=True,
            text=True,
            timeout=60,
            check=False,
        )
        if done.returncode != 0:
            raise HTTPException(500, f"saving {key} failed: {(done.stderr or done.stdout).strip()[-300:]}")
    return {"saved": sorted(body), "restart_needed": True}


@router.post("/restart")
def restart():
    """Restart the sidecar service so new settings apply."""
    custom = os.environ.get("HWV_RESTART_COMMAND", "").strip()
    if custom:
        # Hosts that supervise the sidecar some other way (s6, supervisord, a container's
        # init) name their own restart command, e.g. `s6-svc -r /run/service/whatsapp-voice`.
        cmd = shlex.split(custom)
    elif platform.system() == "Linux" and shutil.which("systemctl"):
        cmd = ["systemctl", "--user", "restart", SERVICE]
    elif platform.system() == "Darwin":
        cmd = ["launchctl", "kickstart", "-k", f"gui/{os.getuid()}/com.hermes.whatsapp-voice"]
    else:
        raise HTTPException(501, "No service manager found; restart `hermes whatsapp-voice serve` yourself.")
    done = subprocess.run(cmd, capture_output=True, text=True, timeout=30, check=False)
    if done.returncode != 0:
        raise HTTPException(500, f"restart failed: {done.stderr.strip() or done.stdout.strip()}")
    return {"restarted": True}


def _pair_state() -> str:
    log = sc.data_dir() / "pair.log"
    text = log.read_text(errors="replace") if log.exists() else ""
    if "Linked." in text:
        return "linked"
    if _pair_proc is not None and _pair_proc.poll() is None:
        return "waiting"
    return "failed" if text else "idle"


@router.post("/pair/start")
def pair_start():
    global _pair_proc
    if _pair_proc is not None and _pair_proc.poll() is None:
        return {"pairing": "waiting"}
    try:
        health = _client().health()
    except sc.SidecarError:
        health = None
    if health and health.get("logged_in"):
        raise HTTPException(409, "Already linked. To re-link, unlink the device on the bot's phone first.")
    exe = sc.binary_path()
    if not exe.exists():
        try:
            sc.download_release(exe)
        except (sc.SidecarError, OSError) as e:
            raise HTTPException(502, f"Could not install the sidecar: {e}") from e
    data = sc.data_dir()
    (data / "pair-qr.svg").unlink(missing_ok=True)
    log = open(data / "pair.log", "w")  # noqa: SIM115 - handed to the child process
    _pair_proc = subprocess.Popen(
        [str(exe), "pair"],
        env={**os.environ, "HWV_DATA_DIR": str(data)},
        stdout=log,
        stderr=subprocess.STDOUT,
        stdin=subprocess.DEVNULL,
        start_new_session=True,
    )
    return {"pairing": "waiting"}


@router.post("/pair/stop")
def pair_stop():
    if _pair_proc is not None and _pair_proc.poll() is None:
        _pair_proc.terminate()
    return {"pairing": _pair_state()}


@router.get("/pair/qr")
def pair_qr():
    # JSON, not an image URL: dashboard auth is a header that <img src> cannot send.
    qr = sc.data_dir() / "pair-qr.svg"
    return {"pairing": _pair_state(), "svg": qr.read_text() if qr.exists() else None}


@router.post("/call")
def test_call(body: dict):
    reason = str(body.get("reason") or "").strip() or "This is a quick test call."
    try:
        return _client().start_call(reason)
    except sc.SidecarError as e:
        raise HTTPException(502, str(e)) from e


@router.get("/calls")
def calls(limit: int = 10):
    try:
        return {"calls": _client().recent_calls(min(limit, 50))}
    except sc.SidecarError as e:
        raise HTTPException(502, str(e)) from e
