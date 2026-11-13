package com.hachiwii.oink.ui

import android.os.Build
import android.view.HapticFeedbackConstants
import androidx.activity.compose.BackHandler
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.EaseOut
import androidx.compose.animation.core.FastOutSlowInEasing
import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.scaleIn
import androidx.compose.animation.scaleOut
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.gestures.waitForUpOrCancellation
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.interaction.collectIsPressedAsState
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyListState
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicText
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.input.TextFieldLineLimits
import androidx.compose.foundation.text.input.rememberTextFieldState
import androidx.compose.foundation.text.input.selectAll
import androidx.compose.foundation.text.selection.LocalTextSelectionColors
import androidx.compose.foundation.text.selection.TextSelectionColors
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.ripple
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.clipToBounds
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.draw.drawWithContent
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.focus.focusProperties
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.geometry.isSpecified
import androidx.compose.ui.graphics.BlendMode
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.CompositingStrategy
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.Shape
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.graphics.drawscope.clipRect
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.graphics.layer.drawLayer
import androidx.compose.ui.input.nestedscroll.NestedScrollConnection
import androidx.compose.ui.input.nestedscroll.NestedScrollSource
import androidx.compose.ui.input.nestedscroll.nestedScroll
import androidx.compose.ui.input.pointer.PointerEventPass
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.layout.Layout
import androidx.compose.ui.layout.layoutId
import androidx.compose.ui.layout.LayoutCoordinates
import androidx.compose.ui.layout.layout
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.layout.positionInRoot
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.platform.LocalGraphicsContext
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.TextLayoutResult
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.Constraints
import androidx.compose.ui.unit.Density
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.IntRect
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.LayoutDirection
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.lerp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.unit.toSize
import androidx.compose.ui.util.lerp
import androidx.compose.ui.window.Popup
import androidx.compose.ui.window.PopupPositionProvider
import androidx.compose.ui.window.PopupProperties
import androidx.compose.ui.zIndex
import java.util.Calendar
import java.util.Locale
import kotlin.math.abs
import kotlin.math.roundToInt
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/** One row of the transfer list: a single message, or a batch shown as a stack of cards. */
internal sealed interface TransferRow {
    val first: MessageUi
    val last: MessageUi
}
internal data class SingleRow(val message: MessageUi) : TransferRow {
    override val first get() = message
    override val last get() = message
}
internal data class StackRow(val batch: String, val items: List<MessageUi>) : TransferRow {
    override val first get() = items.first()
    override val last get() = items.last()
}

/**
 * Consecutive messages of one batch ([MessageUi.batch], two or more) become a [StackRow] unless the
 * user expanded that batch; everything else is a [SingleRow], in conversation order.
 */
internal fun transferRows(messages: List<MessageUi>, expanded: Set<String>): List<TransferRow> {
    val rows = ArrayList<TransferRow>(messages.size)
    var i = 0
    while (i < messages.size) {
        val key = messages[i].batch
        var j = i + 1
        if (key != null) while (j < messages.size && messages[j].batch == key) j++
        if (key != null && j - i >= 2 && key !in expanded) rows += StackRow(key, messages.subList(i, j).toList())
        else for (k in i until j) rows += SingleRow(messages[k])
        i = j
    }
    return rows
}

/** Own content that got through: sent, with no failure and not ended early. */
internal val MessageUi.sentOk get() = self && sent && failure == null && ended == null

/** Success always identifies the receiving phone, on every completed direct bubble. */
internal val MessageUi.showsSuccessCaption get() = direct && sent && failure == null && ended == null && (self || progress == 100)

/** Long text shows this many lines at first; 查看更多 adds [TEXT_MORE_LINES] each time. */
internal const val TEXT_FOLD_LINES = 10
internal const val TEXT_MORE_LINES = 20

/** Where image bubbles are (for the preview's grow / shrink) and which one the preview covers. */
internal class ImageHooks(val hidden: Long?, val bounds: HashMap<Long, Rect>)

/** A row's date line and whether it continues the previous sender's messages (no avatar / sender line). */
private class RowHead(val label: String?, val continuation: Boolean)

private fun rowHead(rows: List<TransferRow>, i: Int): RowHead {
    val prev = rows.getOrNull(i - 1)?.last
    val m = rows[i].first
    val label = timeLabel(m.createdMs, prev?.createdMs, i == 0)
    val continuation = label == null && prev != null && (if (m.self) prev.self else !prev.self && prev.senderId == m.senderId && prev.proxyId == m.proxyId)
    return RowHead(label, continuation)
}

/** Row top → its bubble (or stack box) top, px: the date line, then the sender line or the continuation pull-up. */
private fun Density.bubbleTop(head: RowHead, stack: Boolean) =
    (if (head.label != null) 14.sp.toPx() + 26.dp.toPx() else 0f) + (if (head.continuation) -14.dp.toPx() else 22.dp.toPx()) + (if (stack) 4.dp.toPx() else 0f)

/** Transfer tab (TransferPage.jsx): message log, composer with proxy picker and attachments. */
@Composable
fun TransferPage(model: AppModel, init: UiInit, ui: UiState, actions: AppActions, active: Boolean) {
    val c = Oink.colors
    // The input pill ends 8 above the floating tab bar (bar 68 + 10 under it, minus the composer's
    // own 6 under the pill). The keyboard slides over the bar (AppBase); once it reaches the
    // composer the composer rides on it, following the keyboard's animation both ways.
    val lift = imeLift()
    // The composer's focus (user 2026-11-12, like iOS): typing when the app went to the background (or to the
    // share sheet) is typing again on return — focus and keyboard back; otherwise the keyboard stays closed.
    val composer = remember { ComposerFocus() }
    ComposerReturn(composer, lift, active)
    // Where to send: the last chosen device (computers and phones; native-rewrite.md 「手机点对点传输」).
    var chosenProxy by remember(init) { mutableStateOf(init.transferProxy) }
    val targets = model.targets ?: transferTargetsOf(model.peers)
    val composing = composerTarget(targets, model.peers, chosenProxy)
    val activeTarget=targets.online.firstOrNull { it.id==targets.activeDevice } ?: fallbackTarget(targets.online)
    val known = composing.target
    val proxyId = known?.id
    val proxyOffline = known == null || known.offline
    // Menus, the folder viewer and the message menu cover the page: opening one hides the keyboard (Keyboard.kt).
    var menu by rememberCover(init) { init.transferMenu }
    var toast by remember(init) { mutableStateOf(init.transferToast) }
    var toastKey by remember { mutableStateOf(0) }
    LaunchedEffect(toastKey) { if (toastKey > 0) { delay(2500); toast = null } }
    fun warn() { toast = offlineSendText(known?.name.orEmpty()); toastKey++ }
    // The button and hardware shortcut must share target selection and send guards.
    fun sendText() {
        if (composing.disabled || model.composer.isBlank()) return
        val computer = known?.takeIf { !it.mobile && !it.offline }?.id
        if (targets.online.isNotEmpty()) actions.sendText(computer) else warn()
    }
    // The chosen device went offline: another online one takes over (computers first), with one hint.
    // The page stays composed under other tabs (TabTrack): the switch and its hint wait until it is shown.
    LaunchedEffect(composing.switchedFrom?.id, known?.id, active) {
        if (!active) return@LaunchedEffect
        val from = composing.switchedFrom ?: return@LaunchedEffect
        chosenProxy = null
        actions.chooseTarget(null)
        toast = switchedText(from.name); toastKey++
    }
    // Folder viewer: a stack of (folder message, node, title); the listing itself comes from the model.
    var folderStack by rememberCover(init) { listOf<Triple<Long, Long, String>>() }
    var deleting by remember { mutableStateOf<MessageUi?>(null) }
    // The message whose long-press menu (复制 / 选择 / 删除) is open, and where it was pressed (window
    // coordinates; unspecified opens it at the bubble's edge).
    var messageMenu by rememberCover(init) { init.transferMessageMenu?.let { it to Offset.Unspecified } }
    // The text message in system text selection (选择).
    var selecting by remember(init) { mutableStateOf<Long?>(null) }
    // Batches the user expanded, and the card on top of each stack (message id).
    var expanded by remember(init) { mutableStateOf(init.transferExpanded) }
    val stackTop = remember(init) { mutableStateMapOf<String, Long>() }
    // The running expand / collapse morph, and the stack whose 展开 pill fades in after collapsing.
    var morph by remember { mutableStateOf<BatchMorph?>(null) }
    var pillEnter by remember { mutableStateOf<String?>(null) }
    val registry = remember { MorphRegistry() }
    val graphics = LocalGraphicsContext.current
    val density = LocalDensity.current
    // Long text unfolded beyond TEXT_FOLD_LINES: message id → visible lines.
    val textLines = remember(init) { mutableStateMapOf<Long, Int>() }
    // Password-marked text the user revealed (显示); hidden again like unfolded text folds again.
    val revealed = remember(init) { mutableStateMapOf<Long, Boolean>() }
    val view = LocalView.current
    /** 复制, or with [plain] 复制文本 (a rich text message's plain text only, user 2026-11-12). */
    fun copy(m: MessageUi, plain: Boolean = false) {
        if (!(if (plain) actions.copyMessageText(m) else actions.copyMessage(m))) return
        view.performHapticFeedback(HapticFeedbackConstants.CONFIRM)
        // Android 13+ confirms clipboard writes itself; a toast of our own would repeat it
        // (developer.android.com, "Copy and paste": avoid duplicate notifications on 13+).
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) { toast = appText("已复制", "Copied"); toastKey++ }
    }
    // Leaving the page (not starting under another tab: the page stays composed there, TabTrack).
    var wasActive by remember { mutableStateOf(active) }
    LaunchedEffect(active) {
        if (!active) { menu = null; messageMenu = null; selecting = null; if (wasActive) actions.leaveTransfer() }
        wasActive = active
    }
    BackHandler(enabled = menu != null) { menu = null }
    // The message menu's popup does not take window focus (see MessageMenu), so back reaches the page.
    BackHandler(enabled = messageMenu != null) { messageMenu = null }
    val list = rememberLazyListState()
    val rows = remember(model.messages, expanded) { transferRows(model.messages, expanded) }
    // Follow the newest message (not older pages being prepended at the top).
    val newest = model.messages.lastOrNull()?.id
    // Opened from a notification: at that message (once it is loaded), otherwise at the newest.
    var focus by remember(init) { mutableStateOf(init.transferFocus) }
    LaunchedEffect(newest, active) {
        if (!active || model.messages.isEmpty()) return@LaunchedEffect
        val at = focus?.let { id -> rows.indexOfFirst { row -> row is SingleRow && row.message.id == id || row is StackRow && row.items.any { it.id == id } } }?.takeIf { it >= 0 }
        // The last row (index rows.size: the spacer comes first); the list's own item count is
        // still 0 before its first layout.
        if (at != null) { list.scrollToItem(at + 1); focus = null } else list.scrollToItem(rows.size)
    }
    // Reaching the top loads the next older page.
    val atTop by remember { derivedStateOf { list.firstVisibleItemIndex == 0 } }
    // Only while shown: under another tab the list has not scrolled to the newest message yet and sits at the top.
    LaunchedEffect(atTop, model.hasOlderMessages, model.messages.size, active) { if (active && atTop && !model.olderLoadFailed && model.hasOlderMessages && model.messages.isNotEmpty()) actions.loadOlder() }

    // Unfolded text folds again once it has been off screen for half a viewport's height of scrolling.
    val textFold = remember(list, textLines, revealed) { TextFoldTracker(list, listOf(textLines, revealed)) }
    // The 收起 button: beside the first completely visible message of the topmost expanded batch on
    // screen (message id), shown 80 ms after scrolling (drag and fling) stops.
    val batchOf by rememberUpdatedState(model.messages.filter { it.batch != null && it.batch in expanded }.associate { it.id to it.batch!! })
    val hideAt by remember(list) {
        derivedStateOf {
            val info = list.layoutInfo
            info.visibleItemsInfo.firstOrNull { item ->
                item.key is Long && batchOf.containsKey(item.key) &&
                    item.offset >= info.viewportStartOffset && item.offset + item.size <= info.viewportEndOffset
            }?.key as Long?
        }
    }
    var scrollSettled by remember { mutableStateOf(true) }
    LaunchedEffect(list.isScrollInProgress) { if (list.isScrollInProgress) scrollSettled = false else { delay(80); scrollSettled = true } }
    fun rowKey(row: TransferRow): Any = when (row) { is SingleRow -> row.message.id; is StackRow -> "stack:${row.batch}:${row.first.id}" }
    fun expand(row: StackRow) {
        if (morph != null) return
        morph = BatchMorph(row.batch, true, row.items, row.items.indexOfFirst { it.id == stackTop[row.batch] }.coerceAtLeast(0), rowKey(row))
    }
    fun collapse(batch: String, at: MessageUi) {
        if (morph != null) return
        val all = model.messages
        val i = all.indexOfFirst { it.id == at.id }.takeIf { it >= 0 } ?: return
        var a = i; while (a > 0 && all[a - 1].batch == batch) a--
        var b = i; while (b + 1 < all.size && all[b + 1].batch == batch) b++
        val items = all.subList(a, b + 1).toList()
        morph = BatchMorph(batch, false, items, i - a, "stack:$batch:${items.first().id}")
    }
    // Expand / collapse (SPEC §4): snapshot the rows, switch the layout keeping the anchor where it
    // was (+1 below: the spacer item at the top), then morph every item between card and message.
    LaunchedEffect(morph) {
        val m = morph ?: return@LaunchedEffect
        try {
            // The rows have drawn into their snapshot layers once this frame starts.
            withFrameNanos { }
            val px = density.density
            val aspects = { x: MessageUi -> itemAspect(x, if (x.kind == "image") cachedImage(x.imagePath, 720) ?: cachedImage(x.thumbnailPath, 720) else null) }
            val now = transferRows(model.messages, expanded)
            val first = now.indexOfFirst { rowKey(it) == m.stackKey || (it is SingleRow && it.message.id == m.items.first().id) } + 1
            val captured = m.capture(list.layoutInfo, registry, first)
            if (m.expanding) {
                val box = if (captured) m.rectIn(list.layoutInfo, registry, m.stackKey, registry.stacks[m.batch]) else null
                val next = expanded + m.batch
                val after = transferRows(model.messages, next)
                val idx = after.indexOfFirst { it is SingleRow && it.message.id == m.items[m.anchor].id }
                if (box != null) {
                    m.startExpand(box.topLeft, aspects, px)
                    m.headerKey = m.items.first().id
                    m.headerFrom = m.beforeY(m.stackKey)
                    m.batchIndex = after.indexOfFirst { it is SingleRow && it.message.id == m.items.first().id } + 1
                }
                expanded = next
                // The top card's message lands where the stack box was.
                if (box != null && idx >= 0) list.requestScrollToItem(idx + 1, -(box.top - m.k - density.bubbleTop(rowHead(after, idx), false)).roundToInt())
                if (box == null) return@LaunchedEffect
                m.capturing = false
                withFrameNanos { }
                m.finishExpand(list.layoutInfo, registry, box.topLeft, aspects, px)
            } else {
                val top = if (captured) m.anchorTop(list.layoutInfo, registry) else null
                if (top != null) m.startCollapse(list.layoutInfo, registry, px)
                stackTop[m.batch] = m.items[m.anchor].id
                val next = expanded - m.batch
                val after = transferRows(model.messages, next)
                val idx = after.indexOfFirst { it is StackRow && it.batch == m.batch }
                m.headerKey = m.stackKey
                m.headerFrom = m.beforeY(m.items.first().id)
                m.batchIndex = idx + 1
                expanded = next
                // The stack box lands where the tapped message was.
                if (top != null && idx >= 0) list.requestScrollToItem(idx + 1, -(top - m.k - density.bubbleTop(rowHead(after, idx), true)).roundToInt())
                if (top == null) return@LaunchedEffect
                m.capturing = false
                withFrameNanos { }
                m.rectIn(list.layoutInfo, registry, m.stackKey, registry.stacks[m.batch])?.let { m.finishCollapse(it.topLeft, aspects, px) }
            }
            m.t.animateTo(m.total.toFloat(), tween(m.total, easing = LinearEasing))
        } finally {
            // The real messages show in the frame the overlay goes away.
            m.release(graphics)
            if (morph === m) morph = null
            if (!m.expanding) pillEnter = m.batch
        }
    }
    // Every file (images included) opens in the media viewer, which fetches what is not here yet
    // and hands what it cannot show to another app (native-rewrite.md 「全屏查看器」); opening it
    // hides the keyboard (ui.viewer is a Cover). Folders open their listing. Only images, files
    // and folders come here (text has no tap action).
    fun openFile(m: MessageUi) {
        if (inViewer(m)) ui.viewer = m
        else if (m.kind == "folder") { actions.openFolder(m); folderStack = listOf(Triple(m.id, 0L, m.name)) }
    }
    // Nothing to show when the original is gone or its PC is offline without a thumbnail.
    fun openImage(m: MessageUi) { if (m.opensViewer) openFile(m) }
    val hooks = ImageHooks(ui.viewerShown, ui.imageBounds)
    // Selection ends with a tap outside the selected bubble (the handles live in their own windows).
    var selectionBounds by remember { mutableStateOf<Rect?>(null) }
    val pageCoordinates = remember { arrayOfNulls<LayoutCoordinates>(1) }

    Box(Modifier.fillMaxSize().background(c.bg).onGloballyPositioned { pageCoordinates[0] = it }
        .pointerInput(selecting) {
            if (selecting == null) return@pointerInput
            awaitEachGesture {
                val down = awaitFirstDown(requireUnconsumed = false, pass = PointerEventPass.Initial)
                val at = pageCoordinates[0]?.takeIf { it.isAttached }?.localToRoot(down.position)
                if (at == null || selectionBounds?.contains(at) != true) selecting = null
            }
        }) {
        Column(Modifier.fillMaxSize().spaceAboveKeyboard(TAB_BAR_SPACE, lift)) {
            // A tap on the list's background closes the keyboard.
            Box(Modifier.weight(1f).fillMaxWidth().onGloballyPositioned { registry.container = it }.dismissKeyboardOnTap()) {
                LazyColumn(Modifier.fillMaxSize().nestedScroll(textFold), state = list, userScrollEnabled = morph == null,
                    contentPadding = PaddingValues(start = 20.dp, end = 20.dp, top = 4.dp, bottom = 18.dp)) {
                    item(key = "top") {
                        val localOnly = model.sharedBoundaryMs?.let { bound -> model.messages.firstOrNull()?.createdMs?.let { it < bound } } == true
                        Column(Modifier.fillMaxWidth().padding(vertical = if (localOnly || model.olderLoadFailed || model.loadingOlder) 12.dp else 1.dp), horizontalAlignment = Alignment.CenterHorizontally) {
                            if (localOnly) {
                                T(appText("更早的记录仅保存在本机", "Earlier records are stored only on this phone"),10f,c.hint)
                            }
                            if (model.olderLoadFailed) {
                                T(appText("加载失败", "Loading failed"), 11f, c.hint)
                                T(appText("加载更早的记录", "Load earlier records"), 12f, c.accentInk, Modifier.clickable(onClick = actions.loadOlder).padding(8.dp))
                            } else if (model.loadingOlder) {
                                Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) { Spinner(14.dp); T(appText("正在加载更早的记录…", "Loading earlier records…"), 11f, c.hint) }
                            }
                        }
                    }
                    itemsIndexed(rows, key = { _, row -> rowKey(row) }) { i, row ->
                        if (model.sharedBoundaryMs?.let { row.first.createdMs >= it && (i == 0 || rows[i-1].last.createdMs < it) } == true) {
                            Column(Modifier.fillMaxWidth().padding(vertical=16.dp),horizontalAlignment=Alignment.CenterHorizontally) {
                                T(appText("已显示最近 100 条共享记录", "Showing the latest 100 shared records"),10f,c.hint)
                                Box(Modifier.fillMaxWidth().padding(vertical=8.dp).height(1.dp).background(c.lineSolid))
                                T(appText("更早的本机记录", "Earlier local records"),10f,c.hint)
                            }
                        }
                        val key = rowKey(row)
                        val head = rowHead(rows, i)
                        val m = row.first
                        val mo = morph
                        // While a morph runs the row slides from where it was; just before, it draws into a snapshot.
                        Column(Modifier.graphicsLayer { translationY = mo?.rowShift(key, list.layoutInfo) ?: 0f }
                            .morphSource(registry.rows, key)
                            .drawWithContent {
                                if (mo != null && mo.capturing && !mo.isBatchRow(key)) {
                                    val layer = mo.layers.getOrPut(key) { graphics.createGraphicsLayer() }
                                    layer.record { this@drawWithContent.drawContent() }
                                    drawLayer(layer)
                                } else drawContent()
                            }) {
                            if (head.label != null) T(head.label, 10f, c.hint, Modifier.fillMaxWidth().padding(top = 2.dp, bottom = 24.dp), lineHeight = 14f, letterSpacing = .3f, align = TextAlign.Center)
                            val continuation = head.continuation
                            val menuAt = messageMenu
                            val morphing = mo != null && !mo.capturing && m.batch != null && m.batch == mo.batch
                            // Images entering the list ask for their PC's thumbnail (lazy: only visible rows compose);
                            // the core decides whether one is needed.
                            for (item in if (row is StackRow) row.items else listOf(m)) {
                                if (item.kind == "image" && !item.self) key(item.id) {
                                    LaunchedEffect(item.preview, active) {
                                        if (active && (item.preview == "placeholder" || item.preview == "failed")) actions.requestThumbnail(item)
                                    }
                                }
                            }
                            when (row) {
                                is SingleRow -> {
                                    val batch = m.batch?.takeIf { it in expanded }
                                    // An expanded batch has one status line, under its last message; trouble shows on its own message.
                                    val tail = batch == null || rows.getOrNull(i + 1)?.first?.batch != batch
                                    val shownSecret = revealed[m.id] == true
                                    // The core never fetches password text by itself: 显示 asks for it.
                                    val secret = if (m.sensitive) Secret(shownSecret, onToggle = {
                                        if (shownSecret) revealed.remove(m.id)
                                        else { revealed[m.id] = true; if (m.textPending && m.progress == null) actions.download(m) }
                                    }) else null
                                    Message(m, continuation, hooks, registry, inBatch = batch != null, statusShown = tail || m.direct || m.failure != null || m.ended != null || !m.sent,
                                        hidden = morphing, secret = secret,
                                        lines = textLines[m.id] ?: TEXT_FOLD_LINES, onMore = { textLines[m.id] = (textLines[m.id] ?: TEXT_FOLD_LINES) + TEXT_MORE_LINES },
                                        selecting = selecting == m.id, onSelectionBounds = { selectionBounds = it }, onSelectionEnd = { if (selecting == m.id) selecting = null },
                                        onImage = { openImage(m) }, onFile = { openFile(m) }, onDownload = { actions.download(m) },
                                        onLongPress = { at -> messageMenu = m.id to at },
                                        side = {
                                            // Revealed password text: 隐藏 beside the bubble like 展开 / 收起 (2026-11-11);
                                            // none while the text is still on its way (the bubble says why).
                                            if (secret != null && shownSecret && !m.textPending) SidePill(m.self, visible = true) {
                                                StackPill(DesignIconData.lucideChevronUp, appText("隐藏", "Hide"), null, appText("隐藏内容", "Hide content")) { secret.onToggle() }
                                            }
                                            if (batch != null) SidePill(m.self, visible = mo == null && scrollSettled && !list.isScrollInProgress && hideAt == m.id) {
                                                StackPill(DesignIconData.lucideChevronsDownUp, appText("收起", "Collapse"), null, appText("收起为一叠", "Collapse into a stack")) { collapse(batch, m) }
                                            }
                                        }) {
                                        if (menuAt != null && menuAt.first == m.id) MessageMenu(m, menuAt.second, onDismiss = { messageMenu = null },
                                            onCancel = { messageMenu = null; actions.cancelTransfer(m) },
                                            onCopy = { messageMenu = null; copy(m) },
                                            onCopyText = if (m.kind == "text" && m.richHtml != null && offersCopy(m, false)) ({ messageMenu = null; copy(m, plain = true) }) else null,
                                            // Hidden password text offers no selection (it would show the text).
                                            onSelect = if (m.kind == "text" && !m.textPending && (!m.sensitive || shownSecret)) ({ messageMenu = null; selecting = m.id }) else null,
                                            onDelete = { messageMenu = null; deleting = m })
                                    }
                                }
                                is StackRow -> {
                                    val topIndex = row.items.indexOfFirst { it.id == stackTop[row.batch] }.coerceAtLeast(0)
                                    val top = row.items[topIndex]
                                    // One status line under the stack: trouble first, then the overall progress; 已发送 only for the newest.
                                    val status = row.items.firstOrNull { it.failure != null || it.ended != null } ?: row.items.firstOrNull { !it.sent }?.let { sending ->
                                        sending.copy(progress = row.items.map { if (it.sent) 100 else it.progress ?: 0 }.average().roundToInt())
                                    } ?: top.takeIf { it.direct }
                                    MessageFrame(top, continuation, status = status?.takeIf { top.self || (top.direct && top.progress == 100) }, hidden = morphing) {
                                        Box {
                                            BatchStack(row.items, topIndex, top.self, hooks, registry, row.batch, hidden = morphing,
                                                enter = pillEnter == row.batch, onEntered = { if (pillEnter == row.batch) pillEnter = null },
                                                onTop = { stackTop[row.batch] = row.items[it].id },
                                                onOpen = { t -> if (t.kind == "image") openImage(t) else openFile(t) },
                                                onLongPress = { t, at -> messageMenu = t.id to at },
                                                onExpand = { expand(row) })
                                            // The stack's menu has no 复制, whatever its top card is (offersCopy, user 2026-11-12).
                                            if (menuAt != null && menuAt.first == top.id) MessageMenu(top, menuAt.second, onDismiss = { messageMenu = null },
                                                onCancel = { messageMenu = null; actions.cancelTransfer(top) },
                                                onCopy = {}, onSelect = null,
                                                onDelete = { messageMenu = null; deleting = top }, collapsedStack = true)
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                morph?.let { MorphOverlay(it, { list.layoutInfo }, Modifier.matchParentSize().clipToBounds()) }
            }
            // Composer: proxy picker + input pill (tr-composer-wrap: 8 16 6).
            Row(Modifier.fillMaxWidth().background(c.bg).padding(start = 16.dp, end = 16.dp, top = 8.dp, bottom = 6.dp), horizontalArrangement = Arrangement.spacedBy(10.dp), verticalAlignment = Alignment.CenterVertically) {
                if (composing.picker) Box(Modifier.size(44.dp, 48.dp).press(shape = RoundedCornerShape(14.dp)) { menu = if (menu == "proxy") null else "proxy" }
                    .semantics { contentDescription = appText("发送对象：", "Send to: ") + (if (composing.noneOnline) appText("没有其它设备在线", "no other device online") else known?.name ?: appText("未选择", "none")) },
                    contentAlignment = Alignment.Center) {
                    // Nothing online: a dark grey ban sign in place of a device badge.
                    if (composing.noneOnline) Box(Modifier.size(32.dp, 36.dp), contentAlignment = Alignment.Center) { Lucide(DesignIconData.lucideBan, 24.dp, c.text2) }
                    else PlatformIcon(known?.platform ?: "mac", scale = .8f, faint = proxyOffline)
                    // Dropdown corner: a 6 × 6 triangle at right 2, bottom 6.
                    Canvas(Modifier.align(Alignment.BottomEnd).padding(end = 2.dp, bottom = 6.dp).size(6.dp)) {
                        drawPath(Path().apply { moveTo(size.width, 0f); lineTo(size.width, size.height); lineTo(0f, size.height); close() }, c.ink)
                    }
                }
                // Disabled (nothing to send to), the pill is a step darker than its usual fill (2026-11-11).
                Row(Modifier.weight(1f).heightIn(min = 48.dp).clip(RoundedCornerShape(26.dp)).background(if (composing.disabled) c.inputDisabled else c.fill)
                    .padding(start = 16.dp, end = 4.dp, top = 2.dp, bottom = 2.dp),
                    verticalAlignment = Alignment.CenterVertically) {
                    BasicTextField(model.composer, actions.composer, Modifier.weight(1f).heightIn(min = 40.dp).focusOnlyByUser(composer).composerSendShortcut { sendText() }, enabled = !composing.disabled,
                        textStyle = textStyle(14f, c.ink, lineHeight = 19.6f), cursorBrush = SolidColor(c.accent), maxLines = 4,
                        decorationBox = { inner -> Box(Modifier.heightIn(min = 40.dp), contentAlignment = Alignment.CenterStart) {
                            if (model.composer.isEmpty()) T(composing.placeholder, 13f, c.hint, lineHeight = 18.2f, ellipsis = true)
                            inner()
                        } })
                    // Text is broadcast to every online device whichever device is chosen (DECISIONS 13); a
                    // chosen computer still carries text too long to broadcast.
                    // Nothing to send to: no send, no attachments.
                    if (composing.disabled) Spacer(Modifier.size(12.dp))
                    else if (model.composer.isNotBlank()) Box(Modifier.size(40.dp).clip(CircleShape).background(c.accentSoft3).press {
                        sendText()
                    }
                        .semantics { contentDescription = "发送" }, contentAlignment = Alignment.Center) { Lucide(DesignIconData.lucideArrowRight, 22.dp, c.accentInk) }
                    else Box(Modifier.size(40.dp).clip(CircleShape).press { if (proxyOffline) warn() else menu = if (menu == "attach") null else "attach" }
                        .semantics { contentDescription = "添加附件" }, contentAlignment = Alignment.Center) { Lucide(DesignIconData.lucidePlus, 23.dp, c.muted) }
                }
            }
        }
        // The message menu's popup lets touches outside it through to the page (it is not
        // focusable): this layer takes them, so a tap outside only closes the menu.
        if (messageMenu != null) Box(Modifier.fillMaxSize().press(pressedColor = null) { messageMenu = null })
        if (menu != null) {
            Box(Modifier.fillMaxSize().press(pressedColor = null) { menu = null })
            CompositionLocalProvider(LocalCutout provides c.elev) {
                Column(Modifier.align(if (menu == "proxy") Alignment.BottomStart else Alignment.BottomEnd).spaceAboveKeyboard(TAB_BAR_SPACE, lift).padding(start = 16.dp, end = 16.dp, bottom = 62.dp + 8.dp)
                    // The device menu holds two groups (computers and mobile devices): room for about eight rows.
                    .width(if (menu == "proxy") 250.dp else 158.dp).menuSurface().heightIn(max = if (menu == "proxy") 440.dp else 248.dp).verticalScroll(rememberScrollState())) {
                    if (menu == "proxy") {
                        MenuRow(onClick = { chosenProxy = null; actions.chooseTarget(null); menu = null; toast = null }) {
                            Box(Modifier.size(27.dp, 30.dp), contentAlignment = Alignment.Center) { activeTarget?.let { PlatformIcon(it.platform, scale = .65f) } }
                            Column(Modifier.weight(1f)) { T(appText("当前活跃", "Currently active"), 13f, c.text2); activeTarget?.let { T(it.name, 10f, c.hint) } }
                            if (chosenProxy == null) Lucide(DesignIconData.lucideCheck, 15.dp, c.accentInk)
                        }
                        // Online devices only, live: 电脑, then 移动设备 (2026-11-11 review).
                        val sections = listOf(appText("电脑", "Computers") to targets.online.filter { !it.mobile },
                            appText("移动设备", "Mobile devices") to targets.online.filter { it.mobile }).filter { it.second.isNotEmpty() }
                        if (sections.isEmpty()) T(appText("没有在线的设备", "No devices online"), 12f, c.faint, Modifier.padding(horizontal = 12.dp, vertical = 8.dp), lineHeight = 16.8f)
                        sections.forEach { (label, peers) ->
                            T(label, 10f, c.hint, Modifier.padding(horizontal = 12.dp, vertical = 8.dp), lineHeight = 14f)
                            peers.forEach { p ->
                                val sel = p.id == chosenProxy
                                MenuRow(onClick = { chosenProxy = p.id; actions.chooseTarget(p.id); menu = null; toast = null }) {
                                    Box(Modifier.size(27.dp, 30.dp), contentAlignment = Alignment.Center) { PlatformIcon(p.platform, scale = .65f) }
                                    T(p.name, 13f, if (sel) c.accentInk else c.text2, Modifier.weight(1f), lineHeight = 18.2f)
                                    if (sel) Lucide(DesignIconData.lucideCheck, 15.dp, c.accentInk)
                                }
                            }
                        }
                    } else attachmentItems.forEach { (icon, label, id) ->
                        MenuRow(onClick = { menu = null; when (id) { "camera" -> actions.takePhoto(proxyId); "gallery" -> actions.pickImages(proxyId); "folder" -> actions.pickFolder(proxyId); else -> actions.pickFiles(proxyId) } }) {
                            Lucide(icon, 20.dp, c.text2)
                            T(label, 13f, c.text2, Modifier.weight(1f), lineHeight = 18.2f)
                        }
                    }
                }
            }
        }
        if (toast != null) T(toast!!, 12f, c.toastInk, Modifier.align(Alignment.TopCenter).padding(top = 8.dp).widthIn(max = 369.dp)
            .boxShadow(c.shadow, 4.dp, 16.dp, 18.dp).clip(RoundedCornerShape(18.dp)).background(c.toast).padding(horizontal = 16.dp, vertical = 11.dp), lineHeight = 16.8f, align = TextAlign.Center)
        val top = folderStack.lastOrNull()
        val folder = init.folder?.takeIf { top == null } ?: top?.let { (id, node, name) ->
            (model.folders[id to node] ?: FolderUi(name, emptyList(), id, node, loading = true)).copy(name = name)
        }
        folder?.let { f ->
            FolderPage(f, onBack = { folderStack = folderStack.dropLast(1) },
                onItem = { item ->
                    when {
                        item.kind == "folder" -> { actions.folderEnter(f, item); folderStack = folderStack + Triple(f.messageId, item.entryId, item.name) }
                        // Files open in the media viewer, paging through this folder level's files.
                        else -> { ui.viewerFolder = f; ui.viewer = folderViewerItem(item) }
                    }
                },
                onDownload = { item -> actions.folderDownload(f, item, false) },
                onMore = { actions.folderMore(f) }, onRetry = { actions.folderRetry(f) },
                onThumbnail = { item -> actions.folderThumbnail(f, item) })
        }
        deleting?.let { m ->
            OinkDialog("删除这条消息？", { deleting = null }, listOf(DialogAction("取消") { deleting = null }, DialogAction("删除", danger = true) {
                deleting = null; actions.deleteMessage(m)
            })) { DialogBody(if (m.kind == "text") "删除后无法恢复。" else "已下载的文件会一起删除，删除后无法恢复。") }
        }
    }
}

/**
 * Folds unfolded long text and hides revealed password text again (drops the message from every
 * map in [maps]) once it has been off screen for more than half a viewport's height of scrolling
 * (SPEC §1.5). Rows off screen are not composed, so the list keeps its place and the bubble
 * changes without animation. Scrolling is measured from the list's own nested scroll; a message
 * coming back on screen before that keeps its state.
 */
private class TextFoldTracker(private val list: androidx.compose.foundation.lazy.LazyListState,
                              private val maps: List<MutableMap<Long, *>>) : NestedScrollConnection {
    private var scrolled = 0f
    private val leftAt = HashMap<Long, Float>()
    override fun onPostScroll(consumed: Offset, available: Offset, source: NestedScrollSource): Offset {
        scrolled += consumed.y
        if (maps.all { it.isEmpty() }) return Offset.Zero
        val info = list.layoutInfo
        val visible = info.visibleItemsInfo.mapTo(HashSet()) { it.key }
        val limit = (info.viewportEndOffset - info.viewportStartOffset) / 2f
        for (id in maps.flatMapTo(HashSet()) { it.keys }) {
            if (id in visible) { leftAt.remove(id); continue }
            val left = leftAt.getOrPut(id) { scrolled }
            if (abs(scrolled - left) > limit) { maps.forEach { it.remove(id) }; leftAt.remove(id) }
        }
        return Offset.Zero
    }
}

/** Date line above a message: the first one, and whenever 5 minutes passed since the previous. */
private fun timeLabel(ms: Long, previous: Long?, first: Boolean): String? {
    if (ms <= 0) return null
    if (!first && previous != null && ms - previous < 5 * 60_000) return null
    val now = Calendar.getInstance()
    val at = Calendar.getInstance().apply { timeInMillis = ms }
    val time = String.format(Locale.US, "%02d:%02d", at.get(Calendar.HOUR_OF_DAY), at.get(Calendar.MINUTE))
    val days = run {
        val a = Calendar.getInstance().apply { timeInMillis = ms; set(Calendar.HOUR_OF_DAY, 0); set(Calendar.MINUTE, 0); set(Calendar.SECOND, 0); set(Calendar.MILLISECOND, 0) }
        val b = (now.clone() as Calendar).apply { set(Calendar.HOUR_OF_DAY, 0); set(Calendar.MINUTE, 0); set(Calendar.SECOND, 0); set(Calendar.MILLISECOND, 0) }
        ((b.timeInMillis - a.timeInMillis) / 86_400_000L).toInt()
    }
    return when {
        days == 0 -> "今天 · $time"
        days == 1 -> "昨天 · $time"
        at.get(Calendar.YEAR) == now.get(Calendar.YEAR) -> "${at.get(Calendar.MONTH) + 1}月${at.get(Calendar.DAY_OF_MONTH)}日 · $time"
        else -> "${at.get(Calendar.YEAR)}年${at.get(Calendar.MONTH) + 1}月${at.get(Calendar.DAY_OF_MONTH)}日 · $time"
    }
}

/** The page's menu surface (proxy / attach and the message menu): same shadow, shape and border. */
@Composable
private fun Modifier.menuSurface(): Modifier {
    val c = Oink.colors
    return boxShadow(c.shadow, 8.dp, 30.dp, 20.dp).clip(RoundedCornerShape(20.dp)).background(c.elev)
        .border(1.dp, c.border, RoundedCornerShape(20.dp)).padding(6.dp)
}

/** The composer's attachment (+) menu, top to bottom; 「文件夹」 is last (user 2026-11-11). */
private val attachmentItems = listOf(Triple(DesignIconData.lucideCamera, "拍摄", "camera"), Triple(DesignIconData.lucideImage, "图库", "gallery"),
    Triple(DesignIconData.lucideFile, "文件", "file"), Triple(DesignIconData.lucideFolder, "文件夹", "folder"))

@Composable
private fun MenuRow(onClick: () -> Unit, content: @Composable RowScope.() -> Unit) {
    Row(Modifier.fillMaxWidth().heightIn(min = 48.dp).clip(RoundedCornerShape(14.dp)).press(pressedColor = Oink.colors.press) { onClick() }.padding(horizontal = 12.dp, vertical = 8.dp),
        horizontalArrangement = Arrangement.spacedBy(12.dp), verticalAlignment = Alignment.CenterVertically, content = content)
}

// The popup window leaves room around the menu for its shadow (y 8, blur 30).
private val MENU_PAD_X = 24.dp
private val MENU_PAD_TOP = 16.dp
private val MENU_PAD_BOTTOM = 40.dp

/**
 * Whether the long-press menu offers 复制: a single message or an item of an expanded batch with
 * content to paste; a collapsed stack never, not even when its top card could be copied
 * (user 2026-11-12: 未展开的聚合长按不能复制).
 */
internal fun offersCopy(m: MessageUi, collapsedStack: Boolean): Boolean = !collapsedStack && m.copyable

/**
 * Long-press menu of a message, opened where the finger pressed (window coordinates; unspecified:
 * under the bubble), drawn like the page's own proxy / attach menus. 复制 comes first, as in the
 * system menus, and only for content that can be pasted ([MessageUi.copyable]) outside a collapsed
 * stack ([offersCopy]); 选择 (text only) starts system text selection; 删除 asks before deleting.
 *
 * The popup window is not focusable. A focusable one takes window focus from the activity; the
 * system then hides the composer's keyboard on behalf of the new window, and the activity's IME
 * inset drops to 0 in one frame, without the closing animation (reported on a Phone). The
 * composer also kept Compose focus, so the keyboard could come back after the menu. Instead opening
 * the menu clears focus ([Cover]) and the keyboard closes with its animation; back and taps
 * outside the menu are handled by the page (TransferPage).
 */
@Composable
private fun MessageMenu(m: MessageUi, at: Offset, onDismiss: () -> Unit, onCancel: () -> Unit, onCopy: () -> Unit, onSelect: (() -> Unit)?, onDelete: () -> Unit,
                        collapsedStack: Boolean = false, onCopyText: (() -> Unit)? = null) {
    val c = Oink.colors
    val density = LocalDensity.current
    val position = remember(at, density, m.self) { MenuPosition(at, density, m.self) }
    Popup(popupPositionProvider = position, onDismissRequest = onDismiss, properties = PopupProperties(focusable = false)) {
        Box(Modifier.padding(start = MENU_PAD_X, end = MENU_PAD_X, top = MENU_PAD_TOP, bottom = MENU_PAD_BOTTOM)) {
            Column(Modifier.width(IntrinsicSize.Max).widthIn(min = 158.dp).menuSurface()) {
                when {
                    // Own content still on its way: only 取消发送, in red; cancelling removes it on both sides (2026-11-11 review).
                    m.sendingNow -> MenuRow(onCancel) { T(appText("取消发送", "Cancel sending"), 13f, c.badInk, lineHeight = 18.2f) }
                    // Content arriving straight from a phone: 取消接收 (both sides show 已取消), then 删除.
                    m.receivingNow -> {
                        MenuRow(onCancel) { T(appText("取消接收", "Cancel receiving"), 13f, c.text2, lineHeight = 18.2f) }
                        MenuRow(onDelete) { T(appText("删除", "Delete"), 13f, c.badInk, lineHeight = 18.2f) }
                    }
                    else -> {
                        if (offersCopy(m, collapsedStack)) MenuRow(onCopy) { T(appText("复制", "Copy"), 13f, c.text2, lineHeight = 18.2f) }
                        // Rich text (user 2026-11-12): 复制 writes it with its plain text, 复制文本 the plain text only.
                        if (onCopyText != null) MenuRow(onCopyText) { T(appText("复制文本", "Copy text"), 13f, c.text2, lineHeight = 18.2f) }
                        if (onSelect != null) MenuRow(onSelect) { T(appText("选择", "Select"), 13f, c.text2, lineHeight = 18.2f) }
                        MenuRow(onDelete) { T(appText("删除", "Delete"), 13f, c.badInk, lineHeight = 18.2f) }
                    }
                }
            }
        }
    }
}

/** Puts the menu's top corner at the press point (its right corner for own messages), flipped above when it would not fit below. */
private class MenuPosition(private val at: Offset, private val density: Density, private val alignEnd: Boolean) : PopupPositionProvider {
    override fun calculatePosition(anchorBounds: IntRect, windowSize: IntSize, layoutDirection: LayoutDirection, popupContentSize: IntSize): IntOffset = with(density) {
        val padX = MENU_PAD_X.roundToPx(); val padTop = MENU_PAD_TOP.roundToPx(); val padBottom = MENU_PAD_BOTTOM.roundToPx()
        val gap = 8.dp.roundToPx(); val margin = 8.dp.roundToPx()
        val width = popupContentSize.width - 2 * padX
        val height = popupContentSize.height - padTop - padBottom
        val x = if (at.isSpecified) at.x.roundToInt() else if (alignEnd) anchorBounds.right else anchorBounds.left
        val y = if (at.isSpecified) at.y.roundToInt() else anchorBounds.bottom
        val left = (if (alignEnd) x - width else x).coerceIn(margin, (windowSize.width - margin - width).coerceAtLeast(margin))
        var top = y + gap
        if (top + height > windowSize.height - margin) top = y - gap - height
        top = top.coerceIn(margin, (windowSize.height - margin - height).coerceAtLeast(margin))
        IntOffset(left - padX, top - padTop)
    }
}

/** Text in the app language: Chinese when the locale is Chinese, English otherwise (as on iOS). */
internal fun appText(zh: String, en: String) = if (Locale.getDefault().language == "zh") zh else en

/**
 * Tap plus long press (the message menu), without ripple like [press]. The long press reports where
 * it happened in window coordinates, so the menu opens at the finger.
 */
@OptIn(ExperimentalFoundationApi::class)
@Composable
internal fun Modifier.pressLong(enabled: Boolean = true, onClick: () -> Unit, onLongClick: (Offset) -> Unit): Modifier {
    val source = remember { MutableInteractionSource() }
    val haptic = rememberHaptics()
    val down = remember { floatArrayOf(Float.NaN, Float.NaN) }
    // Plain holders: read only when the long press fires, never during composition.
    val coordinates = remember { arrayOfNulls<LayoutCoordinates>(1) }
    return onGloballyPositioned { coordinates[0] = it }
        .pointerInput(Unit) {
            awaitEachGesture {
                val p = awaitFirstDown(requireUnconsumed = false, pass = PointerEventPass.Initial).position
                down[0] = p.x; down[1] = p.y
            }
        }
        .combinedClickable(source, indication = null, enabled = enabled, onClick = onClick, onLongClick = {
            haptic(true)
            val local = Offset(down[0], down[1])
            onLongClick(coordinates[0]?.takeIf { it.isAttached && local.isSpecified && !local.x.isNaN() }?.localToWindow(local) ?: Offset.Unspecified)
        })
}

/** Registers an image's place for the preview transition and hides it while the preview covers it. */
@Composable
internal fun Modifier.previewSource(id: Long, hooks: ImageHooks): Modifier {
    DisposableEffect(id, hooks.bounds) { onDispose { hooks.bounds.remove(id) } }
    return onGloballyPositioned { hooks.bounds[id] = Rect(it.positionInRoot(), it.size.toSize()) }
        .alpha(if (hooks.hidden == id) 0f else 1f)
}

private val bubbleOther = RoundedCornerShape(4.dp, 18.dp, 18.dp, 18.dp)
private val bubbleSelf = RoundedCornerShape(18.dp, 4.dp, 18.dp, 18.dp)

/**
 * Avatar, sender line and own-message status around a bubble (.tr-message). [hidden] hides the
 * bubble and the status (a morph's overlay stands in for them) but keeps the sender line.
 */
@Composable
private fun MessageFrame(m: MessageUi, continuation: Boolean, status: MessageUi?, hidden: Boolean = false, content: @Composable () -> Unit) {
    // .tr-message: gap 12, 24 below; a continuation pulls up 14 and drops avatar + meta.
    Row(Modifier.fillMaxWidth().then(if (continuation) Modifier.pullUp(14.dp) else Modifier).padding(bottom = 24.dp),
        horizontalArrangement = if (m.self) Arrangement.End else Arrangement.Start) {
        if (!m.self) { Avatar(m.senderPlatform, continuation, m.senderPrivileged); Spacer(Modifier.width(12.dp)) }
        BoxWithConstraints(Modifier.weight(1f, fill = false)) {
            Column(Modifier.widthIn(max = maxWidth), horizontalAlignment = if (m.self) Alignment.End else Alignment.Start) {
                if (!continuation) Meta(m)
                Column(Modifier.alpha(if (hidden) 0f else 1f), horizontalAlignment = if (m.self) Alignment.End else Alignment.Start) {
                    content()
                    if (status != null) SendStatus(status)
                }
            }
        }
        if (m.self) { Spacer(Modifier.width(12.dp)); Avatar(m.senderPlatform, continuation, m.senderPrivileged) }
    }
}

/** Own message: 发送失败 / 发送进度 / 已发送 under the bubble. */
@Composable
private fun SendStatus(m: MessageUi) {
    val c = Oink.colors
    Row(Modifier.padding(top = 5.dp).heightIn(min = 14.dp), verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(3.dp)) {
        when {
            // A phone-to-phone transfer that ended early, on either side.
            m.ended == "cancelled" -> { Lucide(DesignIconData.lucideX, 11.dp, c.hint); T(appText("已取消", "Cancelled"), 9f, c.hint, lineHeight = 12.6f) }
            m.ended == "failed" -> {
                Lucide(DesignIconData.lucideAlertCircle, 11.dp, c.badInk)
                // The other side going away, or another reason (storage full, interrupted).
                T(when {
                    m.failure != null && m.self -> appText("发送失败：${m.failure}", "Send failed: ${m.failure}")
                    m.failure != null -> appText("接收失败：${m.failure}", "Receive failed: ${m.failure}")
                    m.self -> appText("对方已断开，发送失败", "Disconnected, send failed")
                    else -> appText("对方已断开，接收失败", "Disconnected, receive failed")
                }, 9f, c.badInk, lineHeight = 12.6f)
            }
            m.failure != null -> T("发送失败：${m.failure}", 9f, c.badInk, lineHeight = 12.6f)
            !m.sent -> T(when {
                m.progress == null || m.kind == "text" -> appText("正在发送…", "Sending…")
                m.toName != null -> appText("正在发送给 ${m.toName} · ${m.progress}%", "Sending to ${m.toName} · ${m.progress}%")
                else -> appText("发送进度 ${m.progress}%", "Sending ${m.progress}%")
            }, 9f, c.hint, lineHeight = 12.6f, tabular = true)
            else -> { Lucide(DesignIconData.lucideCheck, 11.dp, c.hint); T(m.toName?.let { appText("已发送给 $it", "Sent to $it") } ?: "", 9f, c.hint, lineHeight = 12.6f) }
        }
    }
}

@Composable
private fun Message(m: MessageUi, continuation: Boolean, hooks: ImageHooks, registry: MorphRegistry, inBatch: Boolean, statusShown: Boolean,
                    hidden: Boolean, secret: Secret?,
                    lines: Int, onMore: () -> Unit, selecting: Boolean, onSelectionBounds: (Rect) -> Unit, onSelectionEnd: () -> Unit,
                    onImage: () -> Unit, onFile: () -> Unit, onDownload: () -> Unit, onLongPress: (Offset) -> Unit,
                    side: @Composable BoxScope.() -> Unit, menu: @Composable () -> Unit) {
    // Progress and failures remain per message. Every successful direct bubble keeps its recipient.
    MessageFrame(m, continuation, status = m.takeIf {
        (statusShown && it.self && (it.kind != "text" || it.direct || it.failure != null || !it.sent) && (!it.sentOk || it.showsSuccessCaption)) || (!it.self && (it.ended != null || (it.direct && it.progress == 100)))
    }, hidden = hidden) {
        // The long-press menu and the 收起 button are placed from the bubble.
        Box {
            Bubble(m, hooks, registry, inBatch, secret, lines, onMore, selecting, onSelectionBounds, onSelectionEnd, onImage, onFile, onDownload, onLongPress)
            side()
            menu()
        }
    }
}

/**
 * A pill beside its anchor on the open side (left of own messages, right of received ones), 12
 * from it, that takes no layout space, so showing and hiding it never moves the bubble. It hides
 * in 120 ms and shows in 150 ms with a slight overshoot (SPEC §3.1; shortened from 180 ms with the
 * 80 ms settle delay, 2026-11-11).
 */
@Composable
private fun BoxScope.SidePill(self: Boolean, visible: Boolean, content: @Composable () -> Unit) {
    AnimatedVisibility(visible, Modifier.align(if (self) Alignment.CenterStart else Alignment.CenterEnd).layout { measurable, constraints ->
        val p = measurable.measure(constraints.copy(minWidth = 0, minHeight = 0, maxWidth = Constraints.Infinity, maxHeight = Constraints.Infinity))
        // The pill's touch area reaches 4 beyond it.
        val gap = 8.dp.roundToPx()
        layout(0, 0) { p.place(if (self) -p.width - gap else gap, -p.height / 2) }
    }, enter = fadeIn(tween(150, easing = EaseOut)) + scaleIn(tween(150, easing = EasePillIn), initialScale = .9f),
        exit = fadeOut(tween(120, easing = EasePillOut)) + scaleOut(tween(120, easing = EasePillOut), targetScale = .9f)) {
        content()
    }
}

/** .tr-avatar: the 32 × 36 platform slot scaled .85 from its top centre, 20 down. */
@Composable
private fun Avatar(platform: String, hidden: Boolean, privileged: Boolean) {
    Box(Modifier.padding(top = 20.dp).size(32.dp, 36.dp), contentAlignment = Alignment.TopCenter) {
        if (!hidden) PlatformIcon(platform, scale = .85f, privileged = privileged)
    }
}

@Composable
private fun Meta(m: MessageUi) {
    val c = Oink.colors
    Row(Modifier.padding(bottom = 6.dp).height(16.dp), horizontalArrangement = Arrangement.spacedBy(6.dp), verticalAlignment = Alignment.CenterVertically) {
        T(m.senderName, 10f, c.muted, Modifier.widthIn(max = 220.dp), lineHeight = 16f, ellipsis = true)
        if (m.proxyName != null) Row(horizontalArrangement = Arrangement.spacedBy(3.dp), verticalAlignment = Alignment.CenterVertically) {
            T("经", 10f, c.hint, lineHeight = 16f)
            Box(Modifier.size(12.dp), contentAlignment = Alignment.Center) {
                when (m.proxyPlatform) {
                    "windows" -> SvgIcon(DesignIconData.osWin, 11.dp, tint = c.osWindows)
                    "android" -> SvgIcon(DesignIconData.androidHead, 12.dp, 7.dp, tint = c.osAndroid)
                    else -> SvgIcon(DesignIconData.osMac, 11.dp, tint = c.muted)
                }
            }
            T(m.proxyName, 10f, c.hint, Modifier.widthIn(max = 120.dp), lineHeight = 16f, ellipsis = true)
            Spacer(Modifier.width(0.dp))
            Box(Modifier.size(5.dp).clip(CircleShape).background(if (m.proxyOnline) c.good else c.dotOff))
        } else Box(Modifier.size(5.dp).clip(CircleShape).background(if (m.senderOnline) c.good else c.dotOff))
    }
}

/**
 * The bubble of one message; files and folders are cards ([FileMessageCard]). Items of an expanded batch
 * ([inBatch]) use the batch's message look (SPEC §3): pictures contain-fitted in 220 × 280, file
 * cards 220 wide; they register where they are for the expand / collapse morph.
 */
@Composable
private fun Bubble(m: MessageUi, hooks: ImageHooks, registry: MorphRegistry, inBatch: Boolean, secret: Secret?, lines: Int, onMore: () -> Unit, selecting: Boolean,
                   onSelectionBounds: (Rect) -> Unit, onSelectionEnd: () -> Unit, onImage: () -> Unit, onFile: () -> Unit, onDownload: () -> Unit,
                   onLongPress: (Offset) -> Unit) {
    val c = Oink.colors
    val shape = if (m.self) bubbleSelf else bubbleOther
    val fill = if (m.self) c.accentBubble else c.fill
    // Rich text (user 2026-11-12), parsed once per markup; nothing visible left → plain text.
    val rich = remember(m.richHtml) { m.richHtml?.let(::parseRich)?.takeIf { it.blocks.isNotEmpty() } }
    when {
        m.kind == "text" && selecting -> Box(Modifier.background(fill, shape)
            .onGloballyPositioned { onSelectionBounds(Rect(it.positionInRoot(), it.size.toSize())) }
            .padding(horizontal = 14.dp, vertical = 12.dp)) {
            SelectableText(m.text, c.bubbleInk, onSelectionEnd)
        }
        // A received text without its body says why (pending), in the secondary colour.
        m.kind == "text" -> TextBubble(m.text, fill, shape, lines, onMore, onLongPress, secret, pending = m.textPending, rich = rich)
        m.kind == "image" && inBatch -> {
            val image = rememberImage(m.imagePath, 720)
            val thumbnail = rememberImage(m.thumbnailPath.takeIf { image == null }, 720)
            val size = contain(itemAspect(m, image ?: thumbnail), BATCH_MEDIA_WF, BATCH_MEDIA_HF)
            Box(Modifier.size(size.width.dp, size.height.dp).morphSource(registry.bubbles, m.id).previewSource(m.id, hooks)
                .pressLong(onClick = onImage, onLongClick = onLongPress).semantics { contentDescription = imageDescription(m) }
                .clip(shape).background(c.fill).mediaEdge(c, shape, shown = image != null || thumbnail != null), contentAlignment = Alignment.Center) {
                MediaContent(m, image, thumbnail, 36.dp)
            }
        }
        // Received and own pictures are 220 wide (native-rewrite.md 「图片尺寸」); own ones sit
        // against the right edge like every own bubble.
        m.kind == "image" -> Box(Modifier.width(220.dp).pressLong(onClick = onImage, onLongClick = onLongPress).semantics { contentDescription = imageDescription(m) },
            contentAlignment = if (m.self) Alignment.TopEnd else Alignment.TopStart) {
            val image = rememberImage(m.imagePath, 720)
            val thumbnail = rememberImage(m.thumbnailPath.takeIf { image == null }, 720)
            // The image's own proportions once its thumbnail tells them (own pictures: the file's),
            // within chat-friendly bounds. A received picture keeps its placeholder's shape after it
            // arrives, so the bubble never changes size.
            val aspect = (m.aspect ?: image?.takeIf { m.self }?.let { it.width.toFloat() / it.height.coerceAtLeast(1) } ?: (260f / 146f)).coerceIn(.75f, 2f)
            // The picture edge on the original and the thumbnail, not on the placeholders (user 2026-11-12).
            if (image == null && thumbnail != null) Box(Modifier.fillMaxWidth().aspectRatio(aspect).previewSource(m.id, hooks).clip(shape).background(c.fill)
                .mediaEdge(c, shape), contentAlignment = Alignment.Center) {
                Image(thumbnail, null, Modifier.fillMaxSize(), contentScale = ContentScale.Crop)
                if (m.progress != null && m.progress in 1..99) {
                    Box(Modifier.fillMaxSize().background(Color.Black.copy(alpha = .28f)))
                    ProgressRing(m.progress, 36.dp, numberColor = Color.White, track = Color.White.copy(alpha = .3f))
                }
            } else if (image == null && (m.self || (m.progress ?: 0) < 100)) Box(Modifier.fillMaxWidth().aspectRatio(aspect).previewSource(m.id, hooks).clip(shape).background(c.fill), contentAlignment = Alignment.Center) {
                if (!m.self) ImageStatus(m)
            } else Box(Modifier.fillMaxWidth().aspectRatio(aspect).previewSource(m.id, hooks).clip(shape).background(c.fill)
                .mediaEdge(c, shape, shown = image != null)) {
                if (image != null) Image(image, null, Modifier.fillMaxSize(), contentScale = ContentScale.Crop) else FullVector(DesignIconData.previewImage)
            }
        }
        else -> FileMessageCard(m, if (inBatch) Modifier.width(BATCH_FILE_W).morphSource(registry.bubbles, m.id) else Modifier.widthIn(min = 200.dp),
            onFile, onDownload, onLongPress)
    }
}

/** A password-marked text's state: hidden (***) or revealed; [onToggle] is 显示 (in the bubble) / 隐藏 (the pill beside it). */
private class Secret(val revealed: Boolean, val onToggle: () -> Unit)

/**
 * A text bubble (SPEC §1). Text longer than [TEXT_FOLD_LINES] lines shows [lines] of them; lines
 * 8–10 fade out through an alpha mask (any bubble colour), and 查看更多, centred in a 36 high row
 * at the bubble's bottom, adds [TEXT_MORE_LINES]. Growing animates the height (300 ms) with the
 * mask riding the bottom edge; when the whole text shows, the mask fades within the same time and
 * the row is pushed out of the bubble. Folding again (scrolled away) happens without animation.
 * A password-marked text ([secret]) shows a grey *** with a 显示 row in the same style until it is
 * revealed, then its text (folded the same way); 隐藏 is a pill beside the bubble (TransferPage),
 * not a row in it.
 * The bubble shape is painted, not clipped: the press area is the whole rectangle, so a long press
 * anywhere on a long message opens the menu.
 */
@Composable
private fun TextBubble(text: String, fill: Color, shape: Shape, lines: Int, onMore: () -> Unit, onLongPress: (Offset) -> Unit, secret: Secret? = null,
                       pending: Boolean = false, rich: RichDoc? = null) {
    val c = Oink.colors
    // A tap on a text bubble does nothing else: like the list's background, it closes the keyboard.
    val focusManager = LocalFocusManager.current
    // Revealed but not here yet: the reason / 正在获取… shows instead (it is no secret).
    val masked = secret != null && !secret.revealed
    val body = if (masked) "***" else text
    // Rich text (user 2026-11-12) unless hidden: formatted blocks, folded by height (1 line = 23.1).
    val doc = rich?.takeIf { !masked && !pending }
    val key: Any = doc ?: body
    // Laid-out lines (0 until measured; rich text: its height in 23.1 lines), the lines showing
    // (animated), and where the last unfold to the whole text started (while it runs).
    var total by remember(key) { mutableFloatStateOf(0f) }
    val shown = remember(key) { Animatable(lines.toFloat()) }
    var fullFrom by remember(key) { mutableStateOf<Float?>(null) }
    val density = LocalDensity.current
    LaunchedEffect(lines, total) {
        if (total == 0f) return@LaunchedEffect
        val target = minOf(lines.toFloat(), total)
        if (target > shown.value) {
            if (target >= total) fullFrom = shown.value
            shown.animateTo(target, tween(300, easing = EaseStandard))
            fullFrom = null
        } else shown.snapTo(target)
    }
    // Mask strength: 1 while folded, falling to 0 as the last unfold reaches the end.
    fun strength(n: Float): Float {
        val from = fullFrom
        return when {
            !foldsAt(n) -> 0f
            lines < n -> 1f
            from != null && shown.value < n -> ((n - shown.value) / (n - from)).coerceIn(0f, 1f)
            else -> 0f
        }
    }
    Layout(content = {
        val fold = Modifier
            .graphicsLayer { compositingStrategy = if (strength(total) > 0f) CompositingStrategy.Offscreen else CompositingStrategy.Auto }
            .drawWithContent {
                val n = total
                if (!foldsAt(n)) { drawContent(); return@drawWithContent }
                val line = size.height / n
                val h = minOf(shown.value, n.toFloat()) * line
                val f = strength(n)
                clipRect(bottom = h) {
                    this@drawWithContent.drawContent()
                    if (f > 0f) drawRect(Brush.verticalGradient(0f to Color.Black, 1f / 3f to Color.Black.copy(alpha = 1f - .34f * f),
                        2f / 3f to Color.Black.copy(alpha = 1f - .7f * f), 1f to Color.Black.copy(alpha = 1f - f), startY = h - 3 * line, endY = h),
                        topLeft = Offset(0f, h - 3 * line), size = Size(size.width, 3 * line), blendMode = BlendMode.DstIn)
                }
            }
        if (doc != null) RichContent(doc, c.bubbleInk, fold.onSizeChanged { total = it.height / with(density) { RICH_LINE_DP.dp.toPx() } })
        // Same hyphen break opportunity as T().
        else BasicText(body.replace("-", "-​"), fold.then(if (masked) Modifier.semantics { contentDescription = appText("已隐藏的内容", "Hidden content") } else Modifier),
            style = textStyle(14f, if (masked || pending) c.hint else c.bubbleInk, lineHeight = 23.1f), onTextLayout = { total = it.lineCount.toFloat() })
        if (total == 0f || (foldsAt(total) && (lines < total || fullFrom != null)))
            BubbleRow(appText("查看更多", "Show more"), DesignIconData.lucideChevronDown, Modifier.layoutId("more"), onLongPress, onMore)
        if (secret != null && !secret.revealed) BubbleRow(appText("显示", "Show"), DesignIconData.lucideChevronDown, Modifier.layoutId("secret"), onLongPress, secret.onToggle)
    }, modifier = Modifier.pressLong(onClick = { focusManager.clearFocus(force = true) }, onLongClick = onLongPress).background(fill, shape).clipToBounds()) { measurables, constraints ->
        val padX = 14.dp.roundToPx()
        val padTop = 12.dp.roundToPx()
        val rowH = 36.dp.roundToPx()
        val text = measurables[0].measure(Constraints(maxWidth = (constraints.maxWidth - 2 * padX).coerceAtLeast(0)))
        val n = total
        val folding = foldsAt(n)
        val visible = if (folding) minOf(shown.value, n.toFloat()) * text.height / n else text.height.toFloat()
        val moreOn = folding && (lines < n || shown.value < n)
        val secretRow = measurables.firstOrNull { it.layoutId == "secret" }
        // 查看更多's room: the whole row while folded; pushed out (or, above 隐藏, closing) as the last unfold runs.
        val more = when {
            !moreOn -> 0f
            lines < n -> rowH.toFloat()
            secretRow != null -> rowH * strength(n)
            else -> 28.dp.toPx() * strength(n)
        }
        // 4 under the last row, 12 without rows.
        val bottom = when {
            secretRow != null -> more + rowH + 4.dp.toPx()
            !moreOn -> 12.dp.toPx()
            lines < n -> rowH + 4.dp.toPx()
            else -> 12.dp.toPx() + more
        }
        // At least as wide as a row's label (*** alone is narrower than 显示 / 正在下载).
        val rowsWidth = measurables.drop(1).maxOfOrNull { it.maxIntrinsicWidth(rowH) + 2 * padX } ?: 0
        val width = maxOf(text.width + 2 * padX, rowsWidth).coerceAtMost(constraints.maxWidth)
        val moreRow = if (moreOn) measurables.firstOrNull { it.layoutId == "more" }?.measure(Constraints.fixed(width, rowH)) else null
        val secretPlaced = secretRow?.measure(Constraints.fixed(width, rowH))
        val fade = if (secretRow != null && moreOn && lines >= n) strength(n) else 1f
        layout(width, (padTop + visible + bottom).roundToInt()) {
            text.place(padX, padTop)
            moreRow?.placeWithLayer(0, padTop + visible.roundToInt()) { alpha = fade }
            secretPlaced?.place(0, (padTop + visible + more).roundToInt())
        }
    }
}

/**
 * 查看更多 / 显示 / 隐藏: label and chevron centred in a full-width 36 high row, accent ink; pressing
 * shades the row (rounded like the bubble's bottom) with a ripple. A long press opens the message
 * menu, as anywhere else on the bubble.
 */
@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun BubbleRow(label: String, icon: IconData, modifier: Modifier, onLongPress: (Offset) -> Unit, onClick: () -> Unit) {
    val c = Oink.colors
    val source = remember { MutableInteractionSource() }
    val pressed by source.collectIsPressedAsState()
    Row(modifier.clip(RoundedCornerShape(bottomStart = 18.dp, bottomEnd = 18.dp)).background(if (pressed) c.press else Color.Transparent)
        .combinedClickable(source, ripple(color = c.ink), role = Role.Button, onLongClick = { onLongPress(Offset.Unspecified) }, onClick = onClick),
        horizontalArrangement = Arrangement.spacedBy(2.dp, Alignment.CenterHorizontally), verticalAlignment = Alignment.CenterVertically) {
        T(label, 13f, c.accentInk, weight = 600, lineHeight = 18.2f, letterSpacing = .2f)
        Lucide(icon, 15.dp, c.accentInk, 2.2f, Modifier.padding(top = 1.dp))
    }
}

/**
 * 选择: the whole text in a read-only field with everything selected, so the system selection
 * handles (and, once they move, the copy toolbar) take over. Leaving the field ends selection.
 */
@Composable
private fun SelectableText(text: String, color: Color, onEnd: () -> Unit) {
    val c = Oink.colors
    val state = rememberTextFieldState(text)
    val focus = remember { FocusRequester() }
    var focused by remember { mutableStateOf(false) }
    LaunchedEffect(Unit) { state.edit { selectAll() }; focus.requestFocus() }
    BackHandler { onEnd() }
    CompositionLocalProvider(LocalTextSelectionColors provides TextSelectionColors(handleColor = c.accent, backgroundColor = c.accent.copy(alpha = .3f))) {
        BasicTextField(state, Modifier.focusRequester(focus).onFocusChanged { if (it.isFocused) focused = true else if (focused) onEnd() }
            .semantics { contentDescription = appText("选择文字", "Select text") },
            readOnly = true, textStyle = textStyle(14f, color, lineHeight = 23.1f), lineLimits = TextFieldLineLimits.MultiLine(),
            cursorBrush = SolidColor(c.accent))
    }
}

internal fun imageDescription(m: MessageUi) = when (m.preview) {
    "expired" -> appText("图片已失效", "Image expired")
    "offline" -> appText("图片暂时无法加载", "Image unavailable for now")
    else -> appText("打开图片", "Open image")
}

/**
 * What a picture box shows (inside a centring Box): the original, else the PC's thumbnail (dimmed
 * with the download ring while the original comes), else for a received image why there is none
 * or its progress ([ring] sized), else the placeholder artwork.
 */
@Composable
internal fun MediaContent(m: MessageUi, image: ImageBitmap?, thumbnail: ImageBitmap?, ring: Dp) {
    when {
        image != null -> Image(image, null, Modifier.fillMaxSize(), contentScale = ContentScale.Crop)
        thumbnail != null -> {
            Image(thumbnail, null, Modifier.fillMaxSize(), contentScale = ContentScale.Crop)
            if (m.progress != null && m.progress in 1..99) {
                Box(Modifier.fillMaxSize().background(Color.Black.copy(alpha = .28f)))
                ProgressRing(m.progress, ring, numberColor = Color.White, track = Color.White.copy(alpha = .3f))
            }
        }
        !m.self && (m.progress ?: 0) < 100 -> ImageStatus(m, ring)
        else -> FullVector(DesignIconData.previewImage)
    }
}

/** A received image without a picture to show: why (expired, offline, failed), waiting, or download progress. */
@Composable
internal fun ImageStatus(m: MessageUi, ring: Dp = 36.dp) {
    when {
        m.ended == "failed" -> ImageNotice(DesignIconData.lucideAlertCircle, appText("接收失败", "Not received"))
        m.ended == "cancelled" -> ImageNotice(DesignIconData.lucideX, appText("已取消", "Cancelled"))
        m.preview == "expired" -> ImageNotice(DesignIconData.lucideImageOff, appText("图片已失效", "Image expired"))
        m.preview == "offline" -> ImageNotice(DesignIconData.lucideWifiOff, appText("暂时无法加载", "Unavailable for now"))
        m.preview == "failed" && (m.progress ?: 0) == 0 -> ImageNotice(DesignIconData.lucideRefreshCw, appText("加载失败，点按重试", "Failed, tap to retry"))
        m.preview == "loading" && (m.progress ?: 0) == 0 -> Spinner(22.dp)
        else -> ProgressRing(m.progress ?: 0, ring)
    }
}

/** Icon and caption inside an image bubble without a picture (expired, offline, failed). */
@Composable
private fun ImageNotice(icon: IconData, text: String) {
    val c = Oink.colors
    Column(horizontalAlignment = Alignment.CenterHorizontally, verticalArrangement = Arrangement.spacedBy(6.dp)) {
        Lucide(icon, 24.dp, c.faint)
        T(text, 11f, c.hint, lineHeight = 15.4f, align = TextAlign.Center)
    }
}

/** Decoded images by "size:path": scrolling back and opening the preview show them at once. */
internal object ImageCache : android.util.LruCache<String, ImageBitmap>(48 * 1024 * 1024) {
    override fun sizeOf(key: String, value: ImageBitmap) = value.width * value.height * 4
}

/** An already decoded image, if any (no decoding). */
internal fun cachedImage(path: String?, maxPx: Int): ImageBitmap? = path?.let { ImageCache.get("$maxPx:$it") }

/** Puts a decoded image into the cache (tests render bubbles without decoding files). */
internal fun cacheImage(path: String, maxPx: Int, image: ImageBitmap) { ImageCache.put("$maxPx:$path", image) }

/** Decodes a local image scaled down to about [maxPx] on its long side, off the main thread. */
@Composable
internal fun rememberImage(path: String?, maxPx: Int): ImageBitmap? {
    val key = path?.let { "$maxPx:$it" }
    val image by produceState(key?.let { ImageCache.get(it) }, key) {
        if (path == null || key == null) { value = null; return@produceState }
        ImageCache.get(key)?.let { value = it; return@produceState }
        value = withContext(Dispatchers.IO) {
            runCatching {
                val source = android.graphics.ImageDecoder.createSource(java.io.File(path))
                android.graphics.ImageDecoder.decodeBitmap(source) { decoder, info, _ ->
                    val long = maxOf(info.size.width, info.size.height)
                    if (long > maxPx) decoder.setTargetSampleSize(long / maxPx)
                    decoder.allocator = android.graphics.ImageDecoder.ALLOCATOR_SOFTWARE
                }.asImageBitmap()
            }.getOrNull()
        }?.takeIf { java.io.File(path).isFile }?.also { ImageCache.put(key, it) }
    }
    return image
}

/** Negative top margin (CSS margin-top: -d): draws d higher and gives the space back. */
fun Modifier.pullUp(d: Dp) = layout { m, cons ->
    val p = m.measure(cons); val px = d.roundToPx()
    layout(p.width, (p.height - px).coerceAtLeast(0)) { p.place(0, -px) }
}

/** .tr-progress: 36 ring, track + accent arc (stroke 2.5 of a 36 viewBox), number in the middle. */
@Composable
fun ProgressRing(value: Int, size: Dp, numberColor: Color = Oink.colors.muted, track: Color = Oink.colors.progressTrack) {
    val c = Oink.colors
    val sweep by animateFloatAsState(value.coerceIn(0, 100) / 100f * 360f, tween(250), label = "ring")
    Box(Modifier.size(size), contentAlignment = Alignment.Center) {
        Canvas(Modifier.fillMaxSize()) {
            val u = this.size.width / 36f
            val tl = Offset(3f * u, 3f * u); val s = Size(30f * u, 30f * u)
            drawArc(track, 0f, 360f, false, tl, s, style = Stroke(2.5f * u))
            drawArc(c.accent, -90f, sweep, false, tl, s, style = Stroke(2.5f * u, cap = StrokeCap.Round))
        }
        T("$value", 10f, numberColor, weight = 600, lineHeight = 14f, tabular = true)
    }
}

/**
 * A folder message's listing (native-rewrite.md 「文件夹页」): rows on the page (no card frame), at
 * least 64 high and 4 apart, a divider in the middle of each gap from the text's left edge; pictures
 * show their thumbnail, files the shared download button, subfolders a chevron.
 */
@Composable
private fun FolderPage(f: FolderUi, onBack: () -> Unit, onItem: (FolderItemUi) -> Unit, onDownload: (FolderItemUi) -> Unit, onMore: () -> Unit,
                       onThumbnail: (FolderItemUi) -> Unit, onRetry: () -> Unit = {}) {
    val c = Oink.colors
    BackHandler { onBack() }
    // Each folder level scrolls on its own.
    val list = remember(f.messageId, f.node) { LazyListState() }
    // Scrolling near the end continues the listing.
    val nearEnd by remember(list) { derivedStateOf { list.layoutInfo.let { (it.visibleItemsInfo.lastOrNull()?.index ?: 0) >= it.totalItemsCount - 4 } } }
    LaunchedEffect(nearEnd, f.hasMore, f.items.size) { if (f.hasMore && nearEnd) onMore() }
    Column(Modifier.fillMaxSize().background(c.bg).press(pressedColor = null) {}) {
        // Header 64 with an icon-only back (same 48 target / 23 icon as the gear).
        Row(Modifier.fillMaxWidth().height(64.dp).drawBehind { drawRect(c.lineSolid, Offset(0f, size.height - 1.dp.toPx()), Size(size.width, 1.dp.toPx())) }.padding(horizontal = 16.dp),
            horizontalArrangement = Arrangement.spacedBy(4.dp), verticalAlignment = Alignment.CenterVertically) {
            IconButton48(Modifier, "返回", onClick = onBack) { Lucide(DesignIconData.lucideChevronLeft, 23.dp, c.ink) }
            T(f.name, 17f, c.ink, weight = 700, lineHeight = 23.8f)
        }
        val error = f.error
        // A listing that ended without a page replaces the spinner (it never comes); entries already shown stay.
        if (error != null && f.items.isEmpty()) FolderFailureNotice(error, f.retryable, Modifier.fillMaxSize().padding(bottom = 64.dp), onRetry)
        else if (f.loading && f.items.isEmpty()) Box(Modifier.fillMaxWidth().padding(top = 40.dp), contentAlignment = Alignment.Center) { Spinner(24.dp) }
        // The floating tab bar covers the page's bottom: the last row scrolls up above it.
        if (error == null || f.items.isNotEmpty()) LazyColumn(Modifier.weight(1f).fillMaxWidth(), state = list, contentPadding = PaddingValues(start = 16.dp, end = 16.dp, top = 8.dp, bottom = TAB_BAR_SPACE + 16.dp)) {
            itemsIndexed(f.items, key = { _, item -> item.id }) { i, item ->
                Column {
                    if (i > 0) Spacer(Modifier.fillMaxWidth().height(4.dp).drawBehind {
                        val line = 1.dp.toPx(); val left = 48.dp.toPx()
                        drawRect(c.lineSolid, Offset(left, (size.height - line) / 2f), Size(size.width - left, line))
                    })
                    FolderRow(item, onItem, onDownload, onThumbnail)
                }
            }
            // A failed next page shows why below the entries already listed.
            if (error != null) item(key = "failed") { FolderFailureNotice(error, f.retryable, Modifier.fillMaxWidth(), onRetry) }
            else if (f.hasMore) item(key = "more") { Box(Modifier.fillMaxWidth().padding(vertical = 16.dp), contentAlignment = Alignment.Center) { Spinner(20.dp) } }
        }
    }
}

/** A folder listing that ended without a page (empty-state layout): why, and 重试 when it can help. */
@Composable
private fun FolderFailureNotice(text: String, retryable: Boolean, modifier: Modifier, onRetry: () -> Unit) {
    val c = Oink.colors
    Column(modifier.padding(horizontal = 32.dp, vertical = 24.dp), horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.Center) {
        Box(Modifier.size(56.dp).clip(CircleShape).background(c.badSoft), contentAlignment = Alignment.Center) {
            Lucide(DesignIconData.lucideAlertCircle, 24.dp, c.badInk)
        }
        T(text, 13.5f, c.muted, Modifier.padding(top = 14.dp).widthIn(max = 280.dp), lineHeight = 21.6f, align = TextAlign.Center)
        if (retryable) PrimaryCapsule("重试", Modifier.padding(top = 16.dp), horizontalPadding = 18.dp) { onRetry() }
    }
}

/** One folder row: leading thumbnail or type tile, name (14, up to three lines) over extension + size, then the button or a chevron. */
@Composable
private fun FolderRow(item: FolderItemUi, onItem: (FolderItemUi) -> Unit, onDownload: (FolderItemUi) -> Unit, onThumbnail: (FolderItemUi) -> Unit) {
    val c = Oink.colors
    val state = fileLook(item)
    // A picture with nothing on this phone yet asks for its computer's thumbnail while the row is
    // shown (the core decides whether one is needed), as image bubbles do.
    if (item.kind == "image" && item.path == null && item.thumbnailPath == null) LaunchedEffect(item.id) { onThumbnail(item) }
    Row(Modifier.fillMaxWidth().heightIn(min = 64.dp).press { onItem(item) }.padding(vertical = 10.dp),
        horizontalArrangement = Arrangement.spacedBy(8.dp), verticalAlignment = Alignment.CenterVertically) {
        Row(Modifier.weight(1f), horizontalArrangement = Arrangement.spacedBy(12.dp), verticalAlignment = Alignment.CenterVertically) {
            FolderRowLeading(item)
            FileCardText(item.name, fileExt(item.kind, item.name), item.detail, state.reason, state.reasonBad, row = true)
        }
        if (item.kind == "folder") Box(Modifier.size(40.dp, 44.dp), contentAlignment = Alignment.Center) { Lucide(DesignIconData.lucideChevronRight, 18.dp, c.faint) }
        else FileSlotButton(state, item.name) { onDownload(item) }
    }
}

/** Draws a vector stretched to the composable's size (used for the fixed-aspect placeholder artwork). */
@Composable
fun FullVector(icon: IconData) {
    BoxWithConstraints(Modifier.fillMaxSize()) { SvgIcon(icon, maxWidth, maxHeight) }
}

/**
 * The composer takes focus (and with it the keyboard) only when the user asks for it (user 2026-11-12:
 * entering 传输 from another tab leaves the keyboard closed; it opens when the input is tapped), or when
 * the page gives it back after the background / the share sheet to a user who was typing ([ComposerReturn]).
 *
 * Why a gate on the field and not a fix elsewhere: nothing in the app requests the composer's focus.
 * It gets it from Compose's own focus handling: the tab switch clears focus (the [Cover] of [UiState.tab];
 * the control page holds focus for the hardware keyboard), clearing the root's focus lets the Android
 * view lose focus too, the window then hands focus back to the Compose view, and a Compose view that
 * gains Android focus moves it on to its first focusable node; the same can happen when the window
 * regains focus after the background or the share sheet. Every tab page stays composed ([TabTrack]),
 * and a focused text field shows the keyboard. That hand-over is Compose / framework behaviour we
 * cannot switch off for one node, so the field refuses focus it was not asked for.
 *
 * Asked for: a touch on the field (allowed from touch down, so the field's own tap focuses it; a touch
 * that is cancelled or slides off the field withdraws it; losing focus withdraws it), a hardware
 * keyboard (keyboard input mode: Tab moves focus, and no soft keyboard comes up then), or TalkBack's
 * touch exploration (its double tap is an accessibility click, not a touch).
 */
@Composable
private fun Modifier.focusOnlyByUser(composer: ComposerFocus): Modifier {
    val context = LocalView.current.context
    val inputMode = androidx.compose.ui.platform.LocalInputModeManager.current
    val a11y = remember(context) { context.getSystemService(android.view.accessibility.AccessibilityManager::class.java) }
    return this
        .pointerInput(Unit) {
            awaitEachGesture {
                awaitFirstDown(requireUnconsumed = false, pass = PointerEventPass.Initial)
                composer.asked = true
                if (waitForUpOrCancellation(PointerEventPass.Initial) == null) composer.asked = false
            }
        }
        .focusRequester(composer.requester)
        .focusProperties {
            canFocus = composer.asked || inputMode.inputMode == androidx.compose.ui.input.InputMode.Keyboard || a11y?.isTouchExplorationEnabled == true
        }
        .onFocusChanged { composer.focused = it.isFocused; if (!it.isFocused) composer.asked = false }
}

/** The composer's focus: whether it may take focus ([asked]), whether it has it, and its requester. */
internal class ComposerFocus {
    var asked by mutableStateOf(false)
    var focused by mutableStateOf(false)
    val requester = FocusRequester()
    /** Typing (focused with the keyboard up) as last seen while the app was in the foreground. */
    var typing = false
}

/**
 * Background and share-sheet return (user 2026-11-12, as on iOS): the composer keeps the state it had when
 * the app left. While the app is resumed, "typing" follows the composer (focused with the keyboard up,
 * [lift] above 0); it is not updated once the app pauses (focus and keyboard go away then). On the next
 * resume of the shown page: typing → the composer takes focus again and the keyboard is asked for;
 * otherwise focus is cleared so nothing brings the keyboard up. A tab switch into 传输 is not a return:
 * the keyboard stays closed there (the tab's [Cover] clears focus).
 */
@Composable
private fun ComposerReturn(composer: ComposerFocus, lift: ImeLift, active: Boolean) {
    val lifecycle = androidx.lifecycle.compose.LocalLifecycleOwner.current.lifecycle
    val focusManager = LocalFocusManager.current
    val keyboard = androidx.compose.ui.platform.LocalSoftwareKeyboardController.current
    LaunchedEffect(composer) {
        snapshotFlow { composer.focused && lift.px() > 0 }.collect { typing ->
            if (lifecycle.currentState.isAtLeast(androidx.lifecycle.Lifecycle.State.RESUMED)) composer.typing = typing
        }
    }
    // Only a real pause / resume of the app counts (a tab switch changes [active], not the lifecycle).
    var paused by remember { mutableStateOf(false) }
    val shown by rememberUpdatedState(active)
    androidx.lifecycle.compose.LifecycleEventEffect(androidx.lifecycle.Lifecycle.Event.ON_PAUSE) { paused = true }
    androidx.lifecycle.compose.LifecycleEventEffect(androidx.lifecycle.Lifecycle.Event.ON_RESUME) {
        if (paused && shown) {
            if (composer.typing) {
                composer.asked = true
                runCatching { composer.requester.requestFocus() }
                keyboard?.show()
            } else focusManager.clearFocus(force = true)
        }
        paused = false
    }
}
