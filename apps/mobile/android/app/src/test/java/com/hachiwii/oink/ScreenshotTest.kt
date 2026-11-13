package com.hachiwii.oink

import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.longClick
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onAllNodesWithContentDescription
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.onRoot
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performTouchInput
import androidx.compose.ui.test.swipeLeft
import com.github.takahirom.roborazzi.ExperimentalRoborazziApi
import com.github.takahirom.roborazzi.captureRoboImage
import com.github.takahirom.roborazzi.captureScreenRoboImage
import org.junit.Assume.assumeFalse
import com.hachiwii.oink.ui.*
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode
import org.robolectric.ParameterizedRobolectricTestRunner
import uniffi.oink_mobile_ffi.*

/**
 * Renders the main screens from repository-shaped data (device cards, transfer records) through the
 * same mappings OinkScreen uses, light and dark. Writes PNGs to build/outputs/roborazzi; it asserts
 * nothing about pixels. No emulator is needed (Robolectric native graphics).
 */
@RunWith(ParameterizedRobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
// A plain Application: OinkApplication would start the connection service and load the Rust core.
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xxhdpi", application = android.app.Application::class)
class ScreenshotTest(private val dark: Boolean) {
    companion object {
        @JvmStatic @ParameterizedRobolectricTestRunner.Parameters(name = "dark={0}")
        fun themes() = listOf(arrayOf<Any>(false), arrayOf<Any>(true))
    }

    @get:Rule val compose = createComposeRule()

    private fun on(token: String, command: CardCommand? = CardCommand.SET_LOCAL_OPTION) = ControlAction(token, command)
    private fun resource(mode: ResourceMode?) = ResourceState(mode, CardCondition.READY, on("tap"), on("shared"), on("local"), on("disabled"))
    private fun controls(mouse: ResourceMode, keyboard: ResourceMode, isExit: Boolean, activity: SpeakerActivity, mic: MicrophoneMode,
                         game: GameModePreference, locked: Boolean = false) = DeviceControls(
        resource(ResourceMode.SHARED), resource(mouse), resource(keyboard), resource(ResourceMode.SHARED),
        SpeakerState(isExit, activity, on("exit"), on("fwd-on"), on("fwd-off"), VolumeControl("fwd", "fwd", 130, false, VolumeKind.Forwarding)),
        MicrophoneState(mic, on("share"), on("local"), on("receive"), VolumeControl("mic", "mic", 80, false, VolumeKind.Forwarding)),
        GameState(game, game == GameModePreference.ON, null, on("auto"), on("on"), on("off")),
        FocusLockState(locked, false, on("focus")),
    )
    private val devices = listOf(
        Device("self", "Phone", true, isSelf = true, platform = "android", fingerprint = "4HNM-X8C2-QF7R"),
        Device("windowsPc", "Windows PC", true, computer = true, platform = "windows", latencyMillis = 4.0,
            controls = controls(ResourceMode.SHARED, ResourceMode.DISABLED, false, SpeakerActivity.FORWARDING, MicrophoneMode.RECEIVER, GameModePreference.AUTO, locked = true), paused = false),
        Device("macMini", "Mac mini", true, computer = true, platform = "macos", latencyMillis = 6.0,
            controls = controls(ResourceMode.LOCAL, ResourceMode.LOCAL, true, SpeakerActivity.PLAYING, MicrophoneMode.IDLE, GameModePreference.OFF), paused = false),
        Device("iphone", "iPhone", true, platform = "ios", latencyMillis = 8.0,
            mobileAudio = MobileAudioState(MobileEndpointState.LOCAL, MobileEndpointState.ACTIVE)),
        Device("desktop", "DESKTOP-9AO8GPM", false, computer = true, platform = "windows",
            lastSeenMillis = System.currentTimeMillis() - 5 * 60_000),
    )
    private val exit = ExitState("macMini", "Mac mini", 65, false, VolumeControl("exit", "exit", 65, false, VolumeKind.Output))
    private val peers = devices.filter { !it.isSelf }.map { it.toPeer(exit) }
    private val now = System.currentTimeMillis()
    private fun record(id: Long, outgoing: Boolean, sender: String, kind: String, name: String = "", text: String? = null,
                       size: Long = 0, download: String = "placeholder", state: MobileDeliveryState? = null, mime: String? = null) =
        TransferMessage(id, now - (10 - id) * 60_000, sender, null, "c$id", kind, name, mime, size, text, download, null, true, ByteArray(0),
            isOutgoing = outgoing, deliveryState = state, itemCount = if (kind == "folder") 12 else null, downloadStatus = if (outgoing) null else receivedStatus(download))
    private val records = listOf(
        record(1, false, "windowsPc", "text", text = "这版界面已经整理好了，发给你看看。"),
        record(2, true, "self", "text", text = "收到，我在手机上确认一下。", state = MobileDeliveryState.SENT),
        record(3, false, "macMini", "image", "桌面预览.png", size = 1_800_000, download = "downloading", mime = "image/png"),
        record(4, false, "windowsPc", "file", "设计说明.pdf", size = 2_400_000, mime = "application/pdf"),
        record(5, false, "desktop", "folder", "项目素材", size = 30_000_000),
        record(6, true, "self", "file", "会议纪要.docx", size = 86_000, state = MobileDeliveryState.SENDING),
    )
    private val appState = AppState(name = "Phone", transferMessages = records,
        transferProgress = mapOf(3L to TransferProgress(35, 100), 6L to TransferProgress(60, 100)))
    private val nearby = listOf(
        Device("studio", "Studio-Mac", false, computer = true, platform = "macos", pairTarget = "studio", fingerprint = "7KQ4-M2PA-9DXF",
            address = SocketEndpoint("192.0.2.42", 47470), groupSize = 3),
        Device("desk", "Windows-PC", false, computer = true, platform = "windows", pairTarget = "desk", address = SocketEndpoint("192.0.2.37", 47470)),
        Device("old", "Windows-Laptop", false, computer = true, platform = "windows", incompatible = true, address = SocketEndpoint("192.0.2.19", 47470)),
    ).map { it.toNearby() }
    private val model = AppModel(selfName = "Phone", peers = peers, nearby = nearby,
        self = SelfInfo(listOf("192.0.2.88", "2001:db8::5"), 47470, "4HNM-X8C2-QF7R"),
        messages = records.map { it.toUi(appState, devices.associateBy { d -> d.id }) },
        speakerOn = true, version = "0.4.0-dev (1)", deviceName = DeviceNameUi("Phone", "Phone", false))
    private val prefs = Prefs(scheme = if (dark) "dark" else "light", cacheBytes = 1_352_663_040)

    private fun shot(name: String, model: AppModel, init: UiInit, prefs: Prefs = this.prefs) {
        compose.setContent { OinkApp(model, init.copy(freeze = true), prefs, {}, AppActions(), dark) }
        compose.waitForIdle()
        compose.onRoot().captureRoboImage("build/outputs/roborazzi/${if (dark) "dark" else "light"}/$name.png")
    }

    @Test fun devices() = shot("devices", model, UiInit())
    @Test fun devicesEmpty() = shot("devices-empty", model.copy(peers = emptyList()), UiInit())
    /** Two Android phones: one in privileged mode (crowned badge), one without. */
    private val androidPeers = listOf(
        Device("pixel", "Pixel 9", true, platform = "android", latencyMillis = 12.0, androidPrivileged = true,
            mobileAudio = MobileAudioState(MobileEndpointState.LOCAL, MobileEndpointState.LOCAL)),
        Device("galaxy", "Galaxy S24", true, platform = "android", latencyMillis = 15.0,
            mobileAudio = MobileAudioState(MobileEndpointState.LOCAL, MobileEndpointState.LOCAL)),
    ).map { it.toPeer(exit) }
    /** This phone's speaker and microphone on (top bar), and another phone receiving / sending. */
    private val sendingPhone = Device("galaxy", "Galaxy S24", true, platform = "android", latencyMillis = 15.0,
        mobileAudio = MobileAudioState(MobileEndpointState.ACTIVE, MobileEndpointState.STARTING)).toPeer(exit)
    @Test fun devicesAudioOn() = shot("devices-audio-on", model.copy(peers = peers + sendingPhone, speakerOn = true, micOn = true), UiInit())
    @Test fun devicesAudioFailed() = shot("devices-audio-failed",
        model.copy(speakerOn = true, micOn = true, speakerFailed = true, micFailed = true), UiInit())
    @Test fun devicesAndroidPrivileged() = shot("devices-android-privileged", model.copy(peers = peers + androidPeers), UiInit())
    @Test fun sheetAndroidPrivileged() = shot("sheet-android-privileged", model.copy(peers = peers + androidPeers), UiInit(sheetPeer = "pixel"))
    @Test fun deviceSheet() = shot("sheet", model, UiInit(sheetPeer = "windowsPc"))
    @Test fun deviceSheetExit() = shot("sheet-exit", model, UiInit(sheetPeer = "macMini"))
    @Test fun transfer() = shot("transfer", model, UiInit(tab = Tab.Transfer))
    @Test fun addDevice() = shot("add", model, UiInit(addPage = true))
    @Test fun addDeviceRequests() = shot("add-requests", model.copy(requests = listOf(NearbyUi("r1", "MacBook-Air", "mac", "", 0, "B6XN-1FKD-QW3S", groupSize = 2))), UiInit(addPage = true))
    @Test fun addDeviceInfo() = shot("add-info", model, UiInit(addPage = true, pairingSheet = "info"))
    @Test fun pairingFailed() = shot("pair-failed-declined", model.copy(pairing = PairingView(nearby[0], "out", "failed", reason = pairProblemText(13u))), UiInit(addPage = true))
    @Test fun settings() = shot("settings", model, UiInit(settings = true))
    @Test fun settingsBackground() = shot("settings-no-background", model.copy(worksInBackground = false), UiInit(settings = true, settingsScroll = "system"),
        prefs.copy(persistentNotice = false, audioAvailable = false))

    /** Transfer page long-press menu, light only. The menu is a popup window, so the whole screen is captured. */
    @OptIn(ExperimentalRoborazziApi::class)
    private fun messageMenu(name: String, id: Long) {
        assumeFalse(dark)
        compose.setContent { OinkApp(model, UiInit(tab = Tab.Transfer, transferMessageMenu = id, freeze = true), prefs, {}, AppActions(), dark) }
        compose.waitForIdle()
        captureScreenRoboImage("build/outputs/roborazzi/light/$name.png")
    }
    @Test @Config(qualifiers = "+zh-rCN") fun transferMenuText() = messageMenu("transfer-menu-text", 1)
    @Test @Config(qualifiers = "+zh-rCN") fun transferMenuFile() = messageMenu("transfer-menu-file", 4)

    // ---- batches, long text, selection, preview (light and dark) ----

    private val pictures = java.nio.file.Files.createTempDirectory("oink-shots").toFile()
    /** A downloaded picture: a real file (the mapping checks it) whose decoded bitmap is put in the image cache. */
    private fun picture(name: String, w: Int, h: Int, hue: Float): String {
        val file = java.io.File(pictures, name).apply { writeBytes(ByteArray(1)) }
        val bitmap = android.graphics.Bitmap.createBitmap(w, h, android.graphics.Bitmap.Config.ARGB_8888)
        val canvas = android.graphics.Canvas(bitmap)
        val top = android.graphics.Color.HSVToColor(floatArrayOf(hue, .35f, .95f)); val bottom = android.graphics.Color.HSVToColor(floatArrayOf((hue + 40f) % 360f, .55f, .7f))
        canvas.drawPaint(android.graphics.Paint().apply { shader = android.graphics.LinearGradient(0f, 0f, 0f, h.toFloat(), top, bottom, android.graphics.Shader.TileMode.CLAMP) })
        canvas.drawCircle(w * .68f, h * .34f, minOf(w, h) * .16f, android.graphics.Paint(android.graphics.Paint.ANTI_ALIAS_FLAG).apply { color = 0xFFFFF4D6.toInt() })
        canvas.drawRect(0f, h * .72f, w.toFloat(), h.toFloat(), android.graphics.Paint().apply { color = android.graphics.Color.HSVToColor(floatArrayOf((hue + 180f) % 360f, .3f, .45f)) })
        cacheImage(file.absolutePath, 720, bitmap.asImageBitmap())
        return file.absolutePath
    }
    private val longText = (1..24).joinToString("\n") { "第 $it 行：长消息默认只显示前十行，第八到第十行渐变，点查看更多再加载二十行。" }
    private val sendOp = ByteArray(16) { 7 }
    private val receiveOp = ByteArray(16) { 9 }
    private val batchRecords by lazy {
        listOf(
            TransferMessage(21, now - 9 * 60_000, "windowsPc", null, "c21", "text", "Text", null, 0, longText, "complete", null, true, ByteArray(16) { 3 }),
            TransferMessage(22, now - 8 * 60_000, "windowsPc", null, "c22", "file", "季度报告.pdf", "application/pdf", 2_400_000, null, "complete", null, true, receiveOp,
                downloadStatus = receivedStatus("complete")),
            TransferMessage(23, now - 8 * 60_000, "windowsPc", null, "c23", "file", "数据表.xlsx", null, 380_000, null, "placeholder", null, true, receiveOp,
                downloadStatus = receivedStatus("placeholder")),
            TransferMessage(24, now - 8 * 60_000, "windowsPc", null, "c24", "file", "说明.txt", "text/plain", 4_000, null, "placeholder", null, true, receiveOp,
                downloadStatus = receivedStatus("placeholder")),
        ) + (0 until 4).map { i ->
            TransferMessage(25L + i, now - 2 * 60_000, "self", null, "c${25 + i}", "image", "IMG_$i.jpg", "image/jpeg", 1_200_000, null, "complete",
                picture("img$i.jpg", if (i == 0) 600 else 800, if (i == 0) 800 else 600, 30f + 70f * i), true, sendOp,
                isOutgoing = true, deliveryState = MobileDeliveryState.SENT, imageWidth = if (i == 0) 600 else 800, imageHeight = if (i == 0) 800 else 600)
        }
    }
    private val batchModel by lazy { model.copy(messages = batchRecords.map { it.toUi(appState, devices.associateBy { d -> d.id }) }) }
    private val dir get() = if (dark) "dark" else "light"
    private fun capture(name: String) { compose.waitForIdle(); compose.onRoot().captureRoboImage("build/outputs/roborazzi/$dir/$name.png") }
    @OptIn(ExperimentalRoborazziApi::class)
    private fun captureScreen(name: String) { compose.waitForIdle(); captureScreenRoboImage("build/outputs/roborazzi/$dir/$name.png") }
    private fun open(model: AppModel, init: UiInit) = compose.setContent { OinkApp(model, init, prefs, {}, AppActions(), dark) }

    // ---- transfer-stack design scenes (design specification), same content as the mockup ----

    private val longReceived = listOf("0.4.0 移动端联调记录（周一下午，参会：Alex、Windows PC、Mac mini）", "1. 传输页：长文本默认只显示前 10 行，第 8–10 行渐隐，点「查看更多」每次多显示 20 行；长按复制仍然复制全文。",
        "2. 多文件聚合：一次发送或接收的一批文件显示为一叠，左右划动切换最上面那张，点「展开」恢复成普通消息。", "3. 展开后，第一个完整可见的项旁边显示「收起」，滚动时隐藏。",
        "4. 键鼠：手机触控板单指划动速度再调低一档。", "5. 边缘锁定的进度环换成主题色。", "6. 游戏模式自动开启时在顶部提示一次。", "7. 音频：扬声器转发切换时不再出现爆音；麦克风接收端增加电平指示。",
        "8. 延迟统计放到设置 › 高级。", "9. 待确认：深色模式下文件卡片的底色；收到的消息「收起」按钮放在哪一侧。",
        "10. 已知问题：Phone 上 PC 复制的图片加载失败；输入法抬起时 Tab 栏没有隐藏；代理离线时 toast 被预览挡住；长按菜单阴影太重；在长消息靠后的位置长按没有反应。",
        "11. 测试设备：Phone、iPhone、Android Phone。", "12. 构建号 106，安装包在共享目录。", "13. 传输页输入框的 placeholder 改成「发送给 XXX，可在所有设备粘贴」。",
        "14. 自己发送的图片气泡往左偏了一段，需要对齐。", "15. Android 注册在「打开方式」里的名字改成「文件传输」。", "16. 特权模式引导页先检查是否已经处于开发者模式。",
        "17. 第三步输入配对码时，点输入框不弹键盘。", "18. 设备半页长按进入的详细设置状态，点空白处返回。", "19. Android 和电脑在同一个 Wi-Fi 下延时偏高，需要抓包确认。",
        "20. 本机 Mac 扬声器处于「转发」时，手机上显示成了「本地」。", "下周一再对一次，有问题直接在群里说。谢谢大家。").joinToString("\n")
    private val longSelf = listOf("收到，我这边的检查结果：", "长文本折叠：iOS 和 Android 都已经按 10 行实现，渐隐范围是第 8 到第 10 行。", "「查看更多」放在气泡里文字下方，和文字左对齐。",
        "自己发的气泡用主题色底，渐隐用透明度蒙版，同样适用。", "长按复制的是全文，不是可见部分。", "滚出一屏之后回来会恢复折叠，恢复时列表不跳动。",
        "深色模式也检查过了，横屏和大折叠内屏还没看，明天补上截图。").joinToString("\n")
    private val receivedOp = ByteArray(16) { 5 }
    private val sentOp2 = ByteArray(16) { 6 }
    private fun designText(id: Long, outgoing: Boolean, text: String, at: Long) = TransferMessage(id, at, if (outgoing) "self" else "windowsPc", null, "c$id", "text", "Text", null, 0,
        text, "complete", null, true, ByteArray(16) { (id + 40).toByte() }, isOutgoing = outgoing, deliveryState = if (outgoing) MobileDeliveryState.SENT else null)
    private fun designImage(id: Long, outgoing: Boolean, name: String, w: Int, h: Int, hue: Float, op: ByteArray, at: Long) = TransferMessage(id, at, if (outgoing) "self" else "macMini", null, "c$id",
        "image", name, "image/jpeg", 1_200_000, null, "complete", picture("d$id.jpg", w, h, hue), true, op,
        isOutgoing = outgoing, deliveryState = if (outgoing) MobileDeliveryState.SENT else null,
        downloadStatus = if (outgoing) null else receivedStatus("complete"), imageWidth = w, imageHeight = h)
    private fun designFile(id: Long, kind: String, name: String, size: Long, at: Long, items: Int? = null) = TransferMessage(id, at, "self", null, "c$id", kind, name, null, size,
        null, "complete", null, true, sentOp2, isOutgoing = true, deliveryState = MobileDeliveryState.SENT, itemCount = items)
    private val designRecords by lazy {
        val t = now - 20 * 60_000
        val aspects = listOf(600 to 800, 800 to 600, 800 to 450, 450 to 800, 600 to 600, 640 to 800, 840 to 360, 750 to 500)
        listOf(designText(40, false, longReceived, t), designText(41, true, longSelf, t + 1_000)) +
            aspects.mapIndexed { i, (w, h) -> designImage(50L + i, false, listOf("客厅 · 早上.jpg", "窗边.jpg", "午睡.jpg", "夜景.jpg", "沙丘.jpg", "花.jpg", "全景.jpg", "屏幕截图.png")[i],
                w, h, 20f + 45f * i, receivedOp, t + 60_000) } +
            listOf(designFile(60, "file", "Q4 设计评审.pdf", 2_400_000, t + 120_000), designImage(61, true, "白板照片.jpg", 600, 800, 50f, sentOp2, t + 120_000),
                designFile(62, "file", "图标资源（全部尺寸）.zip", 18_600_000, t + 120_000), designFile(63, "folder", "现场照片", 0, t + 120_000, items = 24),
                designFile(64, "file", "排期表.xlsx", 312_000, t + 120_000), designFile(65, "file", "会议纪要.docx", 86_000, t + 120_000))
    }
    private fun designModel(records: List<TransferMessage>) = model.copy(messages = records.map { it.toUi(appState, devices.associateBy { d -> d.id }) })
    private val stackModel by lazy { designModel(designRecords) }
    private val transferTab get() = UiInit(tab = Tab.Transfer)
    private fun stackNode(n: Int, top: Int) = compose.onNodeWithContentDescription("$n 项，当前第 $top 项", substring = true)
    private val px get() = compose.density.density

    @Test @Config(qualifiers = "+zh-rCN") fun transferStack() { open(stackModel, transferTab); capture("transfer-stack") }
    @Test @Config(qualifiers = "+zh-rCN") fun transferStackFiles() {
        open(stackModel, transferTab)
        stackNode(6, 1).performTouchInput { swipeLeft() }
        stackNode(6, 2).performTouchInput { swipeLeft() }
        capture("transfer-stack-files")
    }
    @Test @Config(qualifiers = "+zh-rCN") fun transferStackSwipe() {
        open(stackModel, transferTab)
        // 50 past the touch slop, held.
        stackNode(8, 1).performTouchInput { down(center); repeat(10) { moveBy(Offset(-5.8f * px, 0f)) } }
        capture("transfer-stack-swipe")
        stackNode(8, 1).performTouchInput { up() }
    }
    @Test @Config(qualifiers = "+zh-rCN") fun transferStackExpanded() {
        open(stackModel, transferTab)
        compose.onNodeWithContentDescription("展开全部 8 项").performClick()
        capture("transfer-stack-expanded")
    }
    @Test @Config(qualifiers = "+zh-rCN") fun transferStackExpandedSelf() {
        open(stackModel, transferTab)
        compose.onNodeWithContentDescription("展开全部 6 项").performClick()
        capture("transfer-stack-expanded-self")
    }
    @Test @Config(qualifiers = "+zh-rCN") fun transferLong() { open(designModel(designRecords.take(2)), transferTab); capture("transfer-long") }
    @Test @Config(qualifiers = "+zh-rCN") fun transferLongMore() {
        open(designModel(designRecords.take(1)), transferTab)
        compose.onNodeWithText("查看更多").performClick()
        capture("transfer-long-more")
    }

    /** Frames of the expand / collapse morph and of a swipe that switches cards ([at] ms in). */
    private fun motion(name: String, at: Long, init: UiInit = transferTab, start: () -> Unit) {
        open(stackModel, init)
        compose.waitForIdle()
        compose.mainClock.autoAdvance = false
        start()
        // Snapshot frame, switch frame, then the animation's own time.
        repeat(3) { compose.mainClock.advanceTimeByFrame() }
        compose.mainClock.advanceTimeBy(at)
        compose.onRoot().captureRoboImage("build/outputs/roborazzi/$dir/$name.png")
    }
    @Test @Config(qualifiers = "+zh-rCN") fun motionExpand() = motion("motion-expand-120ms", 120) { compose.onNodeWithContentDescription("展开全部 8 项").performClick() }
    @Test @Config(qualifiers = "+zh-rCN") fun motionExpandLate() = motion("motion-expand-240ms", 240) { compose.onNodeWithContentDescription("展开全部 8 项").performClick() }
    @Test @Config(qualifiers = "+zh-rCN") fun motionExpandSelf() = motion("motion-expand-self-160ms", 160) { compose.onNodeWithContentDescription("展开全部 6 项").performClick() }
    @Test @Config(qualifiers = "+zh-rCN") fun motionCollapse() = motion("motion-collapse-160ms", 160, UiInit(tab = Tab.Transfer, transferExpanded = setOf(stackModel.messages.first { it.id == 60L }.batch!!))) {
        compose.onNodeWithContentDescription("收起为一叠").performClick()
    }
    @Test @Config(qualifiers = "+zh-rCN") fun motionSwipe() = motion("motion-swipe-commit-120ms", 120 - 48) {
        stackNode(8, 1).performTouchInput { down(center); repeat(10) { moveBy(Offset(-8f * px, 0f)) }; up() }
    }

    /**
     * The transfer page with its tab bar: the input pill ends 8 above the bar. Robolectric never
     * dispatches window insets, so Compose takes the keyboard as shown and hides the bar; this
     * scene dispatches insets, keyboard hidden, to the Compose view (it listens for them itself).
     */
    @Test @Config(qualifiers = "+zh-rCN") fun transferTabBar() {
        var view: android.view.View? = null
        compose.setContent {
            view = androidx.compose.ui.platform.LocalView.current
            OinkApp(model, UiInit(tab = Tab.Transfer, freeze = true), prefs, {}, AppActions(), dark)
        }
        compose.runOnIdle {
            androidx.core.view.ViewCompat.dispatchApplyWindowInsets(view!!, androidx.core.view.WindowInsetsCompat.Builder()
                .setVisible(androidx.core.view.WindowInsetsCompat.Type.ime(), false).build())
        }
        capture("transfer-tabbar")
    }

    // ---- phone-to-phone (design specification scenes transfer-p2p-* and share-pick-*) ----

    private val mbp = PeerUi("mbp", "Mac-mini.local", "mac")
    private val windowsPcP = PeerUi("windowsPc", "Windows PC", "windows")
    private val macMiniP = PeerUi("macMini", "Mac mini", "mac")
    private val iphoneP = PeerUi("iphone", "iPhone", "iphone")
    private val ipadP = PeerUi("ipad", "iPad", "ipad")
    private val androidP = PeerUi("android", "Android", "android")
    private val p2pPeers = listOf(mbp, windowsPcP, macMiniP, iphoneP, ipadP, androidP)
    private val p2pAt = now - 3 * 60_000
    private var p2pId = 300L
    private fun from(peer: PeerUi, kind: String, text: String = "", name: String = "", detail: String = "", proxy: PeerUi? = null, progress: Int? = null,
                     imagePath: String? = null, direct: Boolean = false, ended: String? = null) =
        MessageUi(p2pId++, false, peer.id, peer.name, peer.platform, !peer.offline, kind, text, name, detail, proxyName = proxy?.name, proxyPlatform = proxy?.platform,
            proxyId = proxy?.id, progress = progress, createdMs = p2pAt, imagePath = imagePath, direct = direct, ended = ended)
    private fun mine(kind: String, text: String = "", name: String = "", detail: String = "", progress: Int? = null, to: String? = null, imagePath: String? = null,
                     ended: String? = null) =
        MessageUi(p2pId++, true, "self", "Phone", "android", true, kind, text, name, detail, progress = progress, sent = progress == null || progress >= 100,
            createdMs = p2pAt, imagePath = imagePath, direct = to != null, toName = to, ended = ended)
    private val zipName = "现场照片（原图）.zip"
    private fun history() = listOf(
        from(windowsPcP, "text", "这版界面已经整理好了，发给你看看。"),
        from(macMiniP, "image", name = "桌面预览.png", progress = 100, imagePath = picture("p2p-desk.png", 800, 450, 200f)),
        mine("file", name = "合同扫描.pdf", detail = "PDF · 1.2 MB", to = "iPad"),
        from(windowsPcP, "file", name = "设计说明.pdf", detail = "PDF · 2.4 MB"),
        from(androidP, "file", name = "会议纪要.docx", detail = "DOCX · 86 KB", proxy = windowsPcP))
    private val later get() = from(macMiniP, "text", "下午三点开会，别忘了。")
    private val sentPhoto get() = mine("image", name = "现场照片.jpg", to = "Android", imagePath = picture("p2p-site.jpg", 800, 450, 120f))
    /** This phone sends to Android: the zip at [zipProgress], or ended. */
    private fun sender(zipProgress: Int, ended: String? = null, zip: Boolean = true) = history() + listOf(
        mine("text", "整理好了，原图直接发到 Android 上。"), sentPhoto) +
        (if (zip) listOf(mine("file", name = zipName, detail = "ZIP · 486 MB", progress = zipProgress, to = "Android", ended = ended)) else emptyList()) + later
    /** iPhone sends to this phone: the photo and the zip arriving straight. */
    private fun receiver(photo: Int, zipProgress: Int, ended: String? = null, zip: Boolean = true) = history().filter { it.toName != "iPad" } + listOf(
        from(iphoneP, "text", "原图直接发给你，电脑上就不占地方了。"),
        from(iphoneP, "image", name = "现场照片.jpg", progress = photo, direct = true, ended = ended,
            imagePath = if (photo >= 100) picture("p2p-recv.jpg", 800, 450, 120f) else null)) +
        (if (zip) listOf(from(iphoneP, "file", name = zipName, detail = "ZIP · 486 MB", progress = zipProgress, direct = true, ended = ended)) else emptyList()) + later
    /** Online dots follow the scene's devices, as the real mapping does. */
    private fun p2pShot(name: String, messages: List<MessageUi>, init: UiInit = transferTab, peers: List<PeerUi> = p2pPeers) {
        fun online(id: String?) = peers.firstOrNull { it.id == id }?.offline == false
        shot(name, model.copy(peers = peers, messages = messages.map { m ->
            if (m.self) m else m.copy(senderOnline = online(m.senderId), proxyOnline = m.proxyId == null || online(m.proxyId))
        }), init)
    }

    @Test @Config(qualifiers = "+zh-rCN") fun p2pMenu() = p2pShot("transfer-p2p-menu", sender(45), UiInit(tab = Tab.Transfer, transferMenu = "proxy", transferProxy = "windowsPc"))
    @Test @Config(qualifiers = "+zh-rCN") fun p2pMenuPhone() = p2pShot("transfer-p2p-menu-phone", sender(45), UiInit(tab = Tab.Transfer, transferMenu = "proxy", transferProxy = "android"))
    @Test @Config(qualifiers = "+zh-rCN") fun p2pMenuOffline() = p2pShot("transfer-p2p-menu-offline", sender(100),
        UiInit(tab = Tab.Transfer, transferMenu = "proxy", transferProxy = "windowsPc"), p2pPeers.map { if (it.id == "macMini" || it.id == "iphone") it.copy(offline = true) else it })
    @Test @Config(qualifiers = "+zh-rCN") fun p2pAllOffline() = p2pShot("transfer-p2p-all-offline", sender(100), UiInit(tab = Tab.Transfer, transferProxy = "windowsPc"),
        p2pPeers.map { it.copy(offline = true) })
    @Test @Config(qualifiers = "+zh-rCN") fun p2pPhone() = p2pShot("transfer-p2p-phone", sender(45), UiInit(tab = Tab.Transfer, transferProxy = "android"))
    @Test @Config(qualifiers = "+zh-rCN") fun p2pOffline() = p2pShot("transfer-p2p-offline", sender(45), UiInit(tab = Tab.Transfer, transferProxy = "iphone"),
        p2pPeers.map { if (it.id == "iphone") it.copy(offline = true) else it })
    @Test @Config(qualifiers = "+zh-rCN") fun p2pSendingMenu() {
        val messages = sender(45)
        p2pShot("transfer-p2p-sending-menu", messages, UiInit(tab = Tab.Transfer, transferProxy = "android", transferMessageMenu = messages.first { it.name == zipName }.id))
    }
    @Test @Config(qualifiers = "+zh-rCN") fun p2pCancelledSender() = p2pShot("transfer-p2p-cancelled-sender", sender(0, zip = false), UiInit(tab = Tab.Transfer, transferProxy = "android"))
    @Test @Config(qualifiers = "+zh-rCN") fun p2pSendCancelledByPeer() = p2pShot("transfer-p2p-send-cancelled-by-peer", sender(62, "cancelled"), UiInit(tab = Tab.Transfer, transferProxy = "android"))
    @Test @Config(qualifiers = "+zh-rCN") fun p2pSendFailed() = p2pShot("transfer-p2p-send-failed", sender(62, "failed"), UiInit(tab = Tab.Transfer, transferProxy = "android"))
    @Test @Config(qualifiers = "+zh-rCN") fun p2pReceive() = p2pShot("transfer-p2p-receive", receiver(100, 100), UiInit(tab = Tab.Transfer, transferProxy = "windowsPc"))
    @Test @Config(qualifiers = "+zh-rCN") fun p2pReceivingMenu() {
        val messages = receiver(30, 45)
        p2pShot("transfer-p2p-receiving-menu", messages, UiInit(tab = Tab.Transfer, transferProxy = "windowsPc", transferMessageMenu = messages.first { it.name == zipName }.id))
    }
    @Test @Config(qualifiers = "+zh-rCN") fun p2pReceiveFailed() = p2pShot("transfer-p2p-receive-failed", receiver(30, 62, "failed"), UiInit(tab = Tab.Transfer, transferProxy = "windowsPc"))
    @Test @Config(qualifiers = "+zh-rCN") fun p2pReceiveCancelled() = p2pShot("transfer-p2p-receive-cancelled", receiver(100, 62, "cancelled").map {
        if (it.kind == "image" && it.direct) it.copy(ended = null) else it }, UiInit(tab = Tab.Transfer, transferProxy = "windowsPc"))
    @Test @Config(qualifiers = "+zh-rCN") fun p2pCancelledReceiver() = p2pShot("transfer-p2p-cancelled-receiver", receiver(100, 0, zip = false), UiInit(tab = Tab.Transfer, transferProxy = "windowsPc"))
    @Test @Config(qualifiers = "+zh-rCN") fun p2pOnlyPc() = p2pShot("transfer-p2p-only-pc", listOf(from(windowsPcP, "text", "这版界面已经整理好了，发给你看看。"),
        from(windowsPcP, "file", name = "设计说明.pdf", detail = "PDF · 2.4 MB"), mine("file", name = "合同扫描.pdf", detail = "PDF · 1.2 MB"),
        from(windowsPcP, "text", "今天现场的照片整理好了吗？"), mine("text", "整理好了，晚点发你。")), peers = listOf(windowsPcP))
    @Test @Config(qualifiers = "+zh-rCN") fun p2pOnlyPhone() = p2pShot("transfer-p2p-only-phone", listOf(from(androidP, "text", "照片都在你那儿吗？"),
        mine("file", name = "合同扫描.pdf", detail = "PDF · 1.2 MB", to = "Android"),
        from(androidP, "image", name = "白板照片.jpg", progress = 100, direct = true, imagePath = picture("p2p-board.jpg", 800, 450, 40f)),
        mine("text", "在，我这就发你。")), peers = listOf(androidP))
    @Test @Config(qualifiers = "+zh-rCN") fun p2pOnlyOffline() {
        val desktop = PeerUi("desktop", "DESKTOP-9AO8GPM", "windows", offline = true)
        p2pShot("transfer-p2p-only-offline", listOf(from(desktop, "folder", name = "项目素材", detail = "12 个文件"), mine("text", "收到，我在手机上确认一下。"),
            mine("image", name = "现场照片.jpg", imagePath = picture("p2p-site2.jpg", 800, 450, 120f)), from(desktop, "text", "今天现场的照片整理好了吗？")), peers = listOf(desktop))
    }
    @Test @Config(qualifiers = "+zh-rCN") fun p2pNone() = p2pShot("transfer-p2p-none", emptyList(), peers = emptyList())

    /** The share entry's device picker over the mockup's gallery. */
    private fun pickShot(name: String, searching: Boolean, rows: List<PickRow>, connecting: String? = null, toast: String? = null) {
        compose.setContent {
            OinkTheme(OinkColors.of(dark, Accent.Pink)) {
                SharePicker("3 张照片", searching, rows, connecting, toast?.let { ToastData(it) }, onPick = {}, onDismiss = {}, host = true)
            }
        }
        compose.waitForIdle()
        compose.onRoot().captureRoboImage("build/outputs/roborazzi/$dir/$name.png")
    }
    private val cachedRows = listOf(windowsPcP, macMiniP, androidP, iphoneP).map { PickRow(it, false) }
    @Test @Config(qualifiers = "+zh-rCN") fun sharePickCached() = pickShot("share-pick-cached", true, pickRows(cachedRows.map { it.peer }, emptyList(), true, emptySet()))
    @Test @Config(qualifiers = "+zh-rCN") fun sharePickConnecting() = pickShot("share-pick-connecting", true, pickRows(cachedRows.map { it.peer }, emptyList(), true, emptySet()), connecting = "windowsPc")
    @Test @Config(qualifiers = "+zh-rCN") fun sharePickOffline() = pickShot("share-pick-offline", true, pickRows(cachedRows.map { it.peer }, emptyList(), true, setOf("macMini")), toast = "Mac mini 不在线")
    @Test @Config(qualifiers = "+zh-rCN") fun sharePickLive() = pickShot("share-pick-live", false, pickRows(emptyList(), listOf(windowsPcP, ipadP, androidP), false, emptySet()))
    @Test @Config(qualifiers = "+zh-rCN") fun sharePickEmpty() = pickShot("share-pick-empty", false, emptyList())
    /** Sending to a phone from the share entry: progress with 隐藏 and 取消. */
    @Test @Config(qualifiers = "+zh-rCN") fun sharePickAndroidSending() {
        compose.setContent {
            OinkTheme(OinkColors.of(dark, Accent.Pink)) {
                ShareCardLayer(ShareCardUi("sending", 52, 2_470_000.0, freezeT = .3f, to = "iPhone"), host = true, onHide = {}, onCancel = {})
            }
        }
        compose.waitForIdle()
        compose.onRoot().captureRoboImage("build/outputs/roborazzi/$dir/share-pick-android-sending.png")
    }

    /** Password-marked text: hidden, revealed, and revealed while its text is still downloading. */
    @Test @Config(qualifiers = "+zh-rCN") fun transferSecret() {
        val t = now - 3 * 60_000
        fun secret(id: Long, text: String?) = TransferMessage(id, t + id, "windowsPc", null, "c$id", "text", "Text", null, 0, text, if (text != null) "complete" else "placeholder", null, true,
            ByteArray(16) { (id + 70).toByte() }, sensitive = true)
        open(designModel(listOf(designText(70, true, "把服务器密码发我一下", t), secret(71, "Tr0ub4dor&3"), secret(72, "correct horse battery staple"), secret(73, null))), transferTab)
        compose.onAllNodesWithText("显示")[1].performClick()
        compose.onAllNodesWithText("显示")[1].performClick()
        capture("transfer-secret")
    }

    @Test @Config(qualifiers = "+zh-rCN") fun transferSelect() {
        open(batchModel, UiInit(tab = Tab.Transfer))
        compose.onNodeWithText(longText).performTouchInput { longClick() }
        compose.onNodeWithText("选择").performClick()
        captureScreen("transfer-select")
    }
    @Test @Config(qualifiers = "+zh-rCN") fun transferMenuAtFinger() {
        open(batchModel, UiInit(tab = Tab.Transfer))
        compose.onNodeWithText(longText).performTouchInput { longClick(Offset(width * .3f, height * .4f)) }
        captureScreen("transfer-menu-at-finger")
    }
    private val viewerInit get() = UiInit(tab = Tab.Transfer, viewer = batchModel.messages.first { it.kind == "image" })
    @Test @Config(qualifiers = "+zh-rCN") fun viewer() = shot("viewer", batchModel, viewerInit)
    @Test @Config(qualifiers = "+zh-rCN") fun viewerSaved() {
        compose.mainClock.autoAdvance = false
        open(batchModel, viewerInit)
        compose.mainClock.advanceTimeBy(600)
        compose.onNodeWithContentDescription("保存到相册").performClick()
        compose.mainClock.advanceTimeBy(300)
        compose.onRoot().captureRoboImage("build/outputs/roborazzi/$dir/viewer-saved.png")
    }
    @Test @Config(qualifiers = "+zh-rCN") fun viewerOpening() {
        open(batchModel, UiInit(tab = Tab.Transfer))
        compose.waitForIdle()
        compose.mainClock.autoAdvance = false
        compose.onNodeWithContentDescription("4 项，当前第 1 项：IMG_0.jpg", substring = true).performClick()
        // Frame by frame until the preview is there, then about a third into its 320 ms grow.
        repeat(30) {
            if (compose.onAllNodesWithContentDescription("关闭").fetchSemanticsNodes().isEmpty()) compose.mainClock.advanceTimeByFrame()
        }
        compose.mainClock.advanceTimeBy(64)
        compose.onRoot().captureRoboImage("build/outputs/roborazzi/$dir/viewer-opening.png")
    }
}
