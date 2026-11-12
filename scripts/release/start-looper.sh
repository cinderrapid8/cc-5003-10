#!/usr/bin/env bash
#
# Launch the packaged looper-server against the offline soundbank resources.
#
# The script resolves every path relative to its own location, so the whole
# installation directory can be moved anywhere and still works.
#
# Modes:
#   start-looper.sh            Start the server in the foreground.
#   start-looper.sh --check    Do not serve long-term: run a real-task
#                              self-check (preflight + verify + audition via the
#                              API and the packaged runner) and exit non-zero
#                              if the offline resources are missing or broken.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALL_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
CONFIG="${INSTALL_ROOT}/config/runtime.conf"

fail() { echo "ERROR: $*" >&2; exit 1; }
[[ -f "${CONFIG}" ]] || fail "runtime config missing: ${CONFIG} (package is incomplete)"
# shellcheck source=/dev/null
source "${CONFIG}"

CLI="${INSTALL_ROOT}/bin/looper-cli"
SERVER="${INSTALL_ROOT}/bin/looper-server"
[[ -x "${CLI}" ]] || fail "runner missing: ${CLI}"
[[ -x "${SERVER}" ]] || fail "runner missing: ${SERVER}"

MANIFEST_PATH="${INSTALL_ROOT}/${MANIFEST}"
DEMO_ROOT="${INSTALL_ROOT}/${DEMO_DIR}"
FRONTEND_PATH="${INSTALL_ROOT}/${LOOPER_FRONTEND}"

# A web bundle without audio is exactly the "empty shell" we refuse to ship.
[[ -f "${FRONTEND_PATH}/index.html" ]] || fail "frontend bundle missing: ${FRONTEND_PATH}/index.html"
[[ -f "${MANIFEST_PATH}" ]] || fail "soundbank manifest missing: ${MANIFEST_PATH}"
mapfile -t RESOURCE_FILES < <(python3 - "$MANIFEST_PATH" <<'PY'
import json, sys
m = json.load(open(sys.argv[1]))
for a in m["assets"]:
    for v in a["variants"]:
        for r in v["resources"]:
            print(r["file"])
PY
)
[[ "${#RESOURCE_FILES[@]}" -gt 0 ]] || fail "manifest lists no audio resources"
for f in "${RESOURCE_FILES[@]}"; do
  [[ -f "${DEMO_ROOT}/${DEMO_OUT_DIR}/${f}" ]] \
    || fail "resource listed in manifest is missing: ${f}"
done

run_real_task_check() {
  echo "[check] preflight over packaged project"
  "${CLI}" preflight "${DEMO_ROOT}/${DEMO_PROJECT}" "${DEMO_ROOT}" >/dev/null

  echo "[check] re-decoding every packaged resource (real decoder path)"
  "${CLI}" verify "${MANIFEST_PATH}" "${DEMO_ROOT}/${DEMO_OUT_DIR}" >/dev/null

  echo "[check] rendering real audition loops from the decoded resources"
  local tmp_aud
  tmp_aud="$(mktemp -d)"
  trap 'rm -rf -- "${tmp_aud}"' RETURN
  "${CLI}" audition "${MANIFEST_PATH}" "${DEMO_ROOT}/${DEMO_OUT_DIR}" \
    "${tmp_aud}" "${AUDITION_ROUNDS}" >/dev/null
  local got want
  got="$(find "${tmp_aud}" -name '*.wav' | wc -l | tr -d ' ')"
  want="${#RESOURCE_FILES[@]}"
  [[ "${got}" -eq "${want}" ]] || fail "audition produced ${got} renders, expected ${want}"

  echo "[check] starting server and exercising a real API task"
  local port="${LOOPER_PORT}"
  LOOPER_PORT="${port}" LOOPER_FRONTEND="${FRONTEND_PATH}" \
    "${SERVER}" >/tmp/looper-check.$$.log 2>&1 &
  local srv_pid=$!
  trap 'kill "${srv_pid}" 2>/dev/null || true; rm -rf -- "${tmp_aud}"' RETURN
  local ready=0 i
  for i in $(seq 1 50); do
    if curl -fsS "http://127.0.0.1:${port}/api/manifest?path=$(python3 -c "import urllib.parse,sys;print(urllib.parse.quote(sys.argv[1]))" "${MANIFEST_PATH}")" \
         >/dev/null 2>&1; then ready=1; break; fi
    kill -0 "${srv_pid}" 2>/dev/null || fail "server exited early; see /tmp/looper-check.$$.log"
    sleep 0.2
  done
  [[ "${ready}" -eq 1 ]] || fail "server did not become ready on port ${port}"

  # Real task through the running server: read manifest and one resource file.
  local manifest_resp
  manifest_resp="$(curl -fsS "http://127.0.0.1:${port}/api/manifest?path=$(python3 -c "import urllib.parse,sys;print(urllib.parse.quote(sys.argv[1]))" "${MANIFEST_PATH}")")"
  echo "${manifest_resp}" | python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["ok"] and d["manifest"]["assets"], "empty manifest"; print("[check] API served manifest with", len(d["manifest"]["assets"]), "asset(s)")'
  local first_file
  first_file="${RESOURCE_FILES[0]}"
  curl -fsS -o /dev/null "http://127.0.0.1:${port}/files/${DEMO_DIR}/${DEMO_OUT_DIR}/${first_file}"
  echo "[check] API served real resource file ${first_file}"
  kill "${srv_pid}" 2>/dev/null || true
  wait "${srv_pid}" 2>/dev/null || true
  trap 'rm -rf -- "${tmp_aud}"' RETURN

  echo "[check] manifest summary from actual loop output:"
  "${CLI}" summary "${MANIFEST_PATH}" \
    | python3 -c '
import json, sys
s = json.load(sys.stdin)
codecs = ",".join(s["codecs"])
print("  project={} assets={} variants={} resources={} codecs={} bytes={}".format(
    s["project_name"], s["assets"], s["variants"], s["resources"],
    codecs, s["total_resource_bytes"]))
'
  echo "[check] OK — offline package performs real decode/audition/serve tasks"
}

if [[ "${1:-}" == "--check" ]]; then
  run_real_task_check
  exit 0
fi

# Foreground launch. Working directory is the install root so the server's
# /files service exposes the packaged demo tree and nothing else.
cd "${INSTALL_ROOT}"
echo "starting looper-server on http://127.0.0.1:${LOOPER_PORT}"
echo "frontend: ${FRONTEND_PATH} | manifest: ${MANIFEST_PATH}"
exec env LOOPER_PORT="${LOOPER_PORT}" LOOPER_FRONTEND="${FRONTEND_PATH}" "${SERVER}"
