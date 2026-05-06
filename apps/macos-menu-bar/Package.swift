// swift-tools-version: 5.9

import PackageDescription

let package = Package(
    name: "CodexBridgeMenuBar",
    platforms: [
        .macOS(.v13)
    ],
    products: [
        .executable(
            name: "CodexBridgeMenuBar",
            targets: ["CodexBridgeMenuBar"]
        )
    ],
    targets: [
        .executableTarget(
            name: "CodexBridgeMenuBar"
        )
    ]
)
