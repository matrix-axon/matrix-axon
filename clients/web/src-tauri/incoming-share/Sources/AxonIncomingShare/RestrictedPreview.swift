import Foundation
import SwiftSoup
import os

/// No scripts, styles, frames, favicons or arbitrary page subresources load.
/// Only an explicit page and its selected preview image reach PreviewNetwork.
enum RestrictedPreview {
  private static let log = Logger(subsystem: "org.matrixaxon.axon", category: "device-preview")
  private static let slot = DispatchSemaphore(value: 1)
  struct Metadata {
    var originalURL: URL
    var url: URL
    var title: String?
    var description: String?
    var siteName: String?
    var image: Data?
    var imageUnavailable = false
  }
  typealias Fetch = (URL, PreviewNetwork.Kind, DispatchTime, ShareCancellation) throws ->
    PreviewNetwork.Response

  static func fetch(
    _ url: URL, cancellation: ShareCancellation,
    request: Fetch = { try PreviewNetwork.fetch($0, kind: $1, deadline: $2, cancellation: $3) }
  ) throws -> Metadata {
    try cancellation.check()
    guard slot.wait(timeout: .now() + 1) == .success else { throw ShareFailure.previewBusy }
    defer { slot.signal() }
    let deadline = DispatchTime.now() + 25
    let response = try request(url, .page, deadline, cancellation)
    try cancellation.check()
    if (response.headers["content-type"] ?? "").lowercased().hasPrefix("image/") {
      return Metadata(
        originalURL: url, url: response.url,
        title: TextShare.previewField(
          response.url.lastPathComponent.removingPercentEncoding, limit: 300), image: response.body)
    }
    let type = (response.headers["content-type"] ?? "text/html").lowercased()
    guard type.hasPrefix("text/html") || type.hasPrefix("application/xhtml+xml") else {
      throw ShareFailure.previewUnavailable
    }
    let page = try parse(response.body, contentType: type)
    var metadata = Metadata(
      originalURL: url, url: response.url, title: page.title,
      description: page.description, siteName: page.siteName)
    if let image = page.image {
      // A named image that cannot be used counts as unavailable, whether it
      // fails to resolve here or to load below.
      metadata.imageUnavailable = true
      if let target = URL(string: image, relativeTo: response.url)?.absoluteURL {
        do {
          let response = try request(target, .image, deadline, cancellation)
          guard response.body.count <= ShareLimits.thumbnailBytes else { throw ShareFailure.quota }
          metadata.image = response.body
          metadata.imageUnavailable = false
        } catch {
          try cancellation.check()
          log.info("device_preview_image_unavailable")
        }
      } else {
        log.info("device_preview_image_unavailable")
      }
    }
    guard
      metadata.title != nil || metadata.description != nil || metadata.siteName != nil
        || metadata.image != nil
    else { throw ShareFailure.previewUnavailable }
    log.info("device_preview_fetched retained_bytes=\(metadata.image?.count ?? 0)")
    return metadata
  }

  struct Page {
    var title: String?
    var description: String?
    var siteName: String?
    var image: String?
  }
  static func parse(_ data: Data, contentType: String = "text/html") throws -> Page {
    guard data.count <= PreviewNetwork.headBytes else { throw ShareFailure.quota }
    // Bound the parser's potential tree and recursive destruction before DOM
    // allocation: only the bytes before the 513th `<` are parsed. Preview
    // tags come early in a head, so a heavy page keeps its metadata rather
    // than its card. A tag the cut ends is dropped whole. The Rust reader
    // (`axon-link-preview/src/page.rs`) makes the same cut.
    var tags = 0
    let bounded = data.prefix { byte in
      if byte == 60 { tags += 1 }
      return tags <= 512
    }
    let html: String
    if let utf8 = String(data: bounded, encoding: .utf8) {
      html = utf8
    } else if contentType.contains("iso-8859-1") || contentType.contains("windows-1252"),
      let legacy = String(data: bounded, encoding: .windowsCP1252)
    {
      html = legacy
    } else {
      html = String(decoding: bounded, as: UTF8.self)
    }
    let document = try SwiftSoup.parse(html)
    func content(_ selectors: [String], limit: Int) throws -> String? {
      for selector in selectors {
        if let node = try document.select(selector).first(),
          let value = TextShare.previewField(try node.attr("content"), limit: limit)
        {
          return value
        }
      }
      return nil
    }
    let title =
      try content(["meta[property=og:title]", "meta[name=twitter:title]"], limit: 300)
      ?? TextShare.previewField(try document.title(), limit: 300)
    return try Page(
      title: title,
      description: content(
        [
          "meta[property=og:description]", "meta[name=twitter:description]",
          "meta[name=description]",
        ], limit: 1000),
      siteName: content(["meta[property=og:site_name]"], limit: 120),
      image: content(
        [
          "meta[property=og:image:secure_url]", "meta[property=og:image]",
          "meta[name=twitter:image]", "meta[name=twitter:image:src]",
        ], limit: 4096))
  }
}
