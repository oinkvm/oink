package com.hachiwii.oink

import android.content.ContentProvider
import android.content.ContentValues
import android.net.Uri
import com.hachiwii.oink.ui.*

/**
 * Debug-only design scenes: the states of the mobile design mockup (oink-mobile-ui src/scenes.js),
 * opened with `am start -n com.hachiwii.oink/.MainActivity --es previewScene <id> --es previewTheme light|dark`.
 * The data is the mockup's sample data; release builds do not contain this file.
 */
object DesignFixtures {
    /** The mockup's "今天 · 09:41" date line. */
    private fun todayAt(hour: Int, minute: Int) = java.util.Calendar.getInstance().apply {
        set(java.util.Calendar.HOUR_OF_DAY, hour); set(java.util.Calendar.MINUTE, minute); set(java.util.Calendar.SECOND, 0)
    }.timeInMillis
    private val peers = listOf(
        PeerUi("windowsPc", "Windows PC", "windows", latency = 4.0, mouse = "shared", keyboard = "blocked", speaker = "forward", mic = "receive"),
        PeerUi("macMini", "Mac mini", "mac", latency = 6.0, mouse = "blocked", keyboard = "blocked", speaker = "forward", mic = "unavailable"),
        PeerUi("iphone-peer", "iPhone", "iphone", latency = 8.0, mouse = "shared", keyboard = "blocked", speaker = "outlet", mic = "send"),
        PeerUi("ipad-peer", "iPad", "ipad", latency = 5.0, mouse = "shared", keyboard = "shared", speaker = "outlet", mic = "send"),
        PeerUi("android-peer", "Android", "android", latency = 12.0, mouse = "shared", keyboard = "shared", speaker = "forward", mic = "receive"),
        PeerUi("desktop", "DESKTOP-9AO8GPM", "windows", offline = true, lastSeen = "5 分钟"),
    )
    // The sheet scenes show Windows PC with demo audio modes (levels on the 0–200 scale), pointer lock and auto game mode.
    private val windowsPcSheet = peers[0].copy(speakerLevel = 130, micLevel = 80, locked = true, gaming = "auto")
    private val localMac = PeerUi("local", "Mac-mini.local", "mac", latency = 1.0, mouse = "shared", keyboard = "shared")

    private const val SELF = "Phone"
    private val messages = listOf(
        MessageUi(1, false, "windowsPc", "Windows PC", "windows", true, "text", text = "这版界面已经整理好了，发给你看看。", createdMs = todayAt(9, 41)),
        MessageUi(2, true, "self", SELF, "android", true, "text", text = "收到，我在手机上确认一下。"),
        MessageUi(3, false, "macMini", "Mac mini", "mac", true, "image", name = "桌面预览.png", progress = 35),
        MessageUi(4, false, "windowsPc", "Windows PC", "windows", true, "file", name = "设计说明.pdf", detail = "PDF · 2.4 MB"),
        MessageUi(5, false, "desktop", "DESKTOP-9AO8GPM", "windows", false, "folder", name = "项目素材", detail = "12 个文件"),
        MessageUi(6, false, "android-peer", "Android", "android", true, "file", name = "会议纪要.docx", detail = "DOCX · 86 KB", proxyName = "Windows PC", proxyPlatform = "windows", proxyOnline = true, proxyId = "windowsPc"),
        MessageUi(7, false, "android-peer", "Android", "android", true, "image", name = "白板照片.jpg", proxyName = "Windows PC", proxyPlatform = "windows", proxyOnline = true, proxyId = "windowsPc", progress = 100),
    )
    private val folder = FolderUi("项目素材", listOf(FolderItemUi("f1", "image", "界面预览.png", "1.8 MB", 100), FolderItemUi("f2", "file", "设计说明.pdf", "2.4 MB", 70),
        FolderItemUi("f3", "folder", "图标资源", "8 个文件")))

    // ---- file cards (mockup scenes transfer-file-*, transfer-sent-latest, transfer-images, transfer-folder-states) ----

    private fun from(peer: String, id: Long, kind: String, name: String = "", detail: String = "", text: String = "", progress: Int? = null,
                     direct: Boolean = false, ended: String? = null, failure: String? = null, unavailable: String? = null, batch: String? = null,
                     aspect: Float? = null): MessageUi {
        val p = peers.first { it.id == peer }
        return MessageUi(id, false, p.id, p.name, p.platform, !p.offline, kind, text = text, name = name, detail = detail, progress = progress,
            direct = direct, ended = ended, downloadFailure = failure, unavailable = unavailable, batch = batch, createdMs = todayAt(9, 41), aspect = aspect)
    }
    private fun mine(id: Long, kind: String, name: String = "", detail: String = "", progress: Int = 100, to: String? = null, ended: String? = null, aspect: Float? = null) =
        MessageUi(id, true, "self", SELF, "android", true, kind, name = name, detail = detail, progress = progress.takeIf { it < 100 },
            sent = progress >= 100 || ended != null, direct = to != null, toName = to, ended = ended, createdMs = todayAt(9, 41), aspect = aspect)
    private val batchFiles = listOf(
        from("windowsPc", 11, "file", "Q4 设计评审.pdf", "PDF · 2.4 MB", batch = "windowsPc:x1"),
        from("windowsPc", 12, "file", "会议纪要.docx", "DOCX · 86 KB", progress = 45, batch = "windowsPc:x1"),
        from("windowsPc", 13, "file", "排期表.xlsx", "XLSX · 312 KB", progress = 100, batch = "windowsPc:x1"),
        from("windowsPc", 14, "file", "图标资源（全部尺寸）.zip", "ZIP · 18.6 MB", failure = "下载失败", batch = "windowsPc:x1"),
        from("windowsPc", 15, "folder", "项目素材", "12 个文件", batch = "windowsPc:x1"),
    )
    private val fileScenes: Map<String, List<MessageUi>> by lazy { mapOf(
        "transfer-file-received" to listOf(
            from("windowsPc", 1, "file", "设计说明.pdf", "PDF · 2.4 MB"),
            from("windowsPc", 2, "file", "会议纪要.docx", "DOCX · 86 KB", progress = 45),
            from("windowsPc", 3, "file", "排期表（11 月第二周，含节假日调整）.xlsx", "XLSX · 312 KB", progress = 100),
            from("macMini", 4, "folder", "项目素材", "12 个文件")),
        "transfer-file-problems" to listOf(
            from("android-peer", 1, "file", "现场照片（原图）.zip", "ZIP · 486 MB", progress = 62, direct = true, ended = "failed"),
            from("android-peer", 2, "file", "合同扫描.pdf", "PDF · 1.2 MB", progress = 30, direct = true, ended = "cancelled"),
            from("macMini", 3, "file", "Oink-0.3.4.dmg", "DMG · 48.2 MB", unavailable = "expired"),
            from("desktop", 4, "file", "报价单 v3.docx", "DOCX · 240 KB", unavailable = "offline")),
        "transfer-file-sent" to listOf(
            mine(1, "file", "现场照片（原图）.zip", "ZIP · 486 MB", progress = 45, to = "Android"),
            mine(2, "file", "合同扫描.pdf", "PDF · 1.2 MB"),
            mine(3, "file", "2026 年第四季度产品路线图与里程碑（评审修订版，含附录）.docx", "DOCX · 1.6 MB"),
            mine(4, "folder", "现场照片", "24 个文件", to = "Android", ended = "failed")),
        "transfer-file-batch-received" to listOf(from("windowsPc", 10, "text", text = "这几个文件你看一下。")) + batchFiles,
        "transfer-file-compare-received" to listOf(from("windowsPc", 1, "file", "设计说明.pdf", "PDF · 2.4 MB")) +
            batchFiles.take(4).map { it.copy(progress = null, downloadFailure = null) },
        "transfer-sent-latest" to listOf(
            from("windowsPc", 1, "text", text = "资料都发我一下。"),
            mine(2, "file", "合同扫描.pdf", "PDF · 1.2 MB"),
            mine(3, "image", "白板照片.jpg", aspect = 3f / 4f),
            mine(4, "folder", "现场照片", "24 个文件", to = "Android", ended = "failed"),
            mine(5, "file", "排期表.xlsx", "XLSX · 312 KB")),
        "transfer-images" to listOf(
            from("macMini", 1, "image", "桌面预览.png", progress = 100, aspect = 8f / 5f),
            mine(2, "image", "白板照片.jpg", aspect = 3f / 4f),
            from("windowsPc", 3, "image", "界面截图.png", progress = 40)),
    ) }
    private val folderStates = FolderUi("现场资料", listOf(
        FolderItemUi("1", "image", "现场 01.jpg", "3.2 MB", progress = 100, entryId = 1),
        FolderItemUi("2", "image", "现场 02.jpg", "2.9 MB", entryId = 2),
        FolderItemUi("3", "file", "验收单.pdf", "1.1 MB", progress = 40, entryId = 3),
        FolderItemUi("4", "file", "施工说明（第二版，含图纸目录与变更记录）.docx", "640 KB", entryId = 4),
        FolderItemUi("5", "file", "材料清单.xlsx", "88 KB", progress = 100, entryId = 5),
        FolderItemUi("6", "file", "原始照片.zip", "486 MB", entryId = 6, failure = "下载失败"),
        FolderItemUi("7", "folder", "图纸", "8 个文件", entryId = 7),
    ), messageId = 1)

    private val nearby = listOf(
        NearbyUi("studio", "Studio-Mac", "mac", "192.0.2.42", 47470, "7KQ4-M2PA-9DXF", groupSize = 3),
        NearbyUi("desk", "Windows-PC", "windows", "192.0.2.37", 47470, "C81H-WZ0R-4TNE"),
        NearbyUi("phone", "Phone", "android", "192.0.2.57", 47470, "Q5DJ-7PMV-2KXA"),
        NearbyUi("tablet", "客厅的 iPad", "ipad", "192.0.2.61", 47470, "H2RS-K9GB-6WCZ"),
        NearbyUi("old", "Windows-Laptop", "windows", "192.0.2.19", 47470, "M0VE-3QTY-8JHP", incompatible = true),
    )
    private val requests = listOf(
        NearbyUi("mba", "MacBook-Air", "mac", "192.0.2.66", 47470, "B6XN-1FKD-QW3S", groupSize = 2),
        NearbyUi("pixel", "Pixel 9", "android", "192.0.2.73", 47470, "T9AE-5HCM-0RVG"),
    )
    private val self = SelfInfo(listOf("192.0.2.88", "2001:db8::5", "198.51.100.7"), 47470, "4HNM-X8C2-QF7R")
    private val office = NearbyUi("manual-192.0.2.24", "OFFICE-PC", "windows", "192.0.2.24", 47470, "R3TB-8WQK-1M5Z")

    private val base = AppModel(selfName = SELF, peers = peers, nearby = nearby, self = self, messages = messages,
        speakerOn = true, micOn = false, version = "0.4.0 (106)", pairingPort = 37145)
    private val prefs = Prefs(cacheBytes = 1_352_663_040)

    fun scene(id: String): DesignScene? {
        var model = base
        var init = UiInit(freeze = true)
        var p = prefs
        fun settings(stack: List<String> = listOf("main"), dialog: String? = null, scroll: String? = null, step: Int = 1, toast: String? = null) {
            init = init.copy(settings = true, settingsStack = stack, settingsDialog = dialog, settingsScroll = scroll, privStep = step, settingsToast = toast)
        }
        fun sheet(peer: String, focus: String? = null, kick: Boolean = false, editIp: Boolean = false) {
            init = init.copy(sheetPeer = peer, sheetFocus = focus, sheetKick = kick, sheetEditIp = editIp)
        }
        fun out(status: String, reason: String = "") = PairingView(nearby[0], "out", status, code = if (status == "connecting") "" else "482913", reason = reason)
        fun inn(status: String, input: String = "", wrongs: Int = 0, reason: String = "") = PairingView(requests[0], "in", status, input = input, wrongs = wrongs, reason = reason)
        fun add() { init = init.copy(addPage = true) }
        fun manual(m: ManualState) { add(); init = init.copy(pairingSheet = "manual"); model = model.copy(manual = m) }
        fun pair(v: PairingView) { add(); model = model.copy(pairing = v) }
        when (id) {
            "devices" -> {}
            "devices-end" -> init = init.copy(scrollToEnd = true)
            "devices-online-only" -> model = model.copy(peers = peers.filter { !it.offline })
            "devices-offline-only" -> model = model.copy(peers = peers.filter { it.offline })
            "devices-empty" -> model = model.copy(peers = emptyList())
            "devices-request-badge" -> model = model.copy(requests = requests)
            "control" -> init = init.copy(tab = Tab.Control)
            "sheet" -> { model = model.copy(peers = peers.map { if (it.id == "windowsPc") windowsPcSheet else it }); sheet("windowsPc") }
            "sheet-macMini" -> sheet("macMini")
            "sheet-mobile" -> sheet("iphone-peer")
            "sheet-offline" -> sheet("desktop")
            "sheet-focus-mouse", "sheet-focus-gaming", "sheet-focus-speaker" -> {
                model = model.copy(peers = peers.map { if (it.id == "windowsPc") windowsPcSheet else it }); sheet("windowsPc", focus = id.removePrefix("sheet-focus-"))
            }
            "sheet-speaker-exit" -> { model = model.copy(peers = peers.map { if (it.id == "windowsPc") windowsPcSheet.copy(speaker = "outlet", speakerLevel = 65) else it }); sheet("windowsPc") }
            "sheet-kick" -> { model = model.copy(peers = peers.map { if (it.id == "windowsPc") windowsPcSheet else it }); sheet("windowsPc", kick = true) }
            "sheet-edit-ip" -> sheet("desktop", editIp = true)
            "transfer", "transfer-proxy-tag" -> init = init.copy(tab = Tab.Transfer)
            "transfer-typing" -> { init = init.copy(tab = Tab.Transfer); model = model.copy(composer = "晚上把会议纪要发我一下") }
            "transfer-attach" -> init = init.copy(tab = Tab.Transfer, transferMenu = "attach")
            "transfer-proxy" -> { init = init.copy(tab = Tab.Transfer, transferMenu = "proxy", transferProxy = "windowsPc"); model = model.copy(peers = listOf(localMac) + peers) }
            "transfer-proxy-offline" -> init = init.copy(tab = Tab.Transfer, transferProxy = "desktop", transferToast = "代理设备离线，请更换")
            "transfer-viewer-loading" -> init = init.copy(tab = Tab.Transfer, viewer = messages[2])
            "transfer-viewer" -> { model = model.copy(messages = messages.map { if (it.id == 3L) it.copy(progress = 100) else it }); init = init.copy(tab = Tab.Transfer, viewer = messages[2].copy(progress = 100)) }
            "transfer-folder" -> init = init.copy(tab = Tab.Transfer, folder = folder)
            "transfer-folder-states" -> init = init.copy(tab = Tab.Transfer, folder = folderStates)
            "transfer-file-received", "transfer-file-problems", "transfer-file-sent", "transfer-file-compare-received", "transfer-sent-latest", "transfer-images" -> {
                model = model.copy(messages = fileScenes.getValue(id)); init = init.copy(tab = Tab.Transfer)
            }
            "transfer-file-batch-received" -> {
                model = model.copy(messages = fileScenes.getValue(id)); init = init.copy(tab = Tab.Transfer, transferExpanded = setOf("windowsPc:x1"))
            }
            "share-indeterminate" -> model = model.copy(share = ShareCardUi("indeterminate", freezeT = .30f))
            "share-sending" -> model = model.copy(share = ShareCardUi("sending", 52, 2_470_000.0, freezeT = .30f))
            "share-sending-slow" -> model = model.copy(share = ShareCardUi("sending", 18, 512.0, freezeT = .30f))
            "share-done" -> model = model.copy(share = ShareCardUi("done", 100, elapsed = 3.18, freezeT = .30f))
            "settings" -> settings()
            "settings-transfer" -> settings(scroll = "transfer")
            "settings-audio" -> settings(scroll = "audio")
            "settings-end" -> settings(scroll = "end")
            "settings-accent" -> { p = p.copy(accent = Accent.Violet); settings() }
            "settings-clear-cache" -> settings(scroll = "transfer", dialog = "clearCache")
            "settings-cache-cleared" -> { p = p.copy(cacheBytes = 0); settings(scroll = "transfer", toast = "已清理 1.26 GB") }
            "settings-clipboard-on" -> { p = p.copy(privileged = "on", autoWriteClipboard = true, autoReadClipboard = true); settings(scroll = "clipboard") }
            "settings-openwith" -> { p = p.copy(openWith = true); settings(scroll = "system") }
            "settings-picker-scheme" -> settings(dialog = "picker:scheme")
            "settings-picker-language" -> settings(dialog = "picker:language")
            "settings-picker-home" -> settings(dialog = "picker:homeTab")
            "settings-picker-mic" -> settings(dialog = "picker:micAutoOff", scroll = "audio")
            "priv-off" -> settings(listOf("main", "privileged"))
            "priv-setup-1", "priv-setup-2", "priv-setup-3" -> { p = p.copy(privileged = "setup"); settings(listOf("main", "privileged"), step = id.last().digitToInt()) }
            "priv-starting" -> { p = p.copy(privileged = "starting"); settings(listOf("main", "privileged")) }
            "priv-on" -> { p = p.copy(privileged = "on"); settings(listOf("main", "privileged")) }
            "priv-failed" -> { p = p.copy(privileged = "failed"); settings(listOf("main", "privileged")) }
            "priv-disable-confirm" -> { p = p.copy(privileged = "on"); settings(listOf("main", "privileged"), dialog = "privDisable") }
            "licenses" -> settings(listOf("main", "licenses"))
            "add" -> add()
            "add-empty" -> { add(); model = model.copy(nearby = emptyList()) }
            "add-requests" -> { add(); model = model.copy(requests = requests) }
            "add-info" -> { add(); init = init.copy(pairingSheet = "info") }
            "add-info-all" -> { add(); init = init.copy(pairingSheet = "info-all") }
            "manual" -> manual(ManualState())
            "manual-invalid" -> manual(ManualState(ip = "192.168.1", error = "请输入有效的 IPv4 地址，例如 192.0.2.20", errorField = "ip"))
            "manual-searching" -> manual(ManualState(ip = "192.0.2.24", searching = true))
            "manual-found" -> manual(ManualState(ip = "192.0.2.24", found = office))
            "manual-notfound" -> manual(ManualState(ip = "192.0.2.250", error = "该地址上没有 Oink 设备"))
            "manual-timeout" -> manual(ManualState(ip = "192.0.2.9", error = "连接超时"))
            "pair-connecting" -> pair(out("connecting"))
            "pair-code" -> pair(out("code"))
            "pair-success-out" -> pair(out("success"))
            "pair-failed-rejected" -> pair(out("failed", "对方拒绝了配对"))
            "pair-failed-timeout" -> pair(out("failed", "连接超时"))
            "pair-failed-superseded" -> pair(out("failed", "已由另一配对会话接替"))
            "pair-entry" -> pair(inn("input"))
            "pair-entry-typing" -> pair(inn("input", "2468"))
            "pair-verifying" -> pair(inn("verifying", "246810"))
            "pair-wrong" -> pair(inn("wrong", "111111", 1))
            "pair-wrong-retry" -> pair(inn("input", "", 1))
            "pair-success" -> pair(inn("success", "246810"))
            "pair-failed-attempts" -> pair(inn("failed", wrongs = 3, reason = "尝试次数已用完"))
            "pair-paired-toast" -> { add(); model = model.copy(nearby = nearby.drop(1)); init = init.copy(toast = ToastData("已与 Studio-Mac 配对")) }
            else -> return null
        }
        return DesignScene(model, init, p, null)
    }
}

/** Registers the fixtures before any Activity starts (debug builds only, see src/debug/AndroidManifest.xml). */
class DesignFixtureInstaller : ContentProvider() {
    override fun onCreate(): Boolean { DesignHooks.fixture = DesignFixtures::scene; return true }
    override fun query(uri: Uri, projection: Array<out String>?, selection: String?, selectionArgs: Array<out String>?, sortOrder: String?) = null
    override fun getType(uri: Uri) = null
    override fun insert(uri: Uri, values: ContentValues?) = null
    override fun delete(uri: Uri, selection: String?, selectionArgs: Array<out String>?) = 0
    override fun update(uri: Uri, values: ContentValues?, selection: String?, selectionArgs: Array<out String>?) = 0
}
