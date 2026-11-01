// swift-tools-version:5.7

import PackageDescription

let package = Package(
  name: "tauri-plugin-native-auth",
  platforms: [
    // Match the app and the extension-safe sharing module.
    .iOS(.v15), .macOS(.v12)
  ],
  products: [
    .library(
      name: "tauri-plugin-native-auth",
      type: .static,
      targets: ["tauri-plugin-native-auth"])
  ],
  dependencies: [
    .package(path: "../../incoming-share"),
    .package(name: "Tauri", path: "../.tauri/tauri-api")
  ],
  targets: [
    .target(
      name: "tauri-plugin-native-auth",
      dependencies: [
        .product(name: "AxonIncomingShare", package: "incoming-share"),
        .byName(name: "Tauri")
      ],
      path: "Sources")
  ]
)
