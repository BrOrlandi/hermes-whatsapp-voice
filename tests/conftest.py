"""Load the plugin package the way Hermes does (from its directory), with Hermes modules stubbed."""

from __future__ import annotations

import importlib.util
import json
import re
import sys
import threading
import types
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest
import yaml

ROOT = Path(__file__).resolve().parent.parent
RESERVED_SETTING_ROOTS = {"model", "plugins", "security", "settings"}
SECRETS = {"GEMINI_API_KEY": "gemini-test", "API_SERVER_KEY": "hermes-test"}


def _stub_hermes_modules(data_root: Path) -> None:
    secret_scope = types.ModuleType("agent.secret_scope")
    secret_scope.get_secret = lambda name, default=None: SECRETS.get(name, default)
    storage = types.ModuleType("plugins.plugin_storage")

    def plugin_data_dir(name: str) -> Path:
        path = data_root / name
        path.mkdir(parents=True, exist_ok=True)
        return path

    storage.plugin_data_dir = plugin_data_dir
    hermes_yaml = types.ModuleType("hermes_yaml")
    hermes_yaml.safe_load = yaml.safe_load
    sys.modules.update(
        {
            "agent": types.ModuleType("agent"),
            "agent.secret_scope": secret_scope,
            "plugins": types.ModuleType("plugins"),
            "plugins.plugin_storage": storage,
            "hermes_yaml": hermes_yaml,
        }
    )


@pytest.fixture
def plugin(tmp_path):
    _stub_hermes_modules(tmp_path)
    spec = importlib.util.spec_from_file_location(
        "hermes_whatsapp_voice", ROOT / "__init__.py", submodule_search_locations=[str(ROOT)]
    )
    module = importlib.util.module_from_spec(spec)
    sys.modules["hermes_whatsapp_voice"] = module
    spec.loader.exec_module(module)
    for sub in ("sidecar_client", "tools", "cli"):
        importlib.import_module(f"hermes_whatsapp_voice.{sub}")
    yield module
    for name in [n for n in sys.modules if n.startswith("hermes_whatsapp_voice")]:
        del sys.modules[name]


class FakeCtx:
    def __init__(self, **settings):
        self.settings = settings
        self.tools = {}
        self.cli = {}
        self.hooks = {}

    def get_config(self, key, default=None):
        # Mirrors Hermes's plugin-relative key rules (hermes_cli/plugins_state.py).
        if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_-]{0,127}", key) or key.lower() in RESERVED_SETTING_ROOTS:
            raise ValueError(f"Hermes rejects plugin setting key {key!r}")
        return self.settings.get(key, default)

    def register_tool(self, name, toolset, schema, handler, **kwargs):
        self.tools[name] = {"schema": schema, "handler": handler, **kwargs}

    def register_cli_command(self, name, **kwargs):
        self.cli[name] = kwargs

    def register_hook(self, name, callback):
        self.hooks[name] = callback

    def register_skill(self, name, path):
        assert path.exists(), path
        self.skills = {**getattr(self, "skills", {}), name: path}


class FakeSidecar:
    """Serves the sidecar API; `statuses` is the sequence a placed call walks through."""

    def __init__(self, statuses, token="tok"):
        self.statuses = list(statuses)
        self.token = token
        self.requests = []
        self.active_call = False  # whether /calls/active/text takes texts
        fake = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def _send(self, code, body):
                data = json.dumps(body).encode()
                self.send_response(code)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def _authorized(self):
                if self.headers.get("Authorization") != f"Bearer {fake.token}":
                    self._send(401, {"error": "bad or missing token"})
                    return False
                return True

            def do_POST(self):
                if not self._authorized():
                    return
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                fake.requests.append(("POST", self.path, body))
                if self.path == "/calls/active/text":
                    return self._send(200, {"delivered": True}) if fake.active_call else self._send(404, {})
                self._send(202, {"id": "c1", "status": "dialing"})

            def do_GET(self):
                if not self._authorized():
                    return
                fake.requests.append(("GET", self.path, None))
                if self.path.startswith("/calls/c1"):
                    status = fake.statuses.pop(0) if len(fake.statuses) > 1 else fake.statuses[0]
                    record = {"id": "c1", "status": status, "transcript": [{"role": "owner", "text": "ok"}]}
                    if status == "failed":
                        record["end_reason"] = "boom"
                    self._send(200, record)
                elif self.path.startswith("/calls"):
                    self._send(200, {"calls": [{"id": "c1", "status": "completed"}]})
                else:
                    self._send(200, {"ok": True})

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.listen = f"127.0.0.1:{self.server.server_address[1]}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()
