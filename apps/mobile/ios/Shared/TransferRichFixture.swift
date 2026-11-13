import Foundation

#if DEBUG
/// Rich text design states (`-oink-ui-scene transfer-rich*`, mockup TransferPage.jsx
/// `richMessages`, user 2026-11-12). `html` is the HTML as it arrived (colours, fonts and sizes
/// on purpose); `display` is what the core's cleaning makes of it (rich_text.rs), which the
/// bubble draws. An empty `display` is HTML that kept no formatting: shown and copied as plain text.
enum TransferRichFixture {
    /// The long rich text of transfer-rich-long*.
    static let longID = "9360"

    private struct Item {
        var n: Int
        var sender: String
        var mine = false
        var html: String?
        var display: String?
        var text: String?
    }

    private static let windowsPc = "fixture-windowsPc", macMini = "fixture-macMini", phone = "fixture-android", me = "self"

    /// The conversation of `scene`; `make(n, sender, text, mine)` builds a text record.
    static func messages(_ scene: String, make: (Int, String, String, Bool) -> TransferMessage)
        -> [(message: TransferMessage, rich: TransferRichText?)] {
        items(scene).map { item in
            let display = item.display ?? item.html ?? ""
            let text = item.text ?? RichDocument.parse(display)?.plainText ?? ""
            var message = make(item.n, item.sender, text, item.mine)
            guard let html = item.html else { return (message, nil) }
            message.hasRichText = true
            return (message, TransferRichText(displayHTML: display, html: html, rtf: nil))
        }
    }

    private static func items(_ scene: String) -> [Item] {
        switch scene {
        case "transfer-rich", "transfer-rich-menu", "transfer-rich-menu-self", "transfer-rich-menu-plain": short
        case "transfer-rich-list": list
        case "transfer-rich-links": links
        case "transfer-rich-fallback", "transfer-rich-menu-noformat": fallback
        case "transfer-rich-table", "transfer-rich-menu-table": table
        case "transfer-rich-long", "transfer-rich-long-more", "transfer-rich-long-more-end": long
        default: short
        }
    }

    private static let short: [Item] = [
        Item(n: 300, sender: windowsPc, text: "今晚的安排我整理了一下（这一条是纯文本，作对照）。"),
        Item(n: 301, sender: windowsPc,
             html: #"<p><span style="font-family:Calibri;font-size:16pt;color:#c00000"><b>今晚 8 点</b></span>发版，<i>请勿</i>再合并到 <code>main</code>。旧的 <s>0.3.4 测试包</s> 已删除，<u>新安装包</u>放在共享目录，清单见 <a href="https://example.com/oink/release-0.4.0">发版清单</a>。</p>"#,
             display: #"<p><b>今晚 8 点</b>发版，<i>请勿</i>再合并到 <code>main</code>。旧的 <s>0.3.4 测试包</s> 已删除，<u>新安装包</u>放在共享目录，清单见 <a href="https://example.com/oink/release-0.4.0">发版清单</a>。</p>"#),
        Item(n: 302, sender: me, mine: true,
             html: #"<div>好的，<b>7:30 前</b>我把 <i>Android</i> 的包也放上去，<a href="https://example.com/oink/builds">构建页</a>里能看到。</div>"#),
        Item(n: 303, sender: macMini,
             html: #"<blockquote style="border-left:4px solid #999;background:#eee">发版前先在 Windows 上跑一遍完整安装。</blockquote><p>这条我来，<span style="font-weight:700">7:45</span> 前给结果。</p>"#,
             display: #"<blockquote>发版前先在 Windows 上跑一遍完整安装。</blockquote><p>这条我来，<b>7:45</b> 前给结果。</p>"#),
    ]

    private static let list: [Item] = [
        Item(n: 310, sender: macMini,
             html: #"<h1>0.4.0 发版检查</h1><h2>手机端</h2><ol><li><b>传输页</b>：富文本显示</li><li>长按菜单：<i>复制</i> / <i>复制文本</i></li><li>横屏键盘</li></ol><h2>电脑端</h2><ul><li>自动解锁<ul><li>Windows：凭据界面就绪后</li><li>Mac：只提示密码错误</li></ul></li><li>更新元数据 v2</li></ul><h3>备注</h3><p>没勾的项目周一再对。</p>"#),
        Item(n: 311, sender: me, mine: true, html: #"<h2>今天要买</h2><ul><li>USB-C 线 × 2</li><li>Android Phone 保护壳</li><li>标签纸</li></ul>"#),
    ]

    private static let links: [Item] = [
        Item(n: 320, sender: windowsPc,
             html: #"<p>参考资料：</p><ul><li><a href="https://developer.apple.com/documentation/uikit/uipasteboard">UIPasteboard | Apple Developer</a></li><li><a href="https://developer.android.com/reference/androidx/core/text/HtmlCompat">HtmlCompat | Android Developers</a></li><li><a href="https://developer.android.com/develop/ui/compose/text/style-text">Compose 文本样式</a></li></ul><p>原始讨论在 <a href="https://example.com/oink/issues/412">#412</a>，规格见 <a href="https://example.com/oink/docs/native-rewrite">native-rewrite</a>。</p>"#),
        Item(n: 321, sender: phone,
             html: #"<p>机型数据的出处：<a href="https://counterpointresearch.com/en/insights/iphone-17-global-best-selling-smartphone-in-q2-2026">https://counterpointresearch.com/en/insights/iphone-17-global-best-selling-smartphone-in-q2-2026</a></p>"#),
        Item(n: 322, sender: me, mine: true,
             html: #"<p>会议室换到 <a href="https://maps.apple.com/?q=3B">3B</a>，有事打 <a href="tel:+861012345678">前台</a> 或发邮件到 <a href="mailto:team@example.com">team@example.com</a>。</p>"#),
    ]

    private static let fallback: [Item] = [
        // Only colours, fonts and sizes: nothing kept, plain text (no 复制文本).
        Item(n: 330, sender: macMini,
             html: #"<div style="font-family:Georgia;font-size:20px;color:#1a73e8;background:#fffbe6">这一条只有颜色、字体和字号，</div><div style="color:#c00">清洗后没有留下任何格式，按纯文本处理。</div>"#,
             display: "", text: "这一条只有颜色、字体和字号，\n清洗后没有留下任何格式，按纯文本处理。"),
        Item(n: 331, sender: windowsPc,
             html: #"<h2 style="font-family:Impact;font-size:40px;color:red;text-align:center">网页摘录</h2><p><img src="https://example.com/gantt.png" alt="甘特图"></p><p><span style="background:yellow;color:#0a0;font-size:22px">网页里的高亮和颜色</span>只保留文字，<a href="javascript:alert(1)">不安全的链接</a>也只是文字。</p><script>alert(1)</script>"#,
             display: #"<h2>网页摘录</h2><p><img alt="甘特图"></p><p>网页里的高亮和颜色只保留文字，不安全的链接也只是文字。</p>"#),
        Item(n: 332, sender: phone,
             html: "<pre><code>adb pair 192.0.2.20:37099\nadb connect 192.0.2.20:5555</code></pre><p>特权模式第 3 步用这两条命令（<kbd>Enter</kbd> 执行）。</p><hr><p>其余步骤照 <a href=\"https://example.com/oink/privileged\">引导页</a>。</p>",
             display: "<pre><code>adb pair 192.0.2.20:37099\nadb connect 192.0.2.20:5555</code></pre><p>特权模式第 3 步用这两条命令（<code>Enter</code> 执行）。</p><hr><p>其余步骤照 <a href=\"https://example.com/oink/privileged\">引导页</a>。</p>"),
    ]

    private static let table: [Item] = [
        Item(n: 340, sender: windowsPc,
             html: #"<p>排期：</p><table border="1" style="background:#fffbe6"><tr><th>项目</th><th>负责人</th><th>日期</th></tr><tr><td>Android 包</td><td>Windows PC</td><td>11/14</td></tr><tr><td>iOS 包</td><td>Mac mini</td><td>11/15</td></tr><tr><td>发版说明</td><td>Alex</td><td>11/16</td></tr></table>"#,
             display: #"<p>排期：</p><table><tr><th>项目</th><th>负责人</th><th>日期</th></tr><tr><td>Android 包</td><td>Windows PC</td><td>11/14</td></tr><tr><td>iOS 包</td><td>Mac mini</td><td>11/15</td></tr><tr><td>发版说明</td><td>Alex</td><td>11/16</td></tr></table>"#),
        Item(n: 341, sender: macMini,
             html: #"<p>本周构建（表格较宽，可左右滑）：</p><table><tr><th>构建</th><th>平台</th><th>版本</th><th>大小</th><th>状态</th><th>时间</th></tr><tr><td>106</td><td>Android</td><td>0.4.0-inhouse</td><td>38.2 MB</td><td><b>已安装</b></td><td>11-11 21:40</td></tr><tr><td>106</td><td>iOS</td><td>0.4.0-inhouse</td><td>24.9 MB</td><td>已上传</td><td>11-11 22:05</td></tr><tr><td>105</td><td>Windows</td><td>0.3.6</td><td>61.0 MB</td><td><s>已撤回</s></td><td>11-10 18:12</td></tr></table>"#),
        Item(n: 342, sender: me, mine: true,
             html: #"<table><tr><th>构建</th><th>平台</th><th>版本</th><th>大小</th><th>状态</th></tr><tr><td>106</td><td>Android</td><td>0.4.0-inhouse</td><td>38.2 MB</td><td>已安装</td></tr><tr><td>106</td><td>iOS</td><td>0.4.0-inhouse</td><td>24.9 MB</td><td>已上传</td></tr></table><p>（这张已经往左滑过一段，两侧都有渐隐）</p>"#),
    ]

    private static let long: [Item] = [
        Item(n: 360, sender: windowsPc,
             html: """
             <h1 style="font-size:32px;color:#1a73e8">0.4.0 联调记录</h1>
             <p><b>时间</b>：周一下午　<b>参会</b>：Alex、Windows PC、Mac mini</p>
             <h2>已完成</h2>
             <ol><li><b>传输页</b>：长文本按 10 行的高度折叠，<i>标题和列表</i>也一样，第 8–10 行渐隐。</li>
             <li><b>多文件聚合</b>：左右划动切换最上面那张，点「展开」恢复成普通消息。</li>
             <li><b>图片描边</b>：纯白图片在白色页面上也能看出边界。</li>
             <li><b>键鼠</b>：手机触控板单指划动速度再调低一档。</li></ol>
             <h2>待处理</h2>
             <ul><li>Phone 上 PC 复制的图片加载失败，见 <a href="https://example.com/oink/issues/431">#431</a>。</li>
             <li>输入法抬起时 Tab 栏没有隐藏。</li>
             <li>代理离线时 toast 被预览挡住。</li>
             <li><s>长按菜单阴影太重</s>（已修）</li></ul>
             <blockquote>长按菜单里的「复制」带格式，「复制文本」只复制文字。</blockquote>
             <h3>测试设备</h3>
             <p>Phone、iPhone、Android Phone，构建号 <code>0.4.0-inhouse.106</code>，安装包在共享目录。</p>
             <p>下周一再对一次，有问题直接在群里说。<u>谢谢大家</u>。</p>
             """,
             display: """
             <h1>0.4.0 联调记录</h1>
             <p><b>时间</b>：周一下午　<b>参会</b>：Alex、Windows PC、Mac mini</p>
             <h2>已完成</h2>
             <ol><li><b>传输页</b>：长文本按 10 行的高度折叠，<i>标题和列表</i>也一样，第 8–10 行渐隐。</li>
             <li><b>多文件聚合</b>：左右划动切换最上面那张，点「展开」恢复成普通消息。</li>
             <li><b>图片描边</b>：纯白图片在白色页面上也能看出边界。</li>
             <li><b>键鼠</b>：手机触控板单指划动速度再调低一档。</li></ol>
             <h2>待处理</h2>
             <ul><li>Phone 上 PC 复制的图片加载失败，见 <a href="https://example.com/oink/issues/431">#431</a>。</li>
             <li>输入法抬起时 Tab 栏没有隐藏。</li>
             <li>代理离线时 toast 被预览挡住。</li>
             <li><s>长按菜单阴影太重</s>（已修）</li></ul>
             <blockquote>长按菜单里的「复制」带格式，「复制文本」只复制文字。</blockquote>
             <h3>测试设备</h3>
             <p>Phone、iPhone、Android Phone，构建号 <code>0.4.0-inhouse.106</code>，安装包在共享目录。</p>
             <p>下周一再对一次，有问题直接在群里说。<u>谢谢大家</u>。</p>
             """),
        Item(n: 361, sender: me, mine: true, text: "收到，我周一前把待处理的三条看完。"),
    ]
}
#endif
