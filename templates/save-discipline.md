---
kind: protocol
namespace: agent/protocol
title: Memory Save Discipline
---

# Memory Save Discipline

Scope: governs WHAT the agent persists, WHICH kind and namespace, and WHEN. Capture is agent-curated and typed; there is no extraction model in the loop. The hooks enforce timing (a per-turn signal nudge, a cadence nudge after several quiet turns, and a session-end pass); they never decide content.

Rules:

1. Trigger to kind, call the named tool. A non-obvious choice -> `log_decision`. Something that failed, broke, or was reverted -> `log_incident`. A reusable lesson or a corrected mistake -> `log_lesson`. A dated follow-up -> `add_reminder`. A procedure worth repeating -> `log_runbook`. A standing rule -> `log_convention`. Persona and protocol are the system layer; they are loaded with `dmem import`, not written per turn.

2. Recall before write (dedup). Recall the target namespace first; update or skip an existing record instead of making a near-duplicate.

3. Append vs update is not your choice. The engine fixes it per kind. To retract a wrong save, use `forget`.

4. New writes only, curated not raw. Persist a distilled, self-contained record, never a transcript or a whole-session dump. One event, one record.

5. Approval-gated for sensitive saves. Never persist credentials or secrets; ask before saving anything the user has not agreed to share.

6. Right bucket, chosen by SUBJECT in priority order user > resources > agent. Put it in `user/<area>` if it is a fact, preference, or boundary about the user. Else `resources/<project>/<area>` if it is about a named project, codebase, or host. Else `agent/<area>` for the agent's own self and work. The engine appends `/<kind>/<id>` itself; never put the kind in the path.

7. The hooks back-stop you, they do not replace you. A nudge will remind you and name the exact tool, but saving in the moment beats the back-stop.
