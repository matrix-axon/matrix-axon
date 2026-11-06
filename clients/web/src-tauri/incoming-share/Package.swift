// swift-tools-version:5.7
import PackageDescription

let package = Package(
  name: "AxonIncomingShare",
  platforms: [.iOS(.v15), .macOS(.v12)],
  products: [.library(name: "AxonIncomingShare", targets: ["AxonIncomingShare"])],
  dependencies: [.package(url: "https://github.com/scinfu/SwiftSoup.git", exact: "2.14.3")],
  targets: [
    .target(name: "AxonIncomingShare", dependencies: ["SwiftSoup"]),
    .testTarget(name: "AxonIncomingShareTests", dependencies: ["AxonIncomingShare"]),
  ]
)
