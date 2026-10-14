import Foundation

enum DeepLinkTarget: Equatable {
    case history
    case open(url: String, newTab: Bool)
}

enum DeepLinkParser {
    static func parse(_ value: String) -> DeepLinkTarget? {
        guard let url = URL(string: value.trimmingCharacters(in: .whitespacesAndNewlines)) else {
            return nil
        }
        return parse(url)
    }

    static func parse(_ url: URL) -> DeepLinkTarget? {
        guard url.scheme?.lowercased() == "kitae" else { return nil }
        switch url.host?.lowercased() {
        case "history":
            return .history
        case "open":
            guard let components = URLComponents(url: url, resolvingAgainstBaseURL: false),
                  let raw = components.queryItems?.first(where: { $0.name == "url" })?.value,
                  let target = URL(string: raw),
                  let scheme = target.scheme?.lowercased(),
                  ["http", "https", "about"].contains(scheme)
            else { return nil }
            if scheme == "about" && !["about:blank", "about:start", "about:history"].contains(target.absoluteString.lowercased()) {
                return nil
            }
            let newTab = components.queryItems?.first(where: { $0.name == "newTab" })?.value?
                .lowercased() == "1" || components.queryItems?.first(where: { $0.name == "newTab" })?.value?
                .lowercased() == "true"
            return .open(url: target.absoluteString, newTab: newTab)
        default:
            return nil
        }
    }
}
