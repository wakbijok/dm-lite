#!/usr/bin/env bash
# Stub stand-in for the `ma8e` binary (binary #2), so the dm-lite<->ma8e seam can
# run before ma8e compiles locally. Implements just remember/recall against a JSONL
# store. Swap MA8E_BIN for the real `ma8e` binary later and nothing in dm-lite changes.
set -euo pipefail
STORE="${MA8E_STUB_STORE:-/tmp/ma8e-stub.jsonl}"
cmd="${1:-}"; shift || true
case "$cmd" in
  remember)
    text="${1:-}"; shift || true
    dedup=""
    while [ $# -gt 0 ]; do case "$1" in --dedup-key) dedup="${2:-}"; shift 2;; *) shift || true;; esac; done
    python3 -c "import json,sys;print(json.dumps({'text':sys.argv[1],'dedup_key':sys.argv[2]}))" "$text" "$dedup" >> "$STORE"
    echo "ma8e(stub): stored entry [$dedup]" >&2
    ;;
  recall)
    query="$*"
    [ -f "$STORE" ] || exit 0
    MA8E_STUB_Q="$query" python3 - "$STORE" <<'PY'
import json,os,sys
store=sys.argv[1]; q=os.environ.get("MA8E_STUB_Q","").lower().split()
for line in open(store):
    try: e=json.loads(line)
    except Exception: continue
    if not q or any(w in e["text"].lower() for w in q):
        print(e["text"]); print("----")
PY
    ;;
  *) echo "stub ma8e: unknown cmd '$cmd'" >&2; exit 2;;
esac
