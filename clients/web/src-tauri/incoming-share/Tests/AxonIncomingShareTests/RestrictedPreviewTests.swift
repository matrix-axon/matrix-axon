import Foundation
import XCTest

@testable import AxonIncomingShare

final class RestrictedPreviewTests: XCTestCase {
  func testAddressScopeBoundaries() throws {
    for text in [
      "0.1.2.3", "10.0.0.1", "100.64.0.1", "100.127.255.255", "127.1.2.3", "169.254.169.254",
      "172.16.0.1", "172.31.255.255", "192.0.0.1", "192.0.2.1", "192.168.1.1", "198.18.0.1",
      "198.19.255.255", "198.51.100.1", "203.0.113.1", "224.0.0.1", "255.255.255.255", "::", "::1",
      "::ffff:127.0.0.1", "fe80::1", "fc00::1", "ff02::1", "2001:db8::1", "2001::1",
      "2002:7f00:1::", "3fff::1", "64:ff9b::7f00:1",
    ] {
      XCTAssertFalse(try XCTUnwrap(PreviewNetwork.Address.parse(text)).isPublic, text)
    }
    for text in [
      "1.1.1.1", "8.8.8.8", "100.63.255.255", "100.128.0.1", "172.15.255.255", "172.32.0.1",
      "198.17.255.255", "198.20.0.1", "2001:4860:4860::8888", "2606:4700:4700::1111",
      "64:ff9b::808:808",
    ] {
      XCTAssertTrue(try XCTUnwrap(PreviewNetwork.Address.parse(text)).isPublic, text)
    }
  }
  func testUnsafeURLFormsRejectedBeforeDNS() throws {
    for raw in [
      "http://example.org/", "https://127.0.0.1/", "https://[::1]/", "https://router.lan/",
      "https://service.corp/", "https://router.home/", "https://router.local./",
      "https://user:pass@example.org/", "https://example.org:65536/",
      "https://example.org%0d%0aHeader:value/", "https://-bad.example.org/", "file:///tmp/a",
    ] {
      guard let url = URL(string: raw) else { continue }
      XCTAssertThrowsError(
        try PreviewNetwork.fetch(
          url, kind: .page, deadline: .now() + 1, cancellation: ShareCancellation(),
          resolve: { _, _, _ in
            XCTFail("Blocked URLs must not reach DNS")
            throw ShareFailure.invalid
          }))
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
          return PreviewNetwork.Address.parse(lookups == 1 ? "8.8.8.8" : "127.0.0.1")!
        },
        exchange: { url, host, address, _, _, _ in
          connections.append(address.text)
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
          return PreviewNetwork.Address.parse("8.8.8.8")!
        },
        exchange: { url, _, _, _, _, _ in
          .init(
            status: 302, headers: ["location": "https://192.168.1.1/admin"], body: Data(), url: url)
        }))
    XCTAssertEqual(lookups, 1)
  }
  func testParserExtractsInertMetadataAndDecodesEntities() throws {
    let page = try RestrictedPreview.parse(
      Data(
        "<html><head><title>Fallback</title><meta property='og:title' content='A &amp; B'><meta name=description content='Details &quot;quoted&quot;'><meta property='og:site_name' content='Example'><meta property='og:image' content='/photo.png'><script src='https://127.0.0.1/script'></script><link rel=stylesheet href='https://127.0.0.1/style'></head></html>"
          .utf8))
    XCTAssertEqual(page.title, "A & B")
    XCTAssertEqual(page.description, "Details \"quoted\"")
    XCTAssertEqual(page.siteName, "Example")
    XCTAssertEqual(page.image, "/photo.png")
    XCTAssertThrowsError(
      try RestrictedPreview.parse(Data(String(repeating: "<x>", count: 513).utf8)))
  }
  func testBlockedPreviewImagePreservesTextAndNeverConnects() throws {
    let url = URL(string: "https://example.org/article")!
    var connections = 0
    let result = try RestrictedPreview.fetch(url, cancellation: ShareCancellation()) {
      target, kind, deadline, cancellation in
      try PreviewNetwork.fetch(
        target, kind: kind, deadline: deadline, cancellation: cancellation,
        resolve: { _, _, _ in PreviewNetwork.Address.parse("8.8.8.8")! },
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
