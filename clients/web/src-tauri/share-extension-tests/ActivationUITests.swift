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
  func testPhotoFilenameEditsInlineAndLastRemovalShowsNothingToSend() {
    let app = XCUIApplication()
    chooseSource("source-photo-size-single", app: app)
    let filename = app.textFields.matching(NSPredicate(format: "identifier BEGINSWITH 'share-photo-filename-'" )).firstMatch
    XCTAssertTrue(filename.waitForExistence(timeout: 15))
    reveal(filename, app: app)
    let original = filename.value as? String ?? ""
    XCTAssertFalse(original.isEmpty)
    XCTAssertFalse(app.staticTexts["ready"].exists)
    filename.tap()
    XCTAssertTrue(app.navigationBars["Share to Axon"].exists)
    XCTAssertFalse(app.navigationBars["Rename photo"].exists)
    filename.typeText(String(repeating: XCUIKeyboardKey.delete.rawValue, count: original.count) + "Coast holiday.heic")
    app.buttons["share-photo-filename-done"].tap()
    XCTAssertTrue(app.images["Photo: Coast holiday.heic"].waitForExistence(timeout: 5))
    XCTAssertEqual(filename.value as? String, "Coast holiday.heic")
    XCTAssertFalse(app.buttons["share-photo-filename-save"].exists)
    let remove = app.buttons["Remove photo"].firstMatch
    reveal(remove, app: app, down: true)
    remove.tap()
    let empty = app.staticTexts["share-validation-message"]
    XCTAssertTrue(empty.waitForExistence(timeout: 5))
    XCTAssertEqual(empty.label, "Nothing to send.")
    XCTAssertEqual(app.staticTexts.matching(NSPredicate(format: "label == 'Nothing to send.'")).count, 1)
    XCTAssertFalse(app.buttons["share-send"].isEnabled)
    XCTAssertFalse(app.staticTexts.matching(NSPredicate(format: "label CONTAINS 'Shorten the text' OR label CONTAINS 'empty_text'")).firstMatch.exists)
    app.buttons["Cancel"].firstMatch.tap()
    app.terminate()
  }

  func testMasterCaptionAndNativeDragReordering() {
    let app = XCUIApplication()
    chooseSource("source-photo-size", app: app)
    let master = app.textViews["share-master-caption"]
    XCTAssertTrue(master.waitForExistence(timeout: 15))
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
    XCTAssertTrue(handles.firstMatch.waitForExistence(timeout: 5), app.debugDescription)
    // With 44-point filename controls, the first compact row began at y=742
    // and the second was below the 874-point viewport, outside the AX snapshot.
    if handles.count < 2 { app.swipeUp() }
    XCTAssertEqual(handles.count, 2, app.debugDescription)
    reveal(handles.element(boundBy: 1), app: app)
    let first = handles.element(boundBy: 0)
    let last = handles.element(boundBy: 1)
    first.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5)).press(forDuration: 0.5, thenDragTo: last.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.9)))
    // The status row is virtualized above the viewport here. Verify the saved
    // photo IDs after ending edit mode instead of polling an absent AX row.
    reveal(reorder, app: app, down: true)
    reorder.tap()
    reveal(fields.firstMatch, app: app)
    XCTAssertNotEqual(fields.firstMatch.identifier, firstID)
    XCTAssertEqual(fields.firstMatch.value as? String, "Together")
    reveal(reorder, app: app, down: true)
    reorder.tap()
    let remove = app.buttons["Remove photo"].firstMatch
    reveal(remove, app: app)
    remove.tap()
    XCTAssertTrue(fields.firstMatch.waitForExistence(timeout: 10))
    XCTAssertEqual(fields.count, 1)
    XCTAssertFalse(reorder.exists)
    XCTAssertEqual(fields.firstMatch.value as? String, "Together")
    app.buttons["Cancel"].firstMatch.tap()
    app.terminate()
  }

  func testPhotoCaptionsAreInlineIndependentAndFollowTheirImageOnRemoval() {
    let app = XCUIApplication()
    chooseSource("source-photo-size", app: app)
    let fields = app.textViews.matching(NSPredicate(format: "identifier BEGINSWITH 'share-photo-caption-'"))
    XCTAssertTrue(fields.firstMatch.waitForExistence(timeout: 15))
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
    app.buttons["Cancel"].firstMatch.tap()
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
