package com.hachiwii.oink.ui

import android.os.Build
import androidx.activity.compose.BackHandler
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.tween
import androidx.compose.foundation.ScrollState
import androidx.compose.foundation.interaction.collectIsPressedAsState
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.runtime.*
import kotlinx.coroutines.flow.first
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.draw.drawWithContent
import androidx.compose.ui.draw.rotate
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.RoundRect
import androidx.compose.ui.graphics.BlurEffect
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.ColorFilter
import androidx.compose.ui.graphics.ColorMatrix
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.TileMode
import androidx.compose.ui.graphics.drawscope.clipPath
import androidx.compose.ui.graphics.drawscope.translate
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.graphics.layer.GraphicsLayer
import androidx.compose.ui.graphics.layer.drawLayer
import androidx.compose.ui.graphics.rememberGraphicsLayer
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.layout
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.layout.positionInRoot
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.semantics.clearAndSetSemantics
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.DpSize
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.dp
import androidx.compose.ui.zIndex

/**
 * Navigation and transient UI state of the whole app (MobileApp.jsx). Every page and overlay here is
 * a [Cover]: switching tabs or opening one hides the keyboard first ([dismissKeyboard]).
 */
class UiState(init: UiInit) {
    /** Clears focus, so an open keyboard closes with its animation; the app shell sets it. */
    internal var dismissKeyboard: () -> Unit = {}
    var tab by Cover(init.tab, { old, new -> old != new }) { dismissKeyboard() }
    var addPage by Cover(init.addPage) { dismissKeyboard() }
    var settingsOpen by Cover(init.settings) { dismissKeyboard() }
    var sheetPeer by Cover(init.sheetPeer) { dismissKeyboard() }
    var pairingSheet by Cover(init.pairingSheet) { dismissKeyboard() }
    /** Full-screen media viewer, opened on this item; it covers the tab bar, so it lives at the root. */
    var viewer by Cover(init.viewer) { dismissKeyboard() }
    /** The folder listing the media viewer was opened from (it pages through that level's files); null: the transfer history. */
    var viewerFolder by mutableStateOf<FolderUi?>(null)
    /** The item the media viewer shows now (it pages away from [viewer]); null: [viewer]'s. */
    var viewerPage by mutableStateOf<Long?>(null)
    /** The item under the media viewer, whose bubble is hidden while the viewer covers it. */
    val viewerShown: Long? get() = viewer?.let { viewerPage ?: it.id }
    /**
     * Where each image bubble / stack card currently is (root coordinates), by message id: the
     * preview grows out of it and shrinks back into it. Only composed bubbles have an entry.
     */
    val imageBounds = HashMap<Long, androidx.compose.ui.geometry.Rect>()
}

/** Callbacks into the repository. Every one is optional so design fixtures can run without a core. */
class AppActions(
    /** Mouse / keyboard mode: shared, local, blocked. */
    val deviceInput: (PeerUi, String, String) -> Unit = { _, _, _ -> },
    /** gaming / locked / speaker / mic / paused with the chosen mode. */
    val devicePreference: (PeerUi, String, String) -> Unit = { _, _, _ -> },
    val deviceLevel: (PeerUi, String, Int) -> Unit = { _, _, _ -> },
    val kick: (PeerUi) -> Unit = {},
    /** The member's manual address ("ip" or "ip:port"), to prefill the edit-IP dialog. */
    val memberAddress: suspend (PeerUi) -> String? = { null },
    /** Saves a manual address; returns null, or the reason it was rejected. */
    val editIp: suspend (PeerUi, String, String) -> String? = { _, _, _ -> null },
    val refreshNearby: () -> Unit = {},
    val pair: (NearbyUi) -> Unit = {},
    val accept: (NearbyUi) -> Unit = {},
    val reject: (NearbyUi) -> Unit = {},
    val submitCode: (String) -> Unit = {},
    /** "取消配对", or "关闭" after a failure. */
    val cancelPairing: () -> Unit = {},
    val manualSearch: (String, Int?) -> Unit = { _, _ -> },
    val manualReset: () -> Unit = {},
    val composer: (String) -> Unit = {},
    val sendText: (String?) -> Unit = {},
    val pickImages: (String?) -> Unit = {},
    val pickFiles: (String?) -> Unit = {},
    val takePhoto: (String?) -> Unit = {},
    /** Composer 「文件夹」 (Android, the attachment menu's last item): the system folder picker. */
    val pickFolder: (String?) -> Unit = {},
    val download: (MessageUi) -> Unit = {},
    val openFile: (MessageUi) -> Unit = {},
    /** An image bubble became visible: lazily fetch its thumbnail (the core decides if one is needed). */
    val requestThumbnail: (MessageUi) -> Unit = {},
    /** The media viewer's 保存到相册 (images and videos). */
    val saveImage: (MessageUi) -> Unit = {},
    /** The media viewer's 分享: the system share sheet with the item's file. */
    val shareFile: (MessageUi) -> Unit = {},
    /** The media viewer's 用其他应用打开, for a video or PDF it could not render. */
    val openWith: (MessageUi) -> Unit = {},
    val openFolder: (MessageUi) -> Unit = {},
    val folderEnter: (FolderUi, FolderItemUi) -> Unit = { _, _ -> },
    val folderMore: (FolderUi) -> Unit = {},
    /** 重试 on a folder page whose listing failed. */
    val folderRetry: (FolderUi) -> Unit = {},
    val folderDownload: (FolderUi, FolderItemUi, Boolean) -> Unit = { _, _, _ -> },
    /** A picture's row of a folder listing became visible without a thumbnail: ask its computer for one (the core decides). */
    val folderThumbnail: (FolderUi, FolderItemUi) -> Unit = { _, _ -> },
    /** Long-press 取消发送 / 取消接收 of content still moving; ends it on both sides. */
    val cancelTransfer: (MessageUi) -> Unit = {},
    /** The composer's device was chosen (remembered as the default for next time). */
    val chooseTarget: (String?) -> Unit = {},
    /** Long-press 复制; true when the clipboard now holds the message. */
    val copyMessage: (MessageUi) -> Boolean = { false },
    /** 复制文本 (user 2026-11-12): a rich text message's plain text only. */
    val copyMessageText: (MessageUi) -> Boolean = { false },
    val deleteMessage: (MessageUi) -> Unit = {},
    val loadOlder: () -> Unit = {},
    /** Leaving the transfer page drops a pending "open after download". */
    val leaveTransfer: () -> Unit = {},
    /** Clears the cache; returns null, or the reason it failed. */
    val clearCache: suspend () -> String? = { null },
    /** 退出设备组; returns null, or the reason it failed. */
    val leaveGroup: suspend () -> String? = { null },
    /** 清理传输记录; returns null, or the reason it failed. */
    val clearHistory: suspend () -> String? = { null },
    val refreshCache: () -> Unit = {},
    val privilegeStart: (String) -> Unit = {},
    val privilegeDisable: () -> Unit = {},
    val privilegeReset: () -> Unit = {},
    val openSystem: (String) -> Unit = {},
    val pairingService: (Boolean) -> Unit = {},
    val toggleSpeaker: () -> Unit = {},
    val longPressSpeaker: () -> Unit = {},
    val receiveSpeaker: (() -> Unit)? = null,
    val toggleMic: () -> Unit = {},
    /** null or blank restores the system device name. */
    val setDeviceName: (String?) -> Unit = {},
    /** Start the connection again after it failed to start. */
    val retryConnection: () -> Unit = {},
    /** Opens the control page's session with the core; null while there is no core (design scenes). Keep it stable. */
    val control: com.hachiwii.oink.control.ControlOpener? = null,
)

@Composable
fun OinkApp(model: AppModel, init: UiInit, prefs: Prefs, setPrefs: (Prefs) -> Unit, actions: AppActions, systemDark: Boolean,
            notices: kotlinx.coroutines.flow.Flow<String>? = null) {
    val dark = when (prefs.scheme) { "dark" -> true; "light" -> false; else -> systemDark }
    val colors = remember(dark, prefs.accent) { OinkColors.of(dark, prefs.accent) }
    val ui = remember(init) { UiState(init) }
    val focus = androidx.compose.ui.platform.LocalFocusManager.current
    SideEffect { ui.dismissKeyboard = { focus.clearFocus(force = true) } }
    val control = rememberControlHolder(init)
    val readableViewers = remember { mutableSetOf<Long>() }
    LaunchedEffect(model.messages, ui.viewerShown) {
        val opened = ui.viewer
        if (opened == null) { readableViewers.clear(); return@LaunchedEffect }
        val shown = model.messages.firstOrNull { it.id == ui.viewerShown } ?: opened
        val paths = listOfNotNull(shown.localPath, shown.imagePath, shown.thumbnailPath)
        if (paths.any { java.io.File(it).isFile }) readableViewers.add(shown.id)
        else if (shown.id in readableViewers) {
            // A superseded password can be purged while its decoded image is open.
            // Do not keep the viewer's detached copy after the backing files disappear.
            ui.viewer = null; ui.viewerPage = null; ui.viewerFolder = null
        }
    }
    OinkTheme(colors) {
        val c = Oink.colors
        val toast = rememberToast(init.freeze)
        LaunchedEffect(init) { init.toast?.let { toast.value = it } }
        // Failed operations (with the core's reason) and confirmed results.
        LaunchedEffect(notices) { notices?.collect {
            // Each notice owns its visible interval; buffered notices follow it, never overwrite it.
            toast.value = ToastData(it)
            kotlinx.coroutines.delay(2000)
        } }
        Box(Modifier.fillMaxSize().background(c.bg)) {
            // System bars stay outside the app area; overlays below cover it edge to edge. The
            // keyboard's part of safeDrawing follows its animation through [imeLift], which the
            // tab bar and the composer use too (Keyboard.kt).
            val lift = imeLift()
            // The app area without the keyboard decides the layouts (portrait / landscape / side): a
            // tall IME (Sogou's 超级联想 panel) must never turn portrait into landscape, so the size is
            // taken here, inside the system bars but before the keyboard's lift.
            BoxWithConstraints(Modifier.fillMaxSize().windowInsetsPadding(WindowInsets.systemBars.union(WindowInsets.displayCutout))) {
                val frame = DpSize(maxWidth, maxHeight)
                Box(Modifier.fillMaxSize().imeLiftPadding(lift).consumeWindowInsets(WindowInsets.ime)) {
                    AppBase(model, init, ui, actions, toast, control, frame)
                }
            }
            // The control page's keyboard covers the tab bar, so it is drawn over the whole app.
            ControlKeyboardLayer(control)
            val peer = model.peers.firstOrNull { it.id == ui.sheetPeer }
            // A device that leaves the group (kicked here or elsewhere) takes its sheet out of the
            // composition, often before the close animation reports onClose. The sheet is closed then:
            // a kept id would reopen it as soon as the same device pairs again.
            LaunchedEffect(peer == null) { if (peer == null) ui.sheetPeer = null }
            if (peer != null) DeviceSheet(peer, init.takeIf { it.sheetPeer == peer.id }, actions) { ui.sheetPeer = null }
            PairingLayer(model, init, ui, actions, toast)
            if (ui.settingsOpen) SettingsPage(init, prefs, setPrefs, model, actions, onLeftGroup = {
                // Settings closes onto the (now empty) device page.
                toast.value = ToastData("已退出设备组"); ui.tab = Tab.Devices; ui.addPage = false
            }) { ui.settingsOpen = false }
            ui.viewer?.let { opened ->
                // From a folder listing: that level's files (live, as downloads progress); else the history.
                val folder = ui.viewerFolder?.let { f -> model.folders[f.messageId to f.node] ?: f }
                val download: (MessageUi) -> Unit = if (folder == null) actions.download
                    else { m -> folder.items.firstOrNull { folderItemId(it) == m.id }?.let { actions.folderDownload(folder, it, false) } }
                key(opened.id) {
                    MediaViewer(opened, folder?.let(::folderViewerItems) ?: model.messages, origin = { ui.imageBounds[it] }, onPage = { ui.viewerPage = it },
                        onClosed = { ui.viewer = null; ui.viewerPage = null; ui.viewerFolder = null }, actions = actions, download = download)
                }
            }
            model.share?.let { ShareCardLayer(it) }
            // Above every layer: results and failures stay visible over sheets and settings.
            // Layers that raise their own zIndex (the image viewer uses 10) would otherwise
            // cover it, e.g. 已保存到相册 was hidden behind the viewer.
            Box(Modifier.fillMaxSize().zIndex(100f).windowInsetsPadding(WindowInsets.safeDrawing)) {
                TopToast(toast.value, onAction = { val t = toast.value; toast.value = null; t?.onAction?.invoke() })
            }
        }
    }
}

/**
 * The app area. [frame]: its size without the keyboard (inside the system bars and cutout), which
 * alone picks the layouts; the constraints here are smaller by the keyboard while it is up.
 */
@Composable
private fun AppBase(model: AppModel, init: UiInit, ui: UiState, actions: AppActions, toast: MutableState<ToastData?>, control: ControlHolder, frame: DpSize) {
    val c = Oink.colors
    BackHandler(enabled = ui.addPage) { ui.addPage = false }
    val content = rememberGraphicsLayer()
    var barPos by remember { mutableStateOf(Offset.Zero) }
    var rootPos by remember { mutableStateOf(Offset.Zero) }
    BoxWithConstraints(Modifier.fillMaxSize().onGloballyPositioned { rootPos = it.positionInRoot() }) {
        // Large landscape windows (tablets, unfolded foldables): the tab bar turns 90° onto the side (DECISIONS 23).
        val shape = AppLayout(frame)
        val side = shape.side
        // A phone held sideways (landscape, compact height): the control page puts the tabs into its
        // own header and the tab bar leaves while it is shown (用户 2026-11-12 确认).
        val phoneLand = shape.phoneLandscape
        val onControl = ui.tab == Tab.Control && !ui.addPage
        val controlLayout = when { phoneLand -> ControlLayout.LANDSCAPE; side -> ControlLayout.SIDE; else -> ControlLayout.PORTRAIT }
        // 全屏 (用户 2026-11-12) only where turning to landscape gives the phone's landscape layout.
        val canFullscreen = shape.canFullscreen
        val barGone = phoneLand && onControl
        // The side bar's room goes and comes with the bar (280 ms), read in layout only.
        val sideRoom = animateFloatAsState(if (side && !barGone) 88f else 0f, tween(280, easing = EaseOutExpo), label = "sideRoom")
        Column(Modifier.fillMaxSize().layout { measurable, constraints ->
            val end = sideRoom.value.dp.roundToPx().coerceAtMost(constraints.maxWidth)
            val p = measurable.measure(constraints.copy(minWidth = 0, maxWidth = constraints.maxWidth - end))
            layout(constraints.maxWidth, p.height) { p.place(0, 0) }
        }.drawWithContent {
            content.record { this@drawWithContent.drawContent() }
            drawLayer(content)
        }) {
            // The landscape control page's pad reaches past the app area to the screen edges (用户
            // 2026-11-12): no clipping while it is shown (the neighbouring tab panels lie under it).
            Box(Modifier.weight(1f).fillMaxWidth().then(if (barGone) Modifier else Modifier.clipToBounds())) {
                // The tab pages stay composed under the add-device page, so coming back neither
                // rebuilds them nor loses their state.
                TabTrack(ui.tab, covered = ui.addPage) { tab ->
                    when (tab) {
                        // The devices page's bar is part of the page and slides with it (transfer
                        // and control draw their own headers).
                        Tab.Devices -> Column(Modifier.fillMaxSize()) {
                            TopBar(model, ui, actions)
                            Box(Modifier.weight(1f)) { DevicesPanel(model, init, ui, actions) }
                        }
                        Tab.Transfer -> TransferPage(model, init, ui, actions, active = ui.tab == Tab.Transfer && !ui.addPage)
                        Tab.Control -> ControlPage(control, model, actions, active = ui.tab == Tab.Control && !ui.addPage && !ui.settingsOpen, layout = controlLayout,
                            toast = toast, headerTabs = { HeaderTabs(ui) }, canFullscreen = canFullscreen)
                    }
                }
                // Opaque, and a pointer input of its own: touches on its blank areas never reach the
                // pages under it (hit testing stops at the topmost sibling with pointer input).
                if (ui.addPage) Column(Modifier.fillMaxSize().background(c.bg).pointerInput(Unit) {
                    awaitEachGesture { awaitFirstDown(requireUnconsumed = false) }
                }) {
                    TopBar(model, ui, actions)
                    Box(Modifier.weight(1f)) { AddDevicePage(model, actions, ui) }
                }
            }
        }
        // The bottom bar stays where it is while the system keyboard opens or closes: the app
        // area above it shrinks with the keyboard, so the bar is moved back down by the keyboard's
        // current height and the keyboard slides over it (and off it again) frame by frame. The
        // page's content above the bar stays put until the keyboard reaches it
        // ([spaceAboveKeyboard]), so there is neither a jump nor a blank band. Fully covered, the
        // bar leaves the semantics tree.
        // The control page's own keyboard slides the bar away (280 ms) and back.
        val lift = imeLift()
        val density = LocalDensity.current
        val covered by remember(lift, density) { derivedStateOf { lift.px() >= with(density) { TAB_BAR_REACH.roundToPx() } } }
        val away by animateFloatAsState(if (!side && control.keyboardOpen && ui.tab == Tab.Control) 1f else 0f, tween(280, easing = EaseOutExpo), label = "tabsAway")
        // Landscape control page: the bar slides off (down, or right when on the side) while the header shows the tabs.
        val gone by animateFloatAsState(if (barGone) 1f else 0f, tween(280, easing = EaseOutExpo), label = "tabsGone")
        // The tab switcher and keyboard button form one centred group with an 8 dp gap.
        // The switcher shrinks and the button appears smoothly over 280 ms when entering control.
        val aside by animateFloatAsState(if (controlLayout == ControlLayout.PORTRAIT && onControl) 1f else 0f, tween(280, easing = EaseOutExpo), label = "tabsAside")
        val fullBarWidth = minOf(400.dp, maxWidth - 32.dp)
        val groupLeft = (maxWidth - fullBarWidth) / 2
        val barModifier = if (side) Modifier.align(Alignment.CenterEnd) else {
            val narrow = fullBarWidth - KEYBOARD_FAB - 8.dp
            val w = fullBarWidth + (narrow - fullBarWidth) * aside
            Modifier.align(Alignment.BottomStart).offset { IntOffset(groupLeft.roundToPx(), lift.px()) }.width(w)
        }
        TabBar(ui, content, rootPos, side, barModifier
            .then(if ((!side && covered) || barGone) Modifier.clearAndSetSemantics {} else Modifier)
            .graphicsLayer {
                if (side) translationX = gone * (size.width + 24.dp.toPx())
                else translationY = maxOf(away, gone) * (size.height + 24.dp.toPx())
                alpha = 1f - maxOf(away, gone)
            })
        if (aside > 0f) KeyboardFab(content, rootPos, Modifier.align(Alignment.BottomStart).offset { IntOffset((groupLeft + fullBarWidth - KEYBOARD_FAB).roundToPx(), lift.px()) }
            .then(if (covered || away > 0f) Modifier.clearAndSetSemantics {} else Modifier)
            .graphicsLayer {
                val s = .6f + .4f * aside
                scaleX = s; scaleY = s
                translationY = away * (size.height + 24.dp.toPx())
                alpha = aside * (1f - away)
            }) { control.toggleKeyboard() }
    }
}

/** Below this height a landscape window is a phone held sideways (the control page's full screen). */
internal val PHONE_LANDSCAPE_MAX_HEIGHT = 480.dp

/**
 * How the app is laid out, from its area [frame] (inside the system bars and cutout, **without** the
 * soft keyboard: a tall IME must never make portrait look like landscape, user 2026-11-12).
 * [phoneLandscape]: wider than high and under 480 high, a phone held sideways (the control page's full
 * screen). [side]: wider than high and at least 600 wide, the tab bar on the side (tablets, unfolded
 * foldables, and wide phones held sideways on the other tabs; DECISIONS 23).
 */
internal class AppLayout(frame: DpSize) {
    val phoneLandscape = frame.width > frame.height && frame.height < PHONE_LANDSCAPE_MAX_HEIGHT
    val side = frame.width > frame.height && frame.width >= 600.dp
    /** 全屏 shows only where turning to landscape gives [phoneLandscape]. */
    val canFullscreen = minOf(frame.width, frame.height) < PHONE_LANDSCAPE_MAX_HEIGHT
}

/** Diameter of the portrait control page's keyboard button: as high as the tab bar. */
private val KEYBOARD_FAB = 68.dp

/**
 * The portrait control page's keyboard button beside the narrowed tab bar (用户 2026-11-12 确认):
 * a circle as high as the bar (68), 16 from the right and 10 from the bottom, with the bar's
 * frosted surface, edge and shadow; the PC keyboard glyph 26 in ink. The keyboard opens only from
 * here in portrait (收起键盘 in the control keys closes it).
 */
@Composable
private fun KeyboardFab(content: GraphicsLayer, origin: Offset, modifier: Modifier, onClick: () -> Unit) {
    val c = Oink.colors
    val haptic = rememberHaptics()
    Box(modifier.padding(bottom = 10.dp).size(KEYBOARD_FAB).frosted(content, origin, KEYBOARD_FAB / 2)
        .clip(CircleShape).press(pressedColor = c.press) { haptic(false); onClick() }
        .semantics { contentDescription = "弹出键盘" }, contentAlignment = Alignment.Center) {
        SvgIcon(DesignIconData.pcKeyboard, 26.dp, tint = c.ink, stroke = 1.4f)
    }
}

/**
 * The tabs in the landscape control top bar (用户 2026-11-12 确认): the tab bar's surface, 48 high,
 * radius 24, 4 inside; each tab icon 20 + label 13 side by side, the chosen one on the chip fill in
 * the accent colour.
 */
@Composable
internal fun HeaderTabs(ui: UiState) {
    val c = Oink.colors
    val haptic = rememberHaptics()
    // A hidden landscape top bar takes no touches (they go to the pad under it).
    val live = LocalBarLive.current
    Row(Modifier.height(48.dp).headerSurface(c, 24.dp).padding(4.dp), horizontalArrangement = Arrangement.spacedBy(2.dp), verticalAlignment = Alignment.CenterVertically) {
        Tab.entries.forEach { t ->
            val active = !ui.addPage && ui.tab == t
            val color = if (active) c.accentInk else c.text2
            Row(Modifier.height(38.dp).clip(RoundedCornerShape(19.dp)).background(if (active) c.chip else Color.Transparent)
                .then(if (live) Modifier.press(role = androidx.compose.ui.semantics.Role.Tab) { haptic(false); ui.tab = t; ui.addPage = false } else Modifier)
                .semantics { contentDescription = t.label }.padding(start = 12.dp, end = 14.dp),
                horizontalArrangement = Arrangement.spacedBy(6.dp), verticalAlignment = Alignment.CenterVertically) {
                val stroke = if (active) 2.2f else 1.8f
                when (t) {
                    Tab.Devices -> Lucide(DesignIconData.lucideMonitor, 20.dp, color, stroke)
                    Tab.Transfer -> Lucide(DesignIconData.lucideArrowDownUp, 20.dp, color, stroke)
                    Tab.Control -> Lucide(DesignIconData.tabKeyboardMouse, 20.dp, color, stroke)
                }
                T(t.label, 13f, color, weight = 500, lineHeight = 18.2f)
            }
        }
    }
}

private fun Modifier.clipToBounds() = this.then(Modifier.graphicsLayer { clip = true })

/** How far the floating tab bar reaches above the bottom of the app area: 10 under it, 68 high. */
internal val TAB_BAR_REACH = 78.dp

/** Room a page leaves under its bottom content for the floating tab bar: the bar, plus 8 above it less the composer's own 6. */
internal val TAB_BAR_SPACE = (68 + 10 + 8 - 6).dp

/**
 * Three tab panels side by side; the track slides by the selected index (320 ms, ease-out-expo).
 * Every panel stays composed and the slide only moves their layers (the offset is read while
 * drawing): a switch recomposes nothing per frame and never composes a page in the middle of the
 * slide, and pages keep their state. Only the selected page, and the one it replaces until the
 * slide ends, are in the semantics tree (none while [covered]: the add-device page is over them);
 * both changes happen at the ends of the slide, not during it.
 */
@Composable
private fun TabTrack(tab: Tab, covered: Boolean, panel: @Composable (Tab) -> Unit) {
    val index = animateFloatAsState(tab.ordinal.toFloat(), tween(320, easing = EaseOutExpo), label = "track")
    var shown by remember { mutableStateOf(setOf(tab)) }
    LaunchedEffect(tab) {
        shown = shown + tab
        snapshotFlow { index.value }.first { it == tab.ordinal.toFloat() }
        shown = setOf(tab)
    }
    Box(Modifier.fillMaxSize().then(if (covered) Modifier.alpha(0f) else Modifier)) {
        Tab.entries.forEach { t ->
            key(t) {
                Box(Modifier.fillMaxSize().graphicsLayer { translationX = (t.ordinal - index.value) * size.width }
                    .then(if (t in shown && !covered) Modifier else Modifier.clearAndSetSemantics {})) { panel(t) }
            }
        }
    }
}

@Composable
private fun TopBar(model: AppModel, ui: UiState, actions: AppActions) {
    val c = Oink.colors
    Box(Modifier.fillMaxWidth().height(64.dp).background(c.bg).padding(horizontal = 16.dp)) {
        if (ui.addPage) {
            // Add-device page: an icon-only back in the gear's slot (same 48 target, 23 icon) and
            // the device-info button at the top right.
            IconButton48(Modifier.align(Alignment.CenterStart), "返回", onClick = { ui.addPage = false }) { Lucide(DesignIconData.lucideChevronLeft, 23.dp, c.ink) }
            IconButton48(Modifier.align(Alignment.CenterEnd), "本机信息", onClick = { ui.pairingSheet = "info" }) { Lucide(DesignIconData.lucideInfo, 23.dp, c.ink) }
        } else IconButton48(Modifier.align(Alignment.CenterStart), "打开设置", onClick = { ui.settingsOpen = true }) { Lucide(DesignIconData.lucideSettings, 23.dp, c.ink) }
        if (ui.addPage) T("添加设备", 18f, c.ink, Modifier.align(Alignment.Center), weight = 650, lineHeight = 25.2f)
        // The devices page's actions (this bar is part of that page, so it keeps them while it slides away).
        if (!ui.addPage) Row(Modifier.align(Alignment.CenterEnd), horizontalArrangement = Arrangement.spacedBy(8.dp), verticalAlignment = Alignment.CenterVertically) {
            // This phone's own speaker / microphone. While on (or failed) each draws the PC icon of
            // that device-card state with its badge: speaker accent + receive (作为出口), microphone
            // accent + send (发送), warning when it could not start. Off: the plain glyph in ink.
            IconButton48(Modifier, "扬声器：${if (model.forwardOn) "转发" else if (model.speakerOn) "开" else "关"}", onClick = actions.toggleSpeaker, onLongClick = actions.longPressSpeaker) {
                ToolbarAudioIcon(2, if (model.forwardOn) "forward" else toolbarAudioState(speaker = true, on = model.speakerOn, failed = model.speakerFailed || (!model.speakerOn && model.forwardFailed)))
            }
            IconButton48(Modifier, "麦克风：${if (model.micOn) "开" else "关"}", onClick = actions.toggleMic) {
                ToolbarAudioIcon(3, toolbarAudioState(speaker = false, on = model.micOn, failed = model.micFailed))
            }
            val count = model.requests.size
            IconButton48(Modifier, if (count > 0) "添加设备，$count 个配对请求" else "添加设备", onClick = { ui.addPage = true }) {
                Lucide(DesignIconData.lucidePlus, 25.dp, c.ink)
                if (count > 0) Box(Modifier.align(Alignment.TopEnd).offset(x = (-3).dp, y = 5.dp).heightIn(min = 19.dp).widthIn(min = 19.dp)
                    .clip(RoundedCornerShape(10.dp)).background(c.bad).border(2.dp, c.bg, RoundedCornerShape(10.dp)).padding(horizontal = 5.dp),
                    contentAlignment = Alignment.Center) { T("$count", 11f, c.onAccent, weight = 700, lineHeight = 15f, tabular = true) }
            }
        }
    }
}

@OptIn(androidx.compose.foundation.ExperimentalFoundationApi::class)
@Composable
fun IconButton48(modifier: Modifier, label: String, size: androidx.compose.ui.unit.Dp = 48.dp, onClick: () -> Unit, onLongClick: (() -> Unit)? = null, content: @Composable BoxScope.() -> Unit) {
    val c = Oink.colors
    val source = remember { androidx.compose.foundation.interaction.MutableInteractionSource() }
    val pressed by source.collectIsPressedAsState()
    val input = if (onLongClick == null) Modifier.press(shape = RoundedCornerShape(16.dp), pressedColor = c.chip, onClick = onClick)
        else Modifier.clip(RoundedCornerShape(16.dp)).background(if (pressed) c.chip else Color.Transparent)
            .combinedClickable(interactionSource = source, indication = null, onClick = onClick,
                onLongClick = onLongClick, hapticFeedbackEnabled = false)
    Box(modifier.size(size).then(input).semantics { contentDescription = label },
        contentAlignment = Alignment.Center, content = content)
}

/**
 * The floating tab bar's surface (m-tabs): shadow 0 6 24, frosted where RenderEffect exists (the page
 * content behind it blurred 24 and saturated 150 %, then the bar fill), else the solid fill; a 0.5
 * outer ring in the line colour and a 1 inner edge highlight. [content] is the recorded page layer,
 * [origin] where it sits in the root. Shared by the bar and the control page's keyboard button.
 */
@Composable
private fun Modifier.frosted(content: GraphicsLayer, origin: Offset, radius: androidx.compose.ui.unit.Dp): Modifier {
    var inner by remember { mutableStateOf(Offset.Zero) }
    val c = Oink.colors
    val blur = rememberGraphicsLayer()
    val density = LocalDensity.current
    return onGloballyPositioned { inner = it.positionInRoot() }
        .boxShadow(c.shadow, 6.dp, 24.dp, radius)
        .drawBehind {
            val offset = inner - origin
            val r = radius.toPx()
            val shape = Path().apply { addRoundRect(RoundRect(0f, 0f, size.width, size.height, CornerRadius(r))) }
            if (Build.VERSION.SDK_INT >= 31) {
                // backdrop-filter: blur(24px) saturate(150%) over the content behind the bar.
                val m = 72.dp.roundToPx()
                val sigma = 24.dp.toPx()
                blur.renderEffect = BlurEffect((sigma - .5f) / .57735f, (sigma - .5f) / .57735f, TileMode.Clamp)
                blur.colorFilter = ColorFilter.colorMatrix(ColorMatrix().apply { setToSaturation(1.5f) })
                blur.topLeft = IntOffset(-m, -m)
                blur.record(IntSize(size.width.toInt() + 2 * m, size.height.toInt() + 2 * m)) {
                    drawRect(c.bg)
                    translate(-offset.x + m, -offset.y + m) { drawLayer(content) }
                }
                clipPath(shape) { drawLayer(blur); drawRect(c.tabbar) }
            } else clipPath(shape) { drawRect(c.tabbarSolid) }
            // 0.5 px outer ring in the line colour, 1 px inner edge highlight.
            drawRoundRect(c.line, Offset(-.5f * density.density, -.5f * density.density), androidx.compose.ui.geometry.Size(size.width + density.density, size.height + density.density),
                CornerRadius(r + .5f * density.density), style = androidx.compose.ui.graphics.drawscope.Stroke(.5f * density.density))
            drawRoundRect(c.tabbarEdge, Offset(.5f * density.density, .5f * density.density), androidx.compose.ui.geometry.Size(size.width - density.density, size.height - density.density),
                CornerRadius(r - .5f * density.density), style = androidx.compose.ui.graphics.drawscope.Stroke(density.density))
        }
}

/**
 * Floating capsule tab bar (m-tabs): 68 high, 10 above the bottom, frosted where RenderEffect exists.
 * Horizontal, [modifier] places it and sets its width (full: 16 from each side, at most 400; on the
 * portrait control page narrowed to the left for the keyboard button).
 */
@Composable
private fun TabBar(ui: UiState, content: GraphicsLayer, origin: Offset, vertical: Boolean, modifier: Modifier) {
    val c = Oink.colors
    val haptic = rememberHaptics()
    val index by animateFloatAsState(ui.tab.ordinal.toFloat(), tween(320, easing = EaseOutExpo), label = "pill")
    val pillAlpha by animateFloatAsState(if (ui.addPage) 0f else 1f, tween(320, easing = EaseOutExpo), label = "pillAlpha")
    Box((if (vertical) modifier.padding(end = 10.dp).size(68.dp, 188.dp) else modifier.padding(bottom = 10.dp).height(68.dp))
        .frosted(content, origin, 36.dp).padding(6.dp)) {
        BoxWithConstraints(Modifier.fillMaxSize()) {
            val w = if (vertical) maxWidth else (maxWidth - 8.dp) / 3
            val h = if (vertical) (maxHeight - 8.dp) / 3 else maxHeight
            Box(Modifier.offset(x = if (vertical) 0.dp else (w + 4.dp) * index, y = if (vertical) (h + 4.dp) * index else 0.dp).size(w, h).alpha(pillAlpha).clip(RoundedCornerShape(29.dp)).background(c.chip))
            val buttons: @Composable (Modifier) -> Unit = { cell -> Tab.entries.forEach { t ->
                    val active = !ui.addPage && ui.tab == t
                    val color = if (active) c.accentInk else c.text2
                    Column(cell.clip(RoundedCornerShape(29.dp)).press(role = androidx.compose.ui.semantics.Role.Tab) { if (ui.tab == Tab.Control) haptic(false); ui.tab = t; ui.addPage = false }
                        .semantics { contentDescription = t.label },
                        horizontalAlignment = Alignment.CenterHorizontally, verticalArrangement = Arrangement.spacedBy(2.dp, Alignment.CenterVertically)) {
                        Box(Modifier.size(48.dp, 28.dp), contentAlignment = Alignment.Center) {
                            val stroke = if (active) 2.2f else 1.8f
                            when (t) {
                                Tab.Devices -> Lucide(DesignIconData.lucideMonitor, 23.dp, color, stroke)
                                Tab.Transfer -> Lucide(DesignIconData.lucideArrowDownUp, 23.dp, color, stroke)
                                Tab.Control -> Lucide(DesignIconData.tabKeyboardMouse, 23.dp, color, stroke)
                            }
                        }
                        T(t.label, 11f, color, weight = 500, lineHeight = 15.4f)
                    }
                } }
            if (vertical) Column(Modifier.fillMaxSize(), verticalArrangement = Arrangement.spacedBy(4.dp)) { buttons(Modifier.fillMaxWidth().height(h)) }
            else Row(Modifier.fillMaxSize(), horizontalArrangement = Arrangement.spacedBy(4.dp)) { buttons(Modifier.width(w).fillMaxHeight()) }
        }
    }
}

// ---------- Devices ----------

@Composable
private fun DevicesPanel(model: AppModel, init: UiInit, ui: UiState, actions: AppActions) {
    val scroll = rememberScrollState()
    LaunchedEffect(init) { if (init.scrollToEnd && init.tab == Tab.Devices) { kotlinx.coroutines.delay(60); scroll.scrollTo(scroll.maxValue) } }
    val online = model.peers.filter { !it.offline }
    val offline = model.peers.filter { it.offline }
    if (online.isEmpty() && offline.isEmpty()) {
        Column(Modifier.fillMaxSize()) {
            model.connectionFailure?.let { ConnectionFailure(it, Modifier.padding(start = 20.dp, end = 20.dp, top = 12.dp), actions.retryConnection) }
            Box(Modifier.weight(1f)) { NoDevices { ui.addPage = true } }
        }
        return
    }
    Column(Modifier.fillMaxSize().verticalScroll(scroll).padding(bottom = 92.dp)) {
        Column(Modifier.align(Alignment.CenterHorizontally).widthIn(max = 620.dp).fillMaxWidth().padding(start = 20.dp, end = 20.dp, top = 12.dp, bottom = 28.dp)) {
            model.connectionFailure?.let { ConnectionFailure(it, Modifier.padding(bottom = 20.dp), actions.retryConnection) }
            // A group without devices hides its title and count entirely.
            if (online.isNotEmpty()) DeviceGroup("在线", online) { ui.sheetPeer = it.id }
            if (online.isNotEmpty() && offline.isNotEmpty()) Spacer(Modifier.height(28.dp))
            if (offline.isNotEmpty()) DeviceGroup("离线", offline) { ui.sheetPeer = it.id }
        }
    }
}

/** The connection service could not start its core: the reason and 重试 stay until it runs. */
@Composable
private fun ConnectionFailure(reason: String, modifier: Modifier, retry: () -> Unit) {
    val c = Oink.colors
    Row(modifier.fillMaxWidth().clip(RoundedCornerShape(18.dp)).background(c.badSoft).padding(start = 16.dp, end = 8.dp, top = 10.dp, bottom = 10.dp),
        verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
        T("连接没能启动：$reason", 13f, c.badInk, Modifier.weight(1f), lineHeight = 18.2f)
        PrimaryCapsule("重试", horizontalPadding = 18.dp) { retry() }
    }
}

/** No other device in the group yet: centred empty state leading to the add-device page. */
@Composable
private fun NoDevices(add: () -> Unit) {
    val c = Oink.colors
    Column(Modifier.fillMaxSize().padding(bottom = 92.dp).padding(start = 32.dp, end = 32.dp, top = 24.dp, bottom = 40.dp),
        horizontalAlignment = Alignment.CenterHorizontally, verticalArrangement = Arrangement.Center) {
        Box(Modifier.size(72.dp).clip(CircleShape).background(c.accentSoft), contentAlignment = Alignment.Center) { Lucide(DesignIconData.lucideMonitorSmartphone, 30.dp, c.accentInk) }
        T("还没有其它设备", 17f, c.ink, Modifier.padding(top = 18.dp), weight = 650, lineHeight = 23.8f)
        T("添加一台电脑或手机，就能共享键鼠、剪贴板、文件和音频。", 13.5f, c.muted, Modifier.padding(top = 8.dp).widthIn(max = 280.dp), lineHeight = 21.6f, align = TextAlign.Center)
        PrimaryCapsule("添加设备", Modifier.padding(top = 22.dp), leading = { Lucide(DesignIconData.lucidePlus, 18.dp, c.accentInk, 2.2f) }, horizontalPadding = 26.dp) { add() }
    }
}

@Composable
fun GroupTitle(title: String, count: Int?, modifier: Modifier = Modifier, trailing: @Composable RowScope.() -> Unit = {}) {
    val c = Oink.colors
    Row(modifier.fillMaxWidth().padding(start = 4.dp), horizontalArrangement = Arrangement.spacedBy(8.dp), verticalAlignment = Alignment.CenterVertically) {
        T(title, 13f, c.muted, weight = 600, lineHeight = 18.2f, letterSpacing = .4f)
        if (count != null && count > 0) Box(Modifier.heightIn(min = 20.dp).widthIn(min = 20.dp).clip(RoundedCornerShape(7.dp)).background(c.chip), contentAlignment = Alignment.Center) {
            T("$count", 11f, c.text2, weight = 600, lineHeight = 15.4f, modifier = Modifier.padding(horizontal = 6.dp))
        }
        trailing()
    }
}

@Composable
private fun DeviceGroup(title: String, peers: List<PeerUi>, open: (PeerUi) -> Unit) {
    val c = Oink.colors
    Column {
        // h3: 20 high (the count chip), 10 below.
        GroupTitle(title, peers.size, Modifier.height(20.dp))
        Spacer(Modifier.height(10.dp))
        peers.forEachIndexed { i, peer ->
            Box(Modifier.fillMaxWidth()) {
                DeviceRow(peer) { open(peer) }
                if (i > 0) Box(Modifier.padding(start = 62.dp, end = 16.dp).fillMaxWidth().height(1.dp).background(c.line))
            }
        }
    }
}

@Composable
private fun DeviceRow(peer: PeerUi, onClick: () -> Unit) {
    val c = Oink.colors
    Row(Modifier.fillMaxWidth().heightIn(min = 100.dp).press { onClick() }.padding(horizontal = 16.dp, vertical = 18.dp)
        .semantics { contentDescription = "打开 ${peer.name} 设备控制" }, verticalAlignment = Alignment.CenterVertically) {
        PlatformIcon(peer.platform, faint = peer.offline, modifier = Modifier.offset(x = (-8).dp), privileged = peer.privileged)
        Spacer(Modifier.width(14.dp))
        Column(Modifier.weight(1f)) {
            T(peer.name, 18f, if (peer.offline) c.text2 else c.ink, weight = 650, lineHeight = 25.2f, letterSpacing = -.2f)
            if (peer.offline) {
                Spacer(Modifier.height(8.dp))
                T(if (peer.incompatible) "版本过低，请升级" else "上次在线 · ${peer.lastSeen}前", 13f, c.muted, lineHeight = 18.2f)
            } else {
                Spacer(Modifier.height(10.dp))
                Capabilities(peer)
            }
        }
        if (!peer.offline && peer.latency != null) {
            Spacer(Modifier.width(14.dp))
            Latency(peer.latency)
        }
    }
}

/**
 * This phone's own top-bar speaker / microphone in the device-card state words ([indicator]):
 * on = "outlet" (speaker, receiving as the exit) / "send" (microphone), failed = "unavailable",
 * null while off (the plain glyph without a badge).
 */
internal fun toolbarAudioState(speaker: Boolean, on: Boolean, failed: Boolean): String? = when {
    failed -> "unavailable"
    on -> if (speaker) "outlet" else "send"
    else -> null
}

/** Top-bar audio icon (24, in the PC icon box so it stays put when the badge comes and goes). */
@Composable
private fun ToolbarAudioIcon(index: Int, state: String?) {
    val c = Oink.colors
    val glyph = if (index == 2) speakerGlyph(3) else DesignIconData.pcMicrophone
    if (state == null) { DevIcon(glyph, 24.dp, c.ink); return }
    val (tint, badge) = indicator(index, state)
    DevIcon(glyph, 24.dp, tint, badge, badgeTint = badgeTintFor(badge, tint))
}

/** PC indicators(): state → tone + badge, per capability. */
@Composable
fun indicator(index: Int, state: String): Pair<Color, Badge?> {
    val c = Oink.colors
    if (index < 2) return when (state) {
        "blocked" -> c.bad to Badge.Ban; "local" -> c.faint to null; "unavailable" -> c.warning to Badge.Warning; else -> c.good to Badge.Wifi
    }
    if (state == "unavailable") return c.warning to Badge.Warning
    return if (index == 2) when (state) { "outlet" -> c.accent to Badge.Receive; "forward" -> c.good to Badge.Send; else -> c.faint to null }
    else when (state) { "send" -> c.accent to Badge.Send; "receive" -> c.good to Badge.Receive; else -> c.faint to Badge.Mute }
}

@Composable
private fun Capabilities(peer: PeerUi) {
    val labels = listOf("鼠标", "键盘", "音频", "麦克风")
    val states = listOf(peer.mouse, peer.keyboard, peer.speaker, peer.mic)
    Row(horizontalArrangement = Arrangement.spacedBy(13.dp)) {
        states.forEachIndexed { i, state ->
            if (peer.mobile && i < 2) return@forEachIndexed
            val (tint, badge) = indicator(i, state)
            val glyph = when (i) { 0 -> DesignIconData.pcMouse; 1 -> DesignIconData.pcKeyboard; 2 -> speakerGlyph(3); else -> DesignIconData.pcMicrophone }
            DevIcon(glyph, 18.dp, tint, badge, badgeTint = badgeTintFor(badge, tint), modifier = Modifier.semantics { contentDescription = "${labels[i]}：${if (state == "forward") "转发" else state}" })
        }
    }
}

@Composable
private fun Latency(ms: Double) {
    val c = Oink.colors
    Row(Modifier.semantics { contentDescription = "延迟 ${ms.toInt()} 毫秒" }, verticalAlignment = Alignment.CenterVertically) {
        SignalBars(ms, 20.dp, 18.6.dp)
        Spacer(Modifier.width(2.dp))
        // Value: 3ch wide, right aligned, pulled 8 px towards the bars.
        T("${ms.toInt()}", 12f, c.text2, Modifier.width(22.7.dp), lineHeight = 16.8f, align = TextAlign.End, tabular = true)
        Spacer(Modifier.width(10.dp))
        T("ms", 10f, c.muted, lineHeight = 14f)
    }
}

/** Screen-reader name of the crowned Android badge (native-rewrite.md: 「Android，特权模式已开启」). */
val PRIVILEGED_ANDROID_LABEL: String
    get() = if (java.util.Locale.getDefault().language == "zh") "Android，特权模式已开启" else "Android, privileged mode on"

/**
 * Platform logo in the 32 × 36 slot (MobileApp.jsx PlatformIcon + pc-icons.css placement).
 * [privileged]: an Android device whose privileged mode is on and running wears a gold crown
 * above the unmoved head (two layers, see Icons.kt roles E/G) and reads as「Android，特权模式已开启」.
 */
@Composable
fun PlatformIcon(platform: String, modifier: Modifier = Modifier, faint: Boolean = false, scale: Float = 1f, tintOverride: Color? = null,
                 privileged: Boolean = false) {
    val c = Oink.colors
    val tint = tintOverride ?: if (faint) c.faint else when (platform) { "windows" -> c.osWindows; "android" -> c.osAndroid; else -> c.ink }
    val cut = LocalCutout.current
    val crowned = platform == "android" && privileged
    Box(modifier.size(32.dp * scale, 36.dp * scale).then(if (crowned) Modifier.semantics { contentDescription = PRIVILEGED_ANDROID_LABEL } else Modifier),
        contentAlignment = Alignment.Center) {
        when (platform) {
            "windows" -> SvgIcon(DesignIconData.osWin, 24.dp * scale, tint = tint)
            // The crowned viewBox is 24 units taller above and below (30 × 27.02), so centred
            // in the slot the head sits exactly where the plain badge draws it.
            "android" -> if (crowned) SvgIcon(DesignIconData.androidHeadCrown, 30.dp * scale, (30f * 136f / 151f).dp * scale, tint = tint)
                else SvgIcon(DesignIconData.androidHead, 30.dp * scale, 17.5.dp * scale, tint = tint)
            "mac" -> SvgIcon(DesignIconData.osMac, 28.dp * scale, tint = tint)
            "iphone", "ipad" -> Box(Modifier.size(32.dp * scale, 36.dp * scale)) {
                SvgIcon(DesignIconData.osMac, 28.dp * scale, tint = tint, modifier = Modifier.align(Alignment.Center))
                // Device badge (native-rewrite.md, 2026-11-08): iPhone 12 × 16 at right 2 / bottom 2
                // (x 18–30, y 18–34); iPad a 13 × 16 frame at right 3.5 / bottom 1.5 turned 90°
                // (appears x 14–30, y 21–34). Both sit on the cutout colour with a 1.5 ring, radius 2.5.
                val phone = platform == "iphone"
                Box(Modifier.align(Alignment.BottomEnd).offset(x = (if (phone) -2f else -3.5f).dp * scale, y = (if (phone) -2f else -1.5f).dp * scale)
                    .size((if (phone) 12.dp else 13.dp) * scale, 16.dp * scale)
                    .then(if (phone) Modifier else Modifier.rotate(90f))
                    .drawBehind {
                        val o = 1.5.dp.toPx() * scale
                        drawRoundRect(cut, Offset(-o, -o), androidx.compose.ui.geometry.Size(size.width + 2 * o, size.height + 2 * o), CornerRadius(2.5.dp.toPx() * scale + o))
                    }, contentAlignment = Alignment.Center) {
                    // viewBox 16 × 20 fitted into the box with its aspect ratio kept (as SVG does).
                    SvgIcon(if (phone) DesignIconData.cornerIphone else DesignIconData.cornerIpad, (if (phone) 12.dp else 12.8.dp) * scale, (if (phone) 15.dp else 16.dp) * scale, tint = tint, stroke = 1.8f)
                }
            }
            else -> SvgIcon(DesignIconData.osOther, 24.dp * scale, tint = tint)
        }
    }
}
