import AxonIncomingShare
import UIKit

/// Development-only extension used to establish signed foundation acceptance.
/// It imports no provider data and sends nothing. Distribution excludes it.
final class ShareProbeViewController: UIViewController {
  private let status = UILabel()
  private let queue = DispatchQueue(label: "org.matrixaxon.axon.share-probe")

  override func viewDidLoad() {
    super.viewDidLoad()
    view.backgroundColor = .systemBackground
    status.numberOfLines = 0
    status.adjustsFontForContentSizeCategory = true
    status.font = .preferredFont(forTextStyle: .body)
    let inspect = UIButton(type: .system)
    inspect.setTitle("Check shared session", for: .normal)
    inspect.addTarget(self, action: #selector(check), for: .touchUpInside)
    let seed = UIButton(type: .system)
    seed.setTitle("Save synthetic recovery draft", for: .normal)
    seed.addTarget(self, action: #selector(create), for: .touchUpInside)
    let done = UIButton(type: .system)
    done.setTitle("Close without sending", for: .normal)
    done.addTarget(self, action: #selector(close), for: .touchUpInside)
    let stack = UIStackView(arrangedSubviews: [status, inspect, seed, done])
    stack.axis = .vertical
    stack.spacing = 20
    stack.translatesAutoresizingMaskIntoConstraints = false
    view.addSubview(stack)
    NSLayoutConstraint.activate([
      stack.leadingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.leadingAnchor, constant: 20),
      stack.trailingAnchor.constraint(
        equalTo: view.safeAreaLayoutGuide.trailingAnchor, constant: -20),
      stack.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor, constant: 20),
    ])
    check()
  }
  private func perform(_ action: @escaping (ShareStore, SharedSession) throws -> String) {
    queue.async {
      let message: String
      do {
        guard
          let root = FileManager.default.containerURL(
            forSecurityApplicationGroupIdentifier: SharedSession.group),
          let group = Bundle.main.object(forInfoDictionaryKey: "AxonSharedKeychainGroup") as? String
        else { throw ShareFailure.unavailable }
        let store = try ShareStore(
          root: root.appendingPathComponent("IncomingShares", isDirectory: true))
        message = try action(store, SharedSession(store: store, accessGroup: group))
      } catch {
        message =
          "Unavailable: \((error as? ShareFailure)?.rawValue ?? "storage"). Open Axon first; no send occurred."
      }
      DispatchQueue.main.async { self.status.text = message }
    }
  }
  @objc private func check() {
    perform { store, session in
      let descriptor = try store.session()
      let credential = (try? session.credential()) != nil
      return
        "App Group: accessible\nInstallation initialized: \(descriptor != nil)\nUsable access credential: \(credential)\nPending records: \(try store.list().count)\nNo content imported or sent."
    }
  }
  @objc private func create() {
    perform { store, _ in
      _ = try store.create(text: "Synthetic Axon recovery check https://example.org")
      return "Draft saved, not sent. Open Axon → Pending shares to review or discard it."
    }
  }
  @objc private func close() { extensionContext?.completeRequest(returningItems: nil) }
}
