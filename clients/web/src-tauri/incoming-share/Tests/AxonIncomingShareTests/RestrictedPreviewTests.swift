import Foundation
import XCTest

@testable import AxonIncomingShare

final class RestrictedPreviewTests: XCTestCase {
  func testSharedStaticURLFixturesMatchRecoveryUI() throws {
    struct Case: Decodable { let url: String; let eligible: Bool }
    let path = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
      .appendingPathComponent("../../../../src/incoming-share/fixtures/device-preview-urls.json")
    for entry in try JSONDecoder().decode([Case].self, from: Data(contentsOf: path)) {
      XCTAssertEqual(TextShare.devicePreviewURL(entry.url), entry.eligible, entry.url)
    }
  }
  func testDNSRebindingAndRedirectsCannotReachPrivateEndpoints() throws {
    let initial = URL(string: "https://page.example.org/a")!
    var connections: [String] = []
    var lookups = 0
    XCTAssertThrowsError(
      try PreviewNetwork.fetch(
        initial, kind: .page, deadline: .now() + 2, cancellation: ShareCancellation(),
        resolve: { host, _, _ in
          lookups += 1
          return [PreviewNetwork.Address.parse(lookups == 1 ? "8.8.8.8" : "127.0.0.1")!]
        },
        exchange: { url, host, address, _, _, _ in
          connections.append(contentsOf: address.map(\.text))
          XCTAssertEqual(host, "page.example.org")
          return .init(status: 302, headers: ["location": "/redirected"], body: Data(), url: url)
        }))
    XCTAssertEqual(lookups, 2)
    XCTAssertEqual(connections, ["8.8.8.8"])
  }
  func testLiteralPrivateRedirectRejectedBeforeSecondDNS() throws {
    var lookups = 0
    XCTAssertThrowsError(
      try PreviewNetwork.fetch(
        URL(string: "https://example.org")!, kind: .page, deadline: .now() + 2,
        cancellation: ShareCancellation(),
        resolve: { _, _, _ in
          lookups += 1
          return [PreviewNetwork.Address.parse("8.8.8.8")!]
        },
        exchange: { url, _, _, _, _, _ in
          .init(
            status: 302, headers: ["location": "https://192.168.1.1/admin"], body: Data(), url: url)
        }))
    XCTAssertEqual(lookups, 1)
  }
  func testBlockedPreviewImagePreservesTextAndNeverConnects() throws {
    let url = URL(string: "https://example.org/article")!
    var connections = 0
    let result = try RestrictedPreview.fetch(url, cancellation: ShareCancellation()) {
      target, kind, deadline, cancellation in
      try PreviewNetwork.fetch(
        target, kind: kind, deadline: deadline, cancellation: cancellation,
        resolve: { _, _, _ in [PreviewNetwork.Address.parse("8.8.8.8")!] },
        exchange: { target, _, _, _, _, _ in
          connections += 1
          return .init(
            status: 200, headers: ["content-type": "text/html"],
            body: Data(
              "<title>Keep title</title><meta property='og:image' content='https://127.0.0.1/image'>"
                .utf8), url: target)
        })
    }
    XCTAssertEqual(connections, 1)
    XCTAssertEqual(result.title, "Keep title")
    XCTAssertTrue(result.imageUnavailable)
    XCTAssertNil(result.image)
  }
  func testUnresolvableImageIsUnavailableAndKeepsText() throws {
    var connections = 0
    let result = try RestrictedPreview.fetch(
      URL(string: "https://example.org/article")!, cancellation: ShareCancellation()
    ) { target, _, _, _ in
      connections += 1
      return .init(
        status: 200, headers: ["content-type": "text/html"],
        body: Data("<title>Kept</title><meta property='og:image' content='https://['>".utf8),
        url: target)
    }
    XCTAssertEqual(connections, 1)
    XCTAssertEqual(result.title, "Kept")
    XCTAssertTrue(result.imageUnavailable)
    XCTAssertNil(result.image)
  }
  func testRelativeImageUsesValidatedTransportAndOneDeadline() throws {
    let url = URL(string: "https://example.org/path/article")!
    var urls: [URL] = []
    var deadlines: [UInt64] = []
    let result = try RestrictedPreview.fetch(url, cancellation: ShareCancellation()) {
      target, kind, deadline, _ in
      urls.append(target)
      deadlines.append(deadline.uptimeNanoseconds)
      return .init(
        status: 200, headers: ["content-type": kind == .page ? "text/html" : "image/png"],
        body: kind == .page
          ? Data(
            "<meta property='og:title' content='Title'><meta property='og:image' content='photo.png'>"
              .utf8) : Data([1, 2, 3]), url: target)
    }
    XCTAssertEqual(
      urls.map(\.absoluteString), [url.absoluteString, "https://example.org/path/photo.png"])
    XCTAssertEqual(Set(deadlines).count, 1)
    XCTAssertEqual(result.image, Data([1, 2, 3]))
  }
  func testFramingWithSplitChunksAndInterimHeaders() throws {
    var parser = PreviewHTTPParser(kind: .image)
    let response = Data(
      "HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: image/png\r\n\r\n3\r\nabc\r\n2;test=yes\r\nde\r\n0\r\n\r\n"
        .utf8)
    var done = false
    for byte in response { done = try parser.append(Data([byte])) }
    XCTAssertTrue(done)
    XCTAssertEqual(parser.body, Data("abcde".utf8))
  }
  func testOversizeAndMalformedResponsesFailClosed() throws {
    for raw in [
      "HTTP/1.1 200 OK\r\nContent-Length: 9223372036854775807\r\n\r\n",
      "HTTP/1.1 200 OK\r\nContent-Length: -1\r\n\r\n",
      "HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\nx",
      "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 1\r\n\r\nx",
      "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\n\r\nx",
      "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nabc",
      "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nFFFFFFFFFFFFFFFF\r\n",
      "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n",
    ] {
      var parser = PreviewHTTPParser(kind: .image)
      XCTAssertThrowsError(try parser.append(Data(raw.utf8), eof: true), raw)
    }
  }
  func testHTMLStopsAtHeadAndImagesRequireCompleteBodies() throws {
    var parser = PreviewHTTPParser(kind: .page)
    XCTAssertTrue(
      try parser.append(
        Data(
          "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 99999999\r\n\r\n<title>Title</title></head>ignored body"
            .utf8)))
    XCTAssertTrue(String(decoding: parser.body, as: UTF8.self).contains("Title"))
    var image = PreviewHTTPParser(kind: .image)
    XCTAssertFalse(try image.append(Data("HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nabc".utf8)))
    XCTAssertThrowsError(try image.append(Data(), eof: true))
  }
  func testCancellationPreventsAnyNetworkWork() throws {
    let cancellation = ShareCancellation()
    cancellation.cancel()
    XCTAssertThrowsError(
      try PreviewNetwork.fetch(
        URL(string: "https://example.org")!, kind: .page, deadline: .now() + 1,
        cancellation: cancellation,
        resolve: { _, _, _ in
          XCTFail("Cancelled request must not resolve")
          throw ShareFailure.invalid
        }))
  }
  func testLiveIPv6OnlyOriginReportsPublicIPv4Requirement() throws {
    guard ProcessInfo.processInfo.environment["AXON_PREVIEW_LIVE"] == "1" else {
      throw XCTSkip("Manual public-network negative-DNS probe")
    }
    XCTAssertThrowsError(try RestrictedPreview.fetch(
      URL(string: "https://ipv6.google.com/")!, cancellation: ShareCancellation())
    ) { XCTAssertEqual($0 as? ShareFailure, .publicIPv4Required) }
  }
  func testLivePublicHTTPSMetadata() throws {
    guard ProcessInfo.processInfo.environment["AXON_PREVIEW_LIVE"] == "1" else {
      throw XCTSkip("Manual public-network probe")
    }
    let cancellation = ShareCancellation()
    let url = ProcessInfo.processInfo.environment["AXON_PREVIEW_URL"] ?? "https://example.com/"
    let result = try RestrictedPreview.fetch(URL(string: url)!, cancellation: cancellation)
    XCTAssertNotNil(result.title)
    if let expected = ProcessInfo.processInfo.environment["AXON_PREVIEW_EXPECTED_TITLE"] {
      XCTAssertTrue(result.title?.localizedCaseInsensitiveContains(expected) == true)
    }
    if let image = result.image { XCTAssertNoThrow(try ShareStore.checkedImageSource(image)) }
    print(
      "PREVIEW_LIVE title=\(result.title != nil) image_bytes=\(result.image?.count ?? 0) image_unavailable=\(result.imageUnavailable)"
    )
  }
}
