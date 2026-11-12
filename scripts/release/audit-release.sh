#!/usr/bin/env bash
#
# Clean-environment audit for a packaged looper-soundbank release.
#
# Proves the package is genuinely offline and relocatable and that it performs
# real work, not just that a process stays alive:
#
#   1. Relocation: copy the install directory to an unrelated path and run the
#      launcher self-check there.
#   2. Missing resource: delete one audio resource and confirm the package
#      refuses to verify / audition (non-zero exit), i.e. a hollow shell can
#      not pass.
#   3. Missing decoder / corrupt resource: replace a resource with undecodable
#      bytes and confirm the decoder failure is surfaced with a non-zero exit.
#   4. Clean env: run with a minimal HOME/ PATH (system coreutils only) and no
#      network variables, demonstrating no developer cache or toolchain is
#      needed at run time.
#   5. Real server task: start the server and drive the manifest + file APIs
#      and a CLI verify, then confirm actual loop output is readable.
#
# Usage: audit-release.sh [INSTALL_DIR]
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALL_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
if [[ $# -ge 1 ]]; then INSTALL_ROOT="$(cd "$1" && pwd)"; fi

CLI="${INSTALL_ROOT}/bin/looper-cli"
SERVER="${INSTALL_ROOT}/bin/looper-server"
LAUNCHER="${INSTALL_ROOT}/scripts/start-looper.sh"
CONFIG="${INSTALL_ROOT}/config/runtime.conf"

PASS=0; FAIL=0
ok()   { echo "  PASS: $*"; PASS=$((PASS+1)); }
bad()  { echo "  FAIL: $*"; FAIL=$((FAIL+1)); }
step() { printf '\n\033[1;36m-- %s --\033[0m\n' "$*"; }

[[ -x "${CLI}" && -x "${SERVER}" && -x "${LAUNCHER}" && -f "${CONFIG}" ]] \
  || { echo "ERROR: ${INSTALL_ROOT} is not a complete release" >&2; exit 2; }
# shellcheck source=/dev/null
source "${CONFIG}"
MANIFEST_PATH="${INSTALL_ROOT}/${MANIFEST}"
OUT_DIR="${INSTALL_ROOT}/${DEMO_DIR}/${DEMO_OUT_DIR}"
PROJECT_PATH="${INSTALL_ROOT}/${DEMO_DIR}/${DEMO_PROJECT}"
DEMO_ROOT="${INSTALL_ROOT}/${DEMO_DIR}"

# Minimal environment: only system coreutils and the package itself.
CLEAN_ENV=(env -i
  "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
  "HOME=${INSTALL_ROOT}/.audit-home"
  "LANG=C"
)
mkdir -p "${INSTALL_ROOT}/.audit-home"

step "1. Relocation: run the self-check from an unrelated install path"
RELOC="$(mktemp -d)/moved-looper"
mkdir -p "$(dirname "${RELOC}")"
cp -a -- "${INSTALL_ROOT}" "${RELOC}"
if "${CLEAN_ENV[@]}" "${RELOC}/scripts/start-looper.sh" --check; then
  ok "relocated package self-check passed"
else
  bad "relocated package self-check failed"
fi
rm -rf -- "$(dirname "${RELOC}")"

step "2. Missing resource must fail with a non-zero exit code"
MISSING_WORK="$(mktemp -d)/missing"
cp -a -- "${INSTALL_ROOT}" "${MISSING_WORK}"
VICTIM="$(python3 - "${MISSING_WORK}/${MANIFEST}" <<'PY'
import json, sys
m = json.load(open(sys.argv[1]))
print(m["assets"][0]["variants"][0]["resources"][0]["file"])
PY
)"
rm -f -- "${MISSING_WORK}/${DEMO_DIR}/${DEMO_OUT_DIR}/${VICTIM}"
if "${CLEAN_ENV[@]}" "${MISSING_WORK}/bin/looper-cli" verify \
     "${MISSING_WORK}/${MANIFEST}" "${MISSING_WORK}/${DEMO_DIR}/${DEMO_OUT_DIR}" >/dev/null 2>&1; then
  bad "verify succeeded despite missing resource ${VICTIM}"
else
  ok "verify rejected missing resource ${VICTIM} with non-zero exit"
fi
if "${CLEAN_ENV[@]}" "${MISSING_WORK}/scripts/start-looper.sh" --check >/dev/null 2>&1; then
  bad "launcher self-check passed despite missing resource"
else
  ok "launcher self-check refused to pass with a missing resource"
fi
rm -rf -- "$(dirname "${MISSING_WORK}")"

step "3. Undecodable resource (missing/corrupt decoder path) must fail"
CORRUPT_WORK="$(mktemp -d)/corrupt"
cp -a -- "${INSTALL_ROOT}" "${CORRUPT_WORK}"
VICTIM2="$(python3 - "${CORRUPT_WORK}/${MANIFEST}" <<'PY'
import json, sys
m = json.load(open(sys.argv[1]))
print(m["assets"][0]["variants"][0]["resources"][1]["file"])
PY
)"
head -c 512 /dev/urandom > "${CORRUPT_WORK}/${DEMO_DIR}/${DEMO_OUT_DIR}/${VICTIM2}"
if "${CLEAN_ENV[@]}" "${CORRUPT_WORK}/bin/looper-cli" verify \
     "${CORRUPT_WORK}/${MANIFEST}" "${CORRUPT_WORK}/${DEMO_DIR}/${DEMO_OUT_DIR}" >/dev/null 2>&1; then
  bad "verify succeeded on corrupt resource ${VICTIM2}"
else
  ok "verify reported a decoder failure on corrupt ${VICTIM2} with non-zero exit"
fi
rm -rf -- "$(dirname "${CORRUPT_WORK}")"

step "4. Clean-environment preflight + real audition (no developer cache)"
if "${CLEAN_ENV[@]}" "${CLI}" preflight "${PROJECT_PATH}" "${DEMO_ROOT}" >/dev/null; then
  ok "preflight runs under a minimal environment"
else
  bad "preflight failed under a minimal environment"
fi
AUD_TMP="$(mktemp -d)"
if "${CLEAN_ENV[@]}" "${CLI}" audition "${MANIFEST_PATH}" "${OUT_DIR}" "${AUD_TMP}" "${AUDITION_ROUNDS}" >/dev/null; then
  RENDERED="$(find "${AUD_TMP}" -name '*.wav' | wc -l | tr -d ' ')"
  RESOURCES="$(python3 -c 'import json;print(sum(len(v["resources"]) for a in json.load(open("'"${MANIFEST_PATH}"'"))["assets"] for v in a["variants"]))')"
  if [[ "${RENDERED}" -eq "${RESOURCES}" && "${RENDERED}" -gt 0 ]]; then
    ok "real audition rendered ${RENDERED}/${RESOURCES} loops with no cache/toolchain"
  else
    bad "audition count ${RENDERED} != resources ${RESOURCES}"
  fi
else
  bad "audition failed under a minimal environment"
fi
rm -rf -- "${AUD_TMP}"

step "5. Start the server and execute real tasks (not a process-alive probe)"
PORT="$(( 18000 + ( RANDOM % 2000 ) ))"
env -i \
  "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" \
  "HOME=${INSTALL_ROOT}/.audit-home" \
  "LANG=C" \
  "LOOPER_PORT=${PORT}" \
  "LOOPER_FRONTEND=${INSTALL_ROOT}/${LOOPER_FRONTEND}" \
  "${SERVER}" >/tmp/looper-audit.$$.log 2>&1 &
SRV_PID=$!
cleanup() { kill "${SRV_PID}" 2>/dev/null || true; rm -rf -- "${INSTALL_ROOT}/.audit-home"; }
trap cleanup EXIT
READY=0
for _ in $(seq 1 50); do
  if curl -fsS "http://127.0.0.1:${PORT}/api/manifest?path=$(python3 -c 'import urllib.parse,sys;print(urllib.parse.quote(sys.argv[1]))' "${MANIFEST_PATH}")" >/dev/null 2>&1; then
    READY=1; break; fi
  kill -0 "${SRV_PID}" 2>/dev/null || { echo "server exited:"; cat /tmp/looper-audit.$$.log; break; }
  sleep 0.2
done
if [[ "${READY}" -ne 1 ]]; then
  bad "server did not become ready"
else
  SUMMARY="$(curl -fsS "http://127.0.0.1:${PORT}/api/manifest?path=$(python3 -c 'import urllib.parse,sys;print(urllib.parse.quote(sys.argv[1]))' "${MANIFEST_PATH}")")"
  echo "${SUMMARY}" | python3 -c 'import json,sys; d=json.load(sys.stdin); m=d["manifest"]; assert d["ok"] and m["assets"], "empty manifest"; print("  API manifest:", len(m["assets"]), "assets")' \
    && ok "server served the real manifest over HTTP" || bad "manifest API returned no assets"
  FIRST="$(python3 -c 'import json; m=json.load(open("'"${MANIFEST_PATH}"'")); print(m["assets"][0]["variants"][0]["resources"][0]["file"])')"
  if curl -fsS -o /dev/null "http://127.0.0.1:${PORT}/files/${DEMO_DIR}/${DEMO_OUT_DIR}/${FIRST}"; then
    ok "server streamed a real audio resource (${FIRST})"
  else
    bad "server could not stream resource ${FIRST}"
  fi
  # A genuine audio task through the running service: verify endpoint re-decodes.
  if curl -fsS "http://127.0.0.1:${PORT}/api/verify?manifest=$(python3 -c 'import urllib.parse,sys;print(urllib.parse.quote(sys.argv[1]))' "${MANIFEST_PATH}")&out_dir=$(python3 -c 'import urllib.parse,sys;print(urllib.parse.quote(sys.argv[1]))' "${OUT_DIR}")" \
      | python3 -c 'import json,sys; r=json.load(sys.stdin); assert r["ok"] and r["reports"], "no verify reports"; print("  verify API re-decoded", len(r["reports"]), "resources")'; then
    ok "server executed a real re-decode verification task"
  else
    bad "server verify task failed"
  fi
fi

step "6. Manifest summary reflects actual loop output"
"${CLI}" summary "${MANIFEST_PATH}" | python3 -c '
import json, sys
s = json.load(sys.stdin)
assert s["resources"] > 0 and s["loops"], "no loops in summary"
for l in s["loops"]:
    assert l["loop_length_frames"] > 0, l
    assert l["loop_end_frame"] <= l["decoded_frames"], l
print("  {} resources, {} loops, codecs={}".format(
    s["resources"], len(s["loops"]), ",".join(s["codecs"])))
' && ok "summary loop coordinates match decoded frame bounds" || bad "summary loop output invalid"

printf '\n================ AUDIT RESULT ================\n'
echo "passed=${PASS} failed=${FAIL}"
if [[ "${FAIL}" -ne 0 ]]; then
  echo "AUDIT FAILED — do not publish this release"
  exit 1
fi
echo "AUDIT PASSED — offline package is relocatable and performs real tasks"
