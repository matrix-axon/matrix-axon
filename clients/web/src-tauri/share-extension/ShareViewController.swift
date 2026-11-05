import AxonIncomingShare
import ImageIO
import SwiftUI
import UIKit
import os

final class ShareViewController: UIViewController {
  private var model: ShareComposer?
  override func viewDidLoad() {
    super.viewDidLoad()
    isModalInPresentation = true
    let model = ShareComposer { [weak self] in
      self?.extensionContext?.completeRequest(returningItems: nil)
    }
    self.model = model
    let host = UIHostingController(rootView: ShareComposeView(model: model))
    addChild(host)
    host.view.translatesAutoresizingMaskIntoConstraints = false
    view.addSubview(host.view)
    NSLayoutConstraint.activate([
      host.view.leadingAnchor.constraint(equalTo: view.leadingAnchor),
      host.view.trailingAnchor.constraint(equalTo: view.trailingAnchor),
      host.view.topAnchor.constraint(equalTo: view.topAnchor),
      host.view.bottomAnchor.constraint(equalTo: view.bottomAnchor),
    ])
    host.didMove(toParent: self)
    model.start(extensionContext?.inputItems.compactMap { $0 as? NSExtensionItem } ?? [])
  }
}

/// UI state lives on main; all storage/network work has one serial owner.
final class ShareComposer: ObservableObject {
  private static let log = Logger(subsystem: "org.matrixaxon.axon", category: "share-extension")
  @Published var text = ""
  @Published var account = ""
  @Published var room = ""
  @Published var roomSearch = ""
  @Published var accounts: [ShareAccount] = []
  @Published var rooms: [ShareDestination] = []
  @Published var record: ShareManifest?
  @Published var status = "Importing share…"
  @Published var busy = true
  @Published var images: [String: UIImage] = [:]
  private let queue = DispatchQueue(label: "org.matrixaxon.axon.share-extension")
  private var store: ShareStore?
  private var sender: TextShareSender?
  private var devicePreview: DevicePreview?
  private let transport: ShareHTTP
  private let prepare: () throws -> Session
  private let cancellation = ShareCancellation()
  // Only accessed on queue.
  private var current: ShareManifest?
  private var photoImages: [String: UIImage] = [:]
  private var heartbeat: Timer?
  private let heartbeatInterval: TimeInterval
  private var editSave: DispatchWorkItem?
  private let close: () -> Void
  private var closing = false
  private var completed = false
  struct Session {
    let store: ShareStore
    let credential: () throws -> (ShareSession, String)

    static func live() throws -> Session {
      guard
        let root = FileManager.default.containerURL(
          forSecurityApplicationGroupIdentifier: SharedSession.group),
        let group = Bundle.main.object(forInfoDictionaryKey: "AxonSharedKeychainGroup") as? String
      else { throw ShareFailure.unavailable }
      let store = try ShareStore(
        root: root.appendingPathComponent("IncomingShares", isDirectory: true))
      let session = SharedSession(store: store, accessGroup: group)
      return Session(store: store, credential: { try session.credential() })
    }
  }

  init(
    transport: ShareHTTP = ShareHTTP(), prepare: @escaping () throws -> Session = Session.live,
    heartbeatInterval: TimeInterval = 60,
    close: @escaping () -> Void
  ) {
    self.transport = transport
    self.prepare = prepare
    self.heartbeatInterval = heartbeatInterval
    self.close = close
  }
  deinit {
    transport.close()
    heartbeat?.invalidate()
    editSave?.cancel()
  }

  private var matchingRooms: [ShareDestination] {
    rooms.filter { destination in
      destination.account_id == account
        && (roomSearch.isEmpty
          || destination.title.localizedCaseInsensitiveContains(roomSearch)
          || destination.room_id.localizedCaseInsensitiveContains(roomSearch))
    }
  }
  var matchingRoomCount: Int { matchingRooms.count }
  var visibleRooms: [ShareDestination] {
    Array(matchingRooms.prefix(100))
  }
  var accountTitle: String {
    accounts.first(where: { $0.account_id == account })?.user_id ?? "Choose account"
  }
  var roomTitle: String {
    rooms.first(where: { $0.account_id == account && $0.room_id == room })?.title ?? "Choose room"
  }
  func chooseAccount(_ id: String) {
    guard editable else { return }
    if account != id {
      account = id
      room = ""
      roomSearch = ""
      save()
    }
  }
  func chooseRoom(_ id: String) {
    guard editable else { return }
    room = id
    save()
  }
  var operations: [ShareOperation] {
    let files = record?.files.compactMap(\.operation) ?? []
    return files.isEmpty ? record.map { [$0.operation] } ?? [] : files
  }
  private var fixedContent: Bool {
    operations.contains { [.sent, .dispatching, .unknown].contains($0.state) }
  }
  private var owned: Bool {
    !busy && record?.owner != nil && (record?.leaseUntil ?? 0) > Date().timeIntervalSince1970
  }
  var editable: Bool {
    owned && !fixedContent
  }
  var validation: String? {
    guard var draft = record else { return "No draft" }
    draft.text = text
    let links = Set(TextShare.links(text))
    draft.previews = draft.previews?.filter { links.contains($0.matched_url) }
    return TextShare.validation(draft)
  }
  var canSend: Bool {
    owned && record?.completed == false && !room.isEmpty && !account.isEmpty && validation == nil
      && record?.importFailures.isEmpty == true
      && !operations.contains { [.unknown, .dispatching, .uploading].contains($0.state) }
      && !(record?.files.contains { $0.operation?.state == .failed } ?? false)
  }

  static func importProblem(_ category: String) -> String {
    switch category {
    case "quota", "too_many_photos":
      return "A photo or the selection exceeds the sharing limits (10 photos, 100 MiB total)."
    case "expired", "provider_timeout": return "A source took too long to provide its content."
    case "incomplete_import": return "Import was interrupted before the selection was complete."
    case "unsupported_input", "unsupported_url":
      return "The selection contains unsupported content."
    default: return "A source item could not be read or is not a supported image."
    }
  }
  func start(_ items: [NSExtensionItem]) {
    guard !closing else { return }
    run {
      let session = try self.prepare()
      let store = session.store
      self.store = store
      self.devicePreview = DevicePreview(store: store, cancellation: self.cancellation)
      let http = self.transport
      self.sender = TextShareSender(
        store: store, credential: session.credential, cancellation: self.cancellation,
        request: { descriptor, token, path, method, body, file, mime, limit in
          try http.request(
            descriptor: descriptor, token: token, path: path, method: method, body: body,
            file: file, mime: mime, limit: limit)
        })
      self.current = try ShareIntake.importItems(
        items, store: store, cancellation: self.cancellation, staged: { self.current = $0 })
      let importedText = self.current!.text
      DispatchQueue.main.async { self.text = importedText }
      return try self.loadDestinations(text: importedText)
    }
    heartbeat = Timer.scheduledTimer(withTimeInterval: heartbeatInterval, repeats: true) {
      [weak self] _ in
      guard let self, !self.busy else { return }
      self.renew()
    }
  }
  // Runs on the worker queue. A peer may explicitly discard an idle draft.
  private func refreshAfterFailure() -> Bool {
    guard let record = current, let owner = record.owner, let store else { return false }
    do {
      current = try store.refreshOwned(record.id, owner: owner)
      return false
    } catch {
      guard let failure = error as? ShareFailure, [.stale, .missingFile].contains(failure) else {
        return false
      }
      var fenced = record
      fenced.owner = nil
      fenced.leaseUntil = nil
      current = fenced
      return true
    }
  }

  private func renew() {
    queue.async {
      guard let current = self.current, current.owner != nil, let store = self.store else { return }
      do {
        try self.cancellation.check()
        let renewed = try store.update(current)
        self.current = renewed
        DispatchQueue.main.async { if !self.closing { self.record = renewed } }
      } catch {
        let fenced = self.refreshAfterFailure()
        let record = self.current
        Self.log.error(
          "share_heartbeat_failed fenced=\(fenced) category=\((error as? ShareFailure)?.rawValue ?? "storage", privacy: .public)"
        )
        DispatchQueue.main.async {
          guard !self.closing else { return }
          self.record = record
          if fenced {
            self.heartbeat?.invalidate()
            self.status = Self.explain(ShareFailure.stale)
          }
        }
      }
    }
  }
  func run(blocking: Bool = true, _ action: @escaping () throws -> String) {
    guard !closing else { return }
    if blocking { busy = true }
    queue.async {
      let message: String
      do {
        try self.cancellation.check()
        message = try action()
      } catch {
        if (try? self.cancellation.check()) != nil {
          Self.log.error(
            "share_extension_failed category=\((error as? ShareFailure)?.rawValue ?? (error as? SharePreviewFallback)?.rawValue ?? (error as? DevicePreviewFailure)?.rawValue ?? "unexpected", privacy: .public)"
          )
        }
        // A thumbnail upload may have saved a newer revision, or a peer may
        // have discarded the draft. Never leave stale Send authority enabled.
        let fenced = self.refreshAfterFailure()
        if self.current.map({ [$0.operation] + $0.files.compactMap(\.operation) })?.contains(
          where: { [.dispatching, .unknown].contains($0.state) }) == true
        {
          message =
            "Delivery outcome unknown. A dispatched message may have been delivered. Check the destination before any explicit resend in Axon."
        } else {
          message = Self.explain(
            fenced ? ShareFailure.stale : error, draftSaved: self.current != nil)
        }
      }
      let record = self.current
      var images: [String: UIImage] = [:]
      for preview in record?.previews ?? [] {
        do { try self.cancellation.check() } catch { break }
        if let record, let owner = record.owner, let file = preview.thumbnail,
          let store = self.store
        {
          images[preview.matched_url] = try? store.withVerifiedFile(
            record.id, file: file, owner: owner
          ) { url, _ in
            guard let source = CGImageSourceCreateWithURL(url as CFURL, nil),
              let image = CGImageSourceCreateThumbnailAtIndex(
                source, 0,
                [
                  kCGImageSourceCreateThumbnailFromImageAlways: true,
                  kCGImageSourceThumbnailMaxPixelSize: 480,
                  kCGImageSourceCreateThumbnailWithTransform: true,
                ] as CFDictionary)
            else { throw ShareFailure.invalid }
            return UIImage(cgImage: image)
          }
        }
      }
      let photos = record?.files.filter({ !$0.thumbnail }) ?? []
      self.photoImages = self.photoImages.filter { key, _ in photos.contains { $0.id == key } }
      for file in photos {
        do { try self.cancellation.check() } catch { break }
        if let record, let owner = record.owner, let store = self.store {
          if let cached = self.photoImages[file.id] {
            images[file.id] = cached
            continue
          }
          images[file.id] = try? store.withVerifiedFile(record.id, file: file.id, owner: owner) {
            url, _ in
            guard let image = UIImage(data: try PhotoShare.thumbnail(url)) else {
              throw ShareFailure.invalid
            }
            return image
          }
          self.photoImages[file.id] = images[file.id]
        }
      }
      DispatchQueue.main.async {
        guard !self.closing else { return }
        self.record = record
        if record?.owner == nil { self.heartbeat?.invalidate() }
        self.images = images
        // An empty completed action clears routine status; background
        // autosave must preserve actionable errors and delivery outcomes.
        if blocking || !message.isEmpty { self.status = message }
        if blocking { self.busy = false }
        // A debounce can fire while a blocking action disables saving. Retry
        // the current editor text after it settles; never revive a fenced draft.
        if blocking && self.editable && self.text != record?.text { self.edited() }
        if !message.isEmpty { UIAccessibility.post(notification: .announcement, argument: message) }
      }
    }
  }
  private static func explain(_ error: Error, draftSaved: Bool = true) -> String {
    if let failure = error as? DevicePreviewFailure {
      return failure.explanation
        + " Your original link remains sendable. You can explicitly choose Use homeserver instead."
    }
    if let fallback = error as? SharePreviewFallback {
      return fallback.explanation
        + " Your original link remains sendable without a card. You can explicitly retry Include preview."
    }
    if (error as? ShareFailure) == .pendingLimit {
      return
        "Axon already has \(ShareLimits.pendingCount) pending shares. Open Axon → Pending shares and send or discard drafts you no longer need, then share again from the source. No new draft was saved and nothing was sent."
    }
    if !draftSaved {
      let reason: String
      switch error as? ShareFailure {
      case .authentication:
        reason = "Open Axon and sign in or refresh your session."
      case .quota:
        reason =
          "The share exceeds the import/storage limit. Shorten the source text or free space in Pending shares."
      case .invalid:
        reason = "This source does not contain supported text or a web URL."
      default:
        reason = "Axon could not import this share. Open Axon to check your session."
      }
      return reason + " No draft was saved and nothing was sent. Share again from the source."
    }
    if let failure = error as? ShareResponseLimitFailure {
      return
        "The server response exceeds the sharing limit of \(failure.limit) bytes (observed \(failure.observedBytes) bytes). Your draft is saved; no new message was sent."
    }
    if let failure = error as? ShareRequestFailure {
      if let status = failure.status {
        let action =
          (300..<400).contains(status)
          ? "Configure the final HTTPS server address; sharing does not follow redirects."
          : status == 403
            ? "Check your session and the server's access permissions."
            : status == 404
              ? "Check the configured Axon server address and version."
              : status == 429
                ? "Wait before reloading; the server is rate limiting requests."
                : "Check Axon's server status, then reload accounts and rooms."
        return
          "The server returned HTTP \(status). \(action) Your draft is saved; nothing new was sent."
      }
      let code = failure.networkCode ?? 0
      let action =
        (-1206 ... -1200).contains(code)
        ? "Check the HTTPS certificate and server hostname."
        : code == -1001
          ? "The request timed out. Check server connectivity."
          : "Check network/VPN connectivity and Axon's Local Network permission."
      return
        "Cannot reach the Axon server (network code \(code)). \(action) Your draft is saved; nothing new was sent."
    }
    switch error as? ShareFailure {
    case .authentication:
      return
        "Open Axon and sign in or refresh your session, then review Pending shares. Nothing new was sent."
    case .httpsRequired:
      return "An HTTPS Axon server is required. Your draft is retained in Pending shares."
    case .missingFile:
      return
        "The retained preview image is missing or corrupt. Remove its image or replace the preview explicitly. Your draft is retained."
    case .previewUnavailable:
      return
        "Preview generation is disabled or unavailable. Your original link remains sendable without a card; you can also try Include preview again."
    case .invalidResponse:
      return
        "The server returned an unexpected response. Your draft is saved. Check the server version/status before retrying."
    case .stale:
      return
        "This session, room, or draft ownership changed. Open Axon → Pending shares to review. Nothing new was sent."
    case .quota:
      return
        "The share exceeds the import/storage limit. Reduce the photo selection, text, or previews and share again. Nothing was sent."
    case .invalid:
      return
        "This share is invalid or too large. Edit the text or remove its card. Nothing new was sent."
    default:
      return
        "The server or source is unavailable or timed out. Your saved draft remains in Axon → Pending shares. Review it before retrying."
    }
  }
  private func persist(text: String, account: String, room: String, retry: Bool = false) throws {
    guard var next = current, let store else { throw ShareFailure.unavailable }
    next.text = text
    next.accountId = account.isEmpty ? nil : account
    next.roomId = room.isEmpty ? nil : room
    let links = Set(TextShare.links(text))
    next.previews = next.previews?.filter { links.contains($0.matched_url) }
    if next.accountId != current?.accountId {
      next.previews = next.previews?.map { preview in
        var p = preview
        p.image_upload_id = nil
        return p
      }
    }
    if next.operation.state == .failed && (retry || next != current) {
      next.operation.state = .ready
      next.operation.failure = nil
    }
    if next != current { current = try store.update(next) }
  }
  func removePhoto(_ file: String) {
    guard editable else { return }
    let text = text
    let account = account
    let room = room
    run {
      try self.persist(text: text, account: account, room: room)
      self.current = try self.store!.removeFile(self.current!, file: file)
      return "Photo removed. Review the remaining selection before sending."
    }
  }
  func edited() {
    editSave?.cancel()
    let work = DispatchWorkItem { [weak self] in self?.save() }
    editSave = work
    DispatchQueue.main.asyncAfter(deadline: .now() + 0.4, execute: work)
  }
  func save() {
    guard editable else { return }
    let text = text
    let account = account
    let room = room
    run(blocking: false) {
      try self.persist(text: text, account: account, room: room)
      // Autosave must preserve actionable authentication/offline and delivery
      // status, including the first programmatic population of the editor.
      return ""
    }
  }
  func reloadDestinations() {
    guard editable else { return }
    let text = text
    let account = account
    let room = room
    run {
      try self.persist(text: text, account: account, room: room)
      return try self.loadDestinations(text: text)
    }
  }
  // Initial loading and explicit reloads share the same selection policy.
  // Run on queue; publish only after the complete fresh snapshot is saved.
  private func loadDestinations(text: String) throws -> String {
    let accounts = try sender!.accounts(current!)
    let ids = Set(accounts.map(\.account_id))
    let rooms =
      try accounts.isEmpty
      ? []
      : sender!.destinations(current!).filter {
        ids.contains($0.account_id)
      }
    let suggested = try store!.suggestedDestination(current!, destinations: rooms)
    let selectedAccount = accounts.count == 1 ? accounts[0].account_id : suggested.account
    let selectedRoom = suggested.account == selectedAccount ? suggested.room : ""
    try persist(text: text, account: selectedAccount, room: selectedRoom)
    DispatchQueue.main.async {
      if self.account != selectedAccount { self.roomSearch = "" }
      self.accounts = accounts
      self.rooms = rooms
      self.account = selectedAccount
      self.room = selectedRoom
    }
    if accounts.isEmpty {
      return
        "No active Matrix accounts are available. Open Axon → Accounts to add an account or sign in again, then reload. Your draft is saved; nothing was sent."
    }
    if rooms.isEmpty {
      return
        "No joined rooms are available. Open Axon to join a room, then reload. Your draft is saved; nothing was sent."
    }
    return ""
  }
  func send() {
    guard canSend else { return }
    editSave?.cancel()
    let text = text
    let account = account
    let room = room
    run {
      try self.persist(text: text, account: account, room: room, retry: true)
      self.current = try self.sender!.send(self.current!, checkpoint: { self.current = $0 })
      let record = self.current!
      let operations = record.files.compactMap(\.operation)
      let outcomes = operations.isEmpty ? [record.operation] : operations
      if record.completed { return "Sent successfully." }
      let delivered = outcomes.filter { $0.state == .sent }.count
      if outcomes.contains(where: { [.dispatching, .unknown].contains($0.state) }) {
        return
          "\(delivered) confirmed. Delivery outcome unknown for another item. Check the destination before resending in Axon → Pending shares; resending may duplicate it."
      }
      return
        "\(delivered) confirmed. Sending stopped. Open Axon → Pending shares to review each outcome and explicitly retry remaining items."

    }
  }
  func generateOnDevice(_ url: String, refresh: Bool) {
    guard editable else { return }
    let text = text
    let account = account
    let room = room
    run {
      try self.persist(text: text, account: account, room: room)
      let outcome = try self.devicePreview!.include(self.current!, url: url, refresh: refresh)
      self.current = outcome.manifest
      if outcome.reused { return "Supplied preview retained. Review before sending." }
      switch outcome.image {
      case .ready: return "Preview and image included on this device. Review before sending."
      case .absent:
        return
          "Text preview included on this device. No image was supplied. You can send this card or explicitly choose Use homeserver instead."
      case .unavailable:
        return
          "Text preview included on this device. Its image is unavailable or exceeds the sharing limits. You can send this card or explicitly choose Use homeserver instead."
      }
    }
  }
  func generateUsingHomeserver(_ url: String) {
    guard editable, !account.isEmpty else { return }
    let text = text
    let account = account
    let room = room
    run {
      try self.persist(text: text, account: account, room: room)
      var imageState = SharePreviewImageState.absent
      self.current = try self.sender!.generate(
        self.current!, url: url, imageState: { imageState = $0 })
      switch imageState {
      case .ready: return "Preview and image included. Review before sending."
      case .absent:
        return
          "Text preview included. The homeserver supplied no image for this link. Review before sending."
      case .unavailable:
        return
          "Text preview included. The preview image is unavailable; you can send this text card or explicitly try Include preview again."
      }
    }
  }
  func removePreview(imageOnly: Bool, url: String? = nil) {
    guard editable else { return }
    let text = text
    let account = account
    let room = room
    run {
      try self.persist(text: text, account: account, room: room)
      var next = self.current!
      if imageOnly {
        next.previews = next.previews?.map { p in
          var p = p
          if url == nil || p.matched_url == url {
            p.thumbnail = nil
            p.image_upload_id = nil
          }
          return p
        }
      } else {
        next.previews = next.previews?.filter { url != nil && $0.matched_url != url }
      }
      self.current = try self.store!.update(next)
      return imageOnly
        ? "Preview image removed; original URL retained."
        : "Preview removed; original URL retained."
    }
  }
  func editPreview(url: String, title: String, description: String) {
    guard editable else { return }
    let text = text
    let account = account
    let room = room
    run {
      try self.persist(text: text, account: account, room: room)
      var next = self.current!
      next.previews = next.previews?.map { preview in
        var p = preview
        if p.matched_url == url {
          p.title = title
          p.description = description
        }
        return p
      }
      self.current = try self.store!.update(next)
      return "Card edited. Review before sending."
    }
  }
  func finish(discard: Bool) {
    guard !busy else { return }
    let text = text
    let account = account
    let room = room
    run {
      if let record = self.current, let store = self.store {
        if discard || record.completed {
          try store.discard(
            record.id, owner: record.owner, completed: record.completed)
        } else {
          if !([record.operation] + record.files.compactMap(\.operation)).contains(where: {
            [.unknown, .dispatching].contains($0.state)
          }) {
            try self.persist(text: text, account: account, room: room)
          }
          self.current = try store.update(self.current!, release: true)
        }
      }
      DispatchQueue.main.async { self.complete() }
      return "Closed."
    }
  }
  private func complete() {
    guard !completed else { return }
    completed = true
    closing = true
    heartbeat?.invalidate()
    editSave?.cancel()
    cancellation.cancel()
    transport.close()
    close()
  }

  func cancel() {
    guard !closing else { return }
    closing = true
    cancellation.cancel()
    transport.close()
    Self.log.info("share_cancel_requested")
    heartbeat?.invalidate()
    editSave?.cancel()
    busy = true
    status =
      "Stopping after the current request. A dispatched message may already be delivered; saved outcomes remain in Pending shares."
    queue.async {
      if let record = self.current, let store = self.store {
        do { try store.cancelComposer(record) } catch {
          Self.log.error(
            "share_cancel_cleanup_failed category=\((error as? ShareFailure)?.rawValue ?? "storage", privacy: .public)"
          )
        }
      }
      DispatchQueue.main.async { self.complete() }
    }
  }
}

private enum SharePreviewInformation: String, Identifiable {
  case device, homeserver
  var id: String { rawValue }
  var title: String {
    self == .device ? "On-device previews" : "Homeserver previews"
  }
  var message: String {
    switch self {
    case .device:
      return
        "Generate a link preview on this device, sending a request to the server hosting the link."
    case .homeserver:
      return
        "Generate a link preview from your account's Matrix homeserver, which may log the URL even for an encrypted room."
    }
  }
}

private struct ShareComposeView: View {
  @ObservedObject var model: ShareComposer
  @Environment(\.sizeCategory) private var sizeCategory
  @State private var previewInformation: SharePreviewInformation?
  @State private var previewSearch = ""
  @State private var editingCard = false
  @State private var cardURL = ""
  @State private var cardTitle = ""
  @State private var cardDescription = ""
  var body: some View {
    let links = Array(Set(TextShare.links(model.text))).sorted()
    let matchingLinks = links.filter(TextShare.devicePreviewURL).filter {
      links.count <= 8 || previewSearch.isEmpty
        || $0.localizedCaseInsensitiveContains(previewSearch)
    }
    NavigationView {
      Form {
        if model.busy || !model.status.isEmpty {
          Section {
            if !model.status.isEmpty {
              Text(model.status).accessibilityIdentifier("share-status")
            }
            if model.busy { ProgressView() }
          }
        }
        Section {
          if model.accounts.count > 1 {
            NavigationLink(destination: ShareAccountPicker(model: model)) {
              VStack(alignment: .leading) {
                Text("Sending account")
                Text(model.accountTitle).foregroundColor(.secondary)
              }
            }
          }
          NavigationLink(destination: ShareRoomPicker(model: model)) {
            VStack(alignment: .leading) {
              Text("Room")
              Text(model.roomTitle).foregroundColor(.secondary)
            }
          }.disabled(model.account.isEmpty)
        } header: {
          HStack {
            Text("Destination")
            Spacer()
            Button {
              model.reloadDestinations()
            } label: {
              Image(systemName: "arrow.clockwise").font(.body)
                .frame(minWidth: 44, minHeight: 44).contentShape(Rectangle())
            }
            .buttonStyle(.borderless)
            .accessibilityLabel("Reload accounts and rooms")
            .accessibilityIdentifier("share-reload-destinations")
          }
        }.disabled(!model.editable)
        if !links.isEmpty {
          Section {
            let included = model.record?.previews?.filter { links.contains($0.matched_url) } ?? []
            if matchingLinks.isEmpty {
              if included.isEmpty {
                Label("Link only", systemImage: "link")
              } else {
                Label(
                  included.count == 1
                    ? "Preview included below" : "\(included.count) previews included below",
                  systemImage: "checkmark.circle")
              }
            }
            if links.count > 8 {
              TextField("Find link for preview", text: $previewSearch).autocapitalization(.none)
            }
            if matchingLinks.count > 8 {
              Text(
                "Showing the first 8 matching links. Use Find link for preview to narrow the choices."
              )
              .font(.caption)
            }
            ForEach(Array(matchingLinks.prefix(8)), id: \.self) { url in
              let hasPreview = included.contains(where: { $0.matched_url == url })
              VStack(alignment: .leading, spacing: 8) {
                Text(url).font(.caption).foregroundColor(.secondary)
                let status = Label(
                  hasPreview ? "Preview included" : "Link only",
                  systemImage: hasPreview ? "checkmark.circle" : "link")
                if sizeCategory.isAccessibilityCategory {
                  VStack(alignment: .leading, spacing: 8) {
                    status
                    devicePreviewButton(url, included: hasPreview)
                  }
                } else {
                  HStack {
                    status
                    Spacer()
                    devicePreviewButton(url, included: hasPreview)
                  }
                }
                HStack(spacing: 0) {
                  ShareHomeserverPreviewButton(model: model, url: url)
                  informationButton(.homeserver)
                }
              }
            }
            if model.accounts.count > 1 && model.account.isEmpty {
              Text("Choose a sending account to enable the homeserver option.").font(.caption)
            }
          } header: {
            HStack(spacing: 0) {
              Text("Link preview")
              informationButton(.device)
            }
          }
        }
        if let photos = model.record?.files.filter({ !$0.thumbnail }), !photos.isEmpty {
          Section(header: Text("Photos (\(photos.count))")) {
            ForEach(photos, id: \.id) { photo in
              VStack(alignment: .leading, spacing: 8) {
                if let image = model.images[photo.id] {
                  Image(uiImage: image).resizable().scaledToFit().frame(maxHeight: 160)
                    .accessibilityLabel("Photo: \(photo.name)")
                } else {
                  Text("Photo preview unavailable").font(.caption)
                }
                Text(photo.name)
                Text("\(photo.bytes / 1024) KiB · \(photo.operation?.state.rawValue ?? "ready")")
                  .font(.caption)
                Button("Remove photo") { model.removePhoto(photo.id) }
                  .buttonStyle(.borderless).disabled(!model.editable)
              }
            }
          }
        }
        Section(
          header: Text(
            model.record?.files.contains(where: { !$0.thumbnail }) == true
              ? "First photo caption" : "Message")
        ) {
          TextEditor(text: $model.text).frame(minHeight: 140).accessibilityLabel("Message text")
            .disabled(!model.editable).onChange(of: model.text) { _ in model.edited() }
          if let validation = model.validation, model.record != nil {
            Text(
              "Cannot send: \(validation). Shorten the text or remove excess cards. The editable draft is retained."
            ).foregroundColor(.red)
          }
        }
        if let previews = model.record?.previews?.filter({
          links.contains($0.matched_url)
        }), !previews.isEmpty {
          Section(header: Text("Included previews")) {
            ForEach(previews, id: \.matched_url) { preview in
              VStack(alignment: .leading, spacing: 8) {
                Text(preview.title ?? preview.matched_url).font(.headline)
                if let description = preview.description { Text(description) }
                if let site = preview.site_name { Text(site).font(.caption) }
                Text(preview.matched_url).font(.caption)
                if let image = model.images[preview.matched_url] {
                  Image(uiImage: image).resizable().scaledToFit().frame(maxHeight: 160)
                    .accessibilityLabel("Supplied preview image")
                } else if preview.thumbnail != nil || preview.image_upload_id != nil {
                  Text(
                    "Image preview unavailable. Remove its image or explicitly replace the preview if the retained copy is missing."
                  ).font(.caption)
                }
                Button("Edit preview") {
                  cardURL = preview.matched_url
                  cardTitle = preview.title ?? ""
                  cardDescription = preview.description ?? ""
                  editingCard = true
                }.buttonStyle(.borderless).frame(minHeight: 44).disabled(!model.editable)
                Button("Remove preview") {
                  model.removePreview(imageOnly: false, url: preview.matched_url)
                }.buttonStyle(.borderless).frame(minHeight: 44).disabled(!model.editable)
              }
            }
            if previews.contains(where: { $0.thumbnail != nil || $0.image_upload_id != nil }) {
              Button("Remove preview images") { model.removePreview(imageOnly: true) }.disabled(
                !model.editable)
            }
            if previews.count > 1 {
              Button("Remove previews") { model.removePreview(imageOnly: false) }
                .disabled(!model.editable)
            }
          }
        }
        Section {
          if model.record?.importFailures.isEmpty == false {
            Text(
              "The selection could not be fully imported. Share it again from the source; sending is disabled."
            )
            ForEach(Array(Set(model.record?.importFailures ?? [])).sorted(), id: \.self) {
              category in
              Text(ShareComposer.importProblem(category)).font(.caption)
            }
          }
          if let record = model.record, !record.completed {
            Button("Keep draft in Axon") { model.finish(discard: false) }.disabled(model.busy)
          }
        }
      }
      .navigationTitle("Share to Axon")
      .alert(item: $previewInformation) { information in
        Alert(
          title: Text(information.title), message: Text(information.message),
          dismissButton: .default(Text("OK")))
      }
      .sheet(isPresented: $editingCard) {
        NavigationView {
          GeometryReader { geometry in
            Form {
              TextField("Preview title", text: $cardTitle)
              Section(header: Text("Description")) {
                TextEditor(text: $cardDescription)
                  .frame(height: max(160, geometry.size.height * 0.5))
                  .accessibilityLabel("Preview description")
                  .accessibilityIdentifier("share-preview-description")
              }
              if cardTitle.utf16.count > 300 || cardDescription.utf16.count > 1000 {
                Text(
                  "Title limit: 300 UTF-16 units; description limit: 1000. Shorten these fields before saving."
                ).foregroundColor(.red)
              }
              Button("Save card") {
                model.editPreview(url: cardURL, title: cardTitle, description: cardDescription)
                editingCard = false
              }
              .disabled(cardTitle.utf16.count > 300 || cardDescription.utf16.count > 1000)
              Button("Cancel") { editingCard = false }
            }
          }.navigationTitle("Edit preview")
        }.navigationViewStyle(.stack)
      }
      .toolbar {
        ToolbarItem(placement: .cancellationAction) {
          Button("Cancel") { model.cancel() }
        }
        ToolbarItem(placement: .confirmationAction) {
          if model.record?.completed == true {
            Button("Done") { model.finish(discard: false) }.disabled(model.busy)
          } else {
            Button("Send") { model.send() }.disabled(!model.canSend)
              .accessibilityIdentifier("share-send")
          }
        }
      }
    }.navigationViewStyle(.stack)
  }

  private func informationButton(_ information: SharePreviewInformation) -> some View {
    Button {
      previewInformation = information
    } label: {
      Image(systemName: "info.circle")
        .frame(minWidth: 44, minHeight: 44).contentShape(Rectangle())
    }
    .buttonStyle(.borderless)
    .accessibilityLabel("About \(information.title.lowercased())")
    .accessibilityIdentifier("share-\(information.rawValue)-preview-info")
  }

  private func devicePreviewButton(_ url: String, included: Bool) -> some View {
    Button {
      model.generateOnDevice(url, refresh: included)
    } label: {
      Label(
        included ? "Refresh on this device" : "Include preview",
        systemImage: "photo.on.rectangle"
      )
      .fixedSize(horizontal: false, vertical: true)
    }
    .buttonStyle(.bordered).frame(minHeight: 44)
    .accessibilityIdentifier("share-device-preview")
    .accessibilityLabel(
      (included ? "Refresh on-device preview for " : "Include on-device preview for ") + url
    )
    .accessibilityHint("Uses this device to fetch a title and optional image")
    .disabled(!model.editable)
  }
}

private struct ShareHomeserverPreviewButton: View {
  @ObservedObject var model: ShareComposer
  let url: String
  @State private var homeserverConsent = false

  var body: some View {
    Button("Use homeserver instead…") { homeserverConsent = true }
      .buttonStyle(.borderless).frame(minHeight: 44)
      .accessibilityIdentifier("share-homeserver-preview")
      .accessibilityHint("Shares the URL with your selected homeserver only after confirmation")
      .disabled(!model.editable || model.account.isEmpty)
      .confirmationDialog(
        "Send this URL to your homeserver to fetch a preview?", isPresented: $homeserverConsent,
        titleVisibility: .visible
      ) {
        Button("Fetch using homeserver") { model.generateUsingHomeserver(url) }
      } message: {
        Text("Your homeserver may record this URL in its logs, including for encrypted rooms.")
      }
  }
}

private struct ShareDestinationChoice: View {
  let title: String
  let selected: Bool
  let action: () -> Void
  var body: some View {
    Button(action: action) {
      HStack {
        Text(title)
        Spacer()
        if selected { Image(systemName: "checkmark").accessibilityHidden(true) }
      }.frame(minHeight: 44).contentShape(Rectangle())
    }.buttonStyle(.plain).accessibilityValue(selected ? "Selected" : "")
  }
}

private struct ShareAccountPicker: View {
  @ObservedObject var model: ShareComposer
  @Environment(\.dismiss) private var dismiss
  var body: some View {
    List {
      ForEach(model.accounts, id: \.account_id) { account in
        ShareDestinationChoice(
          title: account.user_id,
          selected: model.account == account.account_id
        ) {
          model.chooseAccount(account.account_id)
          dismiss()
        }
      }
    }.navigationTitle("Sending account").disabled(!model.editable)
  }
}

private struct ShareRoomPicker: View {
  @ObservedObject var model: ShareComposer
  @Environment(\.dismiss) private var dismiss
  var body: some View {
    List {
      Section {
        TextField("Find room", text: $model.roomSearch).autocapitalization(.none)
        if model.matchingRoomCount > 100 {
          Text(
            "Showing the first 100 of \(model.matchingRoomCount) matching rooms. Use Find room to narrow the choices."
          ).font(.caption)
        }
      }
      if model.matchingRoomCount == 0 {
        Text(
          model.roomSearch.isEmpty
            ? "No joined rooms are available for this account. Open Axon to join a room, then reload."
            : "No rooms match your search.")
      }
      ForEach(model.visibleRooms, id: \.room_id) { room in
        ShareDestinationChoice(title: room.title, selected: model.room == room.room_id) {
          model.chooseRoom(room.room_id)
          dismiss()
        }
      }
    }.navigationTitle("Choose room").disabled(!model.editable)
  }
}
