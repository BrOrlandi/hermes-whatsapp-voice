"""Talking to the sidecar process: where its files live, its environment, its HTTP API."""

from __future__ import annotations

import hashlib
import json
import os
import platform
import secrets
import shutil
import subprocess
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any

PLUGIN_NAME = "hermes-whatsapp-voice"
BINARY_NAME = "hermes-wa-voice"
REPO = "ashwanthbalakrishnan5/hermes-whatsapp-voice"
PLUGIN_DIR = Path(__file__).resolve().parent

# Settings forwarded to the sidecar, with the defaults from plugin.yaml.
SETTINGS: dict[str, Any] = {
    "owner_number": "",
    "voice_model": "gemini-3.8-live",
    "voice": "Kore",
    "voice_provider": "grok",
    "grok_model": "grok-voice-latest",
    "grok_voice": "ara",
    "openai_model": "gpt-realtime-2.1",
    "openai_voice": "cedar",
    "assistant_name": "Hermes",
    "languages": "",
    "voice_instructions": "",
    "live_config": {},
    "listen": "127.0.0.1:8796",
    "hermes_api_url": "http://127.0.0.1:8642",
    "ring_timeout_seconds": 45,
    "max_call_minutes": 15,
    "silence_hangup_seconds": 180,
    "answer_inbound": True,
    "send_summary": True,
}


class SidecarError(Exception):
    pass


def data_dir() -> Path:
    from plugins.plugin_storage import plugin_data_dir

    path = plugin_data_dir(PLUGIN_NAME)
    path.chmod(0o700)  # holds the WhatsApp session and the sidecar token
    return path


def binary_path() -> Path:
    return data_dir() / "bin" / BINARY_NAME


def token(create: bool = False) -> str:
    """Shared secret between plugin and sidecar, kept in the plugin's data dir."""
    path = data_dir() / "sidecar.token"
    if path.exists():
        return path.read_text().strip()
    if not create:
        raise SidecarError("sidecar has never been started (no token); run: hermes whatsapp-voice serve")
    value = secrets.token_urlsafe(32)
    with os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w") as f:
        f.write(value)
    return value


def _version() -> str:
    for line in (PLUGIN_DIR / "plugin.yaml").read_text().splitlines():
        if line.startswith("version:"):
            return line.split(":", 1)[1].strip()
    raise SidecarError("version missing from plugin.yaml")


def _release_asset() -> str:
    goos = {"Linux": "linux", "Darwin": "darwin"}.get(platform.system())
    goarch = {"x86_64": "amd64", "amd64": "amd64", "arm64": "arm64", "aarch64": "arm64"}.get(platform.machine().lower())
    if not goos or not goarch:
        raise SidecarError(
            f"No prebuilt sidecar for {platform.system()}/{platform.machine()}; build it with --from-source."
        )
    return f"{BINARY_NAME}_{goos}_{goarch}"


def _gh_logged_in() -> bool:
    return (
        bool(shutil.which("gh"))
        and subprocess.run(["gh", "auth", "status"], capture_output=True, check=False).returncode == 0
    )


def download_release(dest: Path) -> None:
    """Fetch the prebuilt sidecar for plugin.yaml's version and verify it before installing."""
    tag = f"v{_version()}"
    asset = _release_asset()
    # Pinned in this repo for the release matching plugin.yaml's version, so the code a reviewer
    # read decides which binary is acceptable.
    sums_file = PLUGIN_DIR / "sidecar" / "release-checksums.txt"
    sums = dict(reversed(line.split()) for line in sums_file.read_text().splitlines() if line.strip())
    if asset not in sums:
        raise SidecarError(f"No pinned checksum for {asset}; build it with --from-source.")
    print(f"Downloading {asset} from the {tag} release of {REPO} ...")
    with urllib.request.urlopen(f"https://github.com/{REPO}/releases/download/{tag}/{asset}", timeout=120) as resp:
        blob = resp.read()
    if hashlib.sha256(blob).hexdigest() != sums[asset]:
        raise SidecarError(f"Checksum mismatch for {asset}; refusing to install.")
    dest.parent.mkdir(parents=True, exist_ok=True)
    tmp = dest.with_suffix(".download")
    tmp.write_bytes(blob)
    tmp.chmod(0o755)
    if _gh_logged_in():
        # Ties the binary to this repo's release workflow (GitHub build attestation).
        check = subprocess.run(
            ["gh", "attestation", "verify", str(tmp), "--repo", REPO], capture_output=True, text=True, check=False
        )
        if check.returncode != 0:
            tmp.unlink()
            raise SidecarError(f"Build attestation check failed:\n{check.stderr}")
        print("Build attestation verified.")
    else:
        print(
            f"Checksum verified. With a logged-in GitHub CLI, the build attestation is checked too (gh attestation verify --repo {REPO})."
        )
    tmp.replace(dest)


def settings(ctx) -> dict[str, Any]:
    return {key: ctx.get_config(key, default=default) for key, default in SETTINGS.items()}


def gemini_key() -> str:
    """The Gemini key Hermes's own Gemini provider uses, in the same order (GOOGLE_API_KEY first)."""
    from agent.secret_scope import get_secret

    return get_secret("GOOGLE_API_KEY") or get_secret("GEMINI_API_KEY") or ""


def sidecar_env(ctx) -> dict[str, str]:
    """Environment for the sidecar. Secrets are resolved here, through Hermes."""
    from agent.secret_scope import get_secret

    s = settings(ctx)
    env = {
        "HWV_OWNER": str(s["owner_number"]),
        "HWV_MODEL": str(s["voice_model"]),
        "HWV_VOICE": str(s["voice"]),
        "HWV_VOICE_PROVIDER": str(s["voice_provider"]),
        "HWV_GROK_MODEL": str(s["grok_model"]),
        "HWV_GROK_VOICE": str(s["grok_voice"]),
        "HWV_OPENAI_MODEL": str(s["openai_model"]),
        "HWV_OPENAI_VOICE": str(s["openai_voice"]),
        "HWV_ASSISTANT_NAME": str(s["assistant_name"]),
        "HWV_LANGUAGES": str(s["languages"]),
        "HWV_VOICE_INSTRUCTIONS": str(s["voice_instructions"]),
        "HWV_LIVE_CONFIG": json.dumps(s["live_config"] or {}),
        "HWV_LISTEN": str(s["listen"]),
        "HWV_RING_TIMEOUT": f"{int(s['ring_timeout_seconds'])}s",
        "HWV_MAX_DURATION": f"{int(s['max_call_minutes'])}m",
        "HWV_SILENCE_TIMEOUT": f"{int(s['silence_hangup_seconds'])}s",
        "HWV_ANSWER_INBOUND": "true" if s["answer_inbound"] else "false",
        "HWV_SEND_SUMMARY": "true" if s["send_summary"] else "false",
        "HWV_DATA_DIR": str(data_dir()),
        "HWV_TOKEN": token(create=True),
        "HERMES_API_URL": str(s["hermes_api_url"]),
        "GEMINI_API_KEY": gemini_key(),
        "HERMES_API_KEY": get_secret("API_SERVER_KEY") or "",
    }
    return env


class Client:
    def __init__(self, listen: str, auth_token: str, timeout: float = 10.0):
        self.base = f"http://{listen}"
        self.auth_token = auth_token
        self.timeout = timeout

    @classmethod
    def from_ctx(cls, ctx) -> Client:
        return cls(settings(ctx)["listen"], token())

    def _request(self, method: str, path: str, body: dict | None = None) -> dict:
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(self.base + path, data=data, method=method)
        req.add_header("Authorization", f"Bearer {self.auth_token}")
        if data is not None:
            req.add_header("Content-Type", "application/json")
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as resp:
                return json.loads(resp.read() or b"{}")
        except urllib.error.HTTPError as e:
            try:
                detail = json.loads(e.read()).get("error", e.reason)
            except (ValueError, AttributeError):
                detail = e.reason
            raise SidecarError(f"sidecar returned {e.code}: {detail}") from e
        except (urllib.error.URLError, OSError) as e:
            raise SidecarError(
                f"sidecar not reachable at {self.base} ({e}); is `hermes whatsapp-voice serve` running?"
            ) from e

    def health(self) -> dict:
        return self._request("GET", "/health")

    def start_call(self, reason: str, details: str = "") -> dict:
        return self._request("POST", "/calls", {"reason": reason, "details": details})

    def get_call(self, call_id: str) -> dict:
        return self._request("GET", f"/calls/{urllib.parse.quote(call_id, safe='')}")

    def recent_calls(self, limit: int = 5) -> list[dict]:
        return self._request("GET", f"/calls?limit={limit}").get("calls", [])
