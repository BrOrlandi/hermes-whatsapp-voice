"""Hermes WhatsApp voice calls: live calls with Hermes through a linked-device sidecar."""

from __future__ import annotations

from pathlib import Path


def register(ctx):
    # Imported here so the package root has no import-time side effects.
    from . import cli, tools
    from .sidecar_client import data_dir, settings

    def available() -> bool:
        # Cheap and passive: configured, and the sidecar has been started at least once.
        try:
            return bool(settings(ctx)["owner_number"]) and (data_dir() / "sidecar.token").exists()
        except Exception:  # noqa: BLE001 - availability checks must never raise
            return False

    call_owner, call_status = tools.make_handlers(ctx)
    ctx.register_tool(
        name="whatsapp_call_owner",
        toolset="whatsapp_voice",
        schema=tools.CALL_OWNER,
        handler=call_owner,
        check_fn=available,
        emoji="📞",
    )
    ctx.register_tool(
        name="whatsapp_call_status",
        toolset="whatsapp_voice",
        schema=tools.CALL_STATUS,
        handler=call_status,
        check_fn=available,
        emoji="📞",
    )

    async def route_texts_into_call(event=None, **_kwargs):
        # During a core-session call the owner's texts go to the call, not to a second assistant.
        import asyncio

        from .core_session import text_into_call

        try:
            return await asyncio.to_thread(text_into_call, ctx, event)
        except Exception:  # noqa: BLE001 - a hook must never stop the gateway's dispatch
            return None

    ctx.register_hook("pre_gateway_dispatch", route_texts_into_call)
    # The setup runbook, loadable by the owner's agent as hermes-whatsapp-voice:setup.
    ctx.register_skill("setup", Path(__file__).parent / "skills" / "setup" / "SKILL.md")
    ctx.register_cli_command(
        name="whatsapp-voice",
        help="WhatsApp voice calls: install, pair and run the sidecar",
        setup_fn=cli.setup_parser,
        handler_fn=cli.make_handler(ctx),
    )
