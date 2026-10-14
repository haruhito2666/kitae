import Foundation
import Security

/// H6: remembered sign-ins, one Keychain generic-password item per origin.
/// The engine's `pwsave\t{origin}\t{user}\t{pass}` notification offers a
/// save; the auth sheet's "Remember" checkbox stores it; `loginform`
/// notifications look it up to prefill. The origin (scheme://authority) is
/// the `server` attribute so items from different sites never collide, and
/// the user name rides in `account` so it comes back with the secret.
enum KeychainStore {
    private static let service = "Kitae autofill"

    @discardableResult
    static func save(origin: String, user: String, password: String) -> Bool {
        delete(origin: origin)
        let attributes: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrServer as String: origin,
            kSecAttrAccount as String: user,
            kSecValueData as String: Data(password.utf8),
            // This app only: never sync the secret off the machine.
            kSecAttrSynchronizable as String: false,
        ]
        return SecItemAdd(attributes as CFDictionary, nil) == errSecSuccess
    }

    static func lookup(origin: String) -> (user: String, password: String)? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrServer as String: origin,
            kSecReturnAttributes as String: true,
            kSecReturnData as String: true,
        ]
        var item: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &item) == errSecSuccess,
              let dict = item as? [String: Any],
              let secret = dict[kSecValueData as String] as? Data,
              let password = String(data: secret, encoding: .utf8)
        else { return nil }
        let user = (dict[kSecAttrAccount as String] as? String) ?? ""
        return (user, password)
    }

    static func delete(origin: String) {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrServer as String: origin,
        ]
        SecItemDelete(query as CFDictionary)
    }
}
