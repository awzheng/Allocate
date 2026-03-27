// SettingsView.swift
// allocate-ui — Phase 12.2: Native Settings Window
//
// Contains the governor threshold sliders and the Pause toggle, extracted from
// ContentView so they live in a dedicated macOS Settings window (Cmd+,).

import SwiftUI

// MARK: - Settings View

struct SettingsView: View {
    @Environment(XPCClient.self) private var client

    @State private var throttleThreshold: Double = 15.0
    @State private var releaseThreshold:  Double = 5.0

    var body: some View {
        @Bindable var bindableClient = client

        Form {
            Section("Governor Thresholds") {
                ThresholdRow(label: "Throttle above", value: $throttleThreshold, range: 5...95)
                ThresholdRow(label: "Release below",  value: $releaseThreshold,  range: 1...90)
            }

            Section {
                Toggle("Pause Allocate", isOn: $bindableClient.isPaused)
                    .toggleStyle(.switch)
            }

            Section {
                OverrideList(
                    title:     "E-Core (Manual Throttle)",
                    items:     client.forcedEList,
                    color:     .purple,
                    emptyNote: "No manual E-Core assignments"
                )
                OverrideList(
                    title:     "P-Core (Whitelisted)",
                    items:     client.forcedPList,
                    color:     .green,
                    emptyNote: "No manual P-Core assignments"
                )
            } header: {
                Text("Manual Overrides")
            }
        }
        .formStyle(.grouped)
        .frame(width: 420)
        // Enforce hysteresis and send on every change.
        .onChange(of: throttleThreshold) { _, new in
            if releaseThreshold >= new {
                releaseThreshold = max(1, new - 1)
            }
            sendConfig()
        }
        .onChange(of: releaseThreshold) { _, new in
            if throttleThreshold <= new {
                throttleThreshold = min(95, new + 1)
            }
            sendConfig()
        }
        .onChange(of: client.isPaused) { _, _ in
            sendConfig()
        }
    }

    private func sendConfig() {
        client.sendConfig(
            throttleThreshold: throttleThreshold,
            releaseThreshold:  releaseThreshold,
            isPaused:          client.isPaused
        )
    }
}

// MARK: - Override List

/// A compact titled list of process names for one override set (E or P).
private struct OverrideList: View {
    let title:     String
    let items:     [String]
    let color:     Color
    let emptyNote: String

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title)
                .font(.system(size: 11, weight: .semibold))
                .foregroundStyle(color)
                .kerning(0.5)

            if items.isEmpty {
                Text(emptyNote)
                    .font(.system(size: 12))
                    .foregroundStyle(.tertiary)
                    .padding(.leading, 2)
            } else {
                ForEach(items, id: \.self) { name in
                    Label(name, systemImage: "circle.fill")
                        .font(.system(size: 12))
                        .foregroundStyle(color)
                        .labelStyle(SmallDotLabelStyle())
                }
            }
        }
        .padding(.vertical, 2)
    }
}

private struct SmallDotLabelStyle: LabelStyle {
    func makeBody(configuration: Configuration) -> some View {
        HStack(spacing: 5) {
            configuration.icon.imageScale(.small).font(.system(size: 6))
            configuration.title
        }
    }
}

// MARK: - Threshold Row

struct ThresholdRow: View {
    let label: String
    @Binding var value: Double
    let range: ClosedRange<Double>

    @State private var text: String = ""

    var body: some View {
        HStack(spacing: 8) {
            Text(label)
                .font(.system(size: 12))
                .foregroundStyle(.secondary)
                .frame(width: 110, alignment: .leading)

            Slider(value: $value, in: range, step: 5.0)
                .onChange(of: value) { _, new in
                    text = String(format: "%.0f", new)
                }

            TextField("", text: $text)
                .font(.system(size: 12).monospacedDigit())
                .multilineTextAlignment(.trailing)
                .frame(width: 46)
                .onSubmit {
                    if let d = Double(text), range.contains(d) {
                        value = d
                    } else {
                        text = String(format: "%.0f", value)
                    }
                }

            Text("%")
                .font(.system(size: 12))
                .foregroundStyle(.secondary)
        }
        .onAppear { text = String(format: "%.0f", value) }
    }
}

// MARK: - Preview

#Preview {
    let client = XPCClient()
    return SettingsView()
        .environment(client)
}
