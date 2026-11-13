package com.hachiwii.oink.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.stateDescription
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp

/*
 * The system share / 「用 Oink 打开」 entry picks the device first (native-rewrite.md 「分享 / 打开方式
 * 先选设备」 and the 2026-11-11 review; SharePicker.jsx). It opens at once with the devices cached
 * from the last online list (「上次在线」) while the first connection round runs (「正在查找设备…」),
 * then shows the live online list. A tap on a confirmed device sends at once; a cached one is
 * confirmed first (a spinner on its row, the row disabled): online → send, not online → 「X 不在线」
 * and the row leaves the list. There is no selected state and no confirm button.
 */

/** A row of the picker: [confirmed] = online now (「在线」), otherwise only cached (「上次在线」). */
data class PickRow(val peer: PeerUi, val confirmed: Boolean)

/**
 * The rows: while [searching], the cached devices (minus those found not online) plus any online
 * now; afterwards only the online ones. Computers first, in the order given.
 */
fun pickRows(cached: List<PeerUi>, online: List<PeerUi>, searching: Boolean, removed: Set<String>): List<PickRow> {
    val live = online.map { PickRow(it, true) }
    if (!searching) return live
    val ids = online.mapTo(HashSet()) { it.id }
    val rows = live + cached.filter { it.id !in ids && it.id !in removed }.map { PickRow(it, false) }
    return rows.filter { !it.peer.mobile } + rows.filter { it.peer.mobile }
}

/**
 * [summary]: what is being shared (「3 张照片」). [connecting]: the row being confirmed online.
 * [host] draws the mockup's gallery behind the sheet (design scenes); the real entry shows the
 * sharing app there.
 */
@Composable
fun SharePicker(summary: String, searching: Boolean, rows: List<PickRow>, connecting: String?, toast: ToastData?,
                onPick: (PickRow) -> Unit, onDismiss: () -> Unit, host: Boolean = false) {
    val c = Oink.colors
    Box(Modifier.fillMaxSize()) {
        if (host) GalleryHost()
        BoxWithConstraints(Modifier.fillMaxSize()) {
            val tall = maxHeight * .64f
            BottomSheetLayer(onDismiss = onDismiss, minHeight = tall) {
                Column(Modifier.fillMaxWidth().verticalScroll(rememberScrollState()).padding(bottom = 28.dp)) {
                    // pk-head: 发送到 + what.
                    Column(Modifier.padding(top = 6.dp, bottom = 10.dp)) {
                        T(appText("发送到", "Send to"), 20f, c.ink, weight = 650, lineHeight = 26f)
                        T(summary, 13f, c.muted, Modifier.padding(top = 6.dp), lineHeight = 20.15f)
                    }
                    if (searching) Row(Modifier.heightIn(min = 36.dp).padding(start = 4.dp, bottom = 2.dp), horizontalArrangement = Arrangement.spacedBy(8.dp),
                        verticalAlignment = Alignment.CenterVertically) {
                        Dots(c.faint); T(appText("正在查找设备…", "Looking for devices…"), 12.5f, c.faint, lineHeight = 17.5f)
                    }
                    if (!searching && rows.isEmpty()) PickEmpty()
                    else listOf(appText("电脑", "Computers") to rows.filter { !it.peer.mobile }, appText("移动设备", "Mobile devices") to rows.filter { it.peer.mobile })
                        .filter { it.second.isNotEmpty() }.forEach { (title, group) ->
                            GroupTitle(title, group.size, Modifier.heightIn(min = 36.dp))
                            group.forEachIndexed { i, row -> PickDeviceRow(row, first = i == 0, busy = connecting == row.peer.id, enabled = connecting == null) { onPick(row) } }
                            Spacer(Modifier.height(14.dp))
                        }
                }
            }
        }
        TopToast(toast)
    }
}

/** pk-row: 68 high, badge, name 17/650 and 「在线」 (green dot) or 「上次在线」; a Material spinner while confirming. */
@Composable
private fun PickDeviceRow(row: PickRow, first: Boolean, busy: Boolean, enabled: Boolean, onClick: () -> Unit) {
    val c = Oink.colors
    val haptic = rememberHaptics()
    val p = row.peer
    Row(Modifier.fillMaxWidth()
        .then(if (first) Modifier else Modifier.drawBehind { drawRect(c.line, Offset(46.dp.toPx(), 0f), Size(size.width - 46.dp.toPx(), 1.dp.toPx())) })
        .heightIn(min = 68.dp).press(enabled = enabled, pressedColor = c.press) { haptic(false); onClick() }
        .semantics { contentDescription = p.name; if (busy) stateDescription = appText("正在连接 ${p.name}", "Connecting to ${p.name}") }
        .padding(vertical = 10.dp), horizontalArrangement = Arrangement.spacedBy(14.dp), verticalAlignment = Alignment.CenterVertically) {
        PlatformIcon(p.platform, privileged = p.privileged)
        Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(3.dp)) {
            T(p.name, 17f, c.ink, weight = 650, lineHeight = 22.95f, letterSpacing = -.2f)
            if (row.confirmed) Row(horizontalArrangement = Arrangement.spacedBy(6.dp), verticalAlignment = Alignment.CenterVertically) {
                Box(Modifier.size(6.dp).clip(CircleShape).background(c.good))
                T(appText("在线", "Online"), 13f, c.goodInk, lineHeight = 18.2f)
            } else T(appText("上次在线", "Recently online"), 13f, c.faint, lineHeight = 18.2f)
        }
        if (busy) Spinner(22.dp, modifier = Modifier.padding(end = 6.dp))
    }
}

/** ad-empty: 72 circle with the devices glyph, 「没有在线的设备」, 「请在其它设备上打开 Oink」. */
@Composable
private fun PickEmpty() {
    val c = Oink.colors
    Column(Modifier.fillMaxWidth().padding(start = 16.dp, end = 16.dp, top = 36.dp, bottom = 4.dp), horizontalAlignment = Alignment.CenterHorizontally) {
        Box(Modifier.size(72.dp).clip(CircleShape).background(c.accentSoft), contentAlignment = Alignment.Center) {
            Lucide(DesignIconData.lucideMonitorSmartphone, 30.dp, c.accentInk)
        }
        T(appText("没有在线的设备", "No devices online"), 16f, c.ink, Modifier.padding(top = 16.dp), weight = 600, lineHeight = 22.4f)
        T(appText("请在其它设备上打开 Oink", "Open Oink on another device"), 13f, c.muted, Modifier.padding(top = 6.dp).widthIn(max = 280.dp), lineHeight = 20.15f, align = TextAlign.Center)
    }
}

/** The mockup's 「图库」 host behind share scenes (the real entry shows the sharing app). */
@Composable
internal fun GalleryHost() {
    val c = Oink.colors
    Column(Modifier.fillMaxSize().background(c.bg).windowInsetsPadding(WindowInsets.safeDrawing).padding(horizontal = 16.dp)) {
        T(appText("图库", "Photos"), 28f, c.ink, Modifier.padding(start = 4.dp, end = 4.dp, top = 14.dp, bottom = 12.dp), weight = 700, lineHeight = 34f)
        BoxWithConstraints(Modifier.fillMaxWidth()) {
            val cell = (maxWidth - 6.dp) / 3
            Column(verticalArrangement = Arrangement.spacedBy(3.dp)) {
                repeat(6) { r -> Row(horizontalArrangement = Arrangement.spacedBy(3.dp)) { repeat(3) { col ->
                    val i = r * 3 + col
                    Box(Modifier.size(cell).background(if (i % 3 == 1) c.shareTile2 else c.shareTile).then(if (i == 4 || i == 5) Modifier.drawBehind {
                        val w = 3.dp.toPx(); drawRect(c.systemApp, Offset(w / 2, w / 2), Size(size.width - w, size.height - w), style = Stroke(w))
                    } else Modifier))
                } } }
            }
        }
    }
}
