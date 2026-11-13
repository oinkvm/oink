import XCTest
@testable import OinkAudioTestHost

final class TransferLayoutTests: XCTestCase {
    private func message(_ id: Int, _ kind: TransferMessageKind = .image, operation: String? = "op1", sender: String = "pc",
                         mine: Bool = false, delivery: TransferDeliveryState? = nil, failure: String? = nil) -> TransferMessage {
        TransferMessage(id: String(id), createdAt: Date(timeIntervalSince1970: Double(id)), senderDeviceID: sender,
                        senderName: sender, offerID: nil, sourceBoot: nil, operationID: operation, itemID: 0,
                        representationID: 0, originSenderID: sender, proxyDeviceID: nil, proxyName: nil, kind: kind,
                        text: kind == .text ? "hi" : nil, contentID: "c\(id)", attachmentName: "f\(id)", byteSize: 1,
                        uti: nil, state: .stored, availability: "Ready", isMine: mine, delivery: delivery,
                        deliveryFailure: failure)
    }

    func testConsecutiveAttachmentsOfOneShareFormOneStack() {
        let list = [message(1, .text), message(2), message(3, .file), message(4, .folder), message(5, operation: "op2")]
        let entries = TransferLayout.entries(list, expanded: [])
        XCTAssertEqual(entries.map(\.id), ["1", "batch:pc/op1", "5"])
        guard case .stack(_, let items) = entries[1] else { return XCTFail("no stack") }
        XCTAssertEqual(items.map(\.id), ["2", "3", "4"])
    }

    func testSingleAttachmentTextAndOtherSendersStayRows() {
        let list = [message(1), message(2, sender: "mac"), message(3, .text), message(4, .text)]
        XCTAssertEqual(TransferLayout.entries(list, expanded: []).map(\.id), ["1", "2", "3", "4"])
        // Own and received records of the same operation never share a batch.
        let mixed = [message(5, mine: true), message(6)]
        XCTAssertEqual(TransferLayout.entries(mixed, expanded: []).map(\.id), ["5", "6"])
        // Records without an operation (fixtures, folder children) are rows.
        XCTAssertEqual(TransferLayout.entries([message(7, operation: nil), message(8, operation: nil)], expanded: []).map(\.id), ["7", "8"])
    }

    func testExpandedBatchListsItsItemsTaggedWithTheBatch() {
        let list = [message(1), message(2), message(3, .text)]
        let entries = TransferLayout.entries(list, expanded: ["pc/op1"])
        XCTAssertEqual(entries, [.message(list[0], batch: "pc/op1"), .message(list[1], batch: "pc/op1"), .message(list[2], batch: nil)])
    }

    func testBatchDeliveryIsSendingThenFailedThenSent() {
        let sent = message(1, mine: true, delivery: .sent)
        let sending = message(2, mine: true, delivery: .sending)
        let failed = message(3, mine: true, delivery: .failed, failure: "连接中断，发送失败。")
        XCTAssertEqual(TransferLayout.delivery([sent, sending, failed]).0, .sending)
        let result = TransferLayout.delivery([sent, failed])
        XCTAssertEqual(result.0, .failed)
        XCTAssertEqual(result.1, "连接中断，发送失败。")
        XCTAssertEqual(TransferLayout.delivery([sent, sent]).0, .sent)
    }

    func testCollapseButtonGoesToTheFirstFullyVisibleItem() {
        let items = [message(1), message(2), message(3)]
        XCTAssertEqual(TransferLayout.collapseTarget(items, fullyVisible: ["3", "2"]), "2")
        XCTAssertNil(TransferLayout.collapseTarget(items, fullyVisible: []))
    }

    func testSwipeMovesOneCardCircularly() {
        XCTAssertEqual(TransferLayout.swipe(top: 0, count: 3, dx: -60, dy: 5), 1)
        XCTAssertEqual(TransferLayout.swipe(top: 1, count: 3, dx: 60, dy: 5), 0)
        XCTAssertEqual(TransferLayout.swipe(top: 2, count: 3, dx: -60, dy: 5), 0)
        XCTAssertEqual(TransferLayout.swipe(top: 0, count: 3, dx: 60, dy: 0), 2)
        XCTAssertEqual(TransferLayout.swipe(top: 0, count: 3, dx: -20, dy: 0), 0)
        // A fast flick switches below 40 pt.
        XCTAssertEqual(TransferLayout.swipe(top: 0, count: 3, dx: -20, dy: 0, velocity: -500), 1)
        XCTAssertEqual(TransferLayout.swipe(top: 0, count: 3, dx: -60, dy: 80), 0)
    }

    func testStackRolesAreCircularAndDependOnCount() {
        XCTAssertEqual(TransferLayout.roles(count: 2, top: 0), [0: .top, 1: .next])
        XCTAssertEqual(TransferLayout.roles(count: 3, top: 0), [0: .top, 1: .next, 2: .prev])
        XCTAssertEqual(TransferLayout.roles(count: 8, top: 0), [0: .top, 1: .next, 7: .prev, 2: .next2, 3: .farNext, 6: .farPrev])
        XCTAssertEqual(TransferLayout.roles(count: 4, top: 3)[0], .next)
    }

    func testStandardEasingIsAndroidsCurve() {
        XCTAssertEqual(TransferLayout.easeStandard(0), 0, accuracy: 1e-6)
        XCTAssertEqual(TransferLayout.easeStandard(1), 1, accuracy: 1e-6)
        // cubic-bezier(.2, 0, 0, 1) at t = 0.5.
        XCTAssertEqual(TransferLayout.easeStandard(0.5), 0.8778, accuracy: 0.001)
        XCTAssertEqual(TransferLayout.easeStandard(-1), 0, accuracy: 1e-6)
    }

    func testDraggingMovesTheTopCardAndRevealsTheNextOne() {
        let top = TransferLayout.dragPose(.top, dx: -60, count: 4)
        XCTAssertEqual(top.dx, -60)
        XCTAssertEqual(top.rotation, -60.0 / 120 * 11, accuracy: 1e-9)
        // Left swipe: the next card comes 85 % of the way once the finger is 84 pt away.
        let next = TransferLayout.dragPose(.next, dx: -84, count: 4)
        XCTAssertEqual(next.dx, 10 * 0.15, accuracy: 1e-9)
        XCTAssertEqual(next.z, TransferLayout.StackRole.next.pose.z)
        XCTAssertEqual(TransferLayout.dragPose(.prev, dx: -84, count: 4), TransferLayout.StackRole.prev.pose)
        // Right swipe reveals the previous card; with two cards it is always the next one.
        XCTAssertNotEqual(TransferLayout.dragPose(.prev, dx: 84, count: 4), TransferLayout.StackRole.prev.pose)
        XCTAssertNotEqual(TransferLayout.dragPose(.next, dx: 84, count: 2), TransferLayout.StackRole.next.pose)
    }

    func testSwitchFliesTheOldTopOutAndTucksItBehind() {
        let plan = TransferLayout.settle(count: 4, top: 0, dx: -50, direction: 1)
        XCTAssertEqual(plan.newTop, 1)
        XCTAssertEqual(plan.outgoing, 0)
        // Start: where the finger left the cards; end: the new roles around card 1.
        XCTAssertEqual(TransferLayout.settlePose(plan, index: 1, progress: 0), TransferLayout.dragPose(.next, dx: -50, count: 4))
        XCTAssertEqual(TransferLayout.settlePose(plan, index: 1, progress: 1).dx, TransferLayout.StackRole.top.pose.dx, accuracy: 1e-6)
        let out = TransferLayout.settlePose(plan, index: 0, progress: 1)
        XCTAssertEqual(out.dx, TransferLayout.StackRole.prev.pose.dx, accuracy: 1e-6)
        XCTAssertEqual(out.z, TransferLayout.StackRole.prev.pose.z)
        // The outgoing card is above everything while it flies out to the left…
        let early = stride(from: 0.0, to: 0.3, by: 0.005).first { TransferLayout.easeStandard($0) > 0.3 } ?? 0.1
        let flying = TransferLayout.settlePose(plan, index: 0, progress: early)
        XCTAssertEqual(flying.z, 9)
        XCTAssertLessThan(flying.dx, -50)
        // …and below the others in the second half.
        XCTAssertLessThan(TransferLayout.settlePose(plan, index: 0, progress: 0.6).z, 6)
    }

    func testMorphStaggersAroundTheRingAndCrossFadesFiles() {
        XCTAssertEqual(TransferLayout.ringDistance(8, 0, 7), 1)
        XCTAssertEqual(TransferLayout.morphDelay(count: 8, index: 4, anchor: 0), 80)
        XCTAssertEqual(TransferLayout.morphDelay(count: 20, index: 10, anchor: 0), 120)
        let start = TransferLayout.morphFaces(e: 0, hasCard: true, hasBubble: true, toBubble: true)
        XCTAssertEqual(start.card, 1)
        XCTAssertEqual(start.bubble, 0)
        let end = TransferLayout.morphFaces(e: 1, hasCard: true, hasBubble: true, toBubble: true)
        XCTAssertEqual(end.card, 0)
        XCTAssertEqual(end.bubble, 1)
        XCTAssertEqual(TransferLayout.morphFaces(e: 0.5, hasCard: true, hasBubble: true, toBubble: false).card, 0, accuracy: 1e-9)
        XCTAssertEqual(TransferLayout.morphFaces(e: 0.5, hasCard: false, hasBubble: true, toBubble: false).bubble, 1)
    }

    func testMessagesUseTheRowBesideTheAvatarWithEvenMargins() {
        // iPhone Air (420): 20 on both sides, the body is not capped below the row (380 − 44).
        XCTAssertEqual(TransferLayout.listMargin(width: 420), 20)
        XCTAssertEqual(TransferLayout.bodyMaxWidth(width: 420), .infinity)
        XCTAssertEqual(TransferLayout.bodyMaxWidth(width: 440), .infinity)
        // From 600 (iPad): 40 margins, the body at most 65 % of the row.
        XCTAssertEqual(TransferLayout.listMargin(width: 820), 40)
        XCTAssertEqual(TransferLayout.bodyMaxWidth(width: 820), 481)
    }

    func testFitContainsTheAspectInTheBox() {
        XCTAssertEqual(TransferLayout.fit(aspect: 4.0 / 3, in: TransferLayout.stackBox), CGSize(width: 120, height: 90))
        XCTAssertEqual(TransferLayout.fit(aspect: 9.0 / 16, in: TransferLayout.stackBox), CGSize(width: 90, height: 160))
        XCTAssertEqual(TransferLayout.fit(aspect: 3.0 / 4, in: TransferLayout.expandedImageBox), CGSize(width: 210, height: 280))
    }

    func testExpandKeepsTheTopItemWhereTheStackWas() {
        XCTAssertEqual(TransferLayout.expandShift(heights: [210, 70, 120], top: 0), -4)
        XCTAssertEqual(TransferLayout.expandShift(heights: [210, 70, 120], top: 2), 210 + 10 + 70 + 10 - 4)
        XCTAssertEqual(TransferLayout.batchGrowth(heights: [210, 70, 120]), 210 + 70 + 120 + 20 - 168)
    }

    func testFileCardIconAndExtension() {
        XCTAssertEqual(TransferLayout.fileIcon(name: "Q4 评审.pdf", folder: false), "file-text")
        XCTAssertEqual(TransferLayout.fileIcon(name: "排期.XLSX", folder: false), "file-spreadsheet")
        XCTAssertEqual(TransferLayout.fileIcon(name: "icons.zip", folder: false), "file-archive")
        XCTAssertEqual(TransferLayout.fileIcon(name: "素材", folder: true), "folder")
        XCTAssertEqual(TransferLayout.fileIcon(name: "a.bin", folder: false), "file")
        XCTAssertEqual(TransferLayout.fileExtension("会议纪要.docx"), "DOCX")
        XCTAssertNil(TransferLayout.fileExtension(".profile"))
        XCTAssertNil(TransferLayout.fileExtension("README"))
    }


    // MARK: Composer by the group's other devices (2026-11-11)

    private let office = SendTarget(id: "office", name: "Office-PC", isComputer: true)
    private let macbook = SendTarget(id: "macbook", name: "MacBook-Pro", isComputer: true)
    private let android = SendTarget(id: "android", name: "Android", isComputer: false)
    private func off(_ d: SendTarget) -> SendTarget { var d = d; d.online = false; return d }
    private func targets(_ online: [SendTarget], others: Int, only: SendTarget? = nil) -> SendTargets {
        SendTargets(online: online, otherDevices: others, onlyDevice: only)
    }

    func testOneOtherDeviceHasNoPickerAndFollowsItsState() {
        let pc = TransferLayout.composer(targets([office], others: 1, only: office), chosen: "android")
        XCTAssertEqual(pc, .single(office))
        XCTAssertFalse(pc.showsPicker)
        XCTAssertTrue(pc.enabled)
        XCTAssertEqual(TransferLayout.composerPrompt(pc, chinese: true), "发送给 Office-PC")
        XCTAssertEqual(TransferLayout.composerPrompt(.single(android), chinese: false), "Send to Android")
        let offline = TransferLayout.composer(targets([], others: 1, only: off(office)), chosen: nil)
        XCTAssertFalse(offline.enabled)
        XCTAssertEqual(TransferLayout.composerPrompt(offline, chinese: true), "Office-PC 不在线")
        XCTAssertEqual(TransferLayout.composerPrompt(offline, chinese: false), "Office-PC is offline")
    }

    func testNoneOnlineKeepsThePickerAndDisablesTheField() {
        for others in [0, 3] {
            let composer = TransferLayout.composer(targets([], others: others), chosen: "office")
            XCTAssertEqual(composer, .choose(target: nil))
            XCTAssertTrue(composer.showsPicker)
            XCTAssertFalse(composer.enabled)
            XCTAssertEqual(TransferLayout.composerPrompt(composer, chinese: true), "没有其它设备在线")
            XCTAssertEqual(TransferLayout.composerPrompt(composer, chinese: false), "No other devices online")
        }
    }

    func testAComputerPastesEverywhereAPhoneOnlyReceives() {
        let pc = TransferLayout.composer(targets([office, android], others: 2), chosen: "office")
        XCTAssertEqual(TransferLayout.composerPrompt(pc, chinese: true), "发送给 Office-PC，可在所有设备粘贴")
        let phone = TransferLayout.composer(targets([office, android], others: 2), chosen: "android")
        XCTAssertEqual(phone.target, android)
        XCTAssertEqual(TransferLayout.composerPrompt(phone, chinese: true), "发送给 Android")
        XCTAssertEqual(TransferLayout.composerPrompt(phone, chinese: false), "Send to Android")
    }

    func testAnOfflineChoiceGivesWayToAComputerFirst() {
        // The chosen phone is no longer online: the first online computer takes over, in order.
        XCTAssertEqual(TransferLayout.composer(targets([macbook, android], others: 4), chosen: "iphone").target, macbook)
        // No computer online: the first online phone.
        XCTAssertEqual(TransferLayout.composer(targets([android], others: 3), chosen: "iphone").target, android)
        // No choice yet takes the same way.
        XCTAssertEqual(TransferLayout.composer(targets([android, office], others: 2), chosen: nil).target, android)
        XCTAssertEqual(TransferLayout.switchNotice(from: "iPhone", chinese: true), "iPhone 已离线，已切回当前活跃")
        XCTAssertEqual(TransferLayout.switchNotice(from: "iPhone", chinese: false), "iPhone is offline. Switched back to Currently active.")
    }

    // MARK: Phone-to-phone lines

    func testDeliveryLinesNameThePhone() {
        func line(_ state: TransferDeliveryState?, _ peer: String?, _ progress: Int? = nil, _ failure: String? = nil) -> String? {
            TransferLayout.deliveryLine(state, peer: peer, progress: progress, failure: failure, chinese: true)
        }
        XCTAssertEqual(line(.sending, "Android", 45), "正在发送给 Android · 45%")
        XCTAssertEqual(line(.sending, "Android", 0), "正在发送给 Android…")
        XCTAssertEqual(line(.sending, nil, 45), "发送进度 45%")
        XCTAssertEqual(line(.sending, nil), "正在发送…")
        XCTAssertEqual(line(.sent, "Android"), "已发送给 Android")
        XCTAssertNil(line(.sent, nil))
        XCTAssertEqual(line(.failed, "Android", 62, "对方已断开，发送失败"), "对方已断开，发送失败")
        XCTAssertEqual(line(.failed, "Android"), "发送失败")
        XCTAssertEqual(line(.failed, nil, nil, "电脑不可用"), "电脑不可用")
        XCTAssertEqual(line(.cancelled, "Android"), "已取消")
        XCTAssertNil(line(nil, nil))
        XCTAssertEqual(TransferLayout.deliveryLine(.sent, peer: "Android", progress: nil, failure: nil, chinese: false), "Sent to Android")
        XCTAssertEqual(TransferLayout.receiveLine(.failed, failure: "对方已断开，接收失败", chinese: true), "对方已断开，接收失败")
        XCTAssertEqual(TransferLayout.receiveLine(.failed, failure: nil, chinese: true), "接收失败")
        XCTAssertEqual(TransferLayout.receiveLine(.cancelled, failure: nil, chinese: true), "已取消")
        XCTAssertNil(TransferLayout.receiveLine(.downloading, failure: nil, chinese: true))
    }

    func testOnlyTransfersInFlightCanBeCancelled() {
        var own = message(1, .file, mine: true, delivery: .sending)
        XCTAssertEqual(TransferLayout.inFlight(own), .sending)
        own.delivery = .sent
        XCTAssertNil(TransferLayout.inFlight(own))
        var received = message(2, .file)
        received.state = .downloading
        // A download from a computer is not a phone-to-phone receive.
        XCTAssertNil(TransferLayout.inFlight(received))
        received.directPeerID = "android"
        XCTAssertEqual(TransferLayout.inFlight(received), .receiving)
        received.state = .cancelled
        XCTAssertNil(TransferLayout.inFlight(received))
    }

    /// 未展开的聚合长按不能复制 (user 2026-11-12): a collapsed stack's menu has no 复制 in any
    /// state; an expanded batch's items keep their own 复制.
    func testCollapsedStackOffersNoCopyExpandedItemsDo() {
        var downloaded = message(1)
        downloaded.localPath = "/tmp/a.png"
        var placeholder = message(2)
        placeholder.state = .placeholder
        var downloading = message(3)
        downloading.state = .downloading
        let file = message(4, .file)
        for item in [downloaded, placeholder, downloading, file] {
            XCTAssertFalse(TransferLayout.offersCopy(item, collapsedStack: true), item.id)
        }
        // The same records as rows of an expanded batch: 复制 only for the downloaded picture.
        XCTAssertEqual(TransferLayout.entries([downloaded, placeholder], expanded: ["pc/op1"]).count, 2)
        XCTAssertTrue(TransferLayout.offersCopy(downloaded, collapsedStack: false))
        XCTAssertFalse(TransferLayout.offersCopy(placeholder, collapsedStack: false))
        XCTAssertFalse(TransferLayout.offersCopy(downloading, collapsedStack: false))
        XCTAssertFalse(TransferLayout.offersCopy(file, collapsedStack: false))
        XCTAssertTrue(TransferLayout.offersCopy(message(5, .text), collapsedStack: false))
    }

    // MARK: Successful captions belong to every direct phone bubble

    /// The rows that show their send line, by entry id.
    private func deliveryLines(_ list: [TransferMessage], expanded: Set<String> = []) -> [String] {
        let entries = TransferLayout.entries(list, expanded: expanded)
        return entries.filter { entry in
            var head = entry.last
            if case .stack(_, let items) = entry { head.delivery = TransferLayout.delivery(items).0 }
            return head.isMine && TransferLayout.showsDeliveryLine(state: head.delivery, direct: head.isDirect)
        }.map(\.id)
    }

    func testPcAndGroupSuccessHaveNoCaption() {
        // transfer-sent-latest: file, picture, folder (failed), file; only the last says 已发送
        // and the failure keeps its own line.
        let list = [message(1, .text, operation: nil), message(2, .file, operation: nil, mine: true, delivery: .sent),
                    message(3, .image, operation: nil, mine: true, delivery: .sent),
                    message(4, .folder, operation: nil, mine: true, delivery: .failed, failure: "对方已断开，发送失败"),
                    message(5, .file, operation: nil, mine: true, delivery: .sent)]
        XCTAssertEqual(deliveryLines(list), ["4"])
    }

    func testOwnTextAfterAFileDoesNotTakeItsSentLine() {
        let list = [message(1, .file, operation: nil, mine: true, delivery: .sent), message(2, .text, operation: nil, mine: true, delivery: .sent),
                    message(3, .text, operation: nil)]
        XCTAssertEqual(deliveryLines(list), [])
        // A text still shows a send in flight or a failure.
        let failing = [message(4, .file, operation: nil, mine: true, delivery: .sent),
                       message(5, .text, operation: nil, mine: true, delivery: .failed)]
        XCTAssertEqual(deliveryLines(failing), ["5"])
    }

    func testNewestSendingOrFailedHidesEveryEarlierSent() {
        let sending = [message(1, .file, operation: nil, mine: true, delivery: .sent),
                       message(2, .file, operation: nil, mine: true, delivery: .sending)]
        XCTAssertEqual(deliveryLines(sending), ["2"])
        let failed = [message(1, .file, operation: nil, mine: true, delivery: .sent),
                      message(2, .image, operation: nil, mine: true, delivery: .failed, failure: "发送失败")]
        XCTAssertEqual(deliveryLines(failed), ["2"])
        // Received messages after the newest own one do not count.
        let answered = [message(1, .file, operation: nil, mine: true, delivery: .sent), message(2, .file, operation: nil)]
        XCTAssertEqual(deliveryLines(answered), [])
    }

    func testSharedHistoryRoutingUsesActiveComputerAndThe300MBThreshold() {
        let pc=SendTarget(id:"pc",name:"Computer",isComputer:true)
        let phone=SendTarget(id:"phone",name:"Phone",isComputer:false)
        let group=SendTargets(activeDevice:"pc",online:[phone,pc],otherDevices:2)
        XCTAssertEqual(automaticShareTarget(group,bytes:299_999_999),pc)
        XCTAssertNil(automaticShareTarget(group,bytes:300_000_000))
        XCTAssertNil(automaticShareTarget(group,bytes:nil))
        XCTAssertNil(automaticShareTarget(SendTargets(online:[phone],otherDevices:2),bytes:1))
        var offline=phone; offline.online=false
        XCTAssertEqual(automaticShareTarget(SendTargets(otherDevices:1,onlyDevice:offline),bytes:nil),offline)
        XCTAssertEqual(TransferLayout.composer(group,chosen:nil).target,pc)
        XCTAssertEqual(TransferLayout.composer(SendTargets(activeDevice:"phone",online:[phone,pc],otherDevices:2),chosen:"gone").target,phone)
    }

    func testEveryDirectSuccessHasACaptionIncludingOlderBubbles() {
        var first=message(1,.file,operation:nil,mine:true,delivery:.sent)
        first.directPeerID="phone"; first.directPeerName="Phone"
        var second=message(2,.file,operation:nil,mine:true,delivery:.sent)
        second.directPeerID="phone"; second.directPeerName="Phone"
        XCTAssertEqual(deliveryLines([first,second]),["1","2"])
        XCTAssertNil(TransferLayout.deliveryLine(.sent,peer:nil,progress:nil,failure:nil,chinese:true))
    }

    func testABatchIsOneMessageForTheSentLine() {
        let older = message(1, .file, operation: nil, mine: true, delivery: .sent)
        let batch = [message(2, .image, operation: "b", mine: true, delivery: .sent), message(3, .file, operation: "b", mine: true, delivery: .sent)]
        // Collapsed: one line under the stack.
        XCTAssertEqual(deliveryLines([older] + batch), [])
        // Expanded: the batch is still the newest; its last item carries the line.
        let entries = TransferLayout.entries([older] + batch, expanded: ["self/b"])
        XCTAssertEqual(entries.count,3)
        // A batch followed by an own single file gives the line to the file.
        XCTAssertEqual(deliveryLines(batch + [message(4, .folder, operation: nil, mine: true, delivery: .sent)]), [])
    }

    // MARK: Picture sizes (native-rewrite.md「图片尺寸」)

    func testPicturesAre220WideAtTheirRecordedProportions() {
        let size = { (aspect: Double?) in TransferLayout.imageSize(aspect: TransferLayout.imageAspect(aspect)) }
        // An own 4:3 photo as sent (the core's image_width/height): no longer cropped to 260:146.
        XCTAssertEqual(size(4.0 / 3), CGSize(width: 220, height: 165))
        // A received picture at its thumbnail's proportions.
        XCTAssertEqual(size(1.6), CGSize(width: 220, height: 138))
        // Unknown: the 260 : 146 placeholder, 220 × 124.
        XCTAssertEqual(size(nil), CGSize(width: 220, height: 124))
    }

    func testPictureProportionsAreHeldBetweenThreeFourthsAndTwo() {
        // A phone screenshot (9:16) stops at 3:4, a panorama at 2:1.
        XCTAssertEqual(TransferLayout.imageAspect(9.0 / 16), 0.75)
        XCTAssertEqual(TransferLayout.imageSize(aspect: 0.75), CGSize(width: 220, height: 293))
        XCTAssertEqual(TransferLayout.imageAspect(21.0 / 9), 2)
        XCTAssertEqual(TransferLayout.imageSize(aspect: 2), CGSize(width: 220, height: 110))
        // A broken size falls back to the placeholder.
        XCTAssertEqual(TransferLayout.imageAspect(0), 260.0 / 146)
        XCTAssertEqual(TransferLayout.imageAspect(.nan), 260.0 / 146)
    }

    // MARK: File cards (native-rewrite.md「文件卡片」)

    private func card(_ status: TransferDownloadStatus?, kind: TransferMessageKind = .file,
                      progress: Int? = nil) -> TransferLayout.FileCardLook {
        TransferLayout.fileCard(kind: kind, mine: false, status: status, delivery: nil, progress: progress)
    }

    func testReceivedFileButtonFollowsTheDownloadStatus() {
        XCTAssertEqual(card(.notDownloaded), TransferLayout.FileCardLook(slot: .download))
        XCTAssertEqual(card(.downloading, progress: 45).slot, .progress(45))
        XCTAssertEqual(card(.downloading, progress: 100).slot, .progress(99), "the ring never reads 100 before the check")
        XCTAssertEqual(card(.downloaded), TransferLayout.FileCardLook(slot: .done))
        // A download that failed is tried again from the button, with 下载失败 in red.
        XCTAssertEqual(card(.downloadFailed), TransferLayout.FileCardLook(slot: .retry, reason: .downloadFailed))
        // Source offline: a grey download icon that does nothing, the type colour kept.
        XCTAssertEqual(card(.sourceOffline), TransferLayout.FileCardLook(slot: .sourceOffline, reason: .sourceOffline))
        // Content gone: no button, a grey tile, the card does nothing.
        XCTAssertEqual(card(.expired), TransferLayout.FileCardLook(reason: .expired, greyTile: true, inert: true))
        // No status (a folder row without a record): not downloaded.
        XCTAssertEqual(card(nil).slot, .download)
    }

    func testPhoneToPhoneReceiveFailuresAreFinal() {
        XCTAssertEqual(card(.receiveFailed), TransferLayout.FileCardLook(slot: .receiveFailed, greyTile: true))
        XCTAssertEqual(card(.cancelled), TransferLayout.FileCardLook(greyTile: true))
    }

    func testFoldersAndOwnCardsHaveNoButton() {
        for status: TransferDownloadStatus in [.notDownloaded, .downloading, .downloaded, .downloadFailed, .receiveFailed, .sourceOffline] {
            XCTAssertNil(card(status, kind: .folder).slot, status.rawValue)
        }
        XCTAssertEqual(card(.expired, kind: .folder).reason, .expired)
        XCTAssertEqual(card(.sourceOffline, kind: .folder).reason, .sourceOffline)
        XCTAssertNil(card(.downloadFailed, kind: .folder).reason)
        XCTAssertTrue(card(.receiveFailed, kind: .folder).greyTile)
        let own = { (delivery: TransferDeliveryState) in
            TransferLayout.fileCard(kind: .file, mine: true, status: nil, delivery: delivery, progress: 45)
        }
        XCTAssertEqual(own(.sending), TransferLayout.FileCardLook())
        // A failed send keeps its colours; one the receiving phone cancelled turns grey.
        XCTAssertFalse(own(.failed).greyTile)
        XCTAssertTrue(own(.cancelled).greyTile)
    }

    func testFolderListingPicturesByName() {
        XCTAssertTrue(TransferLayout.isPictureName("现场 01.JPG"))
        XCTAssertTrue(TransferLayout.isPictureName("IMG_2041.heic"))
        XCTAssertFalse(TransferLayout.isPictureName("验收单.pdf"))
        XCTAssertFalse(TransferLayout.isPictureName("图纸"))
    }
    func testShareSourceSensitivityUsesMetadataWithoutInspectingContent() {
        XCTAssertEqual(ShareSensitivity.sourceTypes(["public.utf8-plain-text"]), .normal)
        XCTAssertEqual(ShareSensitivity.sourceTypes(["public.html", "org.nspasteboard.ConcealedType"]), .concealed)
        XCTAssertEqual(ShareSensitivity.sourceTypes(["org.nspasteboard.TransientType"]), .transient)
        XCTAssertEqual(ShareSensitivity.sourceTypes(["org.nspasteboard.AutoGeneratedType"]), .autoGenerated)
        let payload = SharePayload(text: "fixture", sensitivity: .concealed)
        XCTAssertEqual(try JSONDecoder().decode(SharePayload.self, from: JSONEncoder().encode(payload)).sensitivity, .concealed)
    }

}
