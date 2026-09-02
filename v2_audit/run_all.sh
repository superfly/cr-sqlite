#!/usr/bin/env bash
# Run every v2 audit reproduction and print a pass/fail table.
#
#   BUG      - the defect reproduced (script exit 1)
#   ok       - behaviour was correct (script exit 0)
#
# Regression scripts (e2e_01/02/04/06, design_02) are expected to be `ok`.
# Everything else is expected to be `BUG` until fixed; a finding flipping to
# `ok` means it is fixed.

set -uo pipefail
cd "$(dirname "$0")/.."

PY=${PY:-/opt/homebrew/bin/python3}
if ! "$PY" -c 'import sqlite3,sys; sys.exit(0 if hasattr(sqlite3.Connection,"enable_load_extension") else 1)' 2>/dev/null; then
  echo "error: $PY cannot load sqlite extensions. Set PY= to a python built with"
  echo "       enable_load_extension (on macOS: /opt/homebrew/bin/python3)." >&2
  exit 2
fi

if [ ! -f core/dist/crsqlite.dylib ] && [ ! -f core/dist/crsqlite.so ]; then
  echo "error: no built extension at core/dist/. Run: (cd core && make loadable)" >&2
  exit 2
fi

bugs=0; oks=0; errs=0
printf '%-52s %s\n' "SCRIPT" "RESULT"
printf '%-52s %s\n' "----------------------------------------------------" "------"
for f in v2_audit/repros/*.py; do
  name=$(basename "$f" .py)
  out=$(timeout 900 "$PY" "$f" 2>&1); rc=$?
  case $rc in
    1) printf '%-52s %s\n' "$name" "BUG";  bugs=$((bugs+1)) ;;
    0) printf '%-52s %s\n' "$name" "ok";   oks=$((oks+1)) ;;
    *) printf '%-52s %s\n' "$name" "ERROR(rc=$rc)"; errs=$((errs+1))
       echo "$out" | tail -5 | sed 's/^/      /' ;;
  esac
done
echo
echo "reproduced: $bugs   correct: $oks   harness errors: $errs"
[ "$errs" -eq 0 ] || exit 2
exit 0
