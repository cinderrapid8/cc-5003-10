#!/usr/bin/env bash
#
# Reproducible offline release builder for the game loop soundbank tool.
#
# Produces a self-contained, relocatable directory containing:
#   bin/        looper-cli + looper-server runners (codecs statically linked)
#   frontend/   offline static web bundle
#   demo/       regenerated authorized examples + built soundbank + auditions
#   config/     runtime config actually consumed by the launcher
#   scripts/    launcher + audit scripts
#   RELEASE.json, MANIFEST.summary.json, SHA256SUMS
#
# The whole pipeline runs real audio work in a clean tree (no developer cache)
# and exits non-zero on any failure. A failed run removes its partial stage,
# so a half-built "web page only" shell can never be published.
set -euo pipefail

# --- Locate repository root and load config --------------------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
cd "${REPO_ROOT}"
# shellcheck source=release.conf
source "${SCRIPT_DIR}/release.conf"

# --- Toolchain resolution ---------------------------------------------------
# Allow a rustup toolchain installed outside PATH. Cargo is otherwise expected
# on PATH on a build machine.
if [[ -z "${CARGO:-}" ]] && [[ ! "$(command -v cargo 2>/dev/null)" ]]; then
  if [[ -f "${HOME}/.cargo/env" ]]; then
    # shellcheck disable=SC1091
    source "${HOME}/.cargo/env"
  fi
fi
CARGO="${CARGO:-$(command -v cargo || true)}"
[[ -n "${CARGO}" ]] || { echo "ERROR: cargo not found (install Rust stable)" >&2; exit 2; }
NPM="${NPM:-$(command -v npm || true)}"
[[ -n "${NPM}" ]] || { echo "ERROR: npm not found (needed to build the offline frontend)" >&2; exit 2; }
JQ="$(command -v jq || true)"
[[ -n "${JQ}" ]] || { echo "ERROR: jq not found" >&2; exit 2; }

# Some containers ship a sandbox wrapper as `cc` that rejects linker flags.
# Point cargo at a real system compiler when the default one is broken.
if [[ -z "${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER:-}" && -x /usr/bin/gcc ]]; then
  export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=/usr/bin/gcc
fi
if [[ -z "${CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER:-}" && -x /usr/bin/gcc ]]; then
  export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=/usr/bin/gcc
fi

# Build into a release-private target dir so the release never reads or
# overwrites a developer's incremental build cache.
PRIVATE_TARGET="${REPO_ROOT}/${RELEASE_WORKDIR}/target"
export CARGO_TARGET_DIR="${PRIVATE_TARGET}"

VERSION="${VERSION:-$("${CARGO}" metadata --no-deps --format-version 1 2>/dev/null \
  | "${JQ}" -r '.packages[] | select(.name=="looper-core") | .version' | head -n1)}"
if [[ -z "${VERSION}" || "${VERSION}" == "null" ]]; then
  VERSION="$(grep -m1 '^version' crates/looper-core/Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
fi
[[ -n "${VERSION}" && "${VERSION}" != *workspace* ]] || fail "could not resolve package version"
ARCH="$(uname -m)"
STAGE="${REPO_ROOT}/${RELEASE_WORKDIR}/stage"
FINAL_NAME="${APP_NAME}-${VERSION}-${ARCH}-linux"
FINAL_DIR="${REPO_ROOT}/${DIST_DIR}/${FINAL_NAME}"
TARBALL="${REPO_ROOT}/${DIST_DIR}/${FINAL_NAME}.tar.gz"

log() { printf '\n\033[1;34m== %s ==\033[0m\n' "$*"; }
fail() { echo "ERROR: $*" >&2; exit 1; }

# A failed build must leave no installable artifact behind.
trap 'rc=$?; if [[ $rc -ne 0 && -d "${STAGE}" ]]; then
        echo "build failed (exit $rc); removing partial stage ${STAGE}" >&2
        rm -rf -- "${STAGE}"; exit $rc; fi' EXIT

log "Clean release workspace (no developer cache is reused)"
rm -rf -- "${STAGE}"
mkdir -p -- "${STAGE}/bin" "${STAGE}/frontend" "${STAGE}/demo" \
            "${STAGE}/config" "${STAGE}/scripts"

# --- 1. Build the runners from locked dependencies --------------------------
log "1/8 Building runners (cargo build --locked --release)"
"${CARGO}" build --workspace --locked --release
[[ -x "${PRIVATE_TARGET}/release/${CLI_BIN}" ]] || fail "runner ${CLI_BIN} missing after build"
[[ -x "${PRIVATE_TARGET}/release/${SERVER_BIN}" ]] || fail "runner ${SERVER_BIN} missing after build"

# Prove the decoder dependency really ships inside the binary rather than as a
# loadable plugin: reject any non-glibc dynamic dependency.
log "Checking runners link only against system libraries (codecs embedded)"
BAD_LIBS="$(ldd "${PRIVATE_TARGET}/release/${CLI_BIN}" "${PRIVATE_TARGET}/release/${SERVER_BIN}" \
  | grep -oE '=> /[^ ]+' | sed 's/=> //' \
  | grep -vE '^(/lib|/usr/lib|/lib64|/usr/lib64)/' || true)"
[[ -z "${BAD_LIBS}" ]] || fail "runner has non-system dynamic dependency: ${BAD_LIBS}"
cp -- "${PRIVATE_TARGET}/release/${CLI_BIN}" "${STAGE}/bin/"
cp -- "${PRIVATE_TARGET}/release/${SERVER_BIN}" "${STAGE}/bin/"

# --- 2. Build the offline frontend bundle -----------------------------------
log "2/8 Building offline frontend bundle"
(
  cd "${FRONTEND_DIR}"
  # ci/clean install from the committed lockfile; no project-level config edits.
  "${NPM}" ci --no-audit --no-fund
  "${NPM}" run build
)
[[ -f "${FRONTEND_DIST}/index.html" ]] || fail "frontend build produced no index.html"
cp -a -- "${FRONTEND_DIST}/." "${STAGE}/frontend/"

# --- 3. Regenerate the authorized examples in the clean tree ----------------
log "3/8 Regenerating deterministic example sources (no copied binaries)"
STAGE_DEMO="${STAGE}/${DEMO_DIR}"
mkdir -p -- "${STAGE_DEMO}"
# Authorized project definition lives in the repo; copy it, then regenerate
# every sample from the synth module so the audio is traceable to source.
cp -- "${DEMO_DIR}/${DEMO_PROJECT}" "${STAGE_DEMO}/${DEMO_PROJECT}"
"${STAGE}/bin/${CLI_BIN}" synth "${STAGE_DEMO}/${DEMO_SAMPLES_DIR}"

# --- 4. Preflight: loop boundaries, referenced files, variant config --------
log "4/8 Preflight: referenced sources, loop boundaries, variant configuration"
"${STAGE}/bin/${CLI_BIN}" preflight \
  "${STAGE_DEMO}/${DEMO_PROJECT}" "${STAGE_DEMO}"

# --- 5. Real build inside the clean tree ------------------------------------
log "5/8 Building soundbank (real decode + encode + seam verification)"
"${STAGE}/bin/${CLI_BIN}" build \
  "${STAGE_DEMO}/${DEMO_PROJECT}" "${STAGE_DEMO}" "${STAGE_DEMO}/${DEMO_OUT_DIR}"

# --- 6. Re-decode every resource, then render real multi-round auditions ----
log "6/8 Re-decoding resources and rendering real audition loops"
"${STAGE}/bin/${CLI_BIN}" verify \
  "${STAGE_DEMO}/${DEMO_OUT_DIR}/soundbank.json" "${STAGE_DEMO}/${DEMO_OUT_DIR}"
"${STAGE}/bin/${CLI_BIN}" audition \
  "${STAGE_DEMO}/${DEMO_OUT_DIR}/soundbank.json" \
  "${STAGE_DEMO}/${DEMO_OUT_DIR}" \
  "${STAGE_DEMO}/audition" "${AUDITION_ROUNDS}"
RENDER_COUNT="$(find "${STAGE_DEMO}/audition" -type f -name '*.wav' | wc -l | tr -d ' ')"
RESOURCE_COUNT="$("${JQ}" '[.assets[].variants[].resources[]] | length' \
  "${STAGE_DEMO}/${DEMO_OUT_DIR}/soundbank.json")"
[[ "${RENDER_COUNT}" -eq "${RESOURCE_COUNT}" ]] || \
  fail "audition rendered ${RENDER_COUNT} files but manifest lists ${RESOURCE_COUNT} resources"
[[ "${RENDER_COUNT}" -gt 0 ]] || fail "no audition output: refusing to publish an empty shell"

# Manifest summary read from the actual loop output (coordinates, frame counts).
"${STAGE}/bin/${CLI_BIN}" summary \
  "${STAGE_DEMO}/${DEMO_OUT_DIR}/soundbank.json" > "${STAGE}/MANIFEST.summary.json"

# --- 7. Runtime config consumed by the launcher -----------------------------
log "7/8 Writing runtime config that binds the packaged offline resources"
cat > "${STAGE}/config/runtime.conf" <<EOF
# Generated by build-release.sh. Consumed by scripts/start-looper.sh.
APP_NAME="${APP_NAME}"
APP_VERSION="${VERSION}"
LOOPER_PORT="${DEFAULT_PORT}"
# Paths are relative to the installation root, so the package is relocatable.
LOOPER_FRONTEND="frontend"
DEMO_DIR="${DEMO_DIR}"
DEMO_PROJECT="${DEMO_PROJECT}"
DEMO_OUT_DIR="${DEMO_OUT_DIR}"
MANIFEST="${DEMO_DIR}/${DEMO_OUT_DIR}/soundbank.json"
AUDITION_DIR="${DEMO_DIR}/audition"
AUDITION_ROUNDS="${AUDITION_ROUNDS}"
EOF
cp -- "${SCRIPT_DIR}/start-looper.sh" "${STAGE}/scripts/"
cp -- "${SCRIPT_DIR}/audit-release.sh" "${STAGE}/scripts/"
chmod +x -- "${STAGE}/scripts/start-looper.sh" "${STAGE}/scripts/audit-release.sh"

# Release provenance.
cat > "${STAGE}/RELEASE.json" <<EOF
{
  "app_name": "${APP_NAME}",
  "version": "${VERSION}",
  "arch": "${ARCH}",
  "platform": "linux",
  "builder": "$("${CARGO}" --version)",
  "generated_at_utc": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "audition_rounds": ${AUDITION_ROUNDS},
  "resources": ${RESOURCE_COUNT},
  "audition_renders": ${RENDER_COUNT},
  "codecs_statically_linked": true,
  "offline": true
}
EOF

# --- 8. Checksums over every packaged file ----------------------------------
log "8/8 Writing SHA256SUMS and assembling final package"
( cd "${STAGE}" && find . -type f ! -name SHA256SUMS -print0 \
    | sort -z | xargs -0 sha256sum > SHA256SUMS )

mkdir -p -- "${DIST_DIR}"
rm -rf -- "${FINAL_DIR}" "${TARBALL}"
# Move (not copy) the validated stage into place: there is no second,
# unvalidated copy of the artifacts anywhere.
mv -- "${STAGE}" "${FINAL_DIR}"
tar -C "${DIST_DIR}" -czf "${TARBALL}" "${FINAL_NAME}"

log "Release complete"
echo "package dir : ${FINAL_DIR}"
echo "tarball     : ${TARBALL}"
echo "resources   : ${RESOURCE_COUNT} (auditions rendered: ${RENDER_COUNT})"
"${JQ}" '{project_name, assets, variants, resources, codecs}' "${FINAL_DIR}/MANIFEST.summary.json"
