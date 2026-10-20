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
      targets: ["tauri-plugin-remote-push"])
  ],
  dependencies: [
    .package(name: "Tauri", path: "../.tauri/tauri-api")
  ],
  targets: [
    .target(
      name: "tauri-plugin-remote-push",
      dependencies: [
        .byName(name: "Tauri")
      ],
      path: "Sources")
  ]
)
