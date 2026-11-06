import XCTest

final class ActivationUITests: XCTestCase {
  override func setUp() { continueAfterFailure = false }

  private func chooseSource(_ name: String, app: XCUIApplication) {
    app.terminate()
    app.launch()
    let source = app.buttons[name]
    XCTAssertTrue(source.waitForExistence(timeout: 8))
    source.tap()
  }
  private func receiver(in app: XCUIApplication) -> XCUIElement {
    app.descendants(matching: .any).matching(NSPredicate(format: "label == 'AxonShareTest'"))
      .firstMatch
  }
  private func showAllActivities(_ app: XCUIApplication) {
    let more = app.descendants(matching: .any)
      .matching(NSPredicate(format: "label == 'More' OR label == 'More Apps'")).firstMatch
    let cell = app.cells.matching(identifier: "shareCell").firstMatch
    XCTAssertTrue(cell.waitForExistence(timeout: 5))
    let y = cell.frame.midY
    for _ in 0..<8 {
      if more.exists && more.isHittable { break }
      let origin = app.coordinate(withNormalizedOffset: .zero)
      origin.withOffset(CGVector(dx: app.frame.width * 0.85, dy: y)).press(
        forDuration: 0.05,
        thenDragTo: origin.withOffset(CGVector(dx: app.frame.width * 0.15, dy: y)))
    }
    XCTAssertTrue(more.exists && more.isHittable)
    more.tap()
    XCTAssertTrue(app.navigationBars.firstMatch.waitForExistence(timeout: 5))
  }
  func testTextURLsAndPhotosExposeTheRealComposerWithoutCredentials() {
    let app = XCUIApplication()
    for name in [
      "source-text", "source-url", "source-decoration", "source-image", "source-image-alternative",
      "source-multiple-images",
    ] {
      chooseSource(name, app: app)
      let target = receiver(in: app)
      if !target.exists || !target.isHittable { showAllActivities(app) }
      for _ in 0..<8 {
        if target.exists && target.isHittable { break }
        app.swipeUp()
      }
      XCTAssertTrue(target.exists && target.isHittable, "Missing receiver for \(name)")
      target.tap()
      let composer = app.navigationBars["Share to Axon"]
      XCTAssertTrue(composer.waitForExistence(timeout: 10))
      // The test extension deliberately has no shared Keychain configuration.
      // It must not send, access a real account, or stage under another identity.
      let send = app.buttons["share-send"]
      XCTAssertTrue(send.exists)
      XCTAssertFalse(send.isEnabled)
      XCTAssertFalse(app.buttons["Keep draft in Axon"].exists)
      XCTAssertTrue(app.staticTexts["share-status"].label.contains("No draft was saved"))
      app.buttons["Cancel"].firstMatch.tap()
      XCTAssertFalse(composer.exists)
    }
    app.terminate()
  }
  private func reveal(_ element: XCUIElement, app: XCUIApplication, down: Bool = false) {
    for _ in 0..<7 {
      if element.exists && element.isHittable {
        // XCTest called the single-photo picker hittable with only its top
        // six points on screen (y=868...902 in an 874-point window). Its tap
        // landed below the window. Reveal the whole control before tapping.
        let frame = element.frame
        if frame.minY >= app.navigationBars.firstMatch.frame.maxY
          && frame.maxY <= app.frame.maxY - 44
        {
          return
        }
      }
      if down { app.swipeDown() } else { app.swipeUp() }
    }
    XCTAssertTrue(element.exists && element.isHittable)
  }
  private func expectSize(_ size: String, on element: XCUIElement) {
    let updated = XCTNSPredicateExpectation(
      predicate: NSPredicate(format: "label CONTAINS %@ OR value CONTAINS %@", size, size),
      object: element)
    XCTAssertEqual(XCTWaiter.wait(for: [updated], timeout: 10), .completed)
  }
  func testNativePhotoSizeMenusApplyToAllAndAllowAnOverride() {
    let app = XCUIApplication()
    chooseSource("source-photo-size", app: app)
    let all = app.buttons["share-photo-size-all"]
    XCTAssertTrue(all.waitForExistence(timeout: 15))
    reveal(all, app: app)
    all.tap()
    app.buttons["Small (640 px)"].firstMatch.tap()
    expectSize("Small", on: all)
    let first = app.buttons.matching(
      NSPredicate(format: "identifier BEGINSWITH 'share-photo-size-item-'")
    ).firstMatch
    reveal(first, app: app)
    first.tap()
    app.buttons["Medium (1280 px)"].firstMatch.tap()
    expectSize("Medium", on: first)
    reveal(all, app: app, down: true)
    expectSize("Mixed", on: all)
    all.tap()
    app.buttons["Original"].firstMatch.tap()
    expectSize("Original", on: all)
    app.buttons["Cancel"].firstMatch.tap()
    app.terminate()
  }
  func testSinglePhotoHasItsOwnSizeMenuWithoutABatchControl() {
    let app = XCUIApplication()
    chooseSource("source-photo-size-single", app: app)
    XCTAssertTrue(app.staticTexts["share-photo-upload-total"].waitForExistence(timeout: 15))
    XCTAssertFalse(app.buttons["share-photo-size-all"].exists)
    let first = app.buttons.matching(
      NSPredicate(format: "identifier BEGINSWITH 'share-photo-size-item-'")
    ).firstMatch
    reveal(first, app: app)
    first.tap()
    app.buttons["Small (640 px)"].firstMatch.tap()
    expectSize("Small", on: first)
    app.buttons["Cancel"].firstMatch.tap()
    app.terminate()
  }
  func testOtherFilesAndVideosDoNotAdvertiseSupport() {
    let app = XCUIApplication()
    for name in ["source-file", "source-video"] {
      chooseSource(name, app: app)
      showAllActivities(app)
      var previousLast = ""
      for _ in 0..<10 {
        XCTAssertFalse(receiver(in: app).exists, "Unexpected media destination for \(name)")
        let table = app.tables.firstMatch
        let last = table.cells.allElementsBoundByIndex.last(where: { $0.isHittable })?.label ?? ""
        if !last.isEmpty && last == previousLast { break }
        previousLast = last
        if table.exists { table.swipeUp() } else { app.swipeUp() }
      }
    }
    app.terminate()
  }
}
