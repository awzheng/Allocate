#!/usr/bin/env bash
# scripts/install_agent.sh
#
# Installs allocate-core as a LaunchAgent so it:
#   • runs automatically at login
#   • provides the MachServices entry XPC requires
#   • writes Brutalist UI output to /tmp/allocate-daemon.log
#
# Usage:
#   chmod +x scripts/install_agent.sh
#   ./scripts/install_agent.sh
#
# After install, switch apps in any app — you will see the brutalist table in:
#   tail -f /tmp/allocate-daemon.log

set -euo pipefail

# ── Config ────────────────────────────────────────────────────────────────────
LABEL="com.andrewzheng.allocate.daemon"
PLIST_DIR="$HOME/Library/LaunchAgents"
PLIST_PATH="$PLIST_DIR/$LABEL.plist"
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BINARY="$REPO_ROOT/target/release/allocate-core"
LOG_FILE="/tmp/allocate-daemon.log"

# ── Preflight checks ──────────────────────────────────────────────────────────
if [[ ! -f "$BINARY" ]]; then
    echo "⚠️  Release binary not found. Building first..."
    cargo build --release --manifest-path "$REPO_ROOT/Cargo.toml"
fi

mkdir -p "$PLIST_DIR"

# ── Unload existing agent (idempotent) ────────────────────────────────────────
if launchctl list | grep -q "$LABEL" 2>/dev/null; then
    echo "→ Unloading existing agent..."
    launchctl unload "$PLIST_PATH" 2>/dev/null || true
fi

# ── Write the plist ───────────────────────────────────────────────────────────
cat > "$PLIST_PATH" << PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <!-- Unique label used by launchd and XPC bootstrap lookup -->
    <key>Label</key>
    <string>$LABEL</string>

    <!-- MachServices: THIS is what lets xpc_connection_create_mach_service work.
         Without this entry, libxpc issues SIGTRAP on any listener creation. -->
    <key>MachServices</key>
    <dict>
        <key>$LABEL</key>
        <true/>
    </dict>

    <!-- Binary path -->
    <key>ProgramArguments</key>
    <array>
        <string>$BINARY</string>
    </array>

    <!-- Pipe stdout/stderr to the same log file so we can follow the brutalist UI -->
    <key>StandardOutPath</key>
    <string>$LOG_FILE</string>
    <key>StandardErrorPath</key>
    <string>$LOG_FILE</string>

    <!-- Enable the XPC listener in the spawned process.
         Without this env var, allocate-core bypasses XPC (terminal dev guard). -->
    <key>EnvironmentVariables</key>
    <dict>
        <key>ALLOCATE_XPC_ENABLE</key>
        <string>1</string>
    </dict>

    <!-- Restart automatically if it crashes -->
    <key>KeepAlive</key>
    <true/>

    <!-- Launch immediately after load -->
    <key>RunAtLoad</key>
    <true/>
</dict>
</plist>
PLIST

echo "→ Plist written to: $PLIST_PATH"

# ── Load the agent ────────────────────────────────────────────────────────────
launchctl load "$PLIST_PATH"

echo ""
echo "✅  LaunchAgent loaded successfully."
echo ""
echo "   Mach service : $LABEL"
echo "   Binary       : $BINARY"
echo "   Log file     : $LOG_FILE"
echo ""
echo "Follow the Brutalist UI output:"
echo "   tail -f $LOG_FILE"
echo ""
echo "To uninstall:"
echo "   launchctl unload $PLIST_PATH && rm $PLIST_PATH"
