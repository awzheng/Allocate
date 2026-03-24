// AllocateApp.swift
// allocate-ui — Phase 6: SwiftUI Floating HUD Glass
//
// Refactored from MenuBarExtra to a persistent Floating HUD.
// Uses an NSPanel to float permanently above all other apps, even during
// app switches, making it perfect for live task monitoring.

import SwiftUI
import AppKit

@main
struct AllocateApp: App {

    @NSApplicationDelegateAdaptor(AppDelegate.self) var appDelegate

    var body: some Scene {
        // We use a Settings scene purely to satisfy the SwiftUI App protocol.
        // The actual floating window is completely managed by the AppDelegate.
        Settings {
            EmptyView()
        }
    }
}

@MainActor
class AppDelegate: NSObject, NSApplicationDelegate {
    
    var panel: NSPanel!
    var client: XPCClient!

    func applicationDidFinishLaunching(_ notification: Notification) {
        // Initialize the single source of truth for telemetry.
        client = XPCClient()
        client.start()
        
        let contentView = ContentView()
            .environment(client)

        // ── Floating NSPanel Configuration ────────────────────────────────────
        // .nonactivatingPanel allows clicking the HUD without stealing OS focus.
        // .titled is required for .fullSizeContentView / dragging by background.
        panel = NSPanel(
            contentRect: NSRect(x: 0, y: 0, width: 560, height: 580),
            styleMask: [
                .titled,
                .closable,
                .resizable,
                .nonactivatingPanel,
                .fullSizeContentView
            ],
            backing: .buffered,
            defer: false
        )
        panel.minSize = NSSize(width: 440, height: 300)
        
        // Float persistently over everything
        panel.level = .floating
        panel.isFloatingPanel = true
        
        // Follow the user across spaces and fullscreen apps
        panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]
        
        // Transparent Apple UI styling: let SwiftUI's .ultraThinMaterial shine
        panel.titlebarAppearsTransparent = true
        panel.titleVisibility = .hidden
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.isMovableByWindowBackground = true
        
        // Wrap the SwiftUI view
        panel.contentView = NSHostingView(rootView: contentView)
        
        // Position top right, below the menu bar
        if let screen = NSScreen.main {
            let f = screen.visibleFrame
            let x = f.maxX - panel.frame.width - 20
            let y = f.maxY - panel.frame.height - 20
            panel.setFrameOrigin(NSPoint(x: x, y: y))
        }
        
        panel.makeKeyAndOrderFront(nil)
    }
}

