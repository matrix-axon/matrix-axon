import Foundation

/// A bounded, staggered connection race. The serial queue owns all attempts,
/// timers and winner selection; start/stop must be nonblocking on that queue.
/// No HTTP request is sent until the caller receives the winning connection.
final class PreviewConnectionRace<Connection> {
  typealias Start = (Connection, DispatchQueue, @escaping (Result<Void, Error>) -> Void) -> Void
  private let connections: [Connection]
  private let queue: DispatchQueue
  private let start: Start
  private let stop: (Connection) -> Void
  private let delay: TimeInterval
  private let result = ShareIntakeResult<Connection>()
  private var started = 0
  private var completed: Set<Int> = []
  private var stopped: Set<Int> = []
  private var selected: Int?
  private var finished = false
  private var fallback: DispatchWorkItem?
  private var deadline = DispatchTime.now()
  private var cancellation: ShareCancellation?

  init(
    _ connections: [Connection], queue: DispatchQueue, delay: TimeInterval = 0.25,
    start: @escaping Start, stop: @escaping (Connection) -> Void
  ) {
    self.connections = connections
    self.queue = queue
    self.delay = delay
    self.start = start
    self.stop = stop
  }

  func connect(deadline: DispatchTime, cancellation: ShareCancellation) throws -> Connection {
    guard (1...2).contains(connections.count) else { throw ShareFailure.previewUnavailable }
    try cancellation.check()
    _ = try PreviewNetwork.remaining(deadline)
    queue.async {
      self.deadline = deadline
      self.cancellation = cancellation
      self.startNext()
    }
    do {
      let connection = try result.wait(until: deadline, cancellation: cancellation)
      queue.sync { cleanUp(keepWinner: true) }
      return connection
    } catch {
      queue.sync { cleanUp(keepWinner: false) }
      throw error
    }
  }

  private func startNext() {
    guard !finished, started < connections.count else { return }
    do {
      try cancellation?.check()
      _ = try PreviewNetwork.remaining(deadline)
    } catch {
      cleanUp(keepWinner: false)
      result.finish(.failure(error))
      return
    }
    let index = started
    started += 1
    start(connections[index], queue) { [weak self] outcome in
      guard let self else { return }
      self.queue.async { self.complete(index, outcome: outcome) }
    }
    if started < connections.count {
      let work = DispatchWorkItem { [weak self] in self?.startNext() }
      fallback = work
      queue.asyncAfter(deadline: .now() + delay, execute: work)
    }
  }

  private func complete(_ index: Int, outcome: Result<Void, Error>) {
    guard !finished, completed.insert(index).inserted else { return }
    switch outcome {
    case .success:
      selected = index
      cleanUp(keepWinner: true)
      result.finish(.success(connections[index]))
    case .failure(let error):
      if started < connections.count {
        fallback?.cancel()
        startNext()
      } else if completed.count == connections.count {
        cleanUp(keepWinner: false)
        result.finish(.failure(error))
      }
    }
  }

  private func cleanUp(keepWinner: Bool) {
    finished = true
    fallback?.cancel()
    fallback = nil
    for index in connections.indices where !keepWinner || index != selected {
      if stopped.insert(index).inserted { stop(connections[index]) }
    }
  }
}
