import Foundation
import XCTest

@testable import AxonIncomingShare

final class PreviewConnectionRaceTests: XCTestCase {
  private final class Probe {
    enum Behavior {
      case hanging
      case ready(TimeInterval)
      case failure(TimeInterval)
    }
    let queue = DispatchQueue(label: "preview-race-test")
    let behaviors: [Behavior]
    // The same serial queue as the production race owns these observations.
    private var starts: [Int] = []
    private var stops: [Int] = []
    init(_ behaviors: [Behavior]) { self.behaviors = behaviors }
    func race(delay: TimeInterval = 0.25) -> PreviewConnectionRace<Int> {
      PreviewConnectionRace(Array(behaviors.indices), queue: queue, delay: delay) {
        index, queue, complete in
        self.starts.append(index)
        switch self.behaviors[index] {
        case .hanging: break
        case .ready(let delay):
          queue.asyncAfter(deadline: .now() + delay) { complete(.success(())) }
        case .failure(let delay):
          queue.asyncAfter(deadline: .now() + delay) {
            complete(.failure(ShareFailure.unavailable))
          }
        }
      } stop: {
        self.stops.append($0)
      }
    }
    var started: [Int] { queue.sync { starts } }
    var stopped: [Int] { queue.sync { stops } }
  }

  func testStalledIPv6FallsBackPromptlyAndCancelsLoser() throws {
    let probe = Probe([.hanging, .ready(0)])
    let start = Date()
    XCTAssertEqual(
      try probe.race().connect(deadline: .now() + 2, cancellation: ShareCancellation()), 1)
    XCTAssertGreaterThanOrEqual(Date().timeIntervalSince(start), 0.2)
    XCTAssertLessThan(Date().timeIntervalSince(start), 1.5)
    XCTAssertEqual(probe.started, [0, 1])
    XCTAssertEqual(probe.stopped, [0])
  }

  func testFastPreferredConnectionDoesNotStartAlternate() throws {
    let probe = Probe([.ready(0), .hanging])
    XCTAssertEqual(
      try probe.race(delay: 0.02).connect(deadline: .now() + 1, cancellation: ShareCancellation()),
      0)
    let settled = expectation(description: "Cancelled fallback timer elapsed")
    probe.queue.asyncAfter(deadline: .now() + 0.04) { settled.fulfill() }
    wait(for: [settled], timeout: 1)
    XCTAssertEqual(probe.started, [0])
    XCTAssertEqual(probe.stopped, [1])
  }

  func testImmediateFailureAcceleratesFallback() throws {
    let probe = Probe([.failure(0), .ready(0)])
    let start = Date()
    XCTAssertEqual(
      try probe.race(delay: 1).connect(deadline: .now() + 0.5, cancellation: ShareCancellation()), 1
    )
    XCTAssertLessThan(Date().timeIntervalSince(start), 0.5)
    XCTAssertEqual(probe.stopped, [0])
  }

  func testSingleFamilyWorksWithoutAlternate() throws {
    let probe = Probe([.ready(0)])
    XCTAssertEqual(
      try probe.race().connect(deadline: .now() + 1, cancellation: ShareCancellation()), 0)
    XCTAssertEqual(probe.started, [0])
    XCTAssertTrue(probe.stopped.isEmpty)
  }

  func testLateReadyCannotReplaceWinnerOrRestartWork() throws {
    let probe = Probe([.ready(0.08), .ready(0)])
    let race = probe.race(delay: 0.01)
    XCTAssertEqual(try race.connect(deadline: .now() + 1, cancellation: ShareCancellation()), 1)
    let settled = expectation(description: "Late ready callback processed")
    probe.queue.asyncAfter(deadline: .now() + 0.12) { settled.fulfill() }
    wait(for: [settled], timeout: 1)
    XCTAssertEqual(probe.started, [0, 1])
    XCTAssertEqual(probe.stopped, [0])
  }

  func testAllFailuresCancelEveryConnection() throws {
    let probe = Probe([.failure(0), .failure(0)])
    XCTAssertThrowsError(
      try probe.race().connect(deadline: .now() + 1, cancellation: ShareCancellation())
    ) { XCTAssertEqual($0 as? ShareFailure, .unavailable) }
    XCTAssertEqual(probe.started, [0, 1])
    XCTAssertEqual(Set(probe.stopped), [0, 1])
  }

  func testDeadlineCancelsStalledAttempts() throws {
    let budget: TimeInterval = 1
    for delay in [0.01, budget + 1] {
      let probe = Probe([.hanging, .hanging])
      XCTAssertThrowsError(
        try probe.race(delay: delay).connect(
          deadline: .now() + budget, cancellation: ShareCancellation())
      ) { XCTAssertEqual($0 as? ShareFailure, .expired) }
      // A busy queue can exhaust the deadline before starting the alternate.
      // Every prepared connection still needs exactly one cancellation.
      XCTAssertEqual(probe.stopped.sorted(), [0, 1])
      if delay > budget { XCTAssertFalse(probe.started.contains(1)) }
    }
  }

  func testCancellationStopsInFlightAndQueuedAlternate() throws {
    let probe = Probe([.hanging, .hanging])
    let cancellation = ShareCancellation()
    probe.queue.asyncAfter(deadline: .now() + 0.02) { cancellation.cancel() }
    XCTAssertThrowsError(
      try probe.race(delay: 1).connect(deadline: .now() + 2, cancellation: cancellation)
    ) { XCTAssertEqual($0 as? ShareFailure, .cancelled) }
    XCTAssertEqual(probe.started, [0])
    XCTAssertEqual(Set(probe.stopped), [0, 1])
  }

  func testCancellationBeforeQueuedStartupPreventsDialing() throws {
    let probe = Probe([.hanging, .hanging])
    let cancellation = ShareCancellation()
    probe.queue.async { cancellation.cancel() }
    XCTAssertThrowsError(
      try probe.race().connect(deadline: .now() + 1, cancellation: cancellation)
    ) { XCTAssertEqual($0 as? ShareFailure, .cancelled) }
    XCTAssertTrue(probe.started.isEmpty)
  }

  func testCancellationStopsBothActiveFamilies() throws {
    let probe = Probe([.hanging, .hanging])
    let cancellation = ShareCancellation()
    probe.queue.asyncAfter(deadline: .now() + 0.03) { cancellation.cancel() }
    XCTAssertThrowsError(
      try probe.race(delay: 0.01).connect(deadline: .now() + 1, cancellation: cancellation)
    ) { XCTAssertEqual($0 as? ShareFailure, .cancelled) }
    XCTAssertEqual(probe.started, [0, 1])
    XCTAssertEqual(Set(probe.stopped), [0, 1])
  }

  func testDeadlineBeforeQueuedStartupPreventsDialing() throws {
    let probe = Probe([.hanging, .hanging])
    probe.queue.async { Thread.sleep(forTimeInterval: 0.03) }
    XCTAssertThrowsError(
      try probe.race().connect(deadline: .now() + 0.01, cancellation: ShareCancellation())
    ) { XCTAssertEqual($0 as? ShareFailure, .expired) }
    XCTAssertTrue(probe.started.isEmpty)
  }

  func testExpiredOrOversizedRaceDoesNotDial() throws {
    for behaviors: [Probe.Behavior] in [[], [.hanging], [.hanging, .hanging, .hanging]] {
      let probe = Probe(behaviors)
      XCTAssertThrowsError(
        try probe.race().connect(deadline: .now(), cancellation: ShareCancellation()))
      XCTAssertTrue(probe.started.isEmpty)
    }
  }
}
