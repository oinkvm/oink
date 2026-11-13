package com.hachiwii.oink.ui

import androidx.activity.compose.PredictiveBackHandler
import kotlinx.coroutines.CancellationException
import androidx.compose.ui.platform.testTag
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.CubicBezierEasing
import androidx.compose.animation.core.tween
import androidx.compose.animation.expandVertically
import androidx.compose.animation.shrinkVertically
import androidx.compose.foundation.ScrollState
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.layout.layout
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.layout.positionInParent
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.withLink
import androidx.compose.foundation.text.appendInlineContent
import androidx.compose.ui.unit.sp
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.launch

private val timeouts = listOf(1 to "1 分钟", 5 to "5 分钟", 10 to "10 分钟", 20 to "20 分钟", 30 to "30 分钟", 60 to "60 分钟", 0 to "无限制")
private class Picker(val title: String, val options: List<Pair<String, String>>, val footer: String? = null)
private val pickers = mapOf(
    "speakerTap" to Picker("单击扬声器按钮行为", com.hachiwii.oink.SpeakerTap.entries.map { it.name to it.label }),
    "scheme" to Picker("深浅色", listOf("system" to "跟随系统", "light" to "浅色", "dark" to "深色")),
    "language" to Picker("语言", listOf("system" to "跟随系统", "zh-Hans" to "简体中文", "en" to "English", "ja" to "日本語", "ko" to "한국어")),
    "homeTab" to Picker("默认页面", listOf("Devices" to "设备", "Transfer" to "传输", "Control" to "控制"), "下次打开 Oink 时先显示这个页面。"),
    // User 2026-11-11: what a tap on the persistent notification does.
    "noticeTap" to Picker("点按通知", listOf("copy" to "从共享剪贴板复制", "open" to "打开 App"),
        "从共享剪贴板复制：把其它设备当前复制的文字或图片复制到本机；没有可复制的内容时打开 Oink。"),
    "noticeTab" to Picker("打开页面", listOf("Devices" to "设备", "Transfer" to "传输", "Control" to "控制"), "点按通知时打开 Oink 的这个页面。"),
    "micAutoOff" to Picker("麦克风自动关闭", timeouts.map { "${it.first}" to it.second }, "本机作为麦克风时，如果这段时间里一直没有设备在用，就自动关闭。刚开启时从开启那一刻开始计时。"),
    "speakerAutoOff" to Picker("扬声器自动关闭", timeouts.map { "${it.first}" to it.second }, "本机作为扬声器时，如果这段时间里一直没有收到声音，就自动关闭。刚开启时从开启那一刻开始计时。"),
)
private val openWith = listOf("image" to ("图片" to "JPG、PNG、HEIC、GIF 等"), "video" to ("视频" to "MP4、MOV、MKV 等"), "audio" to ("音频" to "MP3、AAC、FLAC 等"),
    "document" to ("文档" to "PDF、Office、文本等"), "archive" to ("压缩包" to "ZIP、RAR、7Z 等"), "apk" to ("安装包" to "APK"), "other" to ("其它类型" to "以上之外的所有文件"))

/**
 * Required attribution for the Android robot head (CC BY 3.0), repeated in the licenses page footer;
 * the robot's own entry (generated inventory) carries it with the modification notice and license text.
 */
const val ANDROID_ROBOT_ATTRIBUTION = "The Android robot is reproduced or modified from work created and shared by Google and used according to terms described in the Creative Commons 3.0 Attribution License."
const val ANDROID_TRADEMARK = "Android is a trademark of Google LLC."

fun formatBytes(n: Long): String {
    if (n <= 0) return "0 KB"
    val units = listOf("KB", "MB", "GB")
    var v = n / 1024.0; var i = 0
    while (v >= 1024 && i < units.size - 1) { v /= 1024; i++ }
    val s = if (v >= 100 || i == 0) Math.round(v).toString() else String.format(java.util.Locale.US, if (v >= 10) "%.1f" else "%.2f", v)
    return "$s ${units[i]}"
}

private fun optionLabel(key: String, value: String) = pickers.getValue(key).options.firstOrNull { it.first == value }?.second ?: ""
private fun Prefs.value(key: String) = when (key) {
    "speakerTap" -> speakerTap.name; "scheme" -> scheme; "language" -> language; "homeTab" -> homeTab.name
    "noticeTap" -> noticeTap; "noticeTab" -> noticeTab.name
    "micAutoOff" -> "$micAutoOff"; else -> "$speakerAutoOff"
}
private fun Prefs.with(key: String, v: String) = when (key) {
    "speakerTap" -> copy(speakerTap = com.hachiwii.oink.SpeakerTap.valueOf(v)); "scheme" -> copy(scheme = v); "language" -> copy(language = v); "homeTab" -> copy(homeTab = Tab.valueOf(v))
    "noticeTap" -> copy(noticeTap = v); "noticeTab" -> copy(noticeTab = Tab.valueOf(v))
    "micAutoOff" -> copy(micAutoOff = v.toInt()); else -> copy(speakerAutoOff = v.toInt())
}

private val titles = mapOf("main" to "设置", "privileged" to "特权模式", "licenses" to "开源许可")

/** Settings: full-screen layer with its own push stack; Android pickers are dialogs. */
@Composable
fun SettingsPage(init: UiInit, prefs: Prefs, set: (Prefs) -> Unit, model: AppModel, actions: AppActions, onLeftGroup: () -> Unit = {}, onClose: () -> Unit) {
    val c = Oink.colors
    val scope = rememberCoroutineScope()
    val enter = remember { Animatable(if (init.settings) 0f else 1f) }
    var stack by remember { mutableStateOf(init.settingsStack) }
    var popping by remember { mutableStateOf(false) }
    var dialog by remember { mutableStateOf(init.settingsDialog) }
    var step by remember { mutableStateOf(init.privStep) }
    var cacheBusy by remember { mutableStateOf(false) }
    var leaving by remember { mutableStateOf(false) }
    var historyBusy by remember { mutableStateOf(false) }
    val toast = remember { mutableStateOf(init.settingsToast?.let { ToastData(it) }) }
    val freeze = init.settingsToast != null
    LaunchedEffect(toast.value?.id) { val t = toast.value; if (t != null && !freeze) { kotlinx.coroutines.delay(2000); if (toast.value?.id == t.id) toast.value = null } }
    LaunchedEffect(Unit) { actions.refreshCache(); enter.animateTo(0f, tween(320, easing = EaseOutExpo)) }
    fun close() { scope.launch { enter.animateTo(1f, tween(320, easing = EaseOutExpo)); onClose() } }
    fun push(route: String) { stack = stack + route }
    fun pop() { if (stack.size == 1) close() else popping = true }
    PredictiveBackHandler(enabled = dialog == null) { events ->
        val closingRoot = stack.size == 1
        try {
            events.collect { if (closingRoot) enter.snapTo(it.progress) }
            pop()
        } catch (_: CancellationException) {
            // The predictive collection job is cancelled; restore in the page's still-live scope.
            if (closingRoot) scope.launch { enter.animateTo(0f, tween(320, easing = EaseOutExpo)) }
        }
    }
    Box(Modifier.fillMaxSize().testTag("settings-layer").graphicsLayer { translationX = -enter.value * size.width }.background(c.bg).press(pressedColor = null) {}) {
        stack.forEachIndexed { i, route ->
            key("$i:$route") {
                SettingsView(route, pushed = i > 0 && !(i < init.settingsStack.size && init.settingsStack[i] == route), popping = popping && i == stack.lastIndex,
                    onPopped = { stack = stack.dropLast(1); popping = false }, onBack = { pop() }, scrollTo = if (i == 0) init.settingsScroll else null) {
                    when {
                        route == "main" -> MainView(prefs, set, model, cacheBusy, leaving, historyBusy, push = { push(it) }, openDialog = { dialog = it })
                        route == "privileged" -> PrivilegedView(prefs, set, step, { step = it }, { dialog = it }, actions, model)
                        route == "licenses" -> LicensesView { push("license:$it") }
                        route.startsWith("license:") -> LicenseText(route.removePrefix("license:"))
                    }
                }
            }
        }
        when (val d = dialog) {
            null -> {}
            "clearCache" -> OinkDialog("清理缓存？", { dialog = null }, listOf(DialogAction("取消") { dialog = null }, DialogAction("清理", danger = true) {
                val size = prefs.cacheBytes; dialog = null; cacheBusy = true
                // The size shown afterwards is the core's new count; a failure shows its reason.
                scope.launch { val error = actions.clearCache(); cacheBusy = false; toast.value = ToastData(error ?: "已清理 ${formatBytes(size)}") }
            })) { DialogBody("将删除已下载的图片和文件，共 ${formatBytes(prefs.cacheBytes)}。消息记录会保留。") }
            "clearHistory" -> OinkDialog("清理传输记录？", { dialog = null }, listOf(DialogAction("取消") { dialog = null }, DialogAction("清理", danger = true) {
                dialog = null; historyBusy = true
                scope.launch { val error = actions.clearHistory(); historyBusy = false; toast.value = ToastData(error ?: "已清理传输记录") }
            })) { DialogBody("将删除本机的所有传输记录，以及已下载的图片和文件。其它设备上的记录不受影响。") }
            "deviceName" -> DeviceNameDialog(model.deviceName, onClose = { dialog = null }) { actions.setDeviceName(it); dialog = null }
            "privDisable" -> OinkDialog("关闭特权模式？", { dialog = null }, listOf(DialogAction("取消") { dialog = null }, DialogAction("关闭", danger = true) {
                dialog = null; actions.privilegeDisable(); set(prefs.copy(privileged = "off")); toast.value = ToastData("特权模式已关闭")
            })) { DialogBody("电脑将不能操控本机，自动读取剪贴板会暂停。之后可以直接重新开启，不用再配对。") }
            "privReset" -> OinkDialog("重置配对凭据？", { dialog = null }, listOf(DialogAction("取消") { dialog = null }, DialogAction("重置", danger = true) {
                dialog = null; actions.privilegeReset(); set(prefs.copy(privileged = "off")); step = 1; toast.value = ToastData("已重置，需要重新配对")
            })) { DialogBody("会停止特权服务，并清除本组所有手机的 Oink 配对凭据。每台手机都需要重新配对无线调试。") }
            // Same card as the device sheet's kick confirmation.
            "leaveGroup" -> ConfirmCard(onDismiss = { dialog = null }) {
                T("退出设备组", 17f, c.ink, Modifier.fillMaxWidth().padding(start = 22.dp, end = 22.dp, top = 24.dp), weight = 650, lineHeight = 25.5f, align = TextAlign.Center)
                T("退出后，需要重新配对才能和组内设备共享。", 13f, c.text2, Modifier.fillMaxWidth().padding(start = 22.dp, end = 22.dp, top = 10.dp, bottom = 24.dp), lineHeight = 20.8f, align = TextAlign.Center)
                ConfirmActions("取消", c.ink, 500, "退出", c.bad, 600, { dialog = null }) {
                    dialog = null; leaving = true
                    scope.launch {
                        val error = actions.leaveGroup(); leaving = false
                        if (error != null) toast.value = ToastData(error) else { onLeftGroup(); close() }
                    }
                }
            }
            else -> if (d.startsWith("picker:")) PickerDialog(d.removePrefix("picker:"), prefs, set) { dialog = null }
        }
        Box(Modifier.fillMaxSize().windowInsetsPadding(WindowInsets.safeDrawing)) { TopToast(toast.value) }
    }
}

@Composable
private fun SettingsView(route: String, pushed: Boolean, popping: Boolean, onPopped: () -> Unit, onBack: () -> Unit, scrollTo: String?, content: @Composable () -> Unit) {
    val c = Oink.colors
    val x = remember { Animatable(if (pushed) 1f else 0f) }
    LaunchedEffect(Unit) { if (pushed) x.animateTo(0f, tween(320, easing = EaseOutExpo)) }
    LaunchedEffect(popping) { if (popping) { x.animateTo(1f, tween(280, easing = CubicBezierEasing(.4f, 0f, .2f, 1f))); onPopped() } }
    val scroll = rememberScrollState()
    val anchors = remember { mutableStateMapOf<String, Int>() }
    LaunchedEffect(scrollTo, anchors[scrollTo ?: ""], scroll.maxValue) {
        if (scrollTo == "end") scroll.scrollTo(scroll.maxValue)
        else anchors[scrollTo]?.let { scroll.scrollTo(it) }
    }
    val density = LocalDensity.current
    Column(Modifier.fillMaxSize().graphicsLayer { translationX = x.value * size.width }.background(c.bg).windowInsetsPadding(WindowInsets.safeDrawing)) {
        Box(Modifier.fillMaxWidth().height(64.dp).background(c.bg).padding(horizontal = 16.dp)) {
            IconButton48(Modifier.align(Alignment.CenterStart), if (route == "main") "关闭设置" else "返回", onClick = onBack) { Lucide(DesignIconData.lucideChevronLeft, 23.dp, c.ink) }
            T(titles[route] ?: licenseTitle(route), 18f, c.ink, Modifier.align(Alignment.Center).widthIn(max = 265.dp), weight = 650, lineHeight = 25.2f, ellipsis = true)
        }
        CompositionLocalProvider(LocalSectionAnchors provides { id, y -> anchors[id] = y + with(density) { (4 - 8).dp.roundToPx() } }) {
            Column(Modifier.weight(1f).fillMaxWidth().verticalScroll(scroll)) {
                Column(Modifier.align(Alignment.CenterHorizontally).widthIn(max = 620.dp).fillMaxWidth().padding(start = 16.dp, end = 16.dp, top = 4.dp, bottom = 40.dp)) { content() }
            }
        }
    }
}

private val LocalSectionAnchors = staticCompositionLocalOf<(String, Int) -> Unit> { { _, _ -> } }

/** sp-section: title outside the card, rows inside, optional footer. [top] is the CSS margin-top. */
@Composable
private fun Section(title: String? = null, footer: String? = null, id: String? = null, top: Dp = 24.dp, rows: @Composable ColumnScope.() -> Unit) {
    val c = Oink.colors
    val anchor = LocalSectionAnchors.current
    val topPx = with(LocalDensity.current) { top.roundToPx() }
    // offsetTop of the section's border box: its position plus the CSS margin above it.
    Column(Modifier.then(if (id != null) Modifier.onGloballyPositioned { anchor(id, it.positionInParent().y.toInt() + topPx) } else Modifier).padding(top = top)) {
        if (title != null) T(title, 13f, c.accentInk, Modifier.padding(start = 16.dp, bottom = 8.dp), weight = 600, lineHeight = 18.2f, letterSpacing = .4f)
        Column(Modifier.fillMaxWidth().clip(RoundedCornerShape(20.dp)).background(c.card), content = rows)
        if (footer != null) T(footer, 12.5f, c.muted, Modifier.padding(start = 16.dp, end = 16.dp, top = 8.dp), lineHeight = 19.375f)
    }
}

/** Divider drawn above every row except the first in a card (left inset 16). */
@Composable
private fun Modifier.rowLine(first: Boolean): Modifier {
    val c = Oink.colors
    return if (first) this else this.drawBehind { drawRect(c.line, Offset(16.dp.toPx(), 0f), Size(size.width - 16.dp.toPx(), 1.dp.toPx())) }
}

@Composable
private fun ValueRow(title: String, value: String?, first: Boolean = false, disabled: Boolean = false, busy: Boolean = false, onClick: () -> Unit) {
    val c = Oink.colors
    val haptic = rememberHaptics()
    Row(Modifier.fillMaxWidth().rowLine(first).heightIn(min = 54.dp).press(enabled = !disabled, pressedColor = c.press) { haptic(false); onClick() }
        .padding(horizontal = 16.dp, vertical = 12.dp).semantics { contentDescription = "$title${if (value != null) "，$value" else ""}" },
        horizontalArrangement = Arrangement.spacedBy(12.dp), verticalAlignment = Alignment.CenterVertically) {
        Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(2.dp)) {
            T(title, 16f, if (disabled) c.faint else c.ink, lineHeight = 22.4f)
            if (value != null) T(value, 13f, c.muted, lineHeight = 18.85f)
        }
        if (busy) Spinner(18.dp)
    }
}

@Composable
private fun SwitchRow(title: String, sub: String?, checked: Boolean, first: Boolean = false, disabled: Boolean = false, subColor: Color? = null,
                      onChange: (Boolean) -> Unit, extra: @Composable () -> Unit = {}) {
    val c = Oink.colors
    val haptic = rememberHaptics()
    Column(Modifier.fillMaxWidth().rowLine(first)) {
        Row(Modifier.fillMaxWidth().heightIn(min = 54.dp).press(pressedColor = if (disabled) null else c.press, role = Role.Switch) { if (!disabled) { haptic(false); onChange(!checked) } }
            .padding(horizontal = 16.dp, vertical = 12.dp), horizontalArrangement = Arrangement.spacedBy(12.dp), verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(2.dp)) {
                T(title, 16f, if (disabled) c.faint else c.ink, lineHeight = 22.4f)
                if (sub != null) T(sub, 13f, subColor ?: c.muted, lineHeight = 18.85f)
            }
            OinkSwitch(checked, disabled)
        }
        extra()
    }
}

@Composable
private fun MainView(prefs: Prefs, set: (Prefs) -> Unit, model: AppModel, cacheBusy: Boolean, leaving: Boolean, historyBusy: Boolean, push: (String) -> Unit, openDialog: (String) -> Unit) {
    val c = Oink.colors
    val haptic = rememberHaptics()
    val privOn = prefs.privileged == "on"
    val pick = { key: String -> openDialog("picker:$key") }
    // This phone's name as the group sees it; the core owns it (the system name is the default).
    Section("本机", id = "device", top = 8.dp) {
        val name = model.deviceName
        ValueRow("设备名", name?.name ?: model.selfName, first = true, disabled = name == null) { openDialog("deviceName") }
    }
    Section("外观", id = "appearance") {
        // Theme colour row: 28 swatch in a 40 target; the ring marks the selection.
        Row(Modifier.fillMaxWidth().heightIn(min = 54.dp).padding(horizontal = 16.dp, vertical = 8.dp), verticalAlignment = Alignment.CenterVertically) {
            T("主题色", 16f, c.ink, Modifier.weight(1f).widthIn(min = 64.dp), lineHeight = 22.4f)
            Spacer(Modifier.width(12.dp))
            Row(Modifier.offset(x = 6.dp), horizontalArrangement = Arrangement.spacedBy(2.dp)) {
                Accent.entries.forEach { a ->
                    val sel = prefs.accent == a
                    Box(Modifier.size(40.dp).clip(CircleShape).press(role = Role.RadioButton) { haptic(false); set(prefs.copy(accent = a)) }.semantics { contentDescription = a.label }, contentAlignment = Alignment.Center) {
                        val sw = Color(a.base)
                        Box(Modifier.size(28.dp).drawBehind {
                            if (sel) {
                                drawCircle(sw, size.width / 2 + 4.dp.toPx())
                                drawCircle(c.card, size.width / 2 + 2.dp.toPx())
                            }
                            drawCircle(sw)
                        }, contentAlignment = Alignment.Center) { if (sel) Lucide(DesignIconData.lucideCheck, 15.dp, c.onAccent, 3f) }
                    }
                }
            }
        }
        ValueRow("深浅色", optionLabel("scheme", prefs.scheme)) { pick("scheme") }
        ValueRow("语言", optionLabel("language", prefs.language)) { pick("language") }
    }
    Section("首页", id = "home") { ValueRow("默认页面", optionLabel("homeTab", prefs.homeTab.name), first = true) { pick("homeTab") } }
    Section("传输", "清理缓存会删除已下载的图片和文件，消息记录会保留。", id = "transfer") {
        ValueRow("清理传输记录", null, first = true, disabled = historyBusy || model.messages.isEmpty(), busy = historyBusy) { openDialog("clearHistory") }
        ValueRow("清理缓存", if (cacheBusy) "正在清理…" else formatBytes(prefs.cacheBytes), disabled = cacheBusy || prefs.cacheBytes <= 0, busy = cacheBusy) { openDialog("clearCache") }
    }
    Section("剪贴板", id = "clipboard") {
        SwitchRow("自动写入剪贴板", "其它设备复制的文字会自动写入本机剪贴板", prefs.autoWriteClipboard, first = true, onChange = { set(prefs.copy(autoWriteClipboard = it)) })
        SwitchRow("自动读取剪贴板并广播", if (privOn) "本机复制的文字会自动发到其它设备" else "需要先开启特权模式", prefs.autoReadClipboard, disabled = !privOn,
            onChange = { set(prefs.copy(autoReadClipboard = it)) }) {
            if (!privOn) Row(Modifier.padding(start = 10.dp, bottom = 4.dp).offset(y = (-10).dp).pullUpSpace(10.dp).height(36.dp).clip(RoundedCornerShape(12.dp)).press { haptic(false); push("privileged") }.padding(horizontal = 6.dp),
                horizontalArrangement = Arrangement.spacedBy(1.dp), verticalAlignment = Alignment.CenterVertically) {
                T("去开启", 13.5f, c.accentInk, weight = 600, lineHeight = 18.9f)
                Lucide(DesignIconData.lucideChevronRight, 15.dp, c.accentInk)
            }
        }
    }
    Section("音频", "开启麦克风或扬声器后，如果一直没有设备在用，到时间会自动关闭。", id = "audio") {
        // These live in the running core; while it is not running they are read-only.
        val audio = prefs.audioAvailable
        if (prefs.privileged == "on") ValueRow("单击扬声器按钮行为", prefs.speakerTap.label, first = true) { pick("speakerTap") }
        ValueRow("麦克风自动关闭", if (audio) optionLabel("micAutoOff", "${prefs.micAutoOff}") else "连接就绪后可修改", first = prefs.privileged != "on", disabled = !audio) { pick("micAutoOff") }
        ValueRow("扬声器自动关闭", if (audio) optionLabel("speakerAutoOff", "${prefs.speakerAutoOff}") else "连接就绪后可修改", disabled = !audio) { pick("speakerAutoOff") }
        SwitchRow("回声消除", "消除麦克风录制的本机播放的音频", prefs.echoCancel, disabled = !audio, onChange = { set(prefs.copy(echoCancel = it)) })
    }
    Section("通知与后台", "在控制中心编辑开关，可以添加 Oink 的「复制」「粘贴」「接收音频」「音频转发」「共享麦克风」。", id = "system") {
        SwitchRow("常驻通知", if (model.worksInBackground) "在通知栏显示在线设备和音频状态" else "已关闭：离开 Oink 后，Oink 将无法在后台工作",
            prefs.persistentNotice, first = true, subColor = if (model.worksInBackground) null else c.badInk, onChange = { set(prefs.copy(persistentNotice = it)) })
        // Only meaningful with the notification on (user 2026-11-11); greyed out, choice kept, while it is off.
        ValueRow("点按通知", optionLabel("noticeTap", prefs.noticeTap), disabled = !prefs.persistentNotice) { pick("noticeTap") }
        if (prefs.noticeTap == "open") ValueRow("打开页面", optionLabel("noticeTab", prefs.noticeTab.name), disabled = !prefs.persistentNotice) { pick("noticeTab") }
        SwitchRow("在多任务中隐藏", "最近任务列表里不显示 Oink", prefs.hideRecents, onChange = { set(prefs.copy(hideRecents = it)) })
        SwitchRow("打开方式注册", "在其它应用的「打开方式」里显示 Oink，用来把文件发到其它设备", prefs.openWith, onChange = { set(prefs.copy(openWith = it)) }) {
            AnimatedVisibility(prefs.openWith, enter = expandVertically(tween(320, easing = EaseOutExpo)), exit = shrinkVertically(tween(320, easing = EaseOutExpo))) {
                Column(Modifier.fillMaxWidth().rowLine(false).padding(top = 4.dp, bottom = 6.dp)) {
                    openWith.forEach { (key, labels) ->
                        val on = prefs.openWithGroups[key] == true
                        Row(Modifier.fillMaxWidth().heightIn(min = 52.dp).press(pressedColor = c.press, role = Role.Checkbox) { haptic(false); set(prefs.copy(openWithGroups = prefs.openWithGroups + (key to !on))) }
                            .padding(start = 20.dp, end = 16.dp, top = 7.dp, bottom = 7.dp), horizontalArrangement = Arrangement.spacedBy(16.dp), verticalAlignment = Alignment.CenterVertically) {
                            OinkCheckbox(on)
                            Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(2.dp)) {
                                T(labels.first, 15f, c.ink, lineHeight = 21f)
                                T(labels.second, 12f, c.muted, lineHeight = 17.4f)
                            }
                        }
                    }
                }
            }
        }
    }
    Section("特权模式", "开启后，电脑可以操控本机，Oink 也能在后台读取剪贴板。需要用到无线调试。", id = "privileged") {
        ValueRow("特权模式", mapOf("off" to "未开启", "setup" to "设置中", "starting" to "正在启动", "on" to "已开启", "failed" to "启动失败")[prefs.privileged], first = true) { push("privileged") }
    }
    Section("关于", id = "about") {
        Row(Modifier.fillMaxWidth().heightIn(min = 54.dp).padding(horizontal = 16.dp, vertical = 12.dp), horizontalArrangement = Arrangement.spacedBy(12.dp), verticalAlignment = Alignment.CenterVertically) {
            T("版本", 16f, c.ink, Modifier.weight(1f), lineHeight = 22.4f)
            T(model.version, 15f, c.muted, lineHeight = 21f, tabular = true)
        }
        ValueRow("开源许可", null) { push("licenses") }
    }
    // Only while the group has other devices (user 2026-11-11).
    if (model.peers.isNotEmpty()) Row(Modifier.padding(top = 24.dp).fillMaxWidth().alpha(if (leaving) .4f else 1f).height(48.dp)
        .clip(RoundedCornerShape(24.dp)).background(c.badSoft).press(!leaving) { openDialog("leaveGroup") },
        horizontalArrangement = Arrangement.Center, verticalAlignment = Alignment.CenterVertically) {
        T("退出设备组", 15f, c.badInk, weight = 600, lineHeight = 21f)
    }
}

/** Collapses [d] of space below, the counterpart of a negative CSS margin on the next edge. */
private fun Modifier.pullUpSpace(d: Dp) = this.then(Modifier.layout { m, cons ->
    val p = m.measure(cons); val px = d.roundToPx()
    layout(p.width, (p.height - px).coerceAtLeast(0)) { p.place(0, 0) }
})

@Composable
private fun PickerDialog(key: String, prefs: Prefs, set: (Prefs) -> Unit, onClose: () -> Unit) {
    val c = Oink.colors
    val haptic = rememberHaptics()
    val p = pickers.getValue(key)
    OinkDialog(p.title, onClose, listOf(DialogAction("取消") { onClose() })) {
        Column(Modifier.fillMaxWidth().layoutWiden(12.dp)) {
            p.options.forEach { (value, label) ->
                val on = prefs.value(key) == value
                Row(Modifier.fillMaxWidth().heightIn(min = 48.dp).clip(RoundedCornerShape(14.dp)).press(pressedColor = c.press, role = Role.RadioButton) { haptic(false); set(prefs.with(key, value)); onClose() }
                    .padding(horizontal = 12.dp), horizontalArrangement = Arrangement.spacedBy(16.dp), verticalAlignment = Alignment.CenterVertically) {
                    OinkRadio(on); T(label, 15f, c.ink, lineHeight = 21f)
                }
            }
        }
        if (p.footer != null) T(p.footer, 12.5f, c.muted, Modifier.padding(top = 12.dp), lineHeight = 19.375f)
    }
}

/** margin: 0 -d (the radio list reaches into the dialog padding). */
private fun Modifier.layoutWiden(d: Dp) = this.then(Modifier.layout { m, cons ->
    val px = d.roundToPx()
    val p = m.measure(cons.copy(minWidth = cons.minWidth + 2 * px, maxWidth = cons.maxWidth + 2 * px))
    layout(p.width - 2 * px, p.height) { p.place(-px, 0) }
})

@Composable
private fun StatusCard(tone: String, icon: IconData?, title: String, text: String) {
    val c = Oink.colors
    Row(Modifier.padding(top = 8.dp).fillMaxWidth().clip(RoundedCornerShape(20.dp)).background(c.card).padding(horizontal = 16.dp, vertical = 18.dp), horizontalArrangement = Arrangement.spacedBy(14.dp)) {
        val (bg, fg) = when (tone) { "good" -> c.goodSoft to c.goodInk; "bad" -> c.badSoft to c.badInk; else -> c.chip to c.text2 }
        Box(Modifier.size(48.dp).clip(CircleShape).background(bg), contentAlignment = Alignment.Center) {
            if (icon == null) Spinner(26.dp) else Lucide(icon, 26.dp, fg, 1.9f)
        }
        Column(Modifier.weight(1f)) {
            T(title, 16f, c.ink, Modifier.padding(top = 2.dp), weight = 650, lineHeight = 22.4f)
            T(text, 13.5f, c.text2, Modifier.padding(top = 4.dp), lineHeight = 20.925f)
        }
    }
}

/** [link]: an inline link at the end of the text that opens Developer options. */
private class Step(val title: String, val text: String, val action: String?, val link: String? = null)
private val steps = listOf(
    Step("开启开发者选项", "在「设置 › 关于手机」中，连续点按「版本号」7 次。已开启可以跳过。", "打开关于手机"),
    Step("打开无线调试", "在「开发者选项」中打开「无线调试」。手机需要连接 Wi‑Fi。", "打开开发者选项"),
    // 用户 2026-11-12 确认: the system closes its pairing dialog as soon as Settings is left, so the
    // code goes in through Oink's notification; the field on this page is for split screen only.
    Step("输入配对码", "留在「设置」里：在「无线调试」中点按「使用配对码配对设备」，然后在 Oink 的通知里点「输入配对码」，填入系统显示的六位配对码，部分手机可能需要长按通知显示按钮。" +
        "切回 Oink 会让系统关掉配对窗口；分屏同时打开「设置」和 Oink 时，也可以填在下面。", null, link = "打开开发者选项"),
)

/**
 * A step's text with its inline link at the end (settings-and-pairing.md, 用户 2026-11-12 确认): the
 * link in the accent colour, weight 600, followed by the 13 external-link icon, as the step buttons'
 * text; tapping it runs [open]. Screen readers get it as a link of the paragraph.
 */
@Composable
private fun StepText(text: String, color: Color, link: String?, modifier: Modifier, open: () -> Unit) {
    val c = Oink.colors
    if (link == null) { T(text, 13.5f, color, modifier, lineHeight = 20.925f); return }
    val current by rememberUpdatedState(open)
    val annotated = remember(text, link, c.accentInk) {
        androidx.compose.ui.text.buildAnnotatedString {
            append(text); append(" ")
            withLink(androidx.compose.ui.text.LinkAnnotation.Clickable("developer-options",
                androidx.compose.ui.text.TextLinkStyles(androidx.compose.ui.text.SpanStyle(color = c.accentInk, fontWeight = systemWeight(600)))) { current() }) {
                append(link)
                appendInlineContent("external", "↗")
            }
        }
    }
    // The icon: 3 after the link text, 13 wide, centred on the line.
    val icon = mapOf("external" to androidx.compose.foundation.text.InlineTextContent(
        androidx.compose.ui.text.Placeholder(16.sp, 13.sp, androidx.compose.ui.text.PlaceholderVerticalAlign.TextCenter)) {
        Box(Modifier.fillMaxSize(), contentAlignment = Alignment.CenterEnd) { Lucide(DesignIconData.lucideExternalLink, 13.dp, c.accentInk, 2.2f) }
    })
    androidx.compose.foundation.text.BasicText(annotated, modifier, textStyle(13.5f, color, lineHeight = 20.925f), inlineContent = icon)
}

/**
 * The guide's current step. [requested] is where the user's taps took it (「打开关于手机」 / 「打开开发者选项」
 * advance it, 「重新配对」 jumps to 3); the live switches override it wherever they are known: developer
 * options off → 1, wireless debugging off → at most 2, both on → 3 (the pairing code).
 */
internal fun privilegedSetupStep(requested: Int, developerOptions: Boolean?, wirelessDebugging: Boolean?): Int = when {
    developerOptions == false -> 1
    wirelessDebugging == true -> 3
    wirelessDebugging == false -> if (developerOptions == true) 2 else minOf(requested, 2)
    developerOptions == true -> maxOf(requested, 2)
    else -> requested
}

@Composable
private fun PrivilegedView(prefs: Prefs, set: (Prefs) -> Unit, step: Int, setStep: (Int) -> Unit, openDialog: (String) -> Unit, actions: AppActions, model: AppModel) {
    val c = Oink.colors
    val port = model.pairingPort
    val haptic = rememberHaptics()
    var code by remember { mutableStateOf("") }
    val context = androidx.compose.ui.platform.LocalContext.current
    // Step 1 (开启开发者选项) is already done when the system reports developer options on.
    fun firstStep() = if (android.provider.Settings.Global.getInt(context.contentResolver,
            android.provider.Settings.Global.DEVELOPMENT_SETTINGS_ENABLED, 0) == 1) 2 else 1
    fun start() { haptic(false); actions.privilegeStart(code); set(prefs.copy(privileged = "starting")); code = "" }
    // Steps 1 and 2 are system switches: checked only while the system has them on now, so a reboot
    // (which turns 无线调试 off) or a dropped Wi‑Fi puts the guide back on 第 2 步.
    val shown = privilegedSetupStep(step, model.developerOptionsOn, model.wirelessDebuggingOn)
    when (prefs.privileged) {
        "setup" -> {
            Column(Modifier.padding(top = 12.dp)) {
                steps.forEachIndexed { index, item ->
                    val n = index + 1; val done = n < shown; val active = n == shown
                    Row(Modifier.fillMaxWidth().drawBehind {
                        if (n < steps.size) drawRoundRect(if (done) c.good else c.track, Offset(15.dp.toPx(), 38.dp.toPx()), Size(2.dp.toPx(), size.height - 44.dp.toPx()), CornerRadius(1.dp.toPx()))
                    }.padding(bottom = 24.dp), horizontalArrangement = Arrangement.spacedBy(14.dp)) {
                        Box(Modifier.size(32.dp).clip(CircleShape).background(when { done -> c.goodSoft; active -> c.accentSoft; else -> c.chip }), contentAlignment = Alignment.Center) {
                            if (done) Lucide(DesignIconData.lucideCheck, 15.dp, c.goodInk, 3f)
                            else T("$n", 14f, if (active) c.accentInk else c.muted, weight = 650, lineHeight = 19.6f, tabular = true)
                        }
                        Column(Modifier.weight(1f).padding(top = 5.dp)) {
                            T(item.title, 16f, when { done -> c.muted; active -> c.ink; else -> c.text2 }, weight = if (done) 500 else 600, lineHeight = 22.4f)
                            if (active || !done) StepText(item.text, if (active) c.text2 else c.muted, item.link, Modifier.padding(top = 4.dp)) {
                                haptic(false); actions.openSystem("developer")
                            }
                            if (active && item.action != null) SoftCapsule(item.action, Modifier.padding(top = 12.dp), trailing = { Lucide(DesignIconData.lucideExternalLink, 15.dp, c.text2) }) {
                                haptic(false); actions.openSystem(if (n == 1) "about" else "developer"); setStep(n + 1)
                            }
                            if (active && n == 3) Column(Modifier.padding(top = 16.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
                                DisposableEffect(Unit) { actions.pairingService(true); onDispose { actions.pairingService(false) } }
                                // The field is for split screen only: the code normally goes in through the notification.
                                T("分屏时在这里输入", 12.5f, c.muted, lineHeight = 17.5f)
                                PairCodeField(code) { code = it }
                                Row(Modifier.padding(top = 4.dp), horizontalArrangement = Arrangement.spacedBy(6.dp), verticalAlignment = Alignment.CenterVertically) {
                                    if (port != null) {
                                        Lucide(DesignIconData.lucideWifi, 14.dp, c.good)
                                        T("已找到配对服务 · 端口 $port", 13.5f, c.text2, lineHeight = 20.925f, tabular = true)
                                    } else { Spinner(14.dp); T("正在查找配对服务…", 13.5f, c.text2, lineHeight = 20.925f) }
                                }
                                PrimaryCapsule("配对并开启", Modifier.fillMaxWidth(), enabled = code.length == 6) { start() }
                            }
                        }
                    }
                }
            }
            Box(Modifier.fillMaxWidth().padding(top = 4.dp), contentAlignment = Alignment.Center) {
                Box(Modifier.height(44.dp).clip(RoundedCornerShape(22.dp)).press { haptic(false); set(prefs.copy(privileged = "off")); setStep(1) }.padding(horizontal = 16.dp), contentAlignment = Alignment.Center) {
                    T("取消设置", 14f, c.muted, lineHeight = 19.6f)
                }
            }
        }
        "starting" -> StatusCard("neutral", null, "正在开启特权模式", "通常只需要几秒钟。")
        "on" -> {
            StatusCard("good", DesignIconData.lucideShieldCheck, "特权模式已开启", model.privilegedPointerProblem?.let { "剪贴板可以自动读取并广播，但电脑暂时不能用鼠标操控本机：$it" }
                ?: "电脑可以用键盘鼠标操控本机，剪贴板可以自动读取并广播。")
            // Android 11–12 cannot restore it by itself: wireless debugging is off after every reboot.
            if (model.rebootNeedsWirelessDebugging) T("重启后需先在开发者选项中打开无线调试，再回到 Oink 启动。", 12.5f, c.muted,
                Modifier.padding(start = 16.dp, end = 16.dp, top = 16.dp), lineHeight = 19.375f)
            else Section(footer = "Android 13 及以上，手机重启后会自动恢复特权模式。", top = 8.dp) {
                SwitchRow("开机后自动恢复", null, prefs.autoRecover, first = true, onChange = { set(prefs.copy(autoRecover = it)) })
            }
            Section(footer = "重置会清除本组所有手机的配对凭据，之后需要重新配对。系统「无线调试 › 已配对的设备」中的旧 Oink 记录需要手动删除。") {
                DangerRow("关闭特权模式", true) { haptic(false); openDialog("privDisable") }
                DangerRow("重置配对凭据", false) { haptic(false); openDialog("privReset") }
            }
        }
        "failed" -> {
            StatusCard("bad", DesignIconData.lucideShieldAlert, "特权模式启动失败", model.privilegedFailure ?: "无法连接无线调试。请确认手机已连接 Wi‑Fi，并且「无线调试」已打开。")
            Column(Modifier.padding(top = 16.dp), verticalArrangement = Arrangement.spacedBy(10.dp)) {
                PrimaryCapsule("重试", Modifier.fillMaxWidth()) { start() }
                SoftCapsule("重新配对", Modifier.fillMaxWidth()) { haptic(false); setStep(3); set(prefs.copy(privileged = "setup")) }
            }
        }
        else -> {
            StatusCard("neutral", DesignIconData.lucideShield, "特权模式未开启", "开启后，电脑可以用键盘鼠标操控本机；Oink 可以在后台读取剪贴板并自动广播，控制中心的「粘贴」不用打开 Oink 就能发送文字。")
            Column(Modifier.padding(top = 16.dp)) { PrimaryCapsule("开始设置", Modifier.fillMaxWidth()) { haptic(false); setStep(firstStep()); set(prefs.copy(privileged = "setup")) } }
            Section("会改动的系统设置", "也可以把手机用 USB 连到电脑，在电脑上的 Oink 中开启。") {
                IconRow(DesignIconData.lucideSmartphone, "开发者选项", "需要先开启", true)
                IconRow(DesignIconData.lucideWifi, "无线调试", "用于启动特权服务，需要 Wi‑Fi", false)
                IconRow(DesignIconData.lucideKeyRound, "ADB 授权超时", "会被关闭，以便重启后自动恢复", false)
            }
        }
    }
}

@Composable
private fun DangerRow(title: String, first: Boolean, onClick: () -> Unit) {
    val c = Oink.colors
    Box(Modifier.fillMaxWidth().rowLine(first).heightIn(min = 54.dp).press(pressedColor = c.press) { onClick() }.padding(horizontal = 16.dp, vertical = 12.dp), contentAlignment = Alignment.CenterStart) {
        T(title, 16f, c.badInk, lineHeight = 22.4f)
    }
}

@Composable
private fun IconRow(icon: IconData, title: String, sub: String, first: Boolean) {
    val c = Oink.colors
    Row(Modifier.fillMaxWidth().rowLine(first).heightIn(min = 54.dp).padding(horizontal = 16.dp, vertical = 12.dp), horizontalArrangement = Arrangement.spacedBy(12.dp), verticalAlignment = Alignment.CenterVertically) {
        Lucide(icon, 20.dp, c.muted)
        Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(2.dp)) {
            T(title, 16f, c.ink, lineHeight = 22.4f); T(sub, 13f, c.muted, lineHeight = 18.85f)
        }
    }
}

@Composable
private fun LicensesView(open: (String) -> Unit) {
    val c = Oink.colors
    val haptic = rememberHaptics()
    val context = androidx.compose.ui.platform.LocalContext.current
    val components = remember { OpenSourceLicenses.all(context) }
    Section(footer = "Oink 使用了以上开源项目，感谢它们的作者。\n$ANDROID_ROBOT_ATTRIBUTION $ANDROID_TRADEMARK", top = 8.dp) {
        components.forEachIndexed { i, item ->
            Row(Modifier.fillMaxWidth().rowLine(i == 0).heightIn(min = 54.dp).press(pressedColor = c.press) { haptic(false); open(item.id) }.padding(horizontal = 16.dp, vertical = 12.dp),
                horizontalArrangement = Arrangement.spacedBy(12.dp), verticalAlignment = Alignment.CenterVertically) {
                Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(2.dp)) {
                    T(item.name, 16f, c.ink, lineHeight = 22.4f); T("${item.version} · ${item.license}", 13f, c.muted, lineHeight = 18.85f)
                }
                Lucide(DesignIconData.lucideChevronRight, 18.dp, c.faint, modifier = Modifier.offset(x = 4.dp))
            }
        }
    }
}

/** One component: its notices and full license texts as shipped, selectable for copying. */
@Composable
private fun LicenseText(id: String) {
    val c = Oink.colors
    val context = androidx.compose.ui.platform.LocalContext.current
    val item = remember(id) { OpenSourceLicenses.find(context, id) } ?: return
    val body = remember(id) { (item.notices + item.texts.map { OpenSourceLicenses.text(context, it).trimEnd() }).joinToString("\n\n") }
    androidx.compose.foundation.text.selection.SelectionContainer {
        Column(Modifier.padding(horizontal = 4.dp, vertical = 8.dp)) {
            T("${item.name} ${item.version} · ${item.license}", 13.5f, c.text2, lineHeight = 22.95f, tabular = true)
            T(body, 13.5f, c.text2, Modifier.padding(top = 13.5.dp), lineHeight = 22.95f)
        }
    }
}

/** Page title for a pushed license route: the component's name. */
@Composable
private fun licenseTitle(route: String): String {
    val context = androidx.compose.ui.platform.LocalContext.current
    val id = route.removePrefix("license:")
    return remember(id) { OpenSourceLicenses.find(context, id)?.name ?: id }
}

/** "设备名": edit this phone's name; "恢复默认" goes back to the system device name. */
@Composable
private fun DeviceNameDialog(name: DeviceNameUi?, onClose: () -> Unit, onSave: (String?) -> Unit) {
    val c = Oink.colors
    var value by remember { mutableStateOf(name?.name.orEmpty()) }
    val focus = remember { androidx.compose.ui.focus.FocusRequester() }
    LaunchedEffect(Unit) { runCatching { focus.requestFocus() } }
    OinkDialog("设备名", onClose, buildList {
        if (name?.custom == true) add(DialogAction("恢复默认") { onSave(null) })
        add(DialogAction("取消") { onClose() })
        add(DialogAction("保存") { if (value.isNotBlank()) onSave(value.trim()) })
    }) {
        androidx.compose.foundation.text.BasicTextField(value, { value = it.take(64) }, Modifier.fillMaxWidth().height(48.dp).focusRequester(focus), singleLine = true,
            textStyle = textStyle(16f, c.ink, lineHeight = 22.4f), cursorBrush = androidx.compose.ui.graphics.SolidColor(c.accent),
            decorationBox = { inner ->
                Box(Modifier.fillMaxSize().clip(RoundedCornerShape(14.dp)).background(c.field).border(1.dp, c.track, RoundedCornerShape(14.dp)).padding(horizontal = 14.dp),
                    contentAlignment = Alignment.CenterStart) {
                    if (value.isEmpty()) T(name?.systemName.orEmpty(), 16f, c.faint, lineHeight = 22.4f)
                    inner()
                }
            })
        if (name != null) T("默认：${name.systemName}", 12.5f, c.muted, Modifier.padding(top = 10.dp), lineHeight = 19.375f)
    }
}

/** Wireless-debugging pairing code: six step-sized cells over one hidden numeric field. */
@Composable
private fun PairCodeField(code: String, onChange: (String) -> Unit) {
    val focus = remember { androidx.compose.ui.focus.FocusRequester() }
    val keyboard = androidx.compose.ui.platform.LocalSoftwareKeyboardController.current
    // A tap anywhere on the cells focuses the hidden field and asks for the keyboard explicitly:
    // a field that already holds focus (e.g. after returning from the system's wireless-debugging
    // screen) does not show the keyboard again on its own. Observed in the Initial pass, not consumed.
    Box(Modifier.pointerInput(Unit) {
        awaitEachGesture {
            awaitFirstDown(pass = androidx.compose.ui.input.pointer.PointerEventPass.Initial)
            runCatching { focus.requestFocus() }
            keyboard?.show()
        }
    }) {
        CodeCells(code, 38.dp, 50.dp, 22f, start = true, onClick = { runCatching { focus.requestFocus() } })
        androidx.compose.foundation.text.BasicTextField(code, { v -> onChange(v.filter(Char::isDigit).take(6)) },
            Modifier.matchParentSize().graphicsLayer { alpha = 0f }.focusRequester(focus),
            keyboardOptions = androidx.compose.foundation.text.KeyboardOptions(keyboardType = androidx.compose.ui.text.input.KeyboardType.NumberPassword), singleLine = true)
    }
}

