package com.hachiwii.oink.ui

import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.CubicBezierEasing
import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.interaction.collectIsFocusedAsState
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.draw.rotate
import androidx.compose.ui.draw.scale
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.layout.layout
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.launch

private const val SELF_IP_LIMIT = 2

/** Add-device page (AddDevicePage.jsx): requests, nearby devices, manual add, this device's addresses. */
@Composable
fun AddDevicePage(model: AppModel, actions: AppActions, ui: UiState) {
    val c = Oink.colors
    val scope = rememberCoroutineScope()
    val spin = remember { Animatable(0f) }
    Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(bottom = 92.dp)) {
        Column(Modifier.align(Alignment.CenterHorizontally).widthIn(max = 620.dp).fillMaxWidth().padding(start = 20.dp, end = 20.dp, top = 12.dp, bottom = 28.dp)) {
            if (model.requests.isNotEmpty()) {
                GroupTitle("配对请求", model.requests.size, Modifier.heightIn(min = 36.dp))
                Spacer(Modifier.height(4.dp))
                model.requests.forEachIndexed { i, req ->
                    // Requests carry no address; the device ID stands in for it.
                    val origin = req.ip.ifEmpty { req.deviceId }
                    PeerRow(req, listOfNotNull(origin.ifEmpty { null }, if (req.groupSize > 1) "设备组共 ${req.groupSize} 台" else null).joinToString(" · "), first = i == 0) {
                        Row(horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                            RoundAction(c.badSoft, "拒绝 ${req.name} 的配对请求", { actions.reject(req) }) { Lucide(DesignIconData.lucideX, 20.dp, c.badInk, 2.2f) }
                            RoundAction(c.goodSoft, "接受 ${req.name} 的配对请求", { actions.accept(req) }) { Lucide(DesignIconData.lucideCheck, 20.dp, c.goodInk, 2.4f) }
                        }
                    }
                }
                Spacer(Modifier.height(28.dp))
            }
            GroupTitle("附近设备", model.nearby.size, Modifier.heightIn(min = 36.dp)) {
                Spacer(Modifier.weight(1f))
                // 40 × 40 with margin -4 -8 -4 auto: it lays out 32 high inside the 36 header.
                Box(Modifier.pullUpTop(4.dp).pullUpBottom(4.dp).offset(x = 8.dp).size(40.dp).press(shape = RoundedCornerShape(14.dp)) {
                    if (!spin.isRunning) scope.launch { spin.snapTo(0f); spin.animateTo(360f, tween(720, easing = CubicBezierEasing(.4f, 0f, .2f, 1f))) }
                    actions.refreshNearby()
                }.semantics { contentDescription = "刷新附近设备" }, contentAlignment = Alignment.Center) {
                    Lucide(DesignIconData.lucideRefreshCw, 17.dp, c.muted, modifier = Modifier.rotate(spin.value))
                }
            }
            Spacer(Modifier.height(4.dp))
            if (model.nearby.isEmpty()) Searching()
            else {
                model.nearby.forEachIndexed { i, peer ->
                    PeerRow(peer, if (peer.incompatible) "协议版本不兼容" else peer.address, subColor = if (peer.incompatible) c.faint else c.muted, disabled = peer.incompatible, first = i == 0,
                        onClick = { actions.pair(peer) }) {
                        Box(Modifier.size(40.dp).alpha(if (peer.incompatible) .4f else 1f).clip(CircleShape).background(c.chip), contentAlignment = Alignment.Center) {
                            Lucide(DesignIconData.lucidePlus, 20.dp, c.ink, 2.2f)
                        }
                    }
                }
                Row(Modifier.fillMaxWidth().padding(top = 14.dp), horizontalArrangement = Arrangement.spacedBy(8.dp, Alignment.CenterHorizontally), verticalAlignment = Alignment.CenterVertically) {
                    Dots(c.faint); T("正在搜索附近的设备", 12.5f, c.faint, lineHeight = 17.5f)
                }
            }
            Box(Modifier.fillMaxWidth().padding(top = 16.dp), contentAlignment = Alignment.Center) {
                SoftCapsule("手动添加", leading = { Lucide(DesignIconData.lucidePlus, 17.dp, c.text2, 2.2f) }) { actions.manualReset(); ui.pairingSheet = "manual" }
            }
        }
    }
}

@Composable
private fun Searching() {
    val c = Oink.colors
    val ping by rememberInfiniteTransition(label = "ping").animateFloat(0f, 1f, infiniteRepeatable(tween(2000, easing = EaseOutExpo), RepeatMode.Restart), label = "p")
    Column(Modifier.fillMaxWidth().padding(start = 16.dp, end = 16.dp, top = 28.dp, bottom = 4.dp), horizontalAlignment = Alignment.CenterHorizontally) {
        Box(Modifier.size(72.dp), contentAlignment = Alignment.Center) {
            Box(Modifier.fillMaxSize().clip(CircleShape).background(c.accentSoft))
            Lucide(DesignIconData.lucideRadar, 30.dp, c.accentInk)
            Box(Modifier.fillMaxSize().graphicsLayer { val s = .85f + .75f * ping; scaleX = s; scaleY = s; alpha = .5f * (1 - ping) }
                .drawBehind { drawCircle(c.accent, size.width / 2 - 1.dp.toPx(), style = Stroke(2.dp.toPx())) })
        }
        T("正在搜索附近的设备", 16f, c.ink, Modifier.padding(top = 16.dp), weight = 600, lineHeight = 22.4f)
        T("请确认对方已打开 Oink，并和本机连接到同一个网络。", 13f, c.muted, Modifier.padding(top = 6.dp).widthIn(max = 280.dp), lineHeight = 20.15f, align = TextAlign.Center)
    }
}

@Composable
private fun RoundAction(bg: Color, label: String, onClick: () -> Unit, icon: @Composable () -> Unit) {
    val haptic = rememberHaptics()
    Box(Modifier.size(40.dp).clip(CircleShape).background(bg).press { haptic(false); onClick() }.semantics { contentDescription = label }, contentAlignment = Alignment.Center) { icon() }
}

/** ad-row: 76 min, logo, name 17/650, sub 13; divider from the text (left 46). */
@Composable
fun PeerRow(peer: NearbyUi, sub: String, subColor: Color = Oink.colors.muted, disabled: Boolean = false, first: Boolean = true, minHeight: androidx.compose.ui.unit.Dp = 76.dp,
            onClick: (() -> Unit)? = null, trailing: @Composable () -> Unit) {
    val c = Oink.colors
    val haptic = rememberHaptics()
    Row(Modifier.fillMaxWidth()
        .then(if (first) Modifier else Modifier.drawBehind { drawRect(c.line, Offset(46.dp.toPx(), 0f), Size(size.width - 46.dp.toPx(), 1.dp.toPx())) })
        .heightIn(min = minHeight).then(if (onClick != null) Modifier.press(enabled = !disabled, pressedColor = c.press) { haptic(false); onClick() } else Modifier)
        .padding(vertical = 12.dp), horizontalArrangement = Arrangement.spacedBy(14.dp), verticalAlignment = Alignment.CenterVertically) {
        PlatformIcon(peer.platform, Modifier.alpha(if (disabled) .45f else 1f))
        Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(3.dp)) {
            T(peer.name, 17f, if (disabled) c.faint else c.ink, weight = 650, lineHeight = 22.95f, letterSpacing = -.2f)
            T(sub, 13f, subColor, lineHeight = 18.2f, tabular = true)
        }
        trailing()
    }
}

/** Device info dialog body (AddDevicePage.jsx SelfInfo): IPs (two, then 查看全部), port and ID; tap a value to copy it. */
@Composable
fun SelfInfoDialogBody(self: SelfInfo, all: Boolean, showAll: () -> Unit, copied: (String) -> Unit) {
    val c = Oink.colors
    val clipboard = androidx.compose.ui.platform.LocalClipboardManager.current
    val haptic = rememberHaptics()
    @Composable fun value(text: String) = Row(Modifier.offset(x = 6.dp).heightIn(min = 32.dp).clip(RoundedCornerShape(8.dp))
        .press(pressedColor = c.press) { haptic(false); clipboard.setText(androidx.compose.ui.text.AnnotatedString(text)); copied(text) }
        .padding(horizontal = 6.dp).semantics { contentDescription = "$text，点按复制" }, horizontalArrangement = Arrangement.spacedBy(6.dp), verticalAlignment = Alignment.CenterVertically) {
        T(text, 13.5f, c.ink, lineHeight = 20.25f, tabular = true, ellipsis = true)
        Lucide(DesignIconData.lucideCopy, 13.dp, c.faint)
    }
    @Composable fun line(label: String, values: @Composable ColumnScope.() -> Unit) = Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(16.dp)) {
        T(label, 13.5f, c.muted, Modifier.padding(top = 5.dp), lineHeight = 20.25f)
        Column(Modifier.weight(1f), horizontalAlignment = Alignment.End, content = values)
    }
    Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
        line("本机 IP") {
            val more = !all && self.ips.size > SELF_IP_LIMIT
            (if (all) self.ips else self.ips.take(SELF_IP_LIMIT)).forEach { value(it) }
            if (more) Row(Modifier.offset(x = 4.dp).pullUpTop(2.dp).pullUpSpace2(4.dp).height(28.dp).clip(RoundedCornerShape(8.dp)).press { showAll() }.padding(horizontal = 4.dp),
                horizontalArrangement = Arrangement.spacedBy(1.dp), verticalAlignment = Alignment.CenterVertically) {
                T("查看全部 ${self.ips.size} 个", 12.5f, c.accentInk, weight = 600, lineHeight = 18.75f)
                Lucide(DesignIconData.lucideChevronRight, 14.dp, c.accentInk)
            }
        }
        line("本机端口") { if (self.port != null) value("${self.port}") else T("—", 13.5f, c.faint, Modifier.padding(top = 5.dp), lineHeight = 20.25f) }
        line("本机 ID") { if (self.deviceId.isNotEmpty()) value(self.deviceId) else T("—", 13.5f, c.faint, Modifier.padding(top = 5.dp), lineHeight = 20.25f) }
    }
    T("点按任一项即可复制", 12f, c.faint, Modifier.padding(top = 10.dp), lineHeight = 19.2f)
}

/** margin: -2 -4 -4 0 on a 28-high button: it lays out 6 shorter than it draws. */
private fun Modifier.pullUpSpace2(d: androidx.compose.ui.unit.Dp) = this.then(Modifier.layout { m, cons ->
    val p = m.measure(cons); val px = d.roundToPx()
    layout(p.width, (p.height - px).coerceAtLeast(0)) { p.place(0, 0) }
})

/** Sheets and the self-IP dialog of the add-device flow; rendered above the tab bar. */
@Composable
fun PairingLayer(model: AppModel, init: UiInit, ui: UiState, actions: AppActions, toast: MutableState<ToastData?>) {
    val pairing = model.pairing
    val sheet = ui.pairingSheet
    var closing by remember { mutableStateOf(false) }
    LaunchedEffect(sheet) { closing = false }
    // When the success sheet closes the app returns to the device page, whichever side started the
    // pairing; no device sheet opens (user 2026-11-11).
    var succeeded by remember { mutableStateOf(false) }
    val status = pairing?.status
    LaunchedEffect(status) {
        if (status == null && succeeded) { ui.addPage = false; ui.tab = Tab.Devices }
        succeeded = status == "success"
    }
    when {
        pairing != null && (sheet == null || sheet == "pair") -> PairSheet(pairing, init, actions)
        sheet == "manual" -> ManualSheet(model.manual, init, actions, closing, { closing = true }, { ui.pairingSheet = null })
        sheet == "info" || sheet == "info-all" -> OinkDialog("本机信息", { ui.pairingSheet = null }, listOf(DialogAction("知道了") { ui.pairingSheet = null })) {
            SelfInfoDialogBody(model.self, all = sheet == "info-all", showAll = { ui.pairingSheet = "info-all" }) { toast.value = ToastData("已复制 $it") }
        }
    }
}

@Composable
private fun ManualSheet(m: ManualState, init: UiInit, actions: AppActions, closing: Boolean, close: () -> Unit, closed: () -> Unit) {
    val c = Oink.colors
    var ip by remember { mutableStateOf(m.ip) }
    var port by remember { mutableStateOf(m.port) }
    var localError by remember { mutableStateOf<Pair<String, String?>?>(null) }
    var shakeKey by remember { mutableStateOf<Int?>(null) }
    LaunchedEffect(m.ip) { if (m.ip.isNotEmpty()) ip = m.ip }
    val error = localError ?: m.error?.let { it to m.errorField }
    val shake = rememberShake(shakeKey)
    fun submit() {
        val parts = ip.trim().split(":")
        val host = parts[0]; val inlinePort = parts.getOrNull(1)
        val portText = inlinePort ?: port.trim()
        val v4 = host.split('.').let { s -> s.size == 4 && s.all { it.matches(Regex("\\d{1,3}")) && it.toInt() <= 255 } }
        val p = if (portText.isEmpty()) 47470 else portText.toIntOrNull() ?: -1
        localError = when {
            host.isEmpty() -> "请输入 IP 地址" to "ip"
            parts.size > 2 || !v4 -> "请输入有效的 IPv4 地址，例如 192.168.1.20" to "ip"
            (inlinePort != null && inlinePort.isEmpty()) || p !in 1..65535 -> "端口应在 1–65535 之间" to "port"
            else -> null
        }
        if (localError != null) shakeKey = (shakeKey ?: 0) + 1 else actions.manualSearch(host, p)
    }
    BottomSheetLayer(onDismiss = close, handleColor = c.track, closing = closing, onClosed = closed) {
        Column(Modifier.widthIn(max = 480.dp).align(Alignment.CenterHorizontally).padding(bottom = 20.dp)) {
            T("手动添加", 20f, c.ink, Modifier.padding(top = 10.dp), weight = 650, lineHeight = 26f)
            T("输入对方的 IP 地址。对方的地址显示在它「添加设备」页的底部。", 13f, c.muted, Modifier.padding(top = 6.dp, bottom = 18.dp), lineHeight = 20.15f)
            Row(Modifier.graphicsLayer { translationX = shake * density }, horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                // Editing the address drops a found device or a lookup failure.
                ManualField("IP 地址", ip, "192.168.1.20", Modifier.weight(1f), error?.second == "ip", KeyboardType.Decimal, focusLook = init.pairingSheet == "manual", autoFocus = init.pairingSheet != "manual") {
                    ip = it.filter { ch -> ch.isDigit() || ch == '.' || ch == ':' }.take(21); if (error?.second == "ip") localError = null
                    if (m.found != null || m.error != null) actions.manualReset()
                }
                ManualField("端口", port, "47470", Modifier.width(96.dp), error?.second == "port", KeyboardType.Number) {
                    port = it.filter(Char::isDigit).take(5); if (error?.second == "port") localError = null
                    if (m.found != null || m.error != null) actions.manualReset()
                }
            }
            if (error != null) Row(Modifier.padding(start = 4.dp, top = 10.dp), horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                Lucide(DesignIconData.lucideAlertCircle, 16.dp, c.badInk, modifier = Modifier.padding(top = 1.dp))
                T(error.first, 13f, c.badInk, lineHeight = 18.85f)
            }
            val found = m.found
            if (found != null) Column(Modifier.padding(top = 16.dp).fillMaxWidth().clip(RoundedCornerShape(20.dp)).background(c.elev).border(1.dp, c.line, RoundedCornerShape(20.dp))
                .padding(start = 16.dp, end = 16.dp, top = 2.dp, bottom = 16.dp)) {
                PeerRow(found, "${found.address} · ${found.deviceId}", minHeight = 70.dp) {}
                PrimaryCapsule("配对", Modifier.padding(top = 6.dp).fillMaxWidth(), leading = { Lucide(DesignIconData.lucidePlus, 17.dp, c.accentInk, 2.4f) }) { actions.pair(found); close() }
            } else PrimaryCapsule(if (m.searching) "正在查找…" else "查找", Modifier.padding(top = 18.dp).fillMaxWidth(), enabled = !m.searching,
                leading = { if (m.searching) Spinner(18.dp) }) { submit() }
        }
    }
}

@Composable
private fun ManualField(label: String, value: String, placeholder: String, modifier: Modifier, invalid: Boolean, type: KeyboardType, focusLook: Boolean = false, autoFocus: Boolean = false, onChange: (String) -> Unit) {
    val c = Oink.colors
    val source = remember { MutableInteractionSource() }
    val focused by source.collectIsFocusedAsState()
    val focus = remember { FocusRequester() }
    LaunchedEffect(Unit) { if (autoFocus) runCatching { focus.requestFocus() } }
    val look = focused || focusLook
    Column(modifier, verticalArrangement = Arrangement.spacedBy(6.dp)) {
        T(label, 12.5f, c.text2, Modifier.padding(start = 4.dp), weight = 600, lineHeight = 17.5f)
        BasicTextField(value, onChange, Modifier.fillMaxWidth().height(48.dp).focusRequester(focus), singleLine = true, interactionSource = source,
            textStyle = textStyle(16f, c.ink, lineHeight = 22.4f, tabular = true), cursorBrush = SolidColor(c.accent), keyboardOptions = KeyboardOptions(keyboardType = type),
            decorationBox = { inner ->
                val ring = when { invalid -> c.badSoft; look -> c.accentSoft; else -> Color.Transparent }
                Box(Modifier.fillMaxSize().drawBehind {
                    if (ring.alpha > 0f) { val g = 3.dp.toPx(); drawRoundRect(ring, Offset(-g, -g), Size(size.width + 2 * g, size.height + 2 * g), CornerRadius(14.dp.toPx() + g)) }
                }.clip(RoundedCornerShape(14.dp)).background(c.elev).border(1.dp, when { invalid -> c.bad; look -> c.accent; else -> c.track }, RoundedCornerShape(14.dp))
                    .padding(horizontal = 14.dp), contentAlignment = Alignment.CenterStart) {
                    if (value.isEmpty()) T(placeholder, 16f, c.faint, lineHeight = 22.4f, tabular = true)
                    inner()
                }
            })
    }
}

@Composable
private fun PairSheet(s: PairingView, init: UiInit, actions: AppActions) {
    val c = Oink.colors
    val failed = s.status == "failed"; val success = s.status == "success"
    val shake = rememberShake(if (s.status == "wrong") s.wrongs else null)
    BottomSheetLayer(onDismiss = {}, dismissible = false, onBack = { if (!success) actions.cancelPairing() }) {
        Column(Modifier.widthIn(max = 480.dp).align(Alignment.CenterHorizontally).padding(bottom = 20.dp)) {
            Row(Modifier.padding(top = 10.dp, bottom = 4.dp), horizontalArrangement = Arrangement.spacedBy(12.dp), verticalAlignment = Alignment.CenterVertically) {
                CompositionLocalProvider(LocalCutout provides c.sheet) { PlatformIcon(s.peer.platform) }
                Column {
                    T("与 ${s.peer.name} 配对", 19f, c.ink, weight = 650, lineHeight = 25.65f)
                    T("${s.peer.address} · ${s.peer.deviceId}", 11.5f, c.faint, Modifier.padding(top = 2.dp), lineHeight = 16.1f, tabular = true)
                }
            }
            Box(Modifier.fillMaxWidth().heightIn(min = 186.dp).padding(top = 14.dp, bottom = 10.dp), contentAlignment = Alignment.Center) {
                Column(Modifier.fillMaxWidth(), horizontalAlignment = Alignment.CenterHorizontally, verticalArrangement = Arrangement.spacedBy(14.dp)) {
                    when {
                        s.status == "connecting" -> { Spinner(32.dp); Status(null) { T("正在连接…", 13.5f, c.muted, lineHeight = 18.9f) } }
                        failed -> {
                            Box(Modifier.size(52.dp).clip(CircleShape).background(c.badSoft), contentAlignment = Alignment.Center) { Lucide(DesignIconData.lucideX, 24.dp, c.badInk, 2.6f) }
                            T("配对失败", 17f, c.ink, weight = 650, lineHeight = 23.8f)
                            T(s.reason, 13.5f, c.text2, Modifier.pullUpTop(8.dp), lineHeight = 18.9f, align = TextAlign.Center)
                        }
                        s.dir == "out" -> {
                            T("在 ${s.peer.name} 上输入这个配对码", 14f, c.text2, lineHeight = 19.6f, align = TextAlign.Center)
                            CodeCells(s.code, digitsOnly = true, success = success, showActive = false)
                            if (success) Status(c.goodInk, 600) { Lucide(DesignIconData.lucideCheck, 17.dp, c.goodInk, 2.6f); T("配对成功", 13.5f, c.goodInk, weight = 600, lineHeight = 18.9f) }
                            else Status(null) { Dots(c.muted); T("等待对方输入", 13.5f, c.muted, lineHeight = 18.9f) }
                        }
                        else -> {
                            T("输入 ${s.peer.name} 屏幕上显示的配对码", 14f, c.text2, lineHeight = 19.6f, align = TextAlign.Center)
                            var input by remember(s.status, s.wrongs) { mutableStateOf(s.input) }
                            Box(Modifier.graphicsLayer { translationX = shake * density }) {
                                CodeEntry(input, invalid = s.status == "wrong", disabled = s.status == "verifying" || success, success = success, cellBg = c.elev) {
                                    input = it; if (it.length == 6) actions.submitCode(it)
                                }
                            }
                            when {
                                s.status == "verifying" -> Status(null) { Spinner(16.dp); T("正在验证…", 13.5f, c.muted, lineHeight = 18.9f) }
                                s.status == "wrong" -> Status(c.badInk) { Lucide(DesignIconData.lucideAlertCircle, 16.dp, c.badInk); T("配对码不正确，还可以再试 ${s.attemptsLeft ?: (3 - s.wrongs)} 次", 13.5f, c.badInk, lineHeight = 18.9f) }
                                success -> Status(c.goodInk, 600) { Lucide(DesignIconData.lucideCheck, 17.dp, c.goodInk, 2.6f); T("配对成功", 13.5f, c.goodInk, weight = 600, lineHeight = 18.9f) }
                                s.wrongs > 0 -> Status(null) { T("还可以再试 ${s.attemptsLeft ?: (3 - s.wrongs)} 次", 13.5f, c.muted, lineHeight = 18.9f) }
                                else -> Status(null) {}
                            }
                        }
                    }
                }
            }
            if (!failed) Row(Modifier.fillMaxWidth().drawBehind { drawRect(c.line, Offset.Zero, Size(size.width, 1.dp.toPx())) }.padding(start = 4.dp, end = 4.dp, top = 15.dp),
                horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Lucide(DesignIconData.lucideShieldAlert, 14.dp, c.muted, modifier = Modifier.padding(top = 2.dp))
                T("配对后，对方可以和本机共享剪贴板、文件和音频；开启特权模式后还能操控本机。请确认你信任此设备。", 12f, c.muted, lineHeight = 18.6f)
            }
            SoftCapsule(if (failed) "关闭" else "取消配对", Modifier.padding(top = 18.dp).fillMaxWidth(), enabled = !success) { actions.cancelPairing() }
        }
    }
}

@Composable
private fun Status(color: Color?, weight: Int = 400, content: @Composable RowScope.() -> Unit) {
    Row(Modifier.heightIn(min = 22.dp), horizontalArrangement = Arrangement.spacedBy(6.dp), verticalAlignment = Alignment.CenterVertically, content = content)
}

/** Six code cells over one hidden numeric field. */
@Composable
private fun CodeEntry(value: String, invalid: Boolean, disabled: Boolean, success: Boolean, cellBg: Color, onChange: (String) -> Unit) {
    val focus = remember { FocusRequester() }
    Box {
        CodeCells(value, invalid = invalid, disabled = disabled, success = success, cellBg = cellBg, onClick = { runCatching { focus.requestFocus() } })
        BasicTextField(value, { v -> if (!disabled) onChange(v.filter(Char::isDigit).take(6)) }, Modifier.matchParentSize().alpha(0f).focusRequester(focus),
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.NumberPassword), singleLine = true)
    }
}

/** Negative bottom margin: lays out d shorter than it draws. */
private fun Modifier.pullUpBottom(d: androidx.compose.ui.unit.Dp) = this.then(Modifier.layout { m, cons ->
    val p = m.measure(cons); val px = d.roundToPx()
    layout(p.width, (p.height - px).coerceAtLeast(0)) { p.place(0, 0) }
})

/** Negative top margin inside a column: draws d higher and gives the space back. */
private fun Modifier.pullUpTop(d: androidx.compose.ui.unit.Dp) = this.then(Modifier.layout { m, cons ->
    val p = m.measure(cons); val px = d.roundToPx()
    layout(p.width, (p.height - px).coerceAtLeast(0)) { p.place(0, -px) }
})
