// main.swift — the Kitae app entry point. Keeps the NSApplication run loop
// on the main thread; everything else hangs off AppDelegate.

import AppKit

let app = NSApplication.shared
let delegate = AppDelegate()
app.delegate = delegate
app.setActivationPolicy(.regular)
app.run()
