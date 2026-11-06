import LinkPresentation
import SwiftUI
import UIKit
import UniformTypeIdentifiers

@testable import AxonIncomingShare

/// Simulator-only sender; no Keychain, App Group, server, or account access.
@main
final class SourceApp: UIResponder, UIApplicationDelegate {
  var window: UIWindow?
  func application(
    _ application: UIApplication,
    didFinishLaunchingWithOptions options: [UIApplication.LaunchOptionsKey: Any]?
  ) -> Bool {
    let window = UIWindow(frame: UIScreen.main.bounds)
    window.rootViewController = SourceController()
    window.makeKeyAndVisible()
    self.window = window
    return true
  }
}

private final class DecoratedURL: NSObject, UIActivityItemSource {
  let url = URL(string: "https://example.org/")!
  func activityViewControllerPlaceholderItem(_ controller: UIActivityViewController) -> Any { url }
  func activityViewController(
    _ controller: UIActivityViewController,
    itemForActivityType type: UIActivity.ActivityType?
  ) -> Any? { url }
  func activityViewControllerLinkMetadata(_ controller: UIActivityViewController) -> LPLinkMetadata?
  {
    let metadata = LPLinkMetadata()
    metadata.url = url
    metadata.originalURL = url
    metadata.title = "Sender decoration is not proof of delivered metadata"
    metadata.imageProvider = NSItemProvider(object: SourceController.image())
    return metadata
  }
}

private final class SourceController: UIViewController {
  static func image() -> UIImage {
    UIGraphicsImageRenderer(size: CGSize(width: 16, height: 16)).image { context in
      UIColor.systemBlue.setFill()
      context.fill(CGRect(x: 0, y: 0, width: 16, height: 16))
    }
  }
  override func viewDidLoad() {
    super.viewDidLoad()
    view.backgroundColor = .systemBackground
    let stack = UIStackView()
    stack.axis = .vertical
    stack.spacing = 20
    stack.translatesAutoresizingMaskIntoConstraints = false
    for (name, selector) in [
      ("source-text", #selector(text)), ("source-url", #selector(url)),
      ("source-decoration", #selector(decoration)), ("source-image", #selector(imageOnly)),
      ("source-image-alternative", #selector(imageAlternative)),
      ("source-multiple-images", #selector(multipleImages)),
      ("source-file", #selector(file)), ("source-video", #selector(video)),
      ("source-photo-size", #selector(photoSize)),
      ("source-photo-size-single", #selector(singlePhotoSize)),
    ] {
      let button = UIButton(type: .system)
      button.setTitle(name, for: .normal)
      button.accessibilityIdentifier = name
      button.addTarget(self, action: selector, for: .touchUpInside)
      stack.addArrangedSubview(button)
    }
    view.addSubview(stack)
    NSLayoutConstraint.activate([
      stack.leadingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.leadingAnchor, constant: 20),
      stack.trailingAnchor.constraint(
        equalTo: view.safeAreaLayoutGuide.trailingAnchor, constant: -20),
      stack.centerYAnchor.constraint(equalTo: view.centerYAnchor),
    ])
  }
  private func show(_ controller: UIActivityViewController) {
    controller.popoverPresentationController?.sourceView = view
    controller.popoverPresentationController?.sourceRect = CGRect(x: 1, y: 1, width: 1, height: 1)
    present(controller, animated: true)
  }
  @objc private func text() {
    show(
      UIActivityViewController(
        activityItems: ["Synthetic text https://example.org/"], applicationActivities: nil))
  }
  @objc private func url() {
    show(
      UIActivityViewController(
        activityItems: [URL(string: "https://example.org/")!], applicationActivities: nil))
  }
  @objc private func decoration() {
    show(UIActivityViewController(activityItems: [DecoratedURL()], applicationActivities: nil))
  }
  @objc private func imageOnly() {
    show(UIActivityViewController(activityItems: [Self.image()], applicationActivities: nil))
  }
  @objc private func photoSize() { showPhotoComposer(count: 2) }
  @objc private func singlePhotoSize() { showPhotoComposer(count: 1) }
  private func showPhotoComposer(count: Int) {
    do {
      let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
      let store = try ShareStore(root: root)
      let session = ShareSession(
        installation: "fixture", server: "https://photo-size-fixture.invalid",
        credentialAvailable: true, expiresAt: nil)
      try store.publish(session, credential: {})
      let config = URLSessionConfiguration.ephemeral
      config.protocolClasses = [PhotoSizeFixtureProtocol.self]
      let model = ShareComposer(
        transport: ShareHTTP(configuration: config),
        prepare: {
          .init(store: store, credential: { (session, "synthetic-token") })
        },
        close: { [weak self] in
          self?.dismiss(animated: true)
          try? FileManager.default.removeItem(at: root)
        })
      let image = UIGraphicsImageRenderer(size: CGSize(width: 2400, height: 1600)).image {
        context in
        UIColor.systemBlue.setFill()
        context.fill(CGRect(x: 0, y: 0, width: 2400, height: 1600))
      }
      let item = NSExtensionItem()
      item.attachments = (0..<count).map { _ in NSItemProvider(object: image) }
      let host = UIHostingController(rootView: ShareComposeView(model: model))
      host.isModalInPresentation = true
      present(host, animated: true)
      model.start([item])
    } catch { assertionFailure("Could not prepare isolated photo fixture") }
  }
  @objc private func multipleImages() {
    show(
      UIActivityViewController(
        activityItems: [Self.image(), Self.image()], applicationActivities: nil))
  }
  private func unsupported(_ type: UTType) {
    let provider = NSItemProvider()
    provider.registerDataRepresentation(forTypeIdentifier: type.identifier, visibility: .all) {
      completion in
      completion(Data("synthetic unsupported file".utf8), nil)
      return nil
    }
    show(
      UIActivityViewController(
        activityItemsConfiguration: UIActivityItemsConfiguration(itemProviders: [provider])))
  }
  @objc private func file() { unsupported(.pdf) }
  @objc private func video() { unsupported(.mpeg4Movie) }
  @objc private func imageAlternative() {
    // The image is an alternative on the URL provider, not a second photo.
    let provider = NSItemProvider(object: URL(string: "https://example.org/")! as NSURL)
    let png = Self.image().pngData()!
    provider.registerDataRepresentation(forTypeIdentifier: UTType.png.identifier, visibility: .all)
    {
      completion in
      completion(png, nil)
      return nil
    }
    show(
      UIActivityViewController(
        activityItemsConfiguration: UIActivityItemsConfiguration(itemProviders: [provider])))
  }
}

/// No shared credentials, real server, or outbound requests in the photo UI fixture.
private final class PhotoSizeFixtureProtocol: URLProtocol {
  override class func canInit(with request: URLRequest) -> Bool {
    request.url?.host == "photo-size-fixture.invalid"
  }
  override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
  override func startLoading() {
    client?.urlProtocol(
      self,
      didReceive: HTTPURLResponse(
        url: request.url!, statusCode: 200,
        httpVersion: "HTTP/1.1", headerFields: ["Content-Type": "application/json"])!,
      cacheStoragePolicy: .notAllowed)
    client?.urlProtocol(self, didLoad: Data("{\"data\":[]}".utf8))
    client?.urlProtocolDidFinishLoading(self)
  }
  override func stopLoading() {}
}
