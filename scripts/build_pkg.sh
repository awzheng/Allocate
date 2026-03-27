#!/usr/bin/env bash
# scripts/build_pkg.sh
#
# Production-ready .pkg builder for Allocate 2026.03.
#
# Produces: Allocate-2026.03.pkg
#
# What gets installed on the target Mac:
#   /Library/Application Support/Allocate/allocate-core  — Rust daemon binary
#   /Library/LaunchDaemons/com.andrewzheng.allocate.daemon.plist — launchd job
#   /Applications/Allocate.app                           — Swift UI bundle
#
# The postinstall script runs as root immediately after the pkg expands and
# takes care of ownership, permissions, and daemon bootstrap.
#
# Usage (run from repo root or any directory):
#   chmod +x scripts/build_pkg.sh
#   ./scripts/build_pkg.sh
#
# Dependencies: cargo, xcodebuild, pkgbuild (all present on a standard Mac dev box).

set -euo pipefail

# ─────────────────────────────────────────────────────────────────────────────
# § 0  Constants
# ─────────────────────────────────────────────────────────────────────────────

readonly VERSION="2026.03"
readonly LABEL="com.andrewzheng.allocate.daemon"
readonly PKG_ID="com.andrewzheng.allocate"
readonly PKG_NAME="Allocate-${VERSION}.pkg"

# Canonical path to the repo root regardless of where the script is invoked.
readonly REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Where the Rust release binary lands after `cargo build --release`.
readonly RUST_BIN="${REPO_ROOT}/target/release/allocate-core"

# Xcode project details.
readonly XCODE_PROJ="${REPO_ROOT}/allocate-ui/allocate-ui.xcodeproj"
readonly XCODE_SCHEME="Allocate"
readonly XCODE_CONFIG="Release"

# Temporary derived-data dir for the Xcode build (cleaned up at the end).
readonly DERIVED_DATA="${REPO_ROOT}/.pkg_derived_data"

# The .app bundle that xcodebuild produces inside derived data.
readonly APP_BUNDLE="${DERIVED_DATA}/Build/Products/${XCODE_CONFIG}/Allocate.app"

# Staging roots (populated then handed to pkgbuild).
readonly PKG_ROOT="${REPO_ROOT}/.pkg_root"
readonly SCRIPTS_DIR="${REPO_ROOT}/.pkg_scripts"

# Destination paths *on the target Mac* (mirrored under $PKG_ROOT here).
readonly DEST_SUPPORT="/Library/Application Support/Allocate"
readonly DEST_DAEMON_BIN="${DEST_SUPPORT}/allocate-core"
readonly DEST_PLIST_DIR="/Library/LaunchDaemons"
readonly DEST_PLIST="${DEST_PLIST_DIR}/${LABEL}.plist"
readonly DEST_APP="/Applications/Allocate.app"
readonly LOG_FILE="/var/log/allocate-daemon.log"

# ─────────────────────────────────────────────────────────────────────────────
# § 1  Helpers
# ─────────────────────────────────────────────────────────────────────────────

step()  { echo ""; echo "▶  $*"; }
ok()    { echo "   ✓  $*"; }
die()   { echo ""; echo "✘  ERROR: $*" >&2; exit 1; }

# ─────────────────────────────────────────────────────────────────────────────
# § 2  Preflight
# ─────────────────────────────────────────────────────────────────────────────

step "Preflight checks"

command -v cargo      >/dev/null 2>&1 || die "'cargo' not found — install Rust via rustup."
command -v xcodebuild >/dev/null 2>&1 || die "'xcodebuild' not found — install Xcode."
command -v pkgbuild   >/dev/null 2>&1 || die "'pkgbuild' not found — install Xcode Command Line Tools."

[[ -f "${XCODE_PROJ}/project.pbxproj" ]] \
    || die "Xcode project not found at ${XCODE_PROJ}"

ok "All tools present."

# ─────────────────────────────────────────────────────────────────────────────
# § 3  Build Rust daemon
# ─────────────────────────────────────────────────────────────────────────────

step "Building Rust daemon (release, LTO)"

# --manifest-path ensures we always build from the workspace root regardless of
# the caller's cwd.  -p allocate-core avoids building dummy-hog.
cargo build --release \
    --manifest-path "${REPO_ROOT}/Cargo.toml" \
    -p allocate-core

[[ -f "${RUST_BIN}" ]] || die "Rust build succeeded but binary not found at ${RUST_BIN}"
ok "allocate-core built: $(du -sh "${RUST_BIN}" | cut -f1)"

# ─────────────────────────────────────────────────────────────────────────────
# § 4  Build Swift UI
# ─────────────────────────────────────────────────────────────────────────────

step "Building Swift UI (${XCODE_CONFIG})"

# Clean the derived-data dir so we always get a fresh build.
rm -rf "${DERIVED_DATA}"

# -derivedDataPath keeps all build artefacts in one predictable location so we
# know exactly where Allocate.app will appear.
# CODE_SIGN_IDENTITY="" + CODE_SIGNING_REQUIRED=NO: skip code-signing so the
# pkg build succeeds on a CI machine without a Developer ID certificate installed.
# Remove those two overrides (or set CODE_SIGN_IDENTITY to your cert) if you
# want a notarisation-ready signed app.
xcodebuild build \
    -project         "${XCODE_PROJ}" \
    -scheme          "${XCODE_SCHEME}" \
    -configuration   "${XCODE_CONFIG}" \
    -derivedDataPath "${DERIVED_DATA}" \
    CODE_SIGN_IDENTITY="" \
    CODE_SIGNING_REQUIRED=NO \
    CODE_SIGNING_ALLOWED=NO \
    | grep -E "^(Build|warning:|error:|CompileSwift|Ld |note:)" || true
# The grep filters verbose xcodebuild output to just the meaningful lines.
# "|| true" prevents set -e from triggering on grep's exit-1-when-no-match.

[[ -d "${APP_BUNDLE}" ]] \
    || die "xcodebuild succeeded but Allocate.app not found at ${APP_BUNDLE}"
ok "Allocate.app built: $(du -sh "${APP_BUNDLE}" | cut -f1)"

# ─────────────────────────────────────────────────────────────────────────────
# § 5  Assemble pkg_root staging tree
# ─────────────────────────────────────────────────────────────────────────────

step "Assembling staging directory: ${PKG_ROOT}"

# Start clean so stale files from a previous run cannot contaminate the package.
rm -rf "${PKG_ROOT}"

# Mirror the macOS filesystem layout that pkgbuild will expand into.
mkdir -p "${PKG_ROOT}${DEST_SUPPORT}"
mkdir -p "${PKG_ROOT}${DEST_PLIST_DIR}"
mkdir -p "${PKG_ROOT}/Applications"

# ── 5a  Rust daemon binary ────────────────────────────────────────────────────
cp "${RUST_BIN}" "${PKG_ROOT}${DEST_DAEMON_BIN}"
ok "Staged daemon binary → ${DEST_DAEMON_BIN}"

# ── 5b  LaunchDaemon plist ────────────────────────────────────────────────────
#
# Key design decisions:
#   • MachServices: THIS is mandatory for xpc_connection_create_mach_service.
#     Without it, libxpc issues SIGTRAP when the daemon tries to register.
#   • ALLOCATE_XPC_ENABLE=1: the daemon's XPC listener is gated behind this
#     env var to prevent SIGTRAP when running outside of a launchd session
#     (e.g., direct terminal invocation).
#   • RunAtLoad + KeepAlive: start immediately on install and restart on crash.
#   • StandardOutPath / StandardErrorPath: write to /var/log for sysadmin access.
#
# NOTE on LaunchDaemon vs LaunchAgent:
#   A LaunchDaemon runs as root in the system context (no Window Server session).
#   allocate-core uses NSWorkspace for frontmost-app detection, which requires a
#   GUI session.  If you want full app-switch detection per logged-in user, move
#   this plist to /Library/LaunchAgents (which runs per-user) and change the
#   postinstall bootstrap command accordingly.  For a headless / server deploy
#   the LaunchDaemon path is correct as-is.

cat > "${PKG_ROOT}${DEST_PLIST}" << PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>

    <!-- Unique label — also the Mach service lookup key for XPC clients. -->
    <key>Label</key>
    <string>${LABEL}</string>

    <!-- MachServices: registers this label as an XPC Mach service so
         xpc_connection_create_mach_service() in the daemon can bind to it.
         Without this entry libxpc will SIGTRAP on the first listener call. -->
    <key>MachServices</key>
    <dict>
        <key>${LABEL}</key>
        <true/>
    </dict>

    <!-- Full path to the daemon binary installed by this package. -->
    <key>ProgramArguments</key>
    <array>
        <string>${DEST_DAEMON_BIN}</string>
    </array>

    <!-- Route stdout and stderr to a persistent log file. -->
    <key>StandardOutPath</key>
    <string>${LOG_FILE}</string>
    <key>StandardErrorPath</key>
    <string>${LOG_FILE}</string>

    <!-- Enable the XPC listener inside the daemon process.
         Without ALLOCATE_XPC_ENABLE=1, allocate-core skips XPC setup
         (its terminal-dev guard) and the UI client can never connect. -->
    <key>EnvironmentVariables</key>
    <dict>
        <key>ALLOCATE_XPC_ENABLE</key>
        <string>1</string>
    </dict>

    <!-- Start immediately when launchd loads this job. -->
    <key>RunAtLoad</key>
    <true/>

    <!-- Restart the daemon automatically if it exits for any reason. -->
    <key>KeepAlive</key>
    <true/>

</dict>
</plist>
PLIST

ok "Staged LaunchDaemon plist → ${DEST_PLIST}"

# ── 5c  Swift UI app bundle ───────────────────────────────────────────────────
# ditto preserves resource forks, symlinks, and extended attributes — use it
# instead of cp -r for .app bundles.
ditto "${APP_BUNDLE}" "${PKG_ROOT}/Applications/Allocate.app"
ok "Staged app bundle → /Applications/Allocate.app"

# ─────────────────────────────────────────────────────────────────────────────
# § 6  Generate postinstall script
# ─────────────────────────────────────────────────────────────────────────────
#
# pkgbuild runs scripts/postinstall as root immediately after expanding the
# payload.  This is the earliest safe point to set ownership and start the
# daemon — the files are guaranteed to be on disk.

step "Generating postinstall script"

rm -rf "${SCRIPTS_DIR}"
mkdir -p "${SCRIPTS_DIR}"

cat > "${SCRIPTS_DIR}/postinstall" << 'POSTINSTALL'
#!/usr/bin/env bash
# postinstall — runs as root after Allocate-2026.03.pkg expands its payload.
set -euo pipefail

LABEL="com.andrewzheng.allocate.daemon"
PLIST="/Library/LaunchDaemons/${LABEL}.plist"
DAEMON_BIN="/Library/Application Support/Allocate/allocate-core"
LOG_FILE="/var/log/allocate-daemon.log"

# ── Ownership & permissions ───────────────────────────────────────────────────
# The daemon binary must be owned root:wheel and executable.
# The plist must be owned root:wheel and world-readable (644) — launchd rejects
# plists that are writable by non-root.
chown root:wheel "${DAEMON_BIN}"
chmod 755        "${DAEMON_BIN}"

chown root:wheel "${PLIST}"
chmod 644        "${PLIST}"

# Ensure the log file exists and is writable by root (created on first daemon
# launch if absent, but launchd may complain before the first write).
touch "${LOG_FILE}"
chown root:wheel "${LOG_FILE}"
chmod 644        "${LOG_FILE}"

# ── Bootstrap the daemon into the system launchd domain ──────────────────────
# `launchctl bootstrap system <plist>` is the modern (macOS 10.11+) replacement
# for `launchctl load -w`.  It registers the job in the system domain so all
# users on this machine can reach the Mach service via XPC.
#
# If the job is already bootstrapped (e.g. upgrading over a previous install),
# bootout first so we don't get an "already loaded" error.
if launchctl print "system/${LABEL}" >/dev/null 2>&1; then
    echo "[postinstall] Removing existing job before re-bootstrap…"
    launchctl bootout "system/${LABEL}" 2>/dev/null || true
    # Give launchd a moment to clean up the Mach port before re-registering.
    sleep 1
fi

echo "[postinstall] Bootstrapping ${LABEL}…"
launchctl bootstrap system "${PLIST}"

echo "[postinstall] Done. Daemon is running."
echo "[postinstall] Log: tail -f ${LOG_FILE}"
POSTINSTALL

chmod +x "${SCRIPTS_DIR}/postinstall"
ok "postinstall script written and marked executable."

# ─────────────────────────────────────────────────────────────────────────────
# § 7  Build the .pkg
# ─────────────────────────────────────────────────────────────────────────────

step "Running pkgbuild → ${PKG_NAME}"

# --root        : the staged filesystem tree to embed in the package payload.
# --scripts     : directory containing pre/postinstall scripts.
# --identifier  : reverse-DNS bundle ID written into the package receipt.
# --version     : recorded in the receipt and shown in Installer.app.
# --install-location (implicit /): pkgbuild defaults to installing relative to
#                 /, which is what we want since pkg_root mirrors /.
pkgbuild \
    --root        "${PKG_ROOT}"    \
    --scripts     "${SCRIPTS_DIR}" \
    --identifier  "${PKG_ID}"      \
    --version     "${VERSION}"     \
    "${REPO_ROOT}/${PKG_NAME}"

ok "Package built: ${REPO_ROOT}/${PKG_NAME}  ($(du -sh "${REPO_ROOT}/${PKG_NAME}" | cut -f1))"

# ─────────────────────────────────────────────────────────────────────────────
# § 8  Cleanup staging directories
# ─────────────────────────────────────────────────────────────────────────────

step "Cleaning up staging directories"
rm -rf "${PKG_ROOT}" "${SCRIPTS_DIR}" "${DERIVED_DATA}"
ok "Removed: ${PKG_ROOT}"
ok "Removed: ${SCRIPTS_DIR}"
ok "Removed: ${DERIVED_DATA}"

# ─────────────────────────────────────────────────────────────────────────────
# § 9  Done
# ─────────────────────────────────────────────────────────────────────────────

echo ""
echo "═══════════════════════════════════════════════════════════"
echo "  ✅  ${PKG_NAME} is ready."
echo ""
echo "  Install on any Mac:"
echo "    sudo installer -pkg ${PKG_NAME} -target /"
echo ""
echo "  Or double-click the .pkg to launch Installer.app."
echo ""
echo "  After install the daemon starts immediately."
echo "  Tail its log:"
echo "    tail -f /var/log/allocate-daemon.log"
echo "═══════════════════════════════════════════════════════════"
