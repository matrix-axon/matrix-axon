import XCTest

// Existence waits are ceilings, not delays, so they cost nothing when the UI is
// prompt. Shared CI simulators run these tests about twice as slowly as a
// laptop (490 s against 261 s), and a 5 s wait for the system share sheet
// failed one hosted run, so every existence wait uses `uiTimeout` (20 s).
final class ActivationUITests: XCTestCase {
  /// The one ceiling for every existence wait; tune it here, not per call site.
  private let uiTimeout: TimeInterval = 20
  override func setUp() { continueAfterFailure = false }

  private func chooseSource(_ name: String, app: XCUIApplication) {
    app.terminate()
    app.launch()
    let source = app.buttons[name]
    XCTAssertTrue(source.waitForExistence(timeout: uiTimeout))
    source.tap()
  }
  private func receiver(in app: XCUIApplication) -> XCUIElement {
    app.descendants(matching: .any).matching(NSPredicate(format: "label == 'AxonShareTest'"))
      .firstMatch
  }
  /// Polls until `format` holds for `element`. The share sheet and the extension
  /// are other processes that animate in and out, so one read of the AX tree can
  /// land mid-transition.
  private func waitFor(
    _ element: XCUIElement, _ format: String, timeout: TimeInterval? = nil
  ) -> Bool {
    let condition = XCTNSPredicateExpectation(
      predicate: NSPredicate(format: format), object: element)
    return XCTWaiter.wait(for: [condition], timeout: timeout ?? uiTimeout) == .completed
  }
  private func showAllActivities(_ app: XCUIApplication) {
    let more = app.descendants(matching: .any)
      .matching(NSPredicate(format: "label == 'More' OR label == 'More Apps'")).firstMatch
    let cell = app.cells.matching(identifier: "shareCell").firstMatch
    XCTAssertTrue(cell.waitForExistence(timeout: uiTimeout))
    // The first cell exists while the sheet is still sliding up. On a standard
    // iPhone, More is in the first row once the sheet settles, so wait for that
    // before swiping; only a narrow phone pushes it off the end of the row.
    let hittable = "exists == true AND hittable == true"
    for _ in 0..<8 {
      if waitFor(more, hittable, timeout: 5) { break }
      // Read the row's position before every swipe, not once: a read taken
      // during the slide-up gave midY 55.9, so every swipe hit the status bar.
      let y = cell.frame.midY
      let origin = app.coordinate(withNormalizedOffset: .zero)
      origin.withOffset(CGVector(dx: app.frame.width * 0.85, dy: y)).press(
        forDuration: 0.05,
        thenDragTo: origin.withOffset(CGVector(dx: app.frame.width * 0.15, dy: y)))
    }
    XCTAssertTrue(
      more.exists && more.isHittable,
      "More never became hittable; first shareCell at \(cell.frame)\n\(app.debugDescription)")
    more.tap()
    XCTAssertTrue(app.navigationBars.firstMatch.waitForExistence(timeout: uiTimeout))
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
      XCTAssertTrue(composer.waitForExistence(timeout: uiTimeout))
      // The test extension deliberately has no shared Keychain configuration.
      // It must not send, access a real account, or stage under another identity.
      let send = app.buttons["share-send"]
      XCTAssertTrue(send.exists)
      XCTAssertFalse(send.isEnabled)
      XCTAssertFalse(app.buttons["Keep draft in Axon"].exists)
      XCTAssertTrue(app.staticTexts["share-status"].label.contains("No draft was saved"))
      cancel(app)
      // The extension is still tearing down when Cancel's tap returns; a single
      // read here failed with kAXErrorServerNotFound while the bar was on screen.
      XCTAssertTrue(waitFor(composer, "exists == false"), "Composer stayed open for \(name)")
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
  /// Brings the composer's header, with Cancel and Send, back into view. On a short
  /// screen (the Xcode 27 simulator's iPhone Duo is 466 x 678 points) the header
  /// scrolls out with the content, so after any scrolling it has to be looked for
  /// again; on a tall one it never leaves and nothing is swiped.
  private func showHeader(_ app: XCUIApplication) {
    let cancel = app.buttons["Cancel"].firstMatch
    for _ in 0..<10 where !(cancel.exists && cancel.isHittable) { app.swipeDown() }
  }
  private func cancel(_ app: XCUIApplication) {
    showHeader(app)
    let cancel = app.buttons["Cancel"].firstMatch
    XCTAssertTrue(cancel.waitForExistence(timeout: uiTimeout))
    cancel.tap()
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
    XCTAssertTrue(all.waitForExistence(timeout: uiTimeout))
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
    cancel(app)
    app.terminate()
  }
  func testPhotoFilenameEditsInlineAndLastRemovalShowsNothingToSend() {
    let app = XCUIApplication()
    chooseSource("source-photo-size-single", app: app)
    let filename = app.textFields.matching(NSPredicate(format: "identifier BEGINSWITH 'share-photo-filename-'" )).firstMatch
    XCTAssertTrue(filename.waitForExistence(timeout: uiTimeout))
    reveal(filename, app: app)
    let original = filename.value as? String ?? ""
    XCTAssertFalse(original.isEmpty)
    XCTAssertFalse(app.staticTexts["ready"].exists)
    filename.tap()
    XCTAssertTrue(app.navigationBars["Share to Axon"].exists)
    XCTAssertFalse(app.navigationBars["Rename photo"].exists)
    filename.typeText(String(repeating: XCUIKeyboardKey.delete.rawValue, count: original.count) + "Coast holiday.heic")
    app.buttons["share-photo-filename-done"].tap()
    XCTAssertTrue(app.images["Photo: Coast holiday.heic"].waitForExistence(timeout: uiTimeout))
    XCTAssertEqual(filename.value as? String, "Coast holiday.heic")
    XCTAssertFalse(app.buttons["share-photo-filename-save"].exists)
    let remove = app.buttons["Remove photo"].firstMatch
    reveal(remove, app: app, down: true)
    remove.tap()
    let empty = app.staticTexts["share-validation-message"]
    XCTAssertTrue(empty.waitForExistence(timeout: uiTimeout))
    XCTAssertEqual(empty.label, "Nothing to send.")
    XCTAssertEqual(app.staticTexts.matching(NSPredicate(format: "label == 'Nothing to send.'")).count, 1)
    showHeader(app)
    XCTAssertFalse(app.buttons["share-send"].isEnabled)
    XCTAssertFalse(app.staticTexts.matching(NSPredicate(format: "label CONTAINS 'Shorten the text' OR label CONTAINS 'empty_text'")).firstMatch.exists)
    cancel(app)
    app.terminate()
  }

  func testMasterCaptionAndNativeDragReordering() {
    let app = XCUIApplication()
    chooseSource("source-photo-size", app: app)
    let master = app.textViews["share-master-caption"]
    XCTAssertTrue(master.waitForExistence(timeout: uiTimeout))
    reveal(master, app: app)
    master.tap()
    master.typeText("Together")
    app.buttons["share-photo-caption-done"].tap()
    let fields = app.textViews.matching(NSPredicate(format: "identifier BEGINSWITH 'share-photo-caption-'"))
    reveal(fields.firstMatch, app: app)
    let firstID = fields.firstMatch.identifier
    XCTAssertEqual(fields.firstMatch.value as? String, "Together")
    let reorder = app.buttons["share-photo-reorder"]
    reveal(reorder, app: app, down: true)
    reorder.tap()
    let handles = app.buttons.matching(NSPredicate(format: "label BEGINSWITH 'Reorder' AND identifier != 'share-photo-reorder'"))
    XCTAssertTrue(handles.firstMatch.waitForExistence(timeout: uiTimeout), app.debugDescription)
    // With 44-point filename controls, the first compact row began at y=742
    // and the second was below the 874-point viewport, outside the AX snapshot.
    if handles.count < 2 { app.swipeUp() }
    XCTAssertEqual(handles.count, 2, app.debugDescription)
    reveal(handles.element(boundBy: 1), app: app)
    // Both handles have the same label, so only their position tells them apart,
    // and XCTest looks up an element's coordinate again while the drag runs. Once
    // the first row lifts, "index 1" is no longer the second row: on CI it came
    // back with an infinite frame (an exception inside XCTest) or as a row that
    // made the drop a no-op. Read both frames now and drag between fixed points.
    let from = handles.element(boundBy: 0).frame
    let to = handles.element(boundBy: 1).frame
    XCTAssertFalse(
      from.isInfinite || from.isEmpty || to.isInfinite || to.isEmpty,
      "Handle frames \(from) and \(to)\n\(app.debugDescription)")
    let origin = app.coordinate(withNormalizedOffset: .zero)
    origin.withOffset(CGVector(dx: from.midX, dy: from.midY)).press(
      forDuration: 0.5,
      thenDragTo: origin.withOffset(CGVector(dx: to.midX, dy: to.minY + to.height * 0.9)),
      withVelocity: .default, thenHoldForDuration: 0.3)
    // The status row is virtualized above the viewport here. Verify the saved
    // photo IDs after ending edit mode instead of polling an absent AX row.
    reveal(reorder, app: app, down: true)
    reorder.tap()
    reveal(fields.firstMatch, app: app)
    XCTAssertTrue(
      waitFor(fields.firstMatch, "identifier != '\(firstID)'", timeout: 10),
      "The drag left \(firstID) first")
    XCTAssertEqual(fields.firstMatch.value as? String, "Together")
    reveal(reorder, app: app, down: true)
    reorder.tap()
    let remove = app.buttons["Remove photo"].firstMatch
    reveal(remove, app: app)
    remove.tap()
    XCTAssertTrue(fields.firstMatch.waitForExistence(timeout: uiTimeout))
    XCTAssertEqual(fields.count, 1)
    XCTAssertFalse(reorder.exists)
    XCTAssertEqual(fields.firstMatch.value as? String, "Together")
    cancel(app)
    app.terminate()
  }

  func testPhotoCaptionsAreInlineIndependentAndFollowTheirImageOnRemoval() {
    let app = XCUIApplication()
    chooseSource("source-photo-size", app: app)
    let fields = app.textViews.matching(NSPredicate(format: "identifier BEGINSWITH 'share-photo-caption-'"))
    XCTAssertTrue(fields.firstMatch.waitForExistence(timeout: uiTimeout))
    XCTAssertFalse(app.textViews["Message text"].exists)
    // Form materializes the visible row only: the initial AX query reported
    // one editor for two photos. Scroll before looking up the other row.
    let firstID = fields.firstMatch.identifier
    let first = app.textViews[firstID]
    reveal(first, app: app)
    XCTAssertLessThanOrEqual(first.frame.height, 45)
    first.tap()
    XCTAssertEqual(first.value as? String ?? "", "")
    first.typeText("First coast\nSecond line")
    app.buttons["share-photo-caption-done"].tap()
    let other = app.textViews.matching(NSPredicate(
      format: "identifier BEGINSWITH 'share-photo-caption-' AND identifier != %@", firstID)).firstMatch
    reveal(other, app: app)
    let secondID = other.identifier
    let second = app.textViews[secondID]
    second.tap()
    XCTAssertEqual(second.value as? String ?? "", "")
    second.typeText("Second mountain")
    app.buttons["share-photo-caption-done"].tap()
    XCTAssertEqual(second.value as? String, "Second mountain")
    reveal(first, app: app, down: true)
    XCTAssertEqual(first.value as? String, "First coast\nSecond line")
    let remove = app.buttons["Remove photo"].firstMatch
    reveal(remove, app: app, down: true)
    XCTAssertGreaterThan(remove.frame.minX, first.frame.midX)
    XCTAssertLessThan(remove.frame.minY, first.frame.minY)
    let screenshot = XCTAttachment(screenshot: app.screenshot())
    screenshot.name = "Captions inside photo boxes"
    screenshot.lifetime = .keepAlways
    add(screenshot)
    remove.tap()
    let removed = XCTNSPredicateExpectation(predicate: NSPredicate(format: "exists == false"), object: first)
    XCTAssertEqual(XCTWaiter.wait(for: [removed], timeout: 10), .completed)
    let remaining = app.textViews[secondID]
    reveal(remaining, app: app)
    XCTAssertEqual(remaining.value as? String, "Second mountain")
    cancel(app)
    app.terminate()
  }

  func testSourceMessageSurvivesRemovingFirstAndLastPhoto() {
    let app = XCUIApplication()
    chooseSource("source-photo-message", app: app)
    let message = app.textViews["Message text"]
    let ready = XCTNSPredicateExpectation(predicate: NSPredicate(format: "label CONTAINS 'No active Matrix accounts'"), object: app.staticTexts["share-status"])
    XCTAssertEqual(XCTWaiter.wait(for: [ready], timeout: 15), .completed)
    reveal(message, app: app)
    XCTAssertEqual(message.value as? String, "Source message https://example.org")
    for _ in 0..<2 {
      let trash = app.buttons.matching(NSPredicate(format: "identifier BEGINSWITH 'share-photo-remove-'" )).firstMatch
      reveal(trash, app: app, down: true)
      let removed = app.buttons[trash.identifier]
      trash.tap()
      let updated = XCTNSPredicateExpectation(predicate: NSPredicate(format: "exists == false"), object: removed)
      XCTAssertEqual(XCTWaiter.wait(for: [updated], timeout: 10), .completed)
      reveal(message, app: app)
      XCTAssertEqual(message.value as? String, "Source message https://example.org")
    }
    cancel(app)
    app.terminate()
  }

  func testSinglePhotoHasItsOwnSizeMenuWithoutABatchControl() {
    let app = XCUIApplication()
    chooseSource("source-photo-size-single", app: app)
    XCTAssertTrue(app.staticTexts["share-photo-upload-total"].waitForExistence(timeout: uiTimeout))
    XCTAssertFalse(app.buttons["share-photo-size-all"].exists)
    let first = app.buttons.matching(
      NSPredicate(format: "identifier BEGINSWITH 'share-photo-size-item-'")
    ).firstMatch
    reveal(first, app: app)
    first.tap()
    app.buttons["Small (640 px)"].firstMatch.tap()
    expectSize("Small", on: first)
    cancel(app)
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
