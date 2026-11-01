// swift-tools-version:5.7
import PackageDescription

let package = Package(
  name: "AxonIncomingShare",
  platforms: [.iOS(.v15), .macOS(.v12)],
  products: [.library(name: "AxonIncomingShare", targets: ["AxonIncomingShare"])],
  targets: [
    .target(name: "AxonIncomingShare"),
    .testTarget(name: "AxonIncomingShareTests", dependencies: ["AxonIncomingShare"]),
  ]
)
