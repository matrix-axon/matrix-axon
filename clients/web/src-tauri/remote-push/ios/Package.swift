// swift-tools-version:5.3

import PackageDescription

let package = Package(
  name: "tauri-plugin-remote-push",
  platforms: [
    .iOS(.v13)
  ],
  products: [
    .library(
      name: "tauri-plugin-remote-push",
      type: .static,
      targets: ["tauri-plugin-remote-push", "AxonLaunchPayload"])
  ],
  dependencies: [
    .package(name: "Tauri", path: "../.tauri/tauri-api")
  ],
  targets: [
    // SwiftPM rejects a target that mixes Swift and Objective-C, so the
    // launch-payload capture is a target of its own.
    .target(
      name: "AxonLaunchPayload",
      path: "Sources/AxonLaunchPayload"),
    .target(
      name: "tauri-plugin-remote-push",
      dependencies: [
        .byName(name: "Tauri"),
        .byName(name: "AxonLaunchPayload"),
      ],
      path: "Sources/tauri-plugin-remote-push")
  ]
)
