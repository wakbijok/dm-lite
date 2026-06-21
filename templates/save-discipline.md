---
kind: protocol
namespace: agent/protocol
title: Memory Save Discipline
---

# Memory Save Discipline

Scope: governs WHAT the agent persists, WHICH kind and namespace, and WHEN. Capture is agent-curated and typed; there is no extraction model in the loop. The hooks enforce timing (a per-turn signal nudge, a cadence nudge after several quiet turns, and a session-end pass); they never decide content.

Rules:

1. Trigger to kind, call the named tool. A non-obvious choice -> `log_decision`. Something that failed, broke, or was reverted -> `log_incident`. A reusable lesson or a corrected mistake -> `log_lesson`. A dated follow-up -> `add_reminder`. A procedure worth repeating -> `log_runbook`. A standing rule scoped to ONE project or codebase (build/lint/file-layout) -> `log_convention`. A durable rule about how the AGENT works across all projects is NOT a convention; it is a protocol you evolve per Rule 8. Persona (who the user is, their voice and standing preferences) is user-curated; edit it only when the user states a preference, never unprompted. A user-stated operating preference becomes a protocol rule (Rule 8), not a new persona entry.

2. Recall before write (dedup). Recall the target namespace first; update or skip an existing record instead of making a near-duplicate.

3. Append vs update is the engine's choice, not yours; it is fixed per kind. Most kinds append. Protocols are the exception: re-saving under the same title supersedes the prior version (that is the protocol kind's update rule, see Rule 8). To retract a wrong save, use `forget`.

4. New writes only, curated not raw. Persist a distilled, self-contained record, never a transcript or a whole-session dump. One event, one record.

5. Approval-gated for sensitive saves. Never persist credentials or secrets; ask before saving anything the user has not agreed to share.

6. Right bucket, chosen by SUBJECT (what the record is really about), in priority order user > resources > agent -- but apply the lesson carve-out below BEFORE the resources tier. `user/<area>` for a fact, identity trait, or standing preference about WHO THE USER IS or how they want to be addressed; a user-stated rule about how the AGENT should OPERATE is not a user entry -> protocol per Rule 8. Else `resources/<project>/<area>` if the record is genuinely about a named project, codebase, or host (merely mentioning a project does not count). Else `agent/<area>` for the agent's own self and work. The engine appends `/<kind>/<id>` itself; never put the kind in the path. Lesson carve-out (your growth KB): if a lesson, technique, or gotcha would help in a project OTHER than the one you learned it in, it is agent-knowledge -> `agent/lessons`, so it surfaces everywhere; only a lesson useful solely inside one codebase goes under `resources/<project>`. When a lesson proves recurring or load-bearing, promote it into a standing rule per Rule 8.

7. The hooks back-stop you, they do not replace you. A nudge will remind you and name the exact tool, but saving in the moment beats the back-stop.

8. Evolve governance by updating, not fragmenting. A durable rule about how you WORK (including how you recall or prioritize memory sources) folds into Behavioral Discipline; a durable rule about WHAT, WHICH kind or namespace, or WHEN to persist folds into this doc. To fold one in, re-save the whole protocol with `remember kind=protocol namespace=agent/protocol` and the same title; the engine then supersedes the prior version (Rule 3). Bright line: a rule that travels with the AGENT across all projects is a protocol; a rule true only inside one repo is a `project_convention`. Persona is never a target here; it is user-curated (Rule 1). Create a new protocol record only for a genuinely new governance area, never for a single rule that already belongs in an existing protocol.
