// swift-tools-version:5.3

import PackageDescription

let package = Package(
  name: "tauri-plugin-native-auth",
  platforms: [
    // Sign in with Apple (AuthenticationServices) arrived in iOS 13; the app
    // itself requires 15 (`bundle.iOS.minimumSystemVersion`).
    .iOS(.v13)
  ],
  products: [
    .library(
      name: "tauri-plugin-native-auth",
      type: .static,
      targets: ["tauri-plugin-native-auth"])
  ],
  dependencies: [
    .package(name: "Tauri", path: "../.tauri/tauri-api")
  ],
  targets: [
    .target(
      name: "tauri-plugin-native-auth",
      dependencies: [
        .byName(name: "Tauri")
      ],
      path: "Sources")
  ]
)
