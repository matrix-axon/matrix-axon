import LinkPresentation
import UIKit
import UniformTypeIdentifiers

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
