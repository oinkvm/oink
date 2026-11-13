package com.hachiwii.oink

import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onRoot
import com.github.takahirom.roborazzi.captureRoboImage
import com.hachiwii.oink.ui.*
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.ParameterizedRobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/**
 * File cards, picture width, 已发送 and the folder page (native-rewrite.md 「传输页」, file-bubble SPEC
 * part 5): the mockup's scenes transfer-file-*, transfer-sent-latest, transfer-images and
 * transfer-folder-states with the same content, light and dark. PNGs only, no pixel assertions; the
 * same scenes open on a device through DesignFixtures (debug builds).
 */
@RunWith(ParameterizedRobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@Config(sdk = [34], qualifiers = "zh-rCN-w412dp-h915dp-xxhdpi", application = android.app.Application::class)
class FileCardScreenshotTest(private val dark: Boolean) {
    companion object {
        @JvmStatic @ParameterizedRobolectricTestRunner.Parameters(name = "dark={0}")
        fun themes() = listOf(arrayOf<Any>(false), arrayOf<Any>(true))
    }

    @get:Rule val compose = createComposeRule()

    private val peers = listOf(
        PeerUi("windowsPc", "Windows PC", "windows", latency = 4.0), PeerUi("macMini", "Mac mini", "mac", latency = 6.0),
        PeerUi("android-peer", "Android", "android", latency = 12.0), PeerUi("desktop", "DESKTOP-9AO8GPM", "windows", offline = true),
    )
    private val now = System.currentTimeMillis()
    private fun from(peer: String, id: Long, kind: String, name: String = "", detail: String = "", text: String = "", progress: Int? = null,
                     direct: Boolean = false, ended: String? = null, failure: String? = null, unavailable: String? = null, batch: String? = null,
                     imagePath: String? = null, aspect: Float? = null): MessageUi {
        val p = peers.first { it.id == peer }
        return MessageUi(id, false, p.id, p.name, p.platform, !p.offline, kind, text = text, name = name, detail = detail, progress = progress,
            direct = direct, ended = ended, downloadFailure = failure, unavailable = unavailable, batch = batch, createdMs = now, imagePath = imagePath,
            aspect = aspect, preview = if (kind == "image") (if (imagePath != null) "original" else "loading") else null)
    }
    private fun mine(id: Long, kind: String, name: String = "", detail: String = "", progress: Int = 100, to: String? = null, ended: String? = null,
                     imagePath: String? = null, aspect: Float? = null) =
        MessageUi(id, true, "self", "Phone", "android", true, kind, name = name, detail = detail, progress = progress.takeIf { it < 100 },
            sent = progress >= 100 || ended != null, direct = to != null, toName = to, ended = ended, createdMs = now, imagePath = imagePath, aspect = aspect)

    private fun shot(name: String, messages: List<MessageUi>, init: UiInit = UiInit(tab = Tab.Transfer)) {
        val prefs = Prefs(scheme = if (dark) "dark" else "light")
        compose.setContent { OinkApp(AppModel(selfName = "Phone", peers = peers, messages = messages), init.copy(freeze = true), prefs, {}, AppActions(), dark) }
        compose.waitForIdle()
        compose.onRoot().captureRoboImage("build/outputs/roborazzi/${if (dark) "dark" else "light"}/$name.png")
    }

    @Test fun received() = shot("transfer-file-received", listOf(
        from("windowsPc", 1, "file", "设计说明.pdf", "PDF · 2.4 MB"),
        from("windowsPc", 2, "file", "会议纪要.docx", "DOCX · 86 KB", progress = 45),
        from("windowsPc", 3, "file", "排期表（11 月第二周，含节假日调整）.xlsx", "XLSX · 312 KB", progress = 100),
        from("macMini", 4, "folder", "项目素材", "12 个文件"),
    ))

    @Test fun problems() = shot("transfer-file-problems", listOf(
        from("android-peer", 1, "file", "现场照片（原图）.zip", "ZIP · 486 MB", progress = 62, direct = true, ended = "failed"),
        from("android-peer", 2, "file", "合同扫描.pdf", "PDF · 1.2 MB", progress = 30, direct = true, ended = "cancelled"),
        from("macMini", 3, "file", "Oink-0.3.4.dmg", "DMG · 48.2 MB", unavailable = "expired"),
        from("desktop", 4, "file", "报价单 v3.docx", "DOCX · 240 KB", unavailable = "offline"),
    ))

    @Test fun sent() = shot("transfer-file-sent", listOf(
        mine(1, "file", "现场照片（原图）.zip", "ZIP · 486 MB", progress = 45, to = "Android"),
        mine(2, "file", "合同扫描.pdf", "PDF · 1.2 MB"),
        mine(3, "file", "2026 年第四季度产品路线图与里程碑（评审修订版，含附录）.docx", "DOCX · 1.6 MB"),
        mine(4, "folder", "现场照片", "24 个文件", to = "Android", ended = "failed"),
    ))

    private val batchFiles = listOf(
        from("windowsPc", 11, "file", "Q4 设计评审.pdf", "PDF · 2.4 MB", batch = "windowsPc:x1"),
        from("windowsPc", 12, "file", "会议纪要.docx", "DOCX · 86 KB", progress = 45, batch = "windowsPc:x1"),
        from("windowsPc", 13, "file", "排期表.xlsx", "XLSX · 312 KB", progress = 100, batch = "windowsPc:x1"),
        from("windowsPc", 14, "file", "图标资源（全部尺寸）.zip", "ZIP · 18.6 MB", failure = "下载失败", batch = "windowsPc:x1"),
        from("windowsPc", 15, "folder", "项目素材", "12 个文件", batch = "windowsPc:x1"),
    )

    @Test fun batchReceived() = shot("transfer-file-batch-received", listOf(from("windowsPc", 10, "text", text = "这几个文件你看一下。")) + batchFiles,
        UiInit(tab = Tab.Transfer, transferExpanded = setOf("windowsPc:x1")))

    /** A single file next to a stack of files: the same surface and edge, only the stack has a shadow. */
    @Test fun compareReceived() = shot("transfer-file-compare-received", listOf(from("windowsPc", 1, "file", "设计说明.pdf", "PDF · 2.4 MB")) +
        batchFiles.take(4).map { it.copy(progress = null, downloadFailure = null) })

    @Test fun sentLatest() {
        val whiteboard = ViewerFixtures.picture("/shots/whiteboard.jpg", 600, 800, 50f)
        shot("transfer-sent-latest", listOf(
            from("windowsPc", 1, "text", text = "资料都发我一下。"),
            mine(2, "file", "合同扫描.pdf", "PDF · 1.2 MB"),
            mine(3, "image", "白板照片.jpg", imagePath = whiteboard, aspect = 3f / 4f),
            mine(4, "folder", "现场照片", "24 个文件", to = "Android", ended = "failed"),
            mine(5, "file", "排期表.xlsx", "XLSX · 312 KB"),
        ))
    }

    /** Received and own pictures, both 220 wide at their own aspect; one still downloading. */
    @Test fun images() {
        val desktop = ViewerFixtures.picture("/shots/desktop.png", 800, 500, 210f)
        val whiteboard = ViewerFixtures.picture("/shots/whiteboard.jpg", 600, 800, 50f)
        shot("transfer-images", listOf(
            from("macMini", 1, "image", "桌面预览.png", progress = 100, imagePath = desktop, aspect = 8f / 5f),
            mine(2, "image", "白板照片.jpg", imagePath = whiteboard, aspect = 3f / 4f),
            from("windowsPc", 3, "image", "界面截图.png", progress = 40),
        ))
    }

    /** Folder page rows (option A): thumbnails (made here / from the computer), every download state, a subfolder. */
    @Test fun folderStates() {
        val local = ViewerFixtures.picture("/shots/site-01.jpg", 800, 600, 110f)
        ImageCache.put(LocalThumbnails.memoryKey(local), cachedImage(local, 720)!!)
        val remote = ViewerFixtures.picture("/shots/thumbnails/site-02.jpg", 512, 384, 200f)
        cacheImage(remote, 256, cachedImage(remote, 720)!!)
        val folder = FolderUi("现场资料", listOf(
            FolderItemUi("1", "image", "现场 01.jpg", "3.2 MB", progress = 100, entryId = 1, path = local),
            FolderItemUi("2", "image", "现场 02.jpg", "2.9 MB", entryId = 2, thumbnailPath = remote),
            FolderItemUi("3", "file", "验收单.pdf", "1.1 MB", progress = 40, entryId = 3),
            FolderItemUi("4", "file", "施工说明（第二版，含图纸目录与变更记录）.docx", "640 KB", entryId = 4),
            FolderItemUi("5", "file", "材料清单.xlsx", "88 KB", progress = 100, entryId = 5),
            FolderItemUi("6", "file", "原始照片.zip", "486 MB", entryId = 6, failure = "下载失败"),
            FolderItemUi("7", "folder", "图纸", "8 个文件", entryId = 7),
        ), messageId = 1)
        shot("transfer-folder-states", emptyList(), UiInit(tab = Tab.Transfer, folder = folder))
    }
}
