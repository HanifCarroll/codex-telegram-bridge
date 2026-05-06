import Foundation

struct BridgeStatus: Equatable {
    enum Mode: Equatable {
        case unknown
        case away
        case back
        case unavailable
    }

    var mode: Mode
    var backendHealthy: Bool?
    var backendError: String?
    var pendingNotifications: Int
    var configPath: String?
    var stateFolderPath: String?
    var detail: String

    static let loading = BridgeStatus(
        mode: .unknown,
        backendHealthy: nil,
        backendError: nil,
        pendingNotifications: 0,
        configPath: nil,
        stateFolderPath: nil,
        detail: "Loading"
    )

    init(payload: BridgeStatusPayload) {
        let isAway = payload.away?.away
        let configured = payload.configured ?? false
        let codexConfigured = payload.codexConfigured ?? false

        self.backendHealthy = payload.backend?.healthy
        self.backendError = payload.backend?.lastError
        self.pendingNotifications = payload.pending ?? 0
        self.configPath = payload.configPath
        self.stateFolderPath = payload.stateFolderPath

        if !configured || !codexConfigured {
            self.mode = .unavailable
            self.detail = "Run setup to enable Telegram control."
        } else if isAway == true {
            self.mode = .away
            self.detail = "Telegram notifications are on."
        } else if isAway == false {
            self.mode = .back
            self.detail = "Telegram notifications are off."
        } else {
            self.mode = .unknown
            self.detail = "Status is not available yet."
        }
    }

    private init(
        mode: Mode,
        backendHealthy: Bool?,
        backendError: String?,
        pendingNotifications: Int,
        configPath: String?,
        stateFolderPath: String?,
        detail: String
    ) {
        self.mode = mode
        self.backendHealthy = backendHealthy
        self.backendError = backendError
        self.pendingNotifications = pendingNotifications
        self.configPath = configPath
        self.stateFolderPath = stateFolderPath
        self.detail = detail
    }
}

struct BridgeStatusPayload: Decodable {
    let configured: Bool?
    let codexConfigured: Bool?
    let away: BridgeAwayPayload?
    let backend: BridgeBackendPayload?
    let pending: Int?
    let configPath: String?
    let stateFolderPath: String?
}

struct BridgeAwayPayload: Decodable {
    let away: Bool?
}

struct BridgeBackendPayload: Decodable {
    let healthy: Bool?
    let lastError: String?
}
