package com.hachiwii.oink.ui

import androidx.activity.compose.BackHandler
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.animateColorAsState
import androidx.compose.animation.core.tween
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.gestures.detectVerticalDragGestures
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.scale
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.input.pointer.PointerEventPass
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.input.pointer.positionChange
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withTimeoutOrNull

/**
 * Bottom sheet layer (m-sheet-layer + m-device-sheet): scrim, sheet sliding up 240 ms,
 * closing 200 ms. [dismissible] false: no grabber, scrim and drag do nothing.
 */
@Composable
fun BottomSheetLayer(onDismiss: () -> Unit, dismissible: Boolean = true, handleColor: Color = Oink.colors.handle,
                     closing: Boolean = false, onClosed: () -> Unit = {}, minHeight: Dp = 0.dp, onBack: (() -> Unit)? = null,
                     content: @Composable ColumnScope.() -> Unit) {
    val c = Oink.colors
    val offset = remember { Animatable(1f) }
    val scrim = remember { Animatable(0f) }
    LaunchedEffect(closing) {
        if (!closing) { launch { scrim.animateTo(1f, tween(240)) }; offset.animateTo(0f, tween(240, easing = EaseOutExpo)) }
        else { launch { scrim.animateTo(0f, tween(200)) }; offset.animateTo(1f, tween(200, easing = androidx.compose.animation.core.CubicBezierEasing(.4f, 0f, 1f, 1f))); onClosed() }
    }
    BackHandler { (onBack ?: onDismiss)() }
    var sheetH by remember { mutableStateOf(0) }
    // The activity is adjustNothing (it pads by insets itself), so the sheet lifts its own content above
    // the keyboard. The IME inset already covers the navigation bar, hence union rather than sum; the
    // inset animates with the keyboard. The sheet background still reaches the screen edge behind it.
    val bottom = WindowInsets.navigationBars.union(WindowInsets.ime).asPaddingValues().calculateBottomPadding()
    Box(Modifier.fillMaxSize().graphicsLayer { alpha = 1f }.background(c.overlay.copy(alpha = c.overlay.alpha * scrim.value))
        .press(pressedColor = null) { if (dismissible) onDismiss() }) {
        Column(Modifier.align(Alignment.BottomCenter).fillMaxWidth()
            .windowInsetsPadding(WindowInsets.statusBars).padding(top = 24.dp)
            .onSizeChanged { sheetH = it.height }
            .graphicsLayer { translationY = offset.value * sheetH }
            .clip(RoundedCornerShape(topStart = 28.dp, topEnd = 28.dp)).background(c.sheet)
            .press(pressedColor = null) {}
            .heightIn(min = minHeight + bottom)
            .padding(start = 24.dp, end = 24.dp, bottom = bottom)) {
            if (dismissible) Box(Modifier.fillMaxWidth().height(28.dp).pointerInput(Unit) {
                var total = 0f
                detectVerticalDragGestures(onDragStart = { total = 0f }, onDragEnd = { if (total > 50.dp.toPx()) onDismiss() }) { _, d -> total += d }
            }, contentAlignment = Alignment.Center) { Box(Modifier.size(36.dp, 4.dp).clip(RoundedCornerShape(4.dp)).background(handleColor)) }
            content()
        }
    }
}

private class ControlConfig(val label: String, val options: List<Triple<String, String, Badge?>>)
private val configs = mapOf(
    "mouse" to ControlConfig("鼠标", listOf(Triple("shared", "共享", Badge.Wifi), Triple("local", "本地", null), Triple("blocked", "禁用", Badge.Ban))),
    "keyboard" to ControlConfig("键盘", listOf(Triple("shared", "共享", Badge.Wifi), Triple("local", "本地", null), Triple("blocked", "禁用", Badge.Ban))),
    "locked" to ControlConfig("指针锁定", listOf(Triple("on", "开启", null), Triple("off", "关闭", null))),
    "gaming" to ControlConfig("游戏模式", listOf(Triple("on", "开启", null), Triple("off", "关闭", null), Triple("auto", "自动", null))),
    "speaker" to ControlConfig("扬声器", listOf(Triple("local", "本地", null), Triple("forward", "转发到出口", null), Triple("outlet", "作为出口", null))),
    "mic" to ControlConfig("麦克风", listOf(Triple("local", "本地", null), Triple("receive", "接收", null), Triple("send", "发送", null))),
)
private val words = mapOf("on" to "开", "off" to "关", "auto" to "自动", "local" to "本地", "shared" to "共享", "blocked" to "已禁用",
    "forward" to "转发", "outlet" to "作为出口", "receive" to "接收", "send" to "发送")

/** Speaker maximum per PC: 100 while this device is the exit, otherwise 200; microphone 200. */
fun audioMax(type: String, mode: String) = if (type == "mic") 200 else if (mode == "outlet") 100 else 200
private fun inDetent(raw: Int, max: Int) = max == 200 && raw in 97..103
/** PC volumeDetent: 97–103 snap to 100 on a 0–200 bar; the rest maps linearly around the gap. */
private fun pointerVolume(rawIn: Int, max: Int): Int {
    val raw = rawIn.coerceIn(0, max)
    if (max != 200) return raw
    if (inDetent(raw, max)) return 100
    return if (raw < 97) Math.round(raw * 99f / 96f) else Math.round(101 + (raw - 104) * 99f / 96f)
}

@Composable
fun DeviceSheet(peerIn: PeerUi, init: UiInit?, actions: AppActions, onClose: () -> Unit) {
    val c = Oink.colors
    val haptic = rememberHaptics()
    // The device card is the only state source; while a volume bar is dragged it shows the finger's value.
    var dragLevels by remember(peerIn.id) { mutableStateOf(mapOf<String, Int>()) }
    var dragKey by remember { mutableStateOf(0) }
    LaunchedEffect(dragKey) { if (dragKey > 0) { delay(1500); dragLevels = emptyMap() } }
    val peer = peerIn.copy(speakerLevel = dragLevels["speaker"] ?: peerIn.speakerLevel, micLevel = dragLevels["mic"] ?: peerIn.micLevel)
    var focus by remember { mutableStateOf(init?.sheetFocus) }
    var confirmKick by remember { mutableStateOf(init?.sheetKick == true) }
    var editIp by remember { mutableStateOf(init?.sheetEditIp == true) }
    var notice by remember { mutableStateOf("") }
    var noticeKey by remember { mutableStateOf(0) }
    var closing by remember { mutableStateOf(false) }
    val freeze = init != null
    LaunchedEffect(noticeKey) { if (noticeKey > 0 && !freeze) { delay(2000); notice = "" } }
    fun setNotice(text: String) { notice = text; noticeKey++ }
    LaunchedEffect(Unit) { if (peer.gaming == "auto" && !peer.offline && !peer.mobile) setNotice("游戏模式：自动") }
    val modes = mapOf("mouse" to peer.mouse, "keyboard" to peer.keyboard, "locked" to if (peer.locked) "on" else "off",
        "gaming" to peer.gaming, "speaker" to peer.speaker, "mic" to peer.mic)
    // The notice reports what the device now reports, not what was asked for.
    var shown by remember(peerIn.id) { mutableStateOf(modes to peer.paused) }
    LaunchedEffect(modes, peer.paused) {
        val (before, paused) = shown
        modes.entries.firstOrNull { (k, v) -> before[k] != v && v != "unavailable" }?.let { (k, v) -> setNotice("${configs.getValue(k).label}：${words[v] ?: v}") }
        if (paused != peer.paused) setNotice(if (peer.paused) "已暂停" else "已继续")
        shown = modes to peer.paused
    }
    fun choose(type: String, mode: String) {
        haptic(false)
        if (mode == modes[type]) return
        when (type) {
            "mouse", "keyboard" -> actions.deviceInput(peer, type, mode)
            else -> actions.devicePreference(peer, type, mode)
        }
    }
    fun tap(type: String) {
        val mode = modes.getValue(type)
        choose(type, when (type) {
            "mouse", "keyboard" -> if (mode == "shared") "local" else "shared"
            // A tap flips what is in effect and leaves auto.
            "gaming" -> if (mode == "on" || mode == "auto" && peer.gamingActive) "off" else "on"
            else -> if (mode == "on") "off" else "on"
        })
    }
    var refusedAt by remember { mutableStateOf(0L) }
    fun level(type: String, n: Int) {
        val adjustable = if (type == "speaker") peer.speakerAdjustable else peer.micAdjustable
        if (!adjustable) {
            // Explain once per gesture instead of moving a bar nothing listens to.
            val now = System.currentTimeMillis()
            if (now - refusedAt > 1500) { refusedAt = now; actions.deviceLevel(peer, type, n) }
            return
        }
        dragLevels = dragLevels + (type to n); dragKey++
        actions.deviceLevel(peer, type, n)
        setNotice("${configs.getValue(type).label}：${mapOf("local" to "本地", "forward" to "转发", "outlet" to "作为出口", "receive" to "接收", "send" to "发送")[modes[type]]} · $n%")
    }
    fun requestClose() { closing = true }

    Box(Modifier.fillMaxSize()) {
        BottomSheetLayer(onDismiss = { if (focus != null) focus = null else requestClose() }, closing = closing,
            onClosed = { onClose() }, minHeight = if (focus != null) 340.dp else 0.dp) {
            if (focus == null) {
                // Heading: logo, name, pause and kick (m-sheet-heading: 14 above, 24 below; collapses with the grid's 36).
                Row(Modifier.padding(top = 14.dp).fillMaxWidth().heightIn(min = 44.dp), verticalAlignment = Alignment.CenterVertically) {
                    CompositionLocalProvider(LocalCutout provides c.sheet) { PlatformIcon(peer.platform, privileged = peer.privileged) }
                    Spacer(Modifier.width(12.dp))
                    T(peer.name, 20f, c.ink, Modifier.weight(1f), weight = 650, lineHeight = 26f)
                    Spacer(Modifier.width(12.dp))
                    Row(horizontalArrangement = Arrangement.spacedBy(4.dp)) {
                        // Phones have no paused state (user 2026-11-11): only the kick button.
                        if (!peer.mobile) Box(Modifier.size(44.dp).press(shape = RoundedCornerShape(14.dp)) { haptic(false)
                            actions.devicePreference(peer, "paused", if (peer.paused) "off" else "on") }
                            .semantics { contentDescription = if (peer.paused) "继续" else "暂停" }, contentAlignment = Alignment.Center) {
                            if (peer.paused) Lucide(DesignIconData.lucidePlay, 22.dp, c.bad) else SvgIcon(DesignIconData.actionPause, 22.dp, tint = c.bad, stroke = 1.8f)
                        }
                        Box(Modifier.size(44.dp).press(shape = RoundedCornerShape(14.dp)) { confirmKick = true }.semantics { contentDescription = "踢出设备" }, contentAlignment = Alignment.Center) {
                            SvgIcon(DesignIconData.actionLeave, 22.dp, tint = c.bad, stroke = 1.8f)
                        }
                    }
                }
                when {
                    peer.mobile -> { Spacer(Modifier.height(24.dp)); Box(Modifier.fillMaxWidth().heightIn(min = 150.dp), contentAlignment = Alignment.Center) { T("无法远程控制此设备", 13f, c.faint, align = TextAlign.Center, lineHeight = 18.2f) } }
                    peer.offline -> { Spacer(Modifier.height(24.dp)); Column(Modifier.fillMaxWidth().heightIn(min = 176.dp), horizontalAlignment = Alignment.CenterHorizontally, verticalArrangement = Arrangement.spacedBy(20.dp, Alignment.CenterVertically)) {
                        T(if (peer.incompatible) "版本过低，请升级" else "上次在线：${peer.lastSeen}前", 14f, c.faint, lineHeight = 19.6f)
                        Box(Modifier.height(44.dp).widthIn(min = 132.dp).clip(RoundedCornerShape(24.dp)).background(c.chip).press { editIp = true }.padding(horizontal = 24.dp), contentAlignment = Alignment.Center) {
                            T("编辑 IP", 14f, c.text2, lineHeight = 19.6f)
                        }
                    } }
                    else -> { Spacer(Modifier.height(36.dp)); ControlGrid(peer, modes, onTap = { tap(it) }, onHold = { haptic(true); focus = it }, onLevel = { t, n -> level(t, n) }) }
                }
                Spacer(Modifier.height(16.dp))
                Box(Modifier.fillMaxWidth().height(18.dp), contentAlignment = Alignment.Center) { T(notice, 11f, c.muted, lineHeight = 18f, align = TextAlign.Center) }
                Spacer(Modifier.height(8.dp))
            } else {
                val type = focus!!
                val audio = type == "speaker" || type == "mic"
                // cc-focus: two equal columns, control centred in the first, options in the second.
                // A tap on empty space returns to the grid, like a tap outside the sheet.
                Box(Modifier.fillMaxWidth().heightIn(min = 285.dp).press { focus = null }.padding(top = 16.dp, bottom = 8.dp)) {
                    Row(Modifier.fillMaxWidth().heightIn(min = 261.dp), verticalAlignment = Alignment.CenterVertically) {
                        Box(Modifier.weight(1f), contentAlignment = Alignment.Center) {
                            Box {
                                if (audio) AudioControl(type, modes.getValue(type), if (type == "speaker") peer.speakerLevel else peer.micLevel, false, Modifier.size(76.dp, 218.dp), onHold = {}, onLevel = { level(type, it) })
                                else RoundControl(type, modes.getValue(type), false, Modifier.size(80.dp), active = peer.gamingActive, onTap = {}, onHold = {})
                                T(configs.getValue(type).label, 13f, c.text2, Modifier.align(Alignment.BottomCenter).offset(y = 32.dp).wrapContentWidth(unbounded = true), lineHeight = 20f)
                            }
                        }
                        Column(Modifier.weight(1f).padding(start = 4.dp, end = 8.dp).offset(x = (-4).dp), verticalArrangement = Arrangement.spacedBy(14.dp)) {
                            configs.getValue(type).options.forEach { (value, label, badge) ->
                                val checked = modes[type] == value
                                Row(Modifier.fillMaxWidth().height(48.dp).clip(RoundedCornerShape(24.dp)).background(if (checked) c.accentSoft2 else c.chip)
                                    .press { choose(type, value); focus = null }.padding(horizontal = 15.dp),
                                    horizontalArrangement = Arrangement.spacedBy(7.dp, Alignment.CenterHorizontally), verticalAlignment = Alignment.CenterVertically) {
                                    val col = if (checked) c.accentInk else c.text2
                                    if (type == "mouse" || type == "keyboard") DevIcon(if (type == "mouse") DesignIconData.pcMouse else DesignIconData.pcKeyboard, 18.dp, col, badge,
                                        badgeTint = col, modifier = Modifier.padding(end = 1.dp))
                                    T(label, 14f, col, weight = if (checked) 600 else 400, lineHeight = 19.6f)
                                }
                            }
                        }
                    }
                }
            }
        }
        if (confirmKick) ConfirmCard(onDismiss = { confirmKick = false }) {
            T("将 ${peer.name} 踢出设备组", 17f, c.ink, Modifier.fillMaxWidth().padding(start = 22.dp, end = 22.dp, top = 24.dp), weight = 650, lineHeight = 25.5f, align = TextAlign.Center)
            T("踢出后，需要重新配对才能再次和它共享", 13f, c.text2, Modifier.fillMaxWidth().padding(start = 22.dp, end = 22.dp, top = 10.dp, bottom = 24.dp), lineHeight = 20.8f, align = TextAlign.Center)
            ConfirmActions("取消", c.ink, 500, "踢出", c.bad, 600, { confirmKick = false }) { confirmKick = false; actions.kick(peer); requestClose() }
        }
        if (editIp) EditIpDialog(load = { actions.memberAddress(peer) }, onCancel = { editIp = false }) { ip, port ->
            val error = actions.editIp(peer, ip, port)
            if (error == null) { editIp = false; setNotice("IP 已更新") }
            error
        }
    }
}

/** Centered alert card used inside the device sheet (cc-confirm). */
@Composable
fun ConfirmCard(onDismiss: () -> Unit, content: @Composable ColumnScope.() -> Unit) {
    val c = Oink.colors
    BackHandler { onDismiss() }
    Box(Modifier.fillMaxSize().background(c.overlay).press(pressedColor = null) { onDismiss() }.windowInsetsPadding(WindowInsets.safeDrawing).padding(24.dp), contentAlignment = Alignment.Center) {
        Column(Modifier.widthIn(max = 310.dp).fillMaxWidth().clip(RoundedCornerShape(24.dp)).background(c.elev).border(1.dp, c.border, RoundedCornerShape(24.dp))
            .press(pressedColor = null) {}.padding(1.dp), content = content)
    }
}

@Composable
fun ConfirmActions(left: String, leftColor: Color, leftWeight: Int, right: String, rightColor: Color, rightWeight: Int, onLeft: () -> Unit, onRight: () -> Unit) {
    val c = Oink.colors
    Box(Modifier.fillMaxWidth().height(1.dp).background(c.chip))
    Row(Modifier.fillMaxWidth().height(50.dp)) {
        Box(Modifier.weight(1f).fillMaxHeight().press(pressedColor = c.press) { onLeft() }, contentAlignment = Alignment.Center) { T(left, 16f, leftColor, weight = leftWeight, lineHeight = 22.4f) }
        Box(Modifier.width(1.dp).fillMaxHeight().background(c.chip))
        Box(Modifier.weight(1f).fillMaxHeight().press(pressedColor = c.press) { onRight() }, contentAlignment = Alignment.Center) { T(right, 16f, rightColor, weight = rightWeight, lineHeight = 22.4f) }
    }
}

@Composable
private fun EditIpDialog(load: suspend () -> String?, onCancel: () -> Unit, onSave: suspend (String, String) -> String?) {
    val c = Oink.colors
    val scope = rememberCoroutineScope()
    var ip by remember { mutableStateOf("") }
    var port by remember { mutableStateOf("") }
    var error by remember { mutableStateOf("") }
    var saving by remember { mutableStateOf(false) }
    // Prefill with the member's current manual address ("a.b.c.d", "a.b.c.d:port" or "[v6]:port").
    LaunchedEffect(Unit) {
        val current = load() ?: return@LaunchedEffect
        if (ip.isNotEmpty() || port.isNotEmpty()) return@LaunchedEffect
        val bracket = Regex("^\\[(.+)]:(\\d+)$").find(current)
        when {
            bracket != null -> { ip = bracket.groupValues[1]; port = bracket.groupValues[2] }
            current.count { it == ':' } == 1 -> { ip = current.substringBefore(':'); port = current.substringAfter(':') }
            else -> ip = current
        }
    }
    ConfirmCard(onCancel) {
        T("编辑 IP", 17f, c.ink, Modifier.fillMaxWidth().padding(start = 22.dp, end = 22.dp, top = 24.dp, bottom = 10.dp), weight = 650, lineHeight = 25.5f, align = TextAlign.Center)
        Row(Modifier.padding(start = 20.dp, end = 20.dp, top = 8.dp, bottom = 24.dp), horizontalArrangement = Arrangement.spacedBy(10.dp)) {
            DialogField(ip, "IP 地址", Modifier.weight(1f), KeyboardType.Uri) { ip = it; error = "" }
            DialogField(port, "47470", Modifier.width(88.dp), KeyboardType.Number) { port = it; error = "" }
        }
        if (error.isNotEmpty()) T(error, 12f, c.bad, Modifier.padding(start = 20.dp, end = 20.dp, bottom = 16.dp).offset(y = (-10).dp))
        ConfirmActions("取消", c.ink, 500, if (saving) "正在保存…" else "确认", c.accentInk, 500, onCancel) {
            if (saving) return@ConfirmActions
            val value = ip.trim(); val p = port.trim()
            val v4 = Regex("^(\\d{1,3}\\.){3}\\d{1,3}$").matches(value) && value.split('.').all { it.toInt() <= 255 }
            val v6 = value.contains(':') && runCatching { java.net.InetAddress.getByName(value) is java.net.Inet6Address }.getOrDefault(false)
            error = when {
                value.isEmpty() -> "请输入 IP 地址"
                !v4 && !v6 -> "请输入有效的 IP 地址"
                p.isNotEmpty() && (p.toIntOrNull() ?: 0) !in 1..65535 -> "端口需为 1–65535，或留空"
                else -> ""
            }
            if (error.isEmpty()) scope.launch { saving = true; error = onSave(value, p).orEmpty(); saving = false }
        }
    }
}

@Composable
private fun DialogField(value: String, placeholder: String, modifier: Modifier, type: KeyboardType, onChange: (String) -> Unit) {
    val c = Oink.colors
    BasicTextField(value, onChange, modifier.height(44.dp), singleLine = true, textStyle = textStyle(14f, c.ink, lineHeight = 19.6f),
        cursorBrush = SolidColor(c.accent), keyboardOptions = KeyboardOptions(keyboardType = type),
        decorationBox = { inner ->
            Box(Modifier.fillMaxSize().clip(RoundedCornerShape(12.dp)).background(c.field).border(1.dp, c.track, RoundedCornerShape(12.dp)).padding(horizontal = 12.dp), contentAlignment = Alignment.CenterStart) {
                if (value.isEmpty()) T(placeholder, 14f, c.faint, lineHeight = 19.6f)
                inner()
            }
        })
}

@Composable
private fun ControlGrid(peer: PeerUi, modes: Map<String, String>, onTap: (String) -> Unit, onHold: (String) -> Unit, onLevel: (String, Int) -> Unit) {
    BoxWithConstraints(Modifier.fillMaxWidth()) {
        val col = (minOf(maxWidth, 420.dp) - 36.dp) / 4
        Row(Modifier.align(Alignment.Center), horizontalArrangement = Arrangement.spacedBy(12.dp)) {
            Column(verticalArrangement = Arrangement.spacedBy(20.dp)) {
                RoundControl("mouse", modes.getValue("mouse"), false, Modifier.size(col), active = peer.gamingActive, onTap = { onTap("mouse") }, onHold = { onHold("mouse") })
                RoundControl("locked", modes.getValue("locked"), false, Modifier.size(col), active = peer.gamingActive, onTap = { onTap("locked") }, onHold = { onHold("locked") })
            }
            Column(verticalArrangement = Arrangement.spacedBy(20.dp)) {
                RoundControl("keyboard", modes.getValue("keyboard"), false, Modifier.size(col), active = peer.gamingActive, onTap = { onTap("keyboard") }, onHold = { onHold("keyboard") })
                RoundControl("gaming", modes.getValue("gaming"), false, Modifier.size(col), active = peer.gamingActive, onTap = { onTap("gaming") }, onHold = { onHold("gaming") })
            }
            AudioControl("speaker", modes.getValue("speaker"), peer.speakerLevel, false, Modifier.size(col, col * 2 + 20.dp), { onHold("speaker") }) { onLevel("speaker", it) }
            AudioControl("mic", modes.getValue("mic"), peer.micLevel, false, Modifier.size(col, col * 2 + 20.dp), { onHold("mic") }) { onLevel("mic", it) }
        }
    }
}

/** One recognizer for press, hold (450 ms) and drag, so a hold never toggles a control (usePress). */
// The scale animations run in the composition's scope: Animatable needs its MonotonicFrameClock,
// which a bare MainScope does not have (every tap crashed with IllegalStateException).
internal fun Modifier.pressHold(enabled: Boolean, scaleUp: Boolean, scale: Animatable<Float, *>, scope: kotlinx.coroutines.CoroutineScope,
                               onTap: () -> Unit, onHold: () -> Unit, onDrag: ((Float, Float) -> Unit)? = null) = pointerInput(enabled) {
    if (!enabled) return@pointerInput
    awaitEachGesture {
        // Consumed: this recognizer owns the gesture, so an enclosing tap target (the detail
        // view's "tap anywhere to go back") does not also fire for a press on a control.
        val down = awaitFirstDown().also { it.consume() }
        val job = scope.launch {
            if (scaleUp) { scale.animateTo(1.08f, tween(72, easing = LinearEasing)); scale.animateTo(1.18f, tween(378, easing = LinearEasing)) }
            else { delay(180); scale.animateTo(.92f, tween(270, easing = LinearEasing)) }
        }
        var moved = false
        val up = withTimeoutOrNull(450) {
            while (true) {
                val ev = awaitPointerEvent(PointerEventPass.Main)
                val ch = ev.changes.first()
                if (!ch.pressed) return@withTimeoutOrNull true
                if ((ch.position - down.position).getDistance() > 8.dp.toPx()) { moved = true; return@withTimeoutOrNull false }
            }
            @Suppress("UNREACHABLE_CODE") false
        }
        job.cancel()
        scope.launch { scale.animateTo(1f, tween(140)) }
        when {
            up == null -> {
                onHold()
                while (true) { val ev = awaitPointerEvent(); if (ev.changes.none { it.pressed }) break }
            }
            up == true -> if (onDrag != null) onDrag(down.position.y, size.height.toFloat()) else onTap()
            moved && onDrag != null -> {
                while (true) {
                    val ev = awaitPointerEvent(); val ch = ev.changes.first()
                    if (!ch.pressed) break
                    ch.consume(); onDrag(ch.position.y, size.height.toFloat())
                }
            }
        }
    }
}

@Composable
private fun RoundControl(type: String, mode: String, disabled: Boolean, modifier: Modifier, active: Boolean = false, onTap: () -> Unit, onHold: () -> Unit) {
    val c = Oink.colors
    val input = type == "mouse" || type == "keyboard"
    // Game mode "auto" shows what it resolved to (the AUTO tag stays).
    val on = mode == "on" || mode == "auto" && active
    val target = if (input) when (mode) { "shared" -> c.good; "blocked" -> c.bad; else -> c.faint } else if (on) c.accent else c.faint
    val color by animateColorAsState(target, tween(220), label = "round")
    val strong = input && mode != "local" || !input && on
    val badge = if (input) when (mode) { "shared" -> Badge.Wifi; "blocked" -> Badge.Ban; else -> null } else null
    val scale = remember { Animatable(1f) }
    val scope = rememberCoroutineScope()
    // The gesture loop outlives recompositions (keyed on `enabled` only); it must call the
    // current callbacks, which close over the current mode, not the ones of its first composition.
    val tap by rememberUpdatedState(onTap)
    val hold by rememberUpdatedState(onHold)
    Box(modifier.scale(scale.value).alpha(if (disabled) .45f else 1f).clip(CircleShape).background(c.control)
        .border(if (strong) 2.dp else 1.dp, if (strong) color else c.track, CircleShape)
        .pressHold(!disabled, true, scale, scope, { tap() }, { hold() })
        .semantics { contentDescription = "${configs.getValue(type).label}：${configs.getValue(type).options.firstOrNull { it.first == mode }?.second ?: "自动"}" },
        contentAlignment = Alignment.Center) {
        CompositionLocalProvider(LocalCutout provides c.control) {
            if (type == "locked") Lucide(DesignIconData.lucideLockKeyhole, 28.dp, color)
            else Box {
                DevIcon(when (type) { "mouse" -> DesignIconData.pcMouse; "keyboard" -> DesignIconData.pcKeyboard; else -> DesignIconData.pcGame }, 28.dp, color, badge)
                if (type == "gaming" && mode == "auto") Box(Modifier.align(Alignment.BottomEnd).offset(x = 10.dp).size(27.dp, 12.dp).clip(RoundedCornerShape(3.dp)).background(c.control), contentAlignment = Alignment.Center) {
                    T("AUTO", 8f, color, weight = 700, lineHeight = 12f, letterSpacing = .2f)
                }
            }
        }
    }
}

@Composable
private fun AudioControl(type: String, mode: String, level: Int, disabledIn: Boolean, modifier: Modifier, onHold: () -> Unit, onLevel: (Int) -> Unit) {
    val c = Oink.colors
    val haptic = rememberHaptics()
    // An unavailable endpoint is shown like local (no level) and cannot be used.
    val disabled = disabledIn || mode == "unavailable"
    val local = mode == "local" || mode == "unavailable"
    val max = audioMax(type, mode)
    val value = if (local) 0 else minOf(level, max)
    val tint = if (local) c.faint else if (mode == "forward" || mode == "receive") c.good else c.accent
    val badge = if (local) (if (type == "mic") Badge.Mute else null) else if (mode == "forward" || mode == "send") Badge.Send else Badge.Receive
    val scale = remember { Animatable(1f) }
    val scope = rememberCoroutineScope()
    var inGap by remember { mutableStateOf(false) }
    val frac = value.toFloat() / max
    // Current callbacks for the long-lived gesture loop (see RoundControl).
    val hold by rememberUpdatedState(onHold)
    val drag by rememberUpdatedState { y: Float, h: Float ->
        if (!local && !disabled) {
            val raw = Math.round(((h - y) / h).coerceIn(0f, 1f) * max)
            val entered = inDetent(raw, max)
            if (entered && !inGap) haptic(false)
            inGap = entered
            onLevel(pointerVolume(raw, max))
        }
    }
    Box(modifier.scale(scale.value).alpha(if (disabled) .45f else 1f).clip(RoundedCornerShape(44.dp)).background(c.track).border(1.dp, c.track, RoundedCornerShape(44.dp))
        .pressHold(!disabled, false, scale, scope, {}, { hold() }) { y, h -> drag(y, h) }
        .semantics { contentDescription = "${configs.getValue(type).label}音量 ${if (local) "本地，音量固定为零" else "$value%"}" }) {
        Box(Modifier.align(Alignment.BottomCenter).padding(1.dp).fillMaxWidth().fillMaxHeight(frac).background(c.level))
        CompositionLocalProvider(LocalCutout provides if (frac > .14f) c.level else c.track) {
            DevIcon(if (type == "speaker") speakerGlyph(volumeWaves(value)) else DesignIconData.pcMicrophone, 28.dp, tint, badge, volume = true,
                badgeTint = badgeTintFor(badge, tint), modifier = Modifier.align(Alignment.BottomCenter).padding(bottom = 19.dp))
        }
    }
}
