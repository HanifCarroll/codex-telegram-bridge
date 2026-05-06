import Foundation

struct BridgeBinaryResolver {
    func resolve() throws -> URL {
        let candidates = candidatePaths()
        for path in candidates where FileManager.default.isExecutableFile(atPath: path) {
            return URL(fileURLWithPath: path)
        }
        throw BridgeCommandError.missingBinary(candidates)
    }

    private func candidatePaths() -> [String] {
        var paths: [String] = []
        let environment = ProcessInfo.processInfo.environment

        append(environment["CODEX_TELEGRAM_BRIDGE_BIN"], to: &paths)
        append(Bundle.main.path(forResource: "codex-telegram-bridge", ofType: nil), to: &paths)

        let executableDirectory = URL(fileURLWithPath: CommandLine.arguments[0])
            .deletingLastPathComponent()
            .path
        append("\(executableDirectory)/codex-telegram-bridge", to: &paths)

        let currentDirectory = FileManager.default.currentDirectoryPath
        append("\(currentDirectory)/../../target/debug/codex-telegram-bridge", to: &paths)
        append("\(currentDirectory)/../../target/release/codex-telegram-bridge", to: &paths)
        append("\(currentDirectory)/target/debug/codex-telegram-bridge", to: &paths)
        append("\(currentDirectory)/target/release/codex-telegram-bridge", to: &paths)

        if let home = environment["HOME"] {
            append("\(home)/.cargo/bin/codex-telegram-bridge", to: &paths)
            append("\(home)/.local/bin/codex-telegram-bridge", to: &paths)
        }

        for directory in (environment["PATH"] ?? "").split(separator: ":") {
            append("\(directory)/codex-telegram-bridge", to: &paths)
        }

        append("/opt/homebrew/bin/codex-telegram-bridge", to: &paths)
        append("/usr/local/bin/codex-telegram-bridge", to: &paths)

        return paths
    }

    private func append(_ path: String?, to paths: inout [String]) {
        guard let path, !path.isEmpty, !paths.contains(path) else {
            return
        }
        paths.append(path)
    }
}
