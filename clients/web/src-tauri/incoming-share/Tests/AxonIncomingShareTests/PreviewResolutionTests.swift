import Foundation
import XCTest

@testable import AxonIncomingShare

final class PreviewResolutionTests: XCTestCase {
  private func answer(_ text: String?) -> ShareIntakeResult<PreviewNetwork.Address?> {
    let result = ShareIntakeResult<PreviewNetwork.Address?>()
    result.finish(.success(text.flatMap(PreviewNetwork.Address.parse)))
    return result
  }

  func testBothFamiliesAndIPv6OnlyNAT64RemainSupported() throws {
    for values: [String?] in [
      ["2606:4700:4700::1111", "1.1.1.1"],
      ["2606:4700:4700::1111", nil], [nil, "1.1.1.1"], ["64:ff9b::101:101", nil],
    ] {
      let addresses = try PreviewNetwork.resolvedAddresses(
        values.map(answer), deadline: .now() + 1, cancellation: ShareCancellation())
      XCTAssertEqual(addresses.map(\.text), values.compactMap { $0 })
    }
  }

  func testSlowFamilyKeepsOtherCompletedAnswerWithinSharedBudget() throws {
    let missing = ShareIntakeResult<PreviewNetwork.Address?>()
    let start = Date()
    let addresses = try PreviewNetwork.resolvedAddresses(
      [missing, answer("1.1.1.1")], deadline: .now() + 0.02, cancellation: ShareCancellation())
    XCTAssertEqual(addresses.map(\.text), ["1.1.1.1"])
    XCTAssertLessThan(Date().timeIntervalSince(start), 0.5)
  }

  func testNoAnswersFailAndCancellationDoesNotReturnPartialResults() throws {
    let cancellation = ShareCancellation()
    cancellation.cancel()
    XCTAssertThrowsError(
      try PreviewNetwork.resolvedAddresses(
        [answer(nil), answer("1.1.1.1")], deadline: .now() + 1, cancellation: cancellation)
    ) { XCTAssertEqual($0 as? ShareFailure, .cancelled) }
    XCTAssertThrowsError(
      try PreviewNetwork.resolvedAddresses(
        [answer(nil), answer(nil)], deadline: .now() + 1, cancellation: ShareCancellation())
    ) { XCTAssertEqual($0 as? ShareFailure, .unavailable) }
  }

  func testPrivateAnswerFailsClosedEvenWithPublicAlternate() throws {
    XCTAssertThrowsError(
      try PreviewNetwork.resolvedAddresses(
        [answer("2606:4700:4700::1111"), answer("127.0.0.1")],
        deadline: .now() + 1, cancellation: ShareCancellation())
    ) { XCTAssertEqual($0 as? ShareFailure, .previewUnavailable) }
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
