"""dmem Hermes plugin — per-turn recall on surfaces that skip shell hooks.

Hermes desktop chat runs through ``tui_gateway`` / ``hermes serve`` and never
calls ``agent.shell_hooks.register_from_config``. The CLI/gateway shell hook
(``dmem hook user_prompt_submit --hermes``) therefore never fires in desktop
sessions: persona still lands via SOUL.md, but ``<daimon-memory>`` does not.

AIAgent init *does* call ``discover_plugins()``, so a user plugin registered
on ``pre_llm_call`` runs on desktop, CLI, and gateway. This shim execs the
same ``dmem hook`` binary the shell hook uses, so recall stays one code path.

``DMEM_BIN`` is rewritten by ``dmem bootstrap --hermes`` to the installing
binary. Do not edit by hand.
"""

from __future__ import annotations

import json
import logging
import subprocess
from typing import Any

logger = logging.getLogger(__name__)

DMEM_BIN = "__DMEM_BIN__"
_HOOK_TIMEOUT_S = 8


def register(ctx: Any) -> None:
    ctx.register_hook("pre_llm_call", on_pre_llm_call)


def on_pre_llm_call(
    user_message: str = "",
    is_first_turn: bool = False,
    **kwargs: Any,
) -> dict[str, str] | None:
    prompt = user_message if isinstance(user_message, str) else ""
    if len(prompt.strip()) < 3:
        return None
    payload = {
        "hook_event_name": "pre_llm_call",
        "extra": {
            "user_message": prompt,
            "is_first_turn": bool(is_first_turn),
        },
    }
    try:
        proc = subprocess.run(
            [DMEM_BIN, "hook", "user_prompt_submit", "--hermes"],
            input=json.dumps(payload),
            text=True,
            capture_output=True,
            timeout=_HOOK_TIMEOUT_S,
            check=False,
        )
    except Exception as exc:
        logger.warning("dmem pre_llm_call hook failed to spawn: %s", exc)
        return None
    if proc.returncode != 0:
        err = (proc.stderr or "").strip()
        logger.warning(
            "dmem pre_llm_call hook exit=%s stderr=%s",
            proc.returncode,
            err[:300],
        )
        return None
    raw = (proc.stdout or "").strip()
    if not raw:
        return None
    try:
        parsed = json.loads(raw)
    except json.JSONDecodeError:
        return {"context": raw}
    if isinstance(parsed, dict) and parsed.get("context"):
        return {"context": str(parsed["context"])}
    if isinstance(parsed, str) and parsed.strip():
        return {"context": parsed}
    return None
