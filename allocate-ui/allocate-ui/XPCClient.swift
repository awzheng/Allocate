// XPCClient.swift
// allocate-ui — Phase 6: SwiftUI XPC Client + Telemetry Parser
//
// ── Swift 6 Strict Concurrency ───────────────────────────────────────────────
// @Observable + @MainActor: @ObservationIgnored prevents the macro from
// synthesising nonisolated ObservationTracked backing for `connection` —
// illegal for mutable state on a @MainActor class under Swift 6.
//
// ── C-Pointer FFI ────────────────────────────────────────────────────────────
// xpc_dictionary_get_string returns UnsafePointer<CChar>?. Bound via `if let`.
// UnsafePointer(_:) satisfies String(cString:)'s immutable-pointer requirement.
// xpc_copy_description: same pattern + free(rawPtr) (Create Rule).
//
// ── XPC Handshake ────────────────────────────────────────────────────────────
// xpc_connection_resume() is fully lazy — no Mach message is sent until the
// client transmits. A {type: "hello"} ping is sent immediately after resume
// to force the daemon's listener block to fire and add this client to the
// broadcast list. xpc_release is intentionally omitted: xpc_connection_send_message
// retains the dict internally for async delivery.
//
// ── Threading ────────────────────────────────────────────────────────────────
// XPC delivers on an internal GCD queue. Parsing runs there. @Observable
// property writes hop to @MainActor via Task { @MainActor [weak self] in }.

import Foundation

// MARK: - Data Models

struct ProcessMetrics: Identifiable {
    let id: Int           // process rank (1-based); 0 = synthetic foreground row
    let name: String
    let cpu: String       // e.g. "  5.1%" or "—" for the foreground row
    let ram: String       // e.g. " 120 MB" or "—"
    let threads: String   // e.g. "8" or "—"
    let isThrottled: Bool // true when the governor has applied taskpolicy -b
    let isForeground: Bool // true for the synthetic pinned foreground row
}

struct TelemetryState {
    var activeApp: String = ""
    var battery: String = ""
    var processes: [ProcessMetrics] = []
}

// MARK: - Parser

/// Parses the daemon's brutalist ASCII table into TelemetryState.
/// Fails gracefully: if a line doesn't match expectation it is skipped.
///
/// Expected format (emitted by allocate-core/src/main.rs build_table()):
///   ┌──...
///   │ 🟢 ACTIVE: AppName (PID: 12345) | 🔋 100% (Battery)
///   ├──...
///   │ ⚠️  BACKGROUND HOGS
///   │  1. ProcessName             | CPU:   5.1% | RAM:  120 MB | Threads: 8 | OK
///   │  2. HogProcess              | CPU:  42.0% | RAM:  800 MB | Threads: 4 | THROTTLED
///   └──...
private func parseTelemetry(_ payload: String) -> TelemetryState {
    var state = TelemetryState()
    var rank = 0

    for line in payload.components(separatedBy: "\n") {
        // Strip leading "│ " prefix (± extra spaces)
        let stripped = line.trimmingCharacters(in: .whitespaces)
        guard stripped.hasPrefix("│") else { continue }
        let content = stripped.dropFirst().trimmingCharacters(in: .whitespaces)

        // ── Active app header line ──────────────────────────────────────────
        // "🟢 ACTIVE: Finder (PID: 1234) | 🔋 85% (Battery)"
        if content.hasPrefix("🟢") || content.hasPrefix("ACTIVE:") {
            // Extract app name: between "ACTIVE: " and " (PID:"
            if let activeRange = content.range(of: "ACTIVE: "),
               let pidRange  = content.range(of: " (PID:") {
                state.activeApp = String(content[activeRange.upperBound ..< pidRange.lowerBound])
            }
            // Extract battery: everything after " | "
            if let pipeRange = content.range(of: " | ") {
                state.battery = String(content[pipeRange.upperBound...])
            }
            continue
        }

        // ── Process row ─────────────────────────────────────────────────────
        // "1. ProcessName             | CPU:   5.1% | RAM:  120 MB | Threads: 8 | OK"
        // "2. HogProcess              | CPU:  42.0% | RAM:  800 MB | Threads: 4 | THROTTLED"
        let parts = content.components(separatedBy: " | ")
        guard parts.count == 5 else { continue }

        // parts[0] = "1. ProcessName   "
        let namePart = parts[0]
        guard let dotIdx = namePart.firstIndex(of: ".") else { continue }
        let name = String(namePart[namePart.index(dotIdx, offsetBy: 1)...])
            .trimmingCharacters(in: .whitespaces)
        guard !name.isEmpty, name != "⚠️  BACKGROUND HOGS" else { continue }

        // parts[1] = "CPU:   5.1%"
        let cpu = parts[1].replacingOccurrences(of: "CPU:", with: "")
            .trimmingCharacters(in: .whitespaces)

        // parts[2] = "RAM:  120 MB"
        let ram = parts[2].replacingOccurrences(of: "RAM:", with: "")
            .trimmingCharacters(in: .whitespaces)

        // parts[3] = "Threads: 8"
        let threads = parts[3].replacingOccurrences(of: "Threads:", with: "")
            .trimmingCharacters(in: .whitespaces)

        // parts[4] = "FRONTMOST" | "THROTTLED" | "OK"
        let tag = parts[4].trimmingCharacters(in: .whitespaces)
        let isForeground = tag == "FRONTMOST"
        let isThrottled  = tag == "THROTTLED"

        rank += 1
        state.processes.append(ProcessMetrics(
            id: rank, name: name, cpu: cpu, ram: ram, threads: threads,
            isThrottled: isThrottled, isForeground: isForeground
        ))
    }

    return state
}

// MARK: - XPC Client

@Observable
@MainActor
final class XPCClient {

    /// Parsed telemetry. Nil before first payload arrives.
    private(set) var telemetry: TelemetryState? = nil

    /// Raw payload (retained for debug / fallback).
    private(set) var payload: String? = nil

    /// True while the XPC connection to the daemon is alive.
    private(set) var isConnected: Bool = false

    /// Rolling 60-sample buffer of system-wide CPU% values (one per 1 Hz tick).
    /// Oldest sample is index 0; newest is the last element.
    private(set) var cpuHistory: [Double] = []

    // ── Internals ─────────────────────────────────────────────────────────────

    private static let serviceName = "com.andrewzheng.allocate.daemon"

    /// @ObservationIgnored prevents the @Observable macro from synthesising
    /// nonisolated ObservationTracked backing — illegal for mutable state on a
    /// @MainActor class under Swift 6. nonisolated is intentionally absent.
    @ObservationIgnored private var connection: xpc_connection_t? = nil

    // ── Lifecycle ─────────────────────────────────────────────────────────────

    init() {}

    // MARK: - Connection

    func start() {
        guard connection == nil else { return }

        let conn = xpc_connection_create_mach_service(
            Self.serviceName,
            nil,
            0
        )

        xpc_connection_set_event_handler(conn) { [weak self] event in
            let type = xpc_get_type(event)

            if type == XPC_TYPE_DICTIONARY {
                if let rawPtr = xpc_dictionary_get_string(event, "payload") {
                    let string   = String(cString: UnsafePointer(rawPtr))
                    let parsed   = parseTelemetry(string)
                    let sysCpu   = xpc_dictionary_get_double(event, "system_cpu")
                    Task { @MainActor [weak self] in
                        self?.payload = string
                        self?.telemetry = parsed
                        self?.isConnected = true
                        self?.appendCpuHistory(sysCpu)
                    }
                }

            } else if type == XPC_TYPE_ERROR {
                let errStr: String
                if let rawPtr = xpc_dictionary_get_string(event, "XPCErrorDescription") {
                    errStr = String(cString: UnsafePointer(rawPtr))
                } else {
                    errStr = "<unknown error>"
                }
                print("[XPCClient] ERROR: \(errStr)")
                // Any XPC error means the daemon is unreachable. Mark disconnected
                // without clearing telemetry — preserved stale data lets the
                // ContentView overlay condition (!isConnected && telemetry != nil)
                // evaluate to true and show the ghost UI.
                Task { @MainActor [weak self] in self?.isConnected = false }

            } else {
                // xpc_copy_description: Create Rule — caller must free().
                let rawPtr = xpc_copy_description(event)
                let desc = String(cString: UnsafePointer(rawPtr))
                free(rawPtr)
                print("[XPCClient] Unknown event: \(desc)")
            }
        }

        xpc_connection_resume(conn)

        // ── Handshake ping ────────────────────────────────────────────────────
        // Forces the lazy Mach-port handshake: daemon's listener block fires →
        // this client is added to the broadcast Vec → payloads start arriving.
        // xpc_release omitted: send_message retains for async delivery.
        let ping = xpc_dictionary_create(nil, nil, 0)
        xpc_dictionary_set_string(ping, "type", "hello")
        xpc_connection_send_message(conn, ping)

        self.connection = conn
    }

    // MARK: - CPU History

    private func appendCpuHistory(_ value: Double) {
        cpuHistory.append(value)
        if cpuHistory.count > 60 {
            cpuHistory.removeFirst()
        }
    }

    // MARK: - Config

    /// Sends updated governor thresholds to the daemon.
    ///
    /// Fire-and-forget: the daemon's GCD XPC handler writes the new values into
    /// its Arc<RwLock<GovernorConfig>> asynchronously.  No response is expected.
    ///
    /// - Parameters:
    ///   - throttleThreshold: CPU% at which the governor throttles a process.
    ///   - releaseThreshold:  CPU% below which a throttled process is released.
    func sendConfig(throttleThreshold: Double, releaseThreshold: Double) {
        guard let conn = connection else { return }
        // Enforce the hysteresis invariant before sending.
        guard releaseThreshold < throttleThreshold, throttleThreshold > 0 else { return }

        let msg = xpc_dictionary_create(nil, nil, 0)
        xpc_dictionary_set_string(msg, "type", "config")
        xpc_dictionary_set_double(msg, "throttle_threshold", throttleThreshold)
        xpc_dictionary_set_double(msg, "release_threshold",  releaseThreshold)
        xpc_connection_send_message(conn, msg)
        // xpc_release omitted: send_message retains the dict for async delivery.
    }
}
