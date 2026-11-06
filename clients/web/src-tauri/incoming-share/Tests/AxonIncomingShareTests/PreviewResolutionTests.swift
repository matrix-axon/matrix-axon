import Foundation
import XCTest

@testable import AxonIncomingShare

final class PreviewResolutionTests: XCTestCase {
  func testUntrustedOperatorNAT64AnswersNeverReachTheTransport() throws {
    // These globally scoped operator addresses encode private IPv4 targets.
    // The old scope-only check admitted them to the connection boundary.
    for text in [
      "2606:4700:1234:5678::a00:1", "2606:4700:1234:5678::7f00:1",
      "2606:4700:1234:5678::a9fe:a9fe",
    ] {
      let address = try XCTUnwrap(PreviewNetwork.Address.parse(text))
      XCTAssertTrue(address.isPublic, "Global IPv6 scope alone is not authorization")
      for kind: PreviewNetwork.Kind in [.page, .image] {
        var connected = false
        XCTAssertThrowsError(
          try PreviewNetwork.fetch(
            URL(string: "https://example.org/")!, kind: kind, deadline: .now() + 1,
            cancellation: ShareCancellation(), resolve: { _, _, _ in [address] },
            exchange: { url, _, _, _, _, _ in
              connected = true
              return .init(status: 200, headers: [:], body: Data(), url: url)
            }))
        XCTAssertFalse(connected)
      }
    }
  }
  private func answer(_ text: String?) -> ShareIntakeResult<PreviewNetwork.Address?> {
    let result = ShareIntakeResult<PreviewNetwork.Address?>()
    result.finish(.success(text.flatMap(PreviewNetwork.Address.parse)))
    return result
  }

  func testPublicAWithOptionalNAT64RemainsSupported() throws {
    for (probe, expected) in [
      (nil, ["8.8.4.4"]),
      ("64:ff9b::c000:aa", ["64:ff9b::808:404", "8.8.4.4"]),
      ("2606:4700:1234:5678::c000:aa", ["2606:4700:1234:5678::808:404", "8.8.4.4"]),
    ] {
      let addresses = try PreviewNetwork.resolvedAddresses(
        [answer("8.8.4.4"), answer(probe)], deadline: .now() + 1, cancellation: ShareCancellation())
      XCTAssertEqual(addresses.map(\.text), expected)
      XCTAssertTrue(addresses.allSatisfy(\.approvedForConnection))
    }
  }

  func testSlowDiscoveryKeepsPublicAWithinSharedBudget() throws {
    let missing = ShareIntakeResult<PreviewNetwork.Address?>()
    let start = Date()
    let addresses = try PreviewNetwork.resolvedAddresses(
      [answer("1.1.1.1"), missing], deadline: .now() + 0.02, cancellation: ShareCancellation())
    XCTAssertEqual(addresses.map(\.text), ["1.1.1.1"])
    XCTAssertLessThan(Date().timeIntervalSince(start), 0.5)
    XCTAssertThrowsError(
      try PreviewNetwork.resolvedAddresses(
        [missing, answer("64:ff9b::c000:aa")], deadline: .now() + 0.01,
        cancellation: ShareCancellation())
    ) { XCTAssertEqual($0 as? ShareFailure, .expired) }
  }

  func testNoPublicAFailsAndCancellationDoesNotReturnPartialResults() throws {
    let cancellation = ShareCancellation()
    cancellation.cancel()
    XCTAssertThrowsError(
      try PreviewNetwork.resolvedAddresses(
        [answer("1.1.1.1"), answer(nil)], deadline: .now() + 1, cancellation: cancellation)
    ) {
      XCTAssertEqual($0 as? ShareFailure, .cancelled)
    }
    for probe in [nil, "64:ff9b::c000:aa", "2606:4700:4700::1111"] {
      XCTAssertThrowsError(
        try PreviewNetwork.resolvedAddresses(
          [answer(nil), answer(probe)], deadline: .now() + 1, cancellation: ShareCancellation())
      ) {
        XCTAssertEqual($0 as? ShareFailure, .publicIPv4Required)
      }
    }
  }

  func testPrivateOrWrongFamilySourceAFailsClosed() throws {
    for source in ["127.0.0.1", "10.0.0.1", "169.254.169.254", "2606:4700:4700::1111"] {
      XCTAssertThrowsError(
        try PreviewNetwork.resolvedAddresses(
          [answer(source), answer("64:ff9b::c000:aa")], deadline: .now() + 1,
          cancellation: ShareCancellation())
      ) {
        XCTAssertEqual($0 as? ShareFailure, .previewUnavailable)
      }
    }
  }

  func testOptionalDiscoveryErrorsNeverAuthorizeRawIPv6() throws {
    for failure: ShareFailure in [.expired, .unavailable, .previewUnavailable] {
      let probe = ShareIntakeResult<PreviewNetwork.Address?>()
      probe.finish(.failure(failure))
      XCTAssertEqual(
        try PreviewNetwork.resolvedAddresses(
          [answer("1.1.1.1"), probe], deadline: .now() + 1,
          cancellation: ShareCancellation()
        ).map(\.text), ["1.1.1.1"])
    }
    XCTAssertEqual(
      try PreviewNetwork.resolvedAddresses(
        [answer("1.1.1.1"), answer("2606:4700:4700::1111")], deadline: .now() + 1,
        cancellation: ShareCancellation()
      ).map(\.text), ["1.1.1.1"])
  }

  func testRedirectCannotIntroduceAnOperatorMapping() throws {
    var lookups = 0
    var connections = 0
    XCTAssertThrowsError(
      try PreviewNetwork.fetch(
        URL(string: "https://example.org/")!, kind: .page, deadline: .now() + 1,
        cancellation: ShareCancellation(),
        resolve: { _, _, _ in
          lookups += 1
          return [
            PreviewNetwork.Address.parse(lookups == 1 ? "1.1.1.1" : "2606:4700:1234:5678::a00:1")!
          ]
        },
        exchange: { url, _, _, _, _, _ in
          connections += 1
          return .init(
            status: 302, headers: ["location": "https://other.example.org/"], body: Data(), url: url
          )
        }))
    XCTAssertEqual(lookups, 2)
    XCTAssertEqual(connections, 1)
    XCTAssertThrowsError(
      try PreviewNetwork.exchange(
        URL(string: "https://example.org/")!, host: "example.org",
        addresses: [PreviewNetwork.Address.parse("2606:4700:1234:5678::a00:1")!], kind: .page,
        deadline: .now() + 1, cancellation: ShareCancellation())
    ) {
      XCTAssertEqual($0 as? ShareFailure, .previewUnavailable)
    }
  }

  func testUnsafeOrOversizedCandidateListNeverReachesExchange() throws {
    let publicAddress = PreviewNetwork.Address.parse("1.1.1.1")!
    for addresses in [
      [], [publicAddress, PreviewNetwork.Address.parse("127.0.0.1")!],
      [publicAddress, publicAddress, publicAddress],
    ] {
      XCTAssertThrowsError(
        try PreviewNetwork.fetch(
          URL(string: "https://example.org")!, kind: .page, deadline: .now() + 1,
          cancellation: ShareCancellation(), resolve: { _, _, _ in addresses },
          exchange: { _, _, _, _, _, _ in
            XCTFail("Unsafe candidates must not reach the connection race")
            throw ShareFailure.invalid
          }))
    }
  }

  func testResolutionCannotResetRequestDeadline() throws {
    XCTAssertThrowsError(
      try PreviewNetwork.fetch(
        URL(string: "https://example.org")!, kind: .page, deadline: .now() + 0.01,
        cancellation: ShareCancellation(),
        resolve: { _, _, _ in
          Thread.sleep(forTimeInterval: 0.02)
          return [PreviewNetwork.Address.parse("1.1.1.1")!]
        },
        exchange: { _, _, _, _, _, _ in
          XCTFail("Resolution consumed the original deadline")
          throw ShareFailure.invalid
        })
    ) { XCTAssertEqual($0 as? ShareFailure, .expired) }
  }
}
