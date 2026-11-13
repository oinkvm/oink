package com.hachiwii.oink

import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onRoot
import com.github.takahirom.roborazzi.ExperimentalRoborazziApi
import com.github.takahirom.roborazzi.captureRoboImage
import com.github.takahirom.roborazzi.captureScreenRoboImage
import com.hachiwii.oink.ui.*
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.ParameterizedRobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/**
 * Rich text bubbles (user 2026-11-12), light and dark, into build/outputs/roborazzi/{light,dark}/transfer-rich-*.png,
 * one per rich-text design state (snippets-*, long-*, menu-*).
 */
@RunWith(ParameterizedRobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@Config(sdk = [34], qualifiers = "zh-rCN-w412dp-h915dp-xxhdpi", application = android.app.Application::class)
class RichTextScreenshotTest(private val dark: Boolean) {
    companion object {
        @JvmStatic @ParameterizedRobolectricTestRunner.Parameters(name = "dark={0}")
        fun themes() = listOf(arrayOf<Any>(false), arrayOf<Any>(true))
    }

    @get:Rule val compose = createComposeRule()

    private val peers = listOf(PeerUi("windowsPc", "Windows PC", "windows", latency = 4.0), PeerUi("macMini", "Mac mini", "mac", latency = 6.0))
    private fun msg(id: Long, from: String, plain: String, rich: String?) = when (from) {
        "self" -> MessageUi(id, true, "self", "Android Phone Ultra", "android", true, "text", text = plain, copyable = true, sent = true, createdMs = 1_000L * id, richHtml = rich)
        "macMini" -> MessageUi(id, false, "macMini", "Mac mini", "mac", true, "text", text = plain, copyable = true, sent = true, createdMs = 1_000L * id, richHtml = rich)
        else -> MessageUi(id, false, "windowsPc", "Windows PC", "windows", true, "text", text = plain, copyable = true, sent = true, createdMs = 1_000L * id, richHtml = rich)
    }

    private val short = listOf(
        msg(1, "windowsPc", "今晚 8 点开会", "<p>今晚 <b>8 点</b>开会，<i>记得带电脑</i>。</p><p><u>下划线</u>、<s>删除线</s>和 <code>npm run build</code>。</p>"),
        msg(2, "windowsPc", "同样的话，纯文本", null),
        msg(3, "self", "收到", "<p><b>收到</b>，我把 <i>Release notes</i> 发你。</p>"),
    )
    private val list = listOf(msg(1, "macMini", "发版清单",
        "<h1>发版清单</h1><h2>Android</h2><ul><li>打包 <b>0.4.0</b><ul><li>签名</li><li>上传<ul><li>内测</li></ul></li></ul></li><li>写说明</li></ul>" +
            "<h3>步骤</h3><ol start=\"3\"><li>安装到测试机</li><li>回归</li></ol><blockquote>先测再发，别跳步骤。</blockquote>"))
    private val links = listOf(msg(1, "windowsPc", "链接",
        "<p>文档：<a href=\"https://example.com/docs/release/0.4.0/notes?lang=zh-CN&amp;tab=android\">https://example.com/docs/release/0.4.0/notes?lang=zh-CN&amp;tab=android</a></p>" +
            "<p>有问题发邮件 <a href=\"mailto:dev@example.com\">dev@example.com</a> 或打 <a href=\"tel:+861012345678\">010-1234 5678</a>。</p>"))
    private val table = listOf(
        msg(1, "windowsPc", "排期", "<p>排期：</p><table><tr><th>项目</th><th>负责人</th><th>日期</th></tr><tr><td>Android 包</td><td>Windows PC</td><td>11/14</td></tr>" +
            "<tr><td>iOS 包</td><td>Mac mini</td><td>11/15</td></tr><tr><td>发版说明</td><td>Alex</td><td>11/16</td></tr></table>"),
        msg(2, "macMini", "本周构建", "<p>本周构建（表格较宽，可左右滑）：</p><table><tr><th>构建</th><th>平台</th><th>版本</th><th>大小</th><th>状态</th></tr>" +
            "<tr><td>106</td><td>Android</td><td>0.4.0-inhouse</td><td>38.2 MB</td><td>已安装</td></tr><tr><td>106</td><td>iOS</td><td>0.4.0-inhouse</td><td>24.9 MB</td><td>已上传</td></tr>" +
            "<tr><td>105</td><td>Windows</td><td>0.3.6</td><td>61.0 MB</td><td>已发布</td></tr></table>"),
    )
    private val fallback = listOf(
        msg(1, "windowsPc", "只有颜色和字号的 HTML 按纯文本显示", null),
        msg(2, "windowsPc", "代码", "<p>截图：<img alt=\"登录页\"></p><pre>fun main() {\n    println(\"hello\")\n}</pre><hr><p>不安全的链接只显示文字：点这里</p>"),
    )
    private val long = listOf(msg(1, "windowsPc", "长",
        "<h2>会议纪要</h2>" + (1..14).joinToString("") { "<p>第 $it 条：<b>要点</b>，说明文字说明文字说明文字。</p>" } + "<ul><li>a</li><li>b</li></ul>"))

    private fun path(name: String) = "build/outputs/roborazzi/${if (dark) "dark" else "light"}/$name.png"
    private fun shot(name: String, messages: List<MessageUi>, init: UiInit = UiInit(tab = Tab.Transfer, freeze = true)) {
        compose.setContent { OinkApp(AppModel(peers = peers, messages = messages), init, Prefs(scheme = if (dark) "dark" else "light"), {}, AppActions(), dark) }
        compose.waitForIdle()
        compose.onRoot().captureRoboImage(path(name))
    }
    @OptIn(ExperimentalRoborazziApi::class)
    private fun menu(name: String, messages: List<MessageUi>, id: Long) {
        compose.setContent {
            OinkApp(AppModel(peers = peers, messages = messages), UiInit(tab = Tab.Transfer, transferMessageMenu = id, freeze = true),
                Prefs(scheme = if (dark) "dark" else "light"), {}, AppActions(), dark)
        }
        compose.waitForIdle()
        captureScreenRoboImage(path(name))
    }

    // snippets-short / -list / -links / -table / -fallback
    @Test fun short() = shot("transfer-rich-short", short)
    @Test fun list() = shot("transfer-rich-list", list)
    @Test fun links() = shot("transfer-rich-links", links)
    @Test fun table() = shot("transfer-rich-table", table)
    @Test fun fallback() = shot("transfer-rich-fallback", fallback)
    // long-long (folded at 10 lines of height, 查看更多)
    @Test fun long() = shot("transfer-rich-long", long)
    // menu-menu (rich: 复制 + 复制文本), menu-self, menu-plain, menu-noformat (no 复制文本), menu-table
    @Test fun menuRich() = menu("transfer-rich-menu", short, 1)
    @Test fun menuSelf() = menu("transfer-rich-menu-self", short, 3)
    @Test fun menuPlain() = menu("transfer-rich-menu-plain", short, 2)
    @Test fun menuNoFormat() = menu("transfer-rich-menu-noformat", fallback, 1)
    @Test fun menuTable() = menu("transfer-rich-menu-table", table, 2)
}
