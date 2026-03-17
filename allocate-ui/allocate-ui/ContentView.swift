// ContentView.swift
// allocate-ui — Phase 6: Native Activity Monitor UI
//
// Apple HIG compliance:
//   • System background colors (.windowBackground) — auto Light/Dark
//   • SF Pro .body for labels; .monospacedDigit() on numerics only
//   • SwiftUI Table with native column headers and sorting
//   • SF Symbols for iconography
//   • Semantic colors only — no hardcoded hex

import SwiftUI

// MARK: - Root View

struct ContentView: View {
    @Environment(XPCClient.self) private var client

    var body: some View {
        VStack(spacing: 0) {
            HeaderBar(client: client)
            Divider()

            if let state = client.telemetry {
                TelemetryTable(state: state)
            } else {
                EmptyStateView(isConnected: client.isConnected)
            }
        }
        .background(Color(NSColor.windowBackgroundColor))
        .frame(width: 480)
        .frame(minHeight: 180, maxHeight: 560)
    }
}

// MARK: - Header Bar

private struct HeaderBar: View {
    let client: XPCClient

    var body: some View {
        HStack(spacing: 10) {
            // App icon + name
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
            ConnectionBadge(isConnected: client.isConnected)
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
    }
}

// MARK: - Telemetry Table

private struct TelemetryTable: View {
    let state: TelemetryState
    @State private var sortOrder = [KeyPathComparator(\ProcessMetrics.id)]

    var sortedProcesses: [ProcessMetrics] {
        state.processes.sorted(using: sortOrder)
    }

    var body: some View {
        Table(sortedProcesses, sortOrder: $sortOrder) {
            TableColumn("#", value: \.id) { row in
                Text("\(row.id)")
                    .font(.body.monospacedDigit())
                    .foregroundStyle(.secondary)
            }
            .width(24)

            TableColumn("Process", value: \.name) { row in
                Text(row.name)
                    .font(.body)
                    .lineLimit(1)
            }
            .width(min: 120, ideal: 160)

            TableColumn("CPU", value: \.cpu) { row in
                Text(row.cpu)
                    .font(.body.monospacedDigit())
                    .foregroundStyle(cpuColor(row.cpu))
                    .frame(maxWidth: .infinity, alignment: .trailing)
            }
            .width(64)

            TableColumn("RAM", value: \.ram) { row in
                Text(row.ram)
                    .font(.body.monospacedDigit())
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
    }

    /// Tints high-CPU processes orange/red, matching Activity Monitor's convention.
    private func cpuColor(_ cpuStr: String) -> Color {
        let num = Double(cpuStr.replacingOccurrences(of: "%", with: "")
            .trimmingCharacters(in: .whitespaces)) ?? 0
        if num >= 20 { return .red }
        if num >= 5  { return .orange }
        return .primary
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
            configuration.icon
                .imageScale(.small)
        }
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
