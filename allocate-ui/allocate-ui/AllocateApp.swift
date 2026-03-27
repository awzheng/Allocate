// AllocateApp.swift
// allocate-ui — Phase 12.2: Standard macOS App + Native Settings
//
// Converted from a floating NSPanel HUD to a standard, dock-visible macOS app.
// • WindowGroup: normal resizable window — appears in Dock, standard z-order.
// • Settings scene: auto-wires Cmd+, and the "Settings…" menu item.
// • AppDelegate: used only to start the XPC client; no manual window management.

import SwiftUI
import AppKit

@main
struct AllocateApp: App {

    @NSApplicationDelegateAdaptor(AppDelegate.self) var appDelegate

    var body: some Scene {
        WindowGroup {
            ContentView()
                .environment(appDelegate.client)
        }
        .defaultSize(width: 560, height: 580)

        Settings {
            SettingsView()
                .environment(appDelegate.client)
        }
    }
}

@MainActor
class AppDelegate: NSObject, NSApplicationDelegate {

    var client: XPCClient = XPCClient()

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.regular)
        client.start()
    }
}
