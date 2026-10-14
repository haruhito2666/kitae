import Foundation

enum TabCleanupPolicy: String, CaseIterable {
    case never
    case twelveHours
    case twentyFourHours
    case sevenDays

    var title: String {
        switch self {
        case .never: return "Never"
        case .twelveHours: return "After 12 hours"
        case .twentyFourHours: return "After 24 hours"
        case .sevenDays: return "After 7 days"
        }
    }

    var interval: TimeInterval? {
        switch self {
        case .never: return nil
        case .twelveHours: return 12 * 60 * 60
        case .twentyFourHours: return 24 * 60 * 60
        case .sevenDays: return 7 * 24 * 60 * 60
        }
    }

    var popupIndex: Int {
        switch self {
        case .never: return 0
        case .twelveHours: return 1
        case .twentyFourHours: return 2
        case .sevenDays: return 3
        }
    }

    static func fromPopupIndex(_ index: Int) -> TabCleanupPolicy {
        switch index {
        case 0: return .never
        case 2: return .twentyFourHours
        case 3: return .sevenDays
        default: return .twelveHours
        }
    }
}

enum SearchProviderSetting: Int, CaseIterable {
    case duckDuckGo = 0
    case wiby = 1
    case marginalia = 2
    case wikipedia = 3

    var title: String {
        switch self {
        case .duckDuckGo: return "DuckDuckGo Lite (No-JS)"
        case .wiby: return "Wiby (Classic Web)"
        case .marginalia: return "Marginalia (Indie Web)"
        case .wikipedia: return "Wikipedia"
        }
    }
}

enum AppVersion {
    static var short: String {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "—"
    }

    static var build: String {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleVersion") as? String ?? "—"
    }

    static var display: String {
        "Version \(short) (Build \(build))"
    }
}

final class SettingsStore {
    static let shared = SettingsStore()

    private enum Key {
        static let layoutMode = "layoutMode"
        static let appearanceMode = "appearanceMode"
        static let renderGPU = "renderGPU"
        static let motionReduced = "motionReduced"
        static let showInactiveTabCloseButtons = "showInactiveTabCloseButtons"
        static let warnBeforeClosingLastTab = "warnBeforeClosingLastTab"
        static let smoothPageScrolling = "smoothPageScrolling"
        static let inactiveTabCleanup = "inactiveTabCleanup"
        static let clearHistoryOnQuit = "clearHistoryOnQuit"
        static let alwaysShowFullURL = "alwaysShowFullURL"
        static let httpsOnly = "httpsOnly"
        static let warnInsecureHTTP = "warnInsecureHTTP"
        static let blockThirdPartyCookies = "blockThirdPartyCookies"
        static let sendDNT = "sendDNT"
        static let searchProvider = "searchProvider"
        static let extendPageColor = "extendPageColor"
        static let highlightTopLevelDomain = "highlightTopLevelDomain"
        static let showDebugBounds = "showDebugBounds"
    }

    private let defaults: UserDefaults

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        defaults.register(defaults: [
            Key.layoutMode: ChromeRootView.LayoutMode.sidebar.rawValue,
            Key.appearanceMode: AppearanceMode.light.rawValue,
            Key.renderGPU: true,
            Key.motionReduced: false,
            Key.showInactiveTabCloseButtons: false,
            Key.warnBeforeClosingLastTab: true,
            Key.smoothPageScrolling: true,
            Key.inactiveTabCleanup: TabCleanupPolicy.twelveHours.rawValue,
            Key.clearHistoryOnQuit: false,
            Key.alwaysShowFullURL: true,
            Key.httpsOnly: false,
            Key.warnInsecureHTTP: true,
            Key.blockThirdPartyCookies: true,
            Key.sendDNT: true,
            Key.searchProvider: SearchProviderSetting.duckDuckGo.rawValue,
            Key.extendPageColor: false,
            Key.highlightTopLevelDomain: true,
            Key.showDebugBounds: false,
        ])
    }

    var layoutMode: ChromeRootView.LayoutMode {
        get {
            ChromeRootView.LayoutMode(rawValue: defaults.string(forKey: Key.layoutMode) ?? "")
                ?? .sidebar
        }
        set { defaults.set(newValue.rawValue, forKey: Key.layoutMode) }
    }

    var appearanceMode: AppearanceMode {
        get { AppearanceMode(rawValue: defaults.string(forKey: Key.appearanceMode) ?? "") ?? .light }
        set { defaults.set(newValue.rawValue, forKey: Key.appearanceMode) }
    }

    var gpuEnabled: Bool {
        get { defaults.object(forKey: Key.renderGPU) as? Bool ?? true }
        set { defaults.set(newValue, forKey: Key.renderGPU) }
    }

    var motionReduced: Bool {
        get { defaults.object(forKey: Key.motionReduced) as? Bool ?? false }
        set { defaults.set(newValue, forKey: Key.motionReduced) }
    }

    var showInactiveTabCloseButtons: Bool {
        get { defaults.object(forKey: Key.showInactiveTabCloseButtons) as? Bool ?? false }
        set { defaults.set(newValue, forKey: Key.showInactiveTabCloseButtons) }
    }

    var warnBeforeClosingLastTab: Bool {
        get { defaults.object(forKey: Key.warnBeforeClosingLastTab) as? Bool ?? true }
        set { defaults.set(newValue, forKey: Key.warnBeforeClosingLastTab) }
    }

    var smoothPageScrolling: Bool {
        get { defaults.object(forKey: Key.smoothPageScrolling) as? Bool ?? true }
        set { defaults.set(newValue, forKey: Key.smoothPageScrolling) }
    }

    var tabCleanupPolicy: TabCleanupPolicy {
        get {
            TabCleanupPolicy(rawValue: defaults.string(forKey: Key.inactiveTabCleanup) ?? "")
                ?? .twelveHours
        }
        set { defaults.set(newValue.rawValue, forKey: Key.inactiveTabCleanup) }
    }

    var clearHistoryOnQuit: Bool {
        get { defaults.object(forKey: Key.clearHistoryOnQuit) as? Bool ?? false }
        set { defaults.set(newValue, forKey: Key.clearHistoryOnQuit) }
    }

    var alwaysShowFullURL: Bool {
        get { defaults.object(forKey: Key.alwaysShowFullURL) as? Bool ?? true }
        set { defaults.set(newValue, forKey: Key.alwaysShowFullURL) }
    }

    var httpsOnly: Bool {
        get { defaults.object(forKey: Key.httpsOnly) as? Bool ?? false }
        set { defaults.set(newValue, forKey: Key.httpsOnly) }
    }

    var warnInsecureHTTP: Bool {
        get { defaults.object(forKey: Key.warnInsecureHTTP) as? Bool ?? true }
        set { defaults.set(newValue, forKey: Key.warnInsecureHTTP) }
    }

    var blockThirdPartyCookies: Bool {
        get { defaults.object(forKey: Key.blockThirdPartyCookies) as? Bool ?? true }
        set { defaults.set(newValue, forKey: Key.blockThirdPartyCookies) }
    }

    var sendDNT: Bool {
        get { defaults.object(forKey: Key.sendDNT) as? Bool ?? true }
        set { defaults.set(newValue, forKey: Key.sendDNT) }
    }

    var searchProvider: SearchProviderSetting {
        get {
            SearchProviderSetting(rawValue: defaults.integer(forKey: Key.searchProvider))
                ?? .duckDuckGo
        }
        set { defaults.set(newValue.rawValue, forKey: Key.searchProvider) }
    }

    var extendPageColor: Bool {
        get { defaults.object(forKey: Key.extendPageColor) as? Bool ?? false }
        set { defaults.set(newValue, forKey: Key.extendPageColor) }
    }

    var highlightTopLevelDomain: Bool {
        get { defaults.object(forKey: Key.highlightTopLevelDomain) as? Bool ?? true }
        set { defaults.set(newValue, forKey: Key.highlightTopLevelDomain) }
    }

    var showDebugBounds: Bool {
        get { defaults.object(forKey: Key.showDebugBounds) as? Bool ?? false }
        set { defaults.set(newValue, forKey: Key.showDebugBounds) }
    }

    func reset() {
        [
            Key.layoutMode,
            Key.appearanceMode,
            Key.renderGPU,
            Key.motionReduced,
            Key.showInactiveTabCloseButtons,
            Key.warnBeforeClosingLastTab,
            Key.smoothPageScrolling,
            Key.inactiveTabCleanup,
            Key.clearHistoryOnQuit,
            Key.alwaysShowFullURL,
            Key.httpsOnly,
            Key.warnInsecureHTTP,
            Key.blockThirdPartyCookies,
            Key.sendDNT,
            Key.searchProvider,
            Key.extendPageColor,
            Key.highlightTopLevelDomain,
            Key.showDebugBounds,
        ].forEach(defaults.removeObject)
        defaults.synchronize()
    }
}
