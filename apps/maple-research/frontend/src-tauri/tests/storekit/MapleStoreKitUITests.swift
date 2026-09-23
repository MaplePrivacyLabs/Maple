import Foundation
import StoreKit
import StoreKitTest
import XCTest

private enum HarnessFailure: Error {
    case configurationNotApplied
    case invalidFixtureResponse
    case staleFixtureJournal
}

// BEGIN SNAPSHOT PARSER: Foundation-only code also exercised by the host fixtures.
private enum StoreKitSnapshotError: Error {
    case snapshotTooLarge, missingStatus, conflictingStatus, invalidCertificateSource
}

private func storeKitStatusSnapshot(_ snapshot: String) throws -> [String: Any] {
    let bytes = Array(snapshot.utf8)
    guard bytes.count <= 2_000_000 else { throw StoreKitSnapshotError.snapshotTooLarge }
    let prefixes = [Array("label: '".utf8), Array("value: ".utf8)]
    var found: [String: Any]?
    var canonical: Data?
    var offset = 0
    while offset < bytes.count {
        guard let prefix = prefixes.first(where: {
            offset + $0.count <= bytes.count && bytes[offset..<(offset + $0.count)].elementsEqual($0)
        }) else { offset += 1; continue }
        offset += prefix.count
        // XCTest emits full JSON in the quoted label and may truncate its value.
        if prefix == prefixes[1], offset < bytes.count, bytes[offset] == 39 { offset += 1 }
        while offset < bytes.count && [9, 10, 13, 32].contains(bytes[offset]) { offset += 1 }
        guard offset < bytes.count, bytes[offset] == 123 else { continue }
        let start = offset
        var depth = 0
        var quoted = false
        var escaped = false
        var end: Int?
        for index in start..<min(bytes.count, start + 65_536) {
            let byte = bytes[index]
            if quoted {
                if escaped { escaped = false }
                else if byte == 92 { escaped = true }
                else if byte == 34 { quoted = false }
            } else if byte == 34 { quoted = true }
            else if byte == 123 { depth += 1 }
            else if byte == 125 {
                depth -= 1
                if depth == 0 { end = index + 1; break }
            }
        }
        guard let end,
              let object = try? JSONSerialization.jsonObject(with: Data(bytes[start..<end])) as? [String: Any],
              object["purchaseStatus"] is String else { continue }
        let encoded = try JSONSerialization.data(withJSONObject: object, options: [.sortedKeys])
        if let canonical, canonical != encoded { throw StoreKitSnapshotError.conflictingStatus }
        found = object
        canonical = encoded
        offset = end
    }
    guard let found else { throw StoreKitSnapshotError.missingStatus }
    return found
}

private func storeKitCertificateFromSnapshot(_ object: [String: Any]) throws -> Data {
    let verifiedRecovery = object["certificateSource"] as? String == "verified_recovery"
        && object["signedTransactionsPresent"] as? Bool == true
        && (object["transactionCount"] as? Int ?? 0) > 0
    guard object["environment"] as? String == "Xcode",
          object["purchaseStatus"] as? String == "success" || verifiedRecovery,
          let encoded = object["localSigningCertificate"] as? String,
          let certificate = Data(base64Encoded: encoded), !certificate.isEmpty else {
        throw StoreKitSnapshotError.invalidCertificateSource
    }
    return certificate
}
// END SNAPSHOT PARSER

private func storeKitStatus(_ app: XCUIApplication) -> [String: Any]? {
    let candidates = app.descendants(matching: .any).matching(NSPredicate(
        format: "label CONTAINS %@ OR value CONTAINS %@", "\"purchaseStatus\"", "\"purchaseStatus\""
    ))
    for element in candidates.allElementsBoundByIndex {
        for text in [element.value as? String, element.label].compactMap({ $0 }) {
            guard let data = text.data(using: .utf8),
                  let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
            else { continue }
            return object
        }
    }
    return nil
}

/// Read-only capture from a Maple process already running under Xcode's Run
/// action. This deliberately neither activates SKTestSession nor relaunches Maple.
@MainActor
final class MapleStoreKitCaptureUITests: XCTestCase {
    func testExportPublicSigningCertificate() throws {
        continueAfterFailure = false
        let app = XCUIApplication(bundleIdentifier: "cloud.opensecret.maple")
        XCTAssertEqual(app.state, .runningForeground, "Keep Maple's verified transaction diagnostic screen open")
        let description = app.debugDescription
        let tree = XCTAttachment(string: description)
        tree.name = "Maple-accessibility-state"
        tree.lifetime = .keepAlways
        add(tree)
        // Parse this one actual AX snapshot. No additional element queries or
        // screenshot requests are needed to export the public certificate.
        let object = try storeKitStatusSnapshot(description)
        // Recovery can expose a native-verified transaction without making a
        // new purchase. Export its certificate without changing purchaseStatus.
        let certificate = try storeKitCertificateFromSnapshot(object)
        let attachment = XCTAttachment(data: certificate, uniformTypeIdentifier: "public.data")
        attachment.name = "MapleStoreKitSigningCertificate.cer"
        attachment.lifetime = .keepAlways
        add(attachment)
        let statusData = try JSONSerialization.data(withJSONObject: object, options: [.prettyPrinted, .sortedKeys])
        let statusAttachment = XCTAttachment(data: statusData, uniformTypeIdentifier: "public.json")
        statusAttachment.name = "Maple-diagnostic-status.json"
        statusAttachment.lifetime = .keepAlways
        add(statusAttachment)
    }
}

/// Exercises the actual prebuilt Maple WebView -> Tauri -> StoreKit bridge.
/// run.py must set UITargetAppPath to Maple.app, so SKTestSession targets Maple
/// rather than the build-only placeholder application.
@MainActor
final class MapleStoreKitUITests: XCTestCase {
    private var session: SKTestSession!
    private var app: XCUIApplication!
    private let proMonthly = "cloud.opensecret.maple.pro.monthly"

    override func setUp() async throws {
        continueAfterFailure = false
        let config = try XCTUnwrap(Bundle(for: Self.self).url(forResource: "Maple", withExtension: "storekit"))
        session = try SKTestSession(contentsOf: config)
        session.resetToDefaultState()
        session.clearTransactions()
        XCTAssertTrue(session.allTransactions().isEmpty, "StoreKit transaction history did not clear")
        session.disableDialogs = true
        session.timeRate = .realTime
        session.storefront = "USA"
        // Fail before UI automation when StoreKit's control channel is broken.
        // Property setters can log an error without throwing.
        try await session.setSimulatedError(.generic(.userCancelled), forAPI: .purchase)
        let echoed = await session.simulatedError(forAPI: .purchase)
        guard echoed == SKTestFailures.Purchase.generic(.userCancelled) else {
            XCTFail("StoreKit configuration control did not round-trip; Maple was not launched")
            throw HarnessFailure.configurationNotApplied
        }
        try await session.setSimulatedError(nil, forAPI: .purchase)
        guard await session.simulatedError(forAPI: .purchase) == nil else {
            XCTFail("StoreKit simulated purchase error did not clear; Maple was not launched")
            throw HarnessFailure.configurationNotApplied
        }
        app = XCUIApplication()
        app.launch()
        XCTAssertTrue(button("Load products").waitForExistence(timeout: 30), "Expected the experiment-enabled Maple build, not the placeholder or production paywall")
        button("Load products").tap()
        assertText(proMonthly)
    }

    override func tearDownWithError() throws {
        if let app {
            let screenshot = XCTAttachment(screenshot: app.screenshot())
            screenshot.name = name
            screenshot.lifetime = .keepAlways
            add(screenshot)
            app.terminate()
        }
        session?.resetToDefaultState()
    }

    func test01CatalogAndStorefront() throws {
        let expected = [proMonthly, "cloud.opensecret.maple.max.monthly",
                        "cloud.opensecret.maple.pro.yearly"]
        assertStatus("productIDs", file: #filePath, line: #line) { value in
            guard let ids = value as? [String] else { return false }
            return ids.count == expected.count && Set(ids) == Set(expected)
        }
        assertJSON("storefront", string: "USA")
        XCTAssertTrue(session.allTransactions().isEmpty)
    }

    func test02SigningCertificate() async throws {
        // The bootstrap fixture server provides the fixed local account token,
        // but accepts no transactions until this public certificate is pinned.
        try await requireFreshFixture(allowBootstrap: true)
        button("Purchase Pro").tap()
        assertJSON("purchaseStatus", string: "success")
        assertJSON("environment", string: "Xcode")
        assertJSON("unfinishedCount", number: 1)
        let purchased = try purchaseIdentity()
        try await assertJournal(purchased, present: false)
        let object = try XCTUnwrap(storeKitStatus(app))
        let encoded = try XCTUnwrap(object["localSigningCertificate"] as? String)
        let certificate = try XCTUnwrap(Data(base64Encoded: encoded))
        let attachment = XCTAttachment(data: certificate, uniformTypeIdentifier: "public.data")
        attachment.name = "MapleStoreKitSigningCertificate.cer"
        attachment.lifetime = .keepAlways
        add(attachment)
    }

    func test02PurchaseAcknowledgmentThenFinish() async throws {
        try await requireFreshFixture()
        try await fixtureMode("ok")
        button("Purchase Pro").tap()
        assertJSON("purchaseStatus", string: "success")
        assertJSON("unfinishedCount", number: 1)
        XCTAssertEqual(session.allTransactions().count, 1)
        let purchased = try purchaseIdentity()
        try await assertJournal(purchased, present: false)
        button("Recover purchases").tap()
        assertJSON("acknowledgedCount", number: 1)
        assertJSON("acknowledgedTransactionId", string: purchased.id)
        assertJSON("unfinishedCount", number: 0)
        try await assertJournal(purchased, present: true)
    }

    func test03FailureRelaunchAndRecovery() async throws {
        try await requireFreshFixture()
        try await fixtureMode("unavailable")
        button("Purchase Pro").tap()
        assertJSON("unfinishedCount", number: 1)
        let purchased = try purchaseIdentity()
        try await assertJournal(purchased, present: false)
        button("Recover purchases").tap()
        assertText("storekit_recovery_incomplete")
        button("List transactions").tap()
        assertJSON("unfinishedCount", number: 1)
        try await assertJournal(purchased, present: false)

        app.terminate()
        app.launch()
        XCTAssertTrue(button("List transactions").waitForExistence(timeout: 30))
        button("List transactions").tap()
        assertJSON("unfinishedCount", number: 1)
        try await assertJournal(purchased, present: false)
        try await fixtureMode("ok")
        button("Recover purchases").tap()
        assertJSON("acknowledgedCount", number: 1)
        assertJSON("acknowledgedTransactionId", string: purchased.id)
        assertJSON("unfinishedCount", number: 0)
        try await assertJournal(purchased, present: true)
    }

    func test04PendingApprovalUsesListener() async throws {
        try await requireFreshFixture()
        try await fixtureMode("ok")
        assertJSON("listenersReady", bool: true)
        button("Toggle automatic recovery").tap()
        assertJSON("automaticRecovery", bool: true)
        session.askToBuyEnabled = true
        button("Purchase Pro").tap()
        assertJSON("purchaseStatus", string: "pending")
        let pending = try XCTUnwrap(session.allTransactions().first(where: { $0.pendingAskToBuyConfirmation }))
        let pendingIdentity = JournalIdentity(id: String(pending.identifier),
                                             originalID: String(pending.originalTransactionIdentifier))
        try await assertJournal(pendingIdentity, present: false)
        try session.approveAskToBuyTransaction(identifier: pending.identifier)
        assertStatus("listenerCount", file: #filePath, line: #line) { ($0 as? Int ?? 0) >= 1 }
        assertStatus("acknowledgedCount", file: #filePath, line: #line) { ($0 as? Int ?? 0) >= 1 }
        assertJSON("unfinishedCount", number: 0)
        let status = try XCTUnwrap(storeKitStatus(app))
        let listenerID = try XCTUnwrap(status["listenerTransactionId"] as? String)
        XCTAssertEqual(status["acknowledgedTransactionId"] as? String, listenerID)
        let approved = try XCTUnwrap(session.allTransactions().first { String($0.identifier) == listenerID })
        try await assertJournal(JournalIdentity(id: listenerID,
                                               originalID: String(approved.originalTransactionIdentifier)),
                                present: true)
        // Initial UI state is zero, so it cannot prove finishing. A cold launch
        // removes cached fields; only a new successful native query sets them.
        app.terminate()
        app.launch()
        XCTAssertTrue(button("List transactions").waitForExistence(timeout: 30))
        let initial = try XCTUnwrap(storeKitStatus(app))
        XCTAssertNil(initial["signedTransactionsPresent"])
        button("List transactions").tap()
        assertJSON("signedTransactionsPresent", bool: true)
        let refreshed = try XCTUnwrap(storeKitStatus(app))
        XCTAssertTrue(refreshed["error"] is NSNull)
        let unfinished = try XCTUnwrap(refreshed["unfinishedTransactionIds"] as? [String])
        XCTAssertFalse(unfinished.contains(listenerID), "Approved transaction remained unfinished after acknowledgment")
        XCTAssertTrue(unfinished.isEmpty)
    }

    func test05CancelledPurchase() async throws {
        try await requireFreshFixture(allowBootstrap: true)
        try await session.setSimulatedError(.generic(.userCancelled), forAPI: .purchase)
        let echoed = await session.simulatedError(forAPI: .purchase)
        XCTAssertEqual(echoed, .generic(.userCancelled))
        button("Purchase Pro").tap()
        assertJSON("purchaseStatus", string: "cancelled")
        XCTAssertTrue(session.allTransactions().isEmpty)
        button("List transactions").tap()
        assertJSON("unfinishedCount", number: 0)
        let status = try await fixtureJSON("/__test__/status")
        XCTAssertTrue(try journalRecords(status).isEmpty)
    }

    private func button(_ label: String) -> XCUIElement {
        app.buttons.matching(identifier: label).firstMatch
    }

    private func assertText(_ text: String, file: StaticString = #filePath, line: UInt = #line) {
        let element = app.staticTexts.matching(NSPredicate(format: "label CONTAINS %@", text)).firstMatch
        XCTAssertTrue(element.waitForExistence(timeout: 20), "Missing expected diagnostic text: \(text)", file: file, line: line)
    }

    private func assertJSON(_ key: String, string value: String, file: StaticString = #filePath, line: UInt = #line) {
        assertStatus(key, file: file, line: line) { $0 as? String == value }
    }

    private func assertJSON(_ key: String, number value: Int, file: StaticString = #filePath, line: UInt = #line) {
        assertStatus(key, file: file, line: line) { $0 as? Int == value }
    }

    private func assertJSON(_ key: String, bool value: Bool, file: StaticString = #filePath, line: UInt = #line) {
        assertStatus(key, file: file, line: line) { $0 as? Bool == value }
    }

    private func assertStatus(_ key: String, file: StaticString, line: UInt, matches: @escaping (Any?) -> Bool) {
        let predicate = NSPredicate { [unowned self] _, _ in matches(storeKitStatus(self.app)?[key]) }
        let expectation = XCTNSPredicateExpectation(predicate: predicate, object: app)
        XCTAssertEqual(XCTWaiter.wait(for: [expectation], timeout: 20), .completed,
                       "Expected exact diagnostic field: \(key)", file: file, line: line)
    }

    private struct JournalIdentity {
        let id: String
        let originalID: String
    }

    private func purchaseIdentity() throws -> JournalIdentity {
        let status = try XCTUnwrap(storeKitStatus(app))
        XCTAssertEqual(status["environment"] as? String, "Xcode")
        XCTAssertEqual(status["purchaseJwsPresent"] as? Bool, true)
        let id = try XCTUnwrap(status["purchaseTransactionId"] as? String)
        let record = try XCTUnwrap(session.allTransactions().first { String($0.identifier) == id })
        XCTAssertEqual(record.productIdentifier, proMonthly)
        return JournalIdentity(id: id, originalID: String(record.originalTransactionIdentifier))
    }

    private func journalRecords(_ status: [String: Any]) throws -> [[String: Any]] {
        guard status["fixture_only"] as? Bool == true,
              let records = status["acknowledged_transactions"] as? [[String: Any]] else {
            XCTFail("Unexpected fixture status schema")
            throw HarnessFailure.invalidFixtureResponse
        }
        return records
    }

    private func requireFreshFixture(allowBootstrap: Bool = false) async throws {
        let status = try await fixtureJSON("/__test__/status")
        let records = try journalRecords(status)
        guard records.isEmpty, status["mode"] as? String == "ok",
              let bootstrap = status["bootstrap_only"] as? Bool,
              allowBootstrap || !bootstrap else {
            XCTFail("Use a fresh fixture process/journal for this case; acknowledgment cases also require a certificate pin")
            throw HarnessFailure.staleFixtureJournal
        }
    }

    private func assertJournal(_ transaction: JournalIdentity, present: Bool) async throws {
        let records = try journalRecords(try await fixtureJSON("/__test__/status"))
        let matches = records.filter { $0["transaction_id"] as? String == transaction.id }
        if present {
            XCTAssertEqual(matches.count, 1, "Expected this exact transaction in the durable fixture journal")
            let record = try XCTUnwrap(matches.first)
            XCTAssertEqual(record["original_transaction_id"] as? String, transaction.originalID)
            XCTAssertEqual(record["product_id"] as? String, proMonthly)
            XCTAssertEqual(record["environment"] as? String, "Xcode")
        } else {
            XCTAssertTrue(matches.isEmpty, "This transaction must not already be acknowledged")
        }
    }

    private func fixtureMode(_ mode: String) async throws {
        _ = try await fixtureJSON("/__test__/mode", body: ["mode": mode])
    }

    private func fixtureJSON(_ path: String, body: [String: String]? = nil) async throws -> [String: Any] {
        var request = URLRequest(url: URL(string: "http://127.0.0.1:38863" + path)!)
        request.httpMethod = body == nil ? "GET" : "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.setValue("1", forHTTPHeaderField: "X-Maple-StoreKit-Fixture")
        request.setValue("storekit-harness", forHTTPHeaderField: "X-Maple-Fixture-Control")
        if let body { request.httpBody = try JSONSerialization.data(withJSONObject: body) }
        request.timeoutInterval = 5
        let (data, response) = try await URLSession.shared.data(for: request)
        guard (response as? HTTPURLResponse)?.statusCode == 200,
              let object = try JSONSerialization.jsonObject(with: data) as? [String: Any] else {
            XCTFail("Local fixture request failed; no acknowledgment proof is available")
            throw HarnessFailure.invalidFixtureResponse
        }
        return object
    }
}
