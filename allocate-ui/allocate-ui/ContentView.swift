// ContentView.swift
// allocate-ui — Phase 9.2: Two-way XPC, Hysteresis Sliders, UI Polish
//
// Changes from Phase 9.1:
//   • Window is now fully resizable (flexible frame constraints)
//   • "JAILED" badge renamed to "THROTTLED"
//   • Active foreground app pinned as first table row with "IN FOCUS" badge
//   • ConfigPanel added at bottom: slider + text-field pairs for both thresholds
//   • Config changes send XPC messages back to the daemon in real time

import SwiftUI
import Charts

// MARK: - Root View

struct ContentView: View {
    @Environment(XPCClient.self) private var client

    var body: some View {
        VStack(spacing: 0) {
            HeaderBar(client: client)
            Divider()

            // `disconnected` is true only when we HAVE stale data but lost the
            // connection (e.g. daemon crashed or was killed by launchd).
            // When telemetry is nil — the initial state or after a clean exit
            // (CONNECTION_INVALID clears it) — EmptyStateView handles the empty
            // state at full opacity and this overlay is not shown.
            let disconnected = !client.isConnected && client.telemetry != nil

            ZStack {
                VStack(spacing: 0) {
                    if let state = client.telemetry {
                        TelemetryTable(state: state)
                    } else {
                        EmptyStateView(isConnected: client.isConnected)
                    }

                    Divider()
                    CpuHistoryChart(history: client.cpuHistory)
                }
                .opacity(disconnected ? 0.4 : 1.0)
                .disabled(disconnected)

                if disconnected {
                    DisconnectedOverlay()
                }
            }
            .animation(.easeInOut(duration: 0.3), value: disconnected)
        }
        .background(Color(NSColor.windowBackgroundColor))
        // Flexible sizing — NSPanel handles actual window constraints.
        .frame(minWidth: 440, idealWidth: 560, maxWidth: .infinity)
        .frame(minHeight: 300, idealHeight: 560, maxHeight: .infinity)
    }
}

// MARK: - Header Bar

private struct HeaderBar: View {
    let client: XPCClient

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: "gauge.with.dots.needle.67percent")
                .font(.system(size: 14, weight: .semibold))
                .foregroundStyle(.secondary)

            VStack(alignment: .leading, spacing: 1) {
                if let state = client.telemetry, !state.activeApp.isEmpty {
                    Text(state.activeApp)
                        .font(.system(.body, weight: .semibold))
                        .foregroundStyle(.primary)
                        .lineLimit(1)
                    if !state.battery.isEmpty {
                        Text(state.battery)
                            .font(.system(size: 11))
                            .foregroundStyle(.secondary)
                    }
                } else {
                    Text("Allocate")
                        .font(.system(.body, weight: .semibold))
                        .foregroundStyle(.primary)
                }
            }

            Spacer()
            if client.isPaused {
                Text("PAUSED")
                    .font(.system(size: 9, weight: .bold))
                    .foregroundStyle(.black)
                    .padding(.horizontal, 5)
                    .padding(.vertical, 2)
                    .background(Color.yellow.opacity(0.85), in: Capsule())
                    .transition(.opacity.combined(with: .scale(scale: 0.8)))
            }
            ConnectionBadge(isConnected: client.isConnected)
        }
        .animation(.spring(duration: 0.25), value: client.isPaused)
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
    }
}

// MARK: - Telemetry Table

private struct TelemetryTable: View {
    let state: TelemetryState
    @Environment(XPCClient.self) private var client
    @State private var sortOrder = [KeyPathComparator(\ProcessMetrics.id)]
    @State private var selection: Set<ProcessMetrics.ID> = []

    /// The frontmost row is pinned first; background hogs follow sorted by
    /// the user's chosen column.  The frontmost row carries real CPU/RAM/threads
    /// data from the daemon — no synthetic placeholder needed.
    var displayRows: [ProcessMetrics] {
        let fg   = state.processes.first { $0.isForeground }
        let hogs = state.processes.filter { !$0.isForeground }.sorted(using: sortOrder)
        guard let fg else { return hogs }
        return [fg] + hogs
    }

    var body: some View {
        Table(displayRows, selection: $selection, sortOrder: $sortOrder) {
            TableColumn("#", value: \.id) { row in
                Text(row.isForeground ? "▶" : "\(row.id)")
                    .font(.body.monospacedDigit())
                    .foregroundStyle(row.isForeground ? Color.blue : Color.secondary)
            }
            .width(24)

            TableColumn("Process", value: \.name) { row in
                HStack(spacing: 6) {
                    Text(row.name)
                        .font(.body)
                        .lineLimit(1)
                        .foregroundStyle(nameColor(for: row))
                    if row.isForeground {
                        badge("IN FOCUS",  color: .blue)
                    } else if row.isForcedE {
                        badge("MANUAL E",  color: .purple)
                    } else if row.isForcedP {
                        badge("MANUAL P",  color: .green)
                    } else if row.isThrottled {
                        badge("AUTO E",    color: .orange)
                    }
                }
            }
            .width(min: 120, ideal: 180)

            TableColumn("CPU", value: \.cpu) { row in
                Text(row.cpu)
                    .font(.body.monospacedDigit())
                    .foregroundStyle(row.isForeground ? Color.secondary : cpuColor(row.cpu))
                    .frame(maxWidth: .infinity, alignment: .trailing)
            }
            .width(64)

            TableColumn("RAM", value: \.ram) { row in
                Text(row.ram)
                    .font(.body.monospacedDigit())
                    .foregroundStyle(row.isForeground ? Color.secondary : Color.primary)
                    .frame(maxWidth: .infinity, alignment: .trailing)
            }
            .width(70)

            TableColumn("Threads", value: \.threads) { row in
                Text(row.threads)
                    .font(.body.monospacedDigit())
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, alignment: .trailing)
            }
            .width(58)
        }
        .tableStyle(.inset(alternatesRowBackgrounds: true))
        .animation(.easeInOut(duration: 0.2), value: state.processes.map(\.name))
        .contextMenu(forSelectionType: ProcessMetrics.ID.self) { ids in
            if let id = ids.first, let row = displayRows.first(where: { $0.id == id }) {
                Button("Force E-Core (Throttle)") {
                    client.sendOverride(pid: row.pid, action: "force_e")
                }
                Button("Force P-Core (Whitelist)") {
                    client.sendOverride(pid: row.pid, action: "force_p")
                }
                Divider()
                Button("Clear Override") {
                    client.sendOverride(pid: row.pid, action: "clear")
                }
            }
        }
    }

    @ViewBuilder
    private func badge(_ text: String, color: Color) -> some View {
        Text(text)
            .font(.system(size: 9, weight: .bold))
            .foregroundStyle(.white)
            .padding(.horizontal, 5)
            .padding(.vertical, 2)
            .background(color, in: Capsule())
    }

    private func nameColor(for row: ProcessMetrics) -> Color {
        if row.isForeground { return .blue }
        if row.isForcedE    { return .purple }
        if row.isForcedP    { return .green }
        if row.isThrottled  { return .orange }
        return .primary
    }

    private func cpuColor(_ cpuStr: String) -> Color {
        let num = Double(cpuStr
            .replacingOccurrences(of: "%", with: "")
            .trimmingCharacters(in: .whitespaces)) ?? 0
        if num >= 20 { return .red }
        if num >= 5  { return .orange }
        return .primary
    }
}

// MARK: - CPU History Chart

private struct CpuHistoryChart: View {
    let history: [Double]

    private struct Sample: Identifiable {
        let id: Int
        let value: Double
    }

    private var samples: [Sample] {
        history.enumerated().map { Sample(id: $0.offset, value: $0.element) }
    }

    var body: some View {
        Chart(samples) { s in
            AreaMark(
                x: .value("t", s.id),
                y: .value("CPU", s.value)
            )
            .foregroundStyle(
                LinearGradient(
                    colors: [.blue.opacity(0.25), .blue.opacity(0.04)],
                    startPoint: .top, endPoint: .bottom
                )
            )
            .interpolationMethod(.catmullRom)

            LineMark(
                x: .value("t", s.id),
                y: .value("CPU", s.value)
            )
            .foregroundStyle(.blue)
            .lineStyle(StrokeStyle(lineWidth: 1.5))
            .interpolationMethod(.catmullRom)
        }
        .chartYScale(domain: 0...100)
        .chartXAxis(.hidden)
        .chartYAxis {
            AxisMarks(values: [0, 50, 100]) { value in
                AxisGridLine()
                AxisValueLabel {
                    if let v = value.as(Double.self) {
                        Text("\(Int(v))%")
                            .font(.system(size: 9))
                            .foregroundStyle(.secondary)
                    }
                }
            }
        }
        .frame(height: 60)
        .padding(.horizontal, 14)
        .padding(.vertical, 6)
    }
}

// MARK: - Connection Badge

private struct ConnectionBadge: View {
    let isConnected: Bool

    var body: some View {
        Label(
            isConnected ? "Live" : "No daemon",
            systemImage: isConnected ? "circle.fill" : "circle.dotted"
        )
        .font(.system(size: 11, weight: .medium))
        .foregroundStyle(isConnected ? .green : .secondary)
        .labelStyle(TrailingIconLabelStyle())
        .animation(.spring(duration: 0.3), value: isConnected)
    }
}

private struct TrailingIconLabelStyle: LabelStyle {
    func makeBody(configuration: Configuration) -> some View {
        HStack(spacing: 4) {
            configuration.title
            configuration.icon.imageScale(.small)
        }
    }
}

// MARK: - Disconnected Overlay

/// Shown centred over stale content when the XPC connection drops mid-session
/// (CONNECTION_INTERRUPTED — daemon crashed or launchd killed it).
/// Deliberately plain: no icon, no colour, no bold weight.
private struct DisconnectedOverlay: View {
    var body: some View {
        Text("Daemon disconnected")
            .font(.body)
            .foregroundStyle(.secondary)
    }
}

// MARK: - Empty State

private struct EmptyStateView: View {
    let isConnected: Bool

    var body: some View {
        VStack(spacing: 10) {
            Image(systemName: isConnected
                  ? "antenna.radiowaves.left.and.right"
                  : "antenna.radiowaves.left.and.right.slash")
                .font(.system(size: 28, weight: .light))
                .foregroundStyle(.secondary)
                .symbolEffect(.pulse, isActive: isConnected)

            Text(isConnected
                 ? "Waiting for app switch…"
                 : "Daemon not running")
                .font(.system(.callout, design: .rounded))
                .foregroundStyle(.secondary)

            if !isConnected {
                Text("run scripts/install_agent.sh")
                    .font(.system(size: 11, design: .monospaced))
                    .foregroundStyle(.tertiary)
            }
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 40)
    }
}

// MARK: - Preview

#Preview {
    let client = XPCClient()
    return ContentView()
        .environment(client)
}
