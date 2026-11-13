package com.hachiwii.oink.ui

import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.CubicBezierEasing
import androidx.compose.animation.core.EaseOut
import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.detectHorizontalDragGestures
import androidx.compose.foundation.indication
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.interaction.collectIsPressedAsState
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyListItemInfo
import androidx.compose.foundation.lazy.LazyListLayoutInfo
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.ripple
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawWithContent
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.Shape
import androidx.compose.ui.graphics.drawscope.DrawScope
import androidx.compose.ui.graphics.drawscope.translate
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.graphics.layer.GraphicsLayer
import androidx.compose.ui.graphics.layer.drawLayer
import androidx.compose.ui.graphics.GraphicsContext
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.input.pointer.util.VelocityTracker
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.layout.LayoutCoordinates
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.semantics.CustomAccessibilityAction
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.customActions
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.dp
import androidx.compose.ui.util.lerp
import androidx.compose.ui.zIndex
import kotlinx.coroutines.launch
import java.util.Locale
import kotlin.math.abs
import kotlin.math.min
import kotlin.math.roundToInt

/*
 * Transfer page batches (transfer-stack SPEC, design specification):
 * the collapsed stack of 120 × 160 cards, its swipe, the 展开 / 收起 pills, and the morph between
 * the stack and the batch's normal messages.
 */

// ---- tokens (theme.css --c-stack-*, motion per SPEC §2.4 / §4.1, Android values) ----

internal const val CARD_WF = 120f
internal const val CARD_HF = 160f
internal val CARD_W = CARD_WF.dp
internal val CARD_H = CARD_HF.dp
/** Expanded image bubble of a batch: the picture contain-fits this box. */
internal const val BATCH_MEDIA_WF = 220f
internal const val BATCH_MEDIA_HF = 280f
internal val BATCH_FILE_W = 220.dp
/** Pictures whose size is not known yet (not downloaded) use this aspect. */
private const val PLACEHOLDER_ASPECT = 4f / 3f

internal val EaseStandard = CubicBezierEasing(.2f, 0f, 0f, 1f)
private val EaseSnapBack = CubicBezierEasing(0f, 0f, 0f, 1f)
internal val EasePillIn = CubicBezierEasing(.2f, .9f, .3f, 1.2f)
internal val EasePillOut = CubicBezierEasing(.42f, 0f, 1f, 1f)
internal const val MORPH_MS = 400
private const val MORPH_STAGGER_MS = 20
private const val SWIPE_COMMIT_MS = 320
private const val SWIPE_CANCEL_MS = 250

private val inkRgb = Color(27, 28, 34)
internal val OinkColors.stackCard get() = if (dark) Color(0xFF2D323E) else Color.White
/** 1 dp inner edge of every file card (single, expanded, stacked): the divider colour, lighter (file-bubble SPEC 1.2). */
internal val OinkColors.stackCardEdge get() = if (dark) Color(0xFFECEEF2).copy(alpha = .04f) else inkRgb.copy(alpha = .05f)
internal val OinkColors.stackCardInk get() = if (dark) Color(0xFFECEEF2) else Color(0xFF1B1C22)
internal val OinkColors.stackCardHint get() = if (dark) Color(0xFF868D9D) else Color(0xFFA3A5AE)
/** `--c-media-edge` (user 2026-11-12): the picture edge's colour, ink 10 % / white 12 %. */
internal val OinkColors.mediaEdge get() = if (dark) Color.White.copy(alpha = .12f) else inkRgb.copy(alpha = .10f)
/**
 * The picture edge (user 2026-11-12): a 0.5 dp [mediaEdge] hairline inside the picture, over it (border
 * draws after the content), following [shape]; the same on single pictures, stack cards and folder
 * thumbnails. Placeholders (downloading, failed, cancelled) have their own fill and no edge ([shown] false).
 */
internal fun Modifier.mediaEdge(c: OinkColors, shape: Shape, shown: Boolean = true): Modifier =
    if (shown) border(.5.dp, c.mediaEdge, shape) else this
/** Cards behind the top one are darkened by this × depth (dark mode only). */
internal val OinkColors.stackDim get() = if (dark) .16f else 0f

/** --c-stack-shadow (two layers), scaled by [strength] (the morph fades it out). */
internal fun Modifier.stackShadow(c: OinkColors, radius: Dp, strength: Float = 1f): Modifier = when {
    strength <= 0f -> this
    c.dark -> boxShadow(Color.Black.copy(alpha = .4f * strength), 1.dp, 2.dp, radius).boxShadow(Color.Black.copy(alpha = .5f * strength), 6.dp, 16.dp, radius)
    else -> boxShadow(inkRgb.copy(alpha = .08f * strength), 1.dp, 2.dp, radius).boxShadow(inkRgb.copy(alpha = .12f * strength), 4.dp, 12.dp, radius)
}

private val TEXT_EXT = setOf("PDF", "DOC", "DOCX", "TXT", "MD", "RTF", "PAGES", "ODT", "KEY", "PPT", "PPTX")
private val SHEET_EXT = setOf("XLS", "XLSX", "CSV", "NUMBERS", "ODS", "TSV")
private val ARCHIVE_EXT = setOf("ZIP", "RAR", "7Z", "TAR", "GZ", "TGZ", "BZ2", "XZ", "ZST")

/** Upper-case extension shown on a file card (none for folders). */
internal fun fileExt(kind: String, name: String): String =
    if (kind == "folder") "" else name.substringAfterLast('.', "").uppercase(Locale.US).takeIf { it.length in 1..6 }.orEmpty()
internal fun fileExt(m: MessageUi): String = fileExt(m.kind, m.name)

/** Type icon of a file or folder (SPEC §2.1): document, spreadsheet, archive, folder, other. */
internal fun fileGlyph(kind: String, name: String): IconData = when {
    kind == "folder" -> DesignIconData.lucideFolder
    fileExt(kind, name) in TEXT_EXT -> DesignIconData.lucideFileText
    fileExt(kind, name) in SHEET_EXT -> DesignIconData.lucideFileSpreadsheet
    fileExt(kind, name) in ARCHIVE_EXT -> DesignIconData.lucideFileArchive
    else -> DesignIconData.lucideFile
}
internal fun fileGlyph(m: MessageUi): IconData = fileGlyph(m.kind, m.name)

/** A card shows the size only (the extension is in its corner); folders show their item count as 「N 项」. */
internal fun cardDetail(m: MessageUi): String =
    if (m.kind == "folder") m.detail.replace(Regex("^(\\d+) 个文件$"), "$1 项") else m.detail.substringAfter(" · ")

// ---- geometry ----

/**
 * Width / height of an item's card: the picture's own aspect (decoded original or thumbnail, else
 * the thumbnail's size from the core), 3:4 for files and folders.
 */
internal fun itemAspect(m: MessageUi, image: ImageBitmap?): Float = when {
    m.kind != "image" -> CARD_WF / CARD_HF
    image != null -> image.width.toFloat() / image.height.coerceAtLeast(1)
    else -> m.aspect ?: PLACEHOLDER_ASPECT
}

/** Contain-fit an aspect (w / h) inside a box, in dp, rounded like the mockup. */
internal fun contain(aspect: Float, w: Float = CARD_WF, h: Float = CARD_HF): Size =
    if (aspect >= w / h) Size(w, (w / aspect).roundToInt().toFloat()) else Size((h * aspect).roundToInt().toFloat(), h)

/** A card's place in the stack: shift (dp), scale, tilt (degrees), opacity, z order, depth (dimming). */
internal data class Pose(val tx: Float, val s: Float, val r: Float, val o: Float, val z: Float, val d: Float)

private val POSES = mapOf(
    "top" to Pose(0f, 1f, 0f, 1f, 6f, 0f),
    "next" to Pose(10f, .93f, 3f, 1f, 5f, 1f),
    "prev" to Pose(-10f, .93f, -3f, 1f, 4f, 1f),
    "next2" to Pose(19f, .86f, 5.7f, 1f, 3f, 1.6f),
    // Invisible: every card has somewhere to come from and go to while swiping.
    "farNext" to Pose(26f, .8f, 7.8f, 0f, 2f, 2f),
    "farPrev" to Pose(-18f, .86f, -5.4f, 0f, 1f, 2f),
)
internal fun poseOf(role: String) = POSES.getValue(role)

private fun lerpPose(a: Pose, b: Pose, t: Float, z: Float = a.z) =
    Pose(lerp(a.tx, b.tx, t), lerp(a.s, b.s, t), lerp(a.r, b.r, t), lerp(a.o, b.o, t), z, lerp(a.d, b.d, t))

/** Item index → role around [top], circular; earlier roles win when there are few items. */
internal fun stackRoles(n: Int, top: Int): LinkedHashMap<Int, String> {
    val out = LinkedHashMap<Int, String>()
    for ((role, off) in listOf("top" to 0, "next" to 1, "prev" to -1, "next2" to 2, "farNext" to 3, "farPrev" to -2)) {
        if (role == "prev" && n < 3) continue
        if (role == "next2" && n < 4) continue
        val i = ((top + off) % n + n) % n
        if (i !in out) out[i] = role
    }
    return out
}

private fun ringDistance(n: Int, i: Int, j: Int) = min(abs(i - j), n - abs(i - j))

// ---- card faces ----

/** File / folder card content (SPEC §2.1): type block top left, extension top right, name and size at the bottom; no download state in a stack. */
@Composable
internal fun FileCardContent(m: MessageUi) {
    val c = Oink.colors
    val folder = m.kind == "folder"
    val tint = if (folder) c.folder else c.file
    Column(Modifier.size(CARD_W, CARD_H).padding(10.dp), verticalArrangement = Arrangement.SpaceBetween) {
        Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.Top) {
            Box(Modifier.size(30.dp).clip(RoundedCornerShape(9.dp)).background(OinkColors.mix(tint, c.stackCard, if (folder) .18f else .16f)),
                contentAlignment = Alignment.Center) {
                Lucide(fileGlyph(m), 17.dp, tint, 1.8f)
            }
            Spacer(Modifier.weight(1f))
            val ext = fileExt(m)
            if (ext.isNotEmpty()) T(ext, 9f, c.stackCardHint, Modifier.padding(top = 1.dp), weight = 650, lineHeight = 12f, letterSpacing = .4f)
        }
        Column(verticalArrangement = Arrangement.spacedBy(3.dp)) {
            T(m.name, 12f, c.stackCardInk, weight = 600, lineHeight = 15.6f, maxLines = 3, ellipsis = true)
            val detail = cardDetail(m)
            if (detail.isNotEmpty()) T(detail, 10f, c.stackCardHint, lineHeight = 14f, ellipsis = true, tabular = true)
        }
    }
}

/** Darkens a card behind the top one (dark mode): black over everything drawn after this modifier. */
private fun Modifier.dim(amount: () -> Float) = drawWithContent {
    drawContent()
    val a = amount()
    if (a > 0f) drawRect(Color.Black.copy(alpha = a.coerceAtMost(1f)))
}

/** One stack card: a picture contain-fitted in 120 × 160 (rounded and shadowed itself), or a file card. */
@Composable
private fun StackCard(m: MessageUi, image: ImageBitmap?, thumbnail: ImageBitmap?, modifier: Modifier, depth: () -> Float) {
    val c = Oink.colors
    if (m.kind == "image") {
        val shape = RoundedCornerShape(9.dp)
        Box(modifier.stackShadow(c, 9.dp).clip(shape).background(c.fill).dim { c.stackDim * depth() }
            .mediaEdge(c, shape, shown = image != null || thumbnail != null),
            contentAlignment = Alignment.Center) {
            MediaContent(m, image, thumbnail, 26.dp)
        }
    } else {
        val shape = RoundedCornerShape(12.dp)
        // Same surface and edge as the flat file cards; only stacked cards keep the shadow (they overlap).
        Box(modifier.stackShadow(c, 12.dp).clip(shape).background(c.stackCard).dim { c.stackDim * depth() }.border(1.dp, c.stackCardEdge, shape)) {
            FileCardContent(m)
        }
    }
}

// ---- pills ----

/**
 * 展开 / 收起 (SPEC §2.3, Android): 32 high capsule, chevrons + label (+ count), darker chip and a
 * ripple while pressed. The touch area reaches 8 above and below and 4 to the sides (48 high).
 */
@Composable
internal fun StackPill(icon: IconData, label: String, count: Int?, description: String, modifier: Modifier = Modifier, onClick: () -> Unit) {
    val c = Oink.colors
    val source = remember { MutableInteractionSource() }
    val pressed by source.collectIsPressedAsState()
    Box(modifier.clickable(source, indication = null, role = Role.Button, onClick = onClick).padding(horizontal = 4.dp, vertical = 8.dp)
        .semantics { contentDescription = description }, contentAlignment = Alignment.Center) {
        Row(Modifier.height(32.dp).clip(RoundedCornerShape(16.dp)).background(if (pressed) OinkColors.mix(c.ink, c.chip, .1f) else c.chip)
            .indication(source, ripple()).padding(start = 9.dp, end = 12.dp),
            horizontalArrangement = Arrangement.spacedBy(3.dp), verticalAlignment = Alignment.CenterVertically) {
            Lucide(icon, 14.dp, c.text2)
            T(label, 12f, c.text2, weight = 600, lineHeight = 16.8f)
            if (count != null) T("$count", 12f, c.muted, Modifier.padding(start = 1.dp), weight = 600, lineHeight = 16.8f, tabular = true)
        }
    }
}

// ---- the stack ----

private class Settle(val dir: Int, val newTop: Int, val outgoing: Int, val roles: Map<Int, String>, val from: Map<Int, Pose>, val to: Map<Int, Pose?>)

/**
 * A batch as a stack of 120 × 160 cards around [top] (SPEC §2.2): the cards follow a horizontal
 * swipe; letting go past 40 dp (or fast) brings the next (swipe left) or previous card to the top,
 * circularly. A tap acts on the top card, a long press opens its menu; 展开 sits on the open side.
 * [hidden] hides the cards and the pill while the morph overlay stands in for them; [enter] plays
 * the pill's fade-in once (after collapsing).
 */
@Composable
internal fun BatchStack(items: List<MessageUi>, top: Int, self: Boolean, hooks: ImageHooks, registry: MorphRegistry, batch: String,
                        hidden: Boolean, enter: Boolean, onEntered: () -> Unit, onTop: (Int) -> Unit, onOpen: (MessageUi) -> Unit,
                        onLongPress: (MessageUi, Offset) -> Unit, onExpand: () -> Unit) {
    val n = items.size
    val density = LocalDensity.current.density
    val scope = rememberCoroutineScope()
    val drag = remember(batch) { Animatable(0f) }
    val settle = remember(batch) { Animatable(0f) }
    var settling by remember(batch) { mutableStateOf<Settle?>(null) }
    val current by rememberUpdatedState(top)

    fun dragPose(role: String, dx: Float): Pose {
        val base = poseOf(role)
        if (dx == 0f) return base
        if (role == "top") return base.copy(tx = dx, r = dx / CARD_WF * 11f)
        val incoming = if (dx < 0f || n < 3) "next" else "prev"
        if (role != incoming) return base
        return lerpPose(base, poseOf("top"), min(1f, abs(dx) / (CARD_WF * .7f)) * .85f)
    }
    fun commit(dir: Int) {
        if (settling != null) return
        val dx = drag.value
        val from = current
        val newTop = ((from + dir) % n + n) % n
        val now = stackRoles(n, from)
        val next = stackRoles(n, newTop)
        settling = Settle(dir, newTop, from, now, now.mapValues { (_, r) -> dragPose(r, dx) }, now.mapValues { (i, _) -> next[i]?.let(::poseOf) })
        scope.launch {
            drag.snapTo(0f); settle.snapTo(0f)
            settle.animateTo(1f, tween(SWIPE_COMMIT_MS, easing = LinearEasing))
            onTop(newTop)
            settling = null
        }
    }
    fun pose(i: Int, role: String): Pose {
        val s = settling ?: return dragPose(role, drag.value)
        val from = s.from[i] ?: return poseOf(role)
        val to = s.to[i] ?: return from
        val p = EaseStandard.transform(settle.value)
        if (i != s.outgoing) return lerpPose(from, to, p, lerp(from.z, to.z, p))
        // The card that leaves the top flies out in the swipe direction, drops below the others
        // around 46–50 %, then tucks into its new place (SPEC §2.4).
        val out = Pose(-s.dir * CARD_WF * .78f, .97f, -s.dir * 12f, 1f, 9f, 0f)
        val tuck = Pose(-s.dir * CARD_WF * .74f, .95f, -s.dir * 11f, 1f, to.z, 0f)
        val fade = lerpPose(out, to, ((p - .46f) / .54f).coerceIn(0f, 1f))
        return when {
            p < .46f -> lerpPose(from.copy(z = 9f), out, p / .46f, 9f)
            p < .5f -> lerpPose(out, tuck, (p - .46f) / .04f, if (p < .48f) 9f else to.z).copy(o = fade.o, d = fade.d)
            else -> lerpPose(tuck, to, (p - .5f) / .5f, to.z).copy(o = fade.o, d = fade.d)
        }
    }

    val pillIn = remember(batch) { Animatable(if (enter) 0f else 1f) }
    LaunchedEffect(enter) { if (enter) { pillIn.snapTo(0f); pillIn.animateTo(1f, tween(200, easing = EaseOut)); onEntered() } }
    val topItem = items[top]
    val pill: @Composable () -> Unit = {
        StackPill(DesignIconData.lucideChevronsUpDown, appText("展开", "Expand"), n, appText("展开全部 $n 项", "Expand all $n items"),
            Modifier.graphicsLayer { alpha = pillIn.value * (if (hidden) 0f else 1f); scaleX = .85f + .15f * pillIn.value; scaleY = scaleX }, onExpand)
    }
    Row(verticalAlignment = Alignment.CenterVertically) {
        if (self) { pill(); Spacer(Modifier.width(6.dp)) }
        // 120 × 160 box; the side margins leave room for the cards peeking out (11 on the avatar side).
        Box(Modifier.padding(vertical = 4.dp).padding(start = if (self) 12.dp else 11.dp, end = if (self) 11.dp else 12.dp).size(CARD_W, CARD_H)
            .morphSource(registry.stacks, batch)
            .alpha(if (hidden) 0f else 1f)
            .pointerInput(batch, n) {
                if (n < 2) return@pointerInput
                val tracker = VelocityTracker()
                detectHorizontalDragGestures(
                    onDragStart = { tracker.resetTracking() },
                    onDragEnd = {
                        // px/s → dp/ms
                        val v = tracker.calculateVelocity().x / density / 1000f
                        val dx = drag.value
                        if (abs(dx) > 40f || abs(v) > .35f) commit(if (dx < 0f || (dx == 0f && v < 0f)) 1 else -1)
                        else scope.launch { drag.animateTo(0f, tween(SWIPE_CANCEL_MS, easing = EaseSnapBack)) }
                    },
                    onDragCancel = { scope.launch { drag.animateTo(0f, tween(SWIPE_CANCEL_MS, easing = EaseSnapBack)) } },
                ) { change, amount ->
                    if (settling != null) return@detectHorizontalDragGestures
                    change.consume()
                    tracker.addPosition(change.uptimeMillis, change.position)
                    scope.launch { drag.snapTo(drag.value + amount / density) }
                }
            }
            .pressLong(onClick = { onOpen(items[current]) }, onLongClick = { onLongPress(items[current], it) })
            .semantics {
                contentDescription = appText("$n 项，当前第 ${top + 1} 项：${topItem.name}。左右划动切换，轻点打开",
                    "$n items, item ${top + 1}: ${topItem.name}. Swipe to switch, tap to open")
                customActions = listOf(
                    CustomAccessibilityAction(appText("下一项", "Next item")) { onTop((current + 1) % n); true },
                    CustomAccessibilityAction(appText("上一项", "Previous item")) { onTop((current - 1 + n) % n); true },
                )
            }) {
            val roles = settling?.roles ?: stackRoles(n, top)
            for ((i, role) in roles) key(items[i].id) {
                val m = items[i]
                val image = if (m.kind == "image") rememberImage(m.imagePath, 720) else null
                val thumbnail = if (m.kind == "image") rememberImage(m.thumbnailPath.takeIf { image == null }, 720) else null
                val size = contain(itemAspect(m, image ?: thumbnail))
                val p = pose(i, role)
                val isTop = settling == null && role == "top"
                StackCard(m, image, thumbnail, Modifier.offset(((CARD_WF - size.width) / 2f).dp, ((CARD_HF - size.height) / 2f).dp)
                    .size(size.width.dp, size.height.dp).zIndex(p.z)
                    .graphicsLayer { translationX = p.tx * density; scaleX = p.s; scaleY = p.s; rotationZ = p.r; alpha = p.o }
                    .then(if (isTop && m.kind == "image") Modifier.previewSource(m.id, hooks) else Modifier), depth = { p.d })
            }
        }
        if (!self) { Spacer(Modifier.width(6.dp)); pill() }
    }
}

// ---- morph between the stack and the messages (SPEC §4) ----

/** Where the morph looks things up: layout coordinates by row key / message id / batch, kept while composed. */
internal class MorphRegistry {
    var container: LayoutCoordinates? = null
    /** Inside each row's own translation, so positions within a row are never shifted. */
    val rows = HashMap<Any, LayoutCoordinates>()
    val bubbles = HashMap<Any, LayoutCoordinates>()
    val stacks = HashMap<Any, LayoutCoordinates>()
}

/** Registers this node's coordinates in [map] under [key] while it is composed. */
@Composable
internal fun Modifier.morphSource(map: HashMap<Any, LayoutCoordinates>, key: Any): Modifier {
    DisposableEffect(map, key) { onDispose { map.remove(key) } }
    return onGloballyPositioned { map[key] = it }
}

/**
 * One item's stand-in while it morphs: an untransformed rect (container px) plus the card's pose
 * around its centre ([tx] px). [card] picks the card look (radius, plate, shadow) or the message look.
 */
internal data class Geo(val x: Float, val y: Float, val w: Float, val h: Float, val tx: Float = 0f, val s: Float = 1f, val r: Float = 0f,
                        val o: Float = 1f, val d: Float = 0f, val card: Boolean)

internal class Ghost(val m: MessageUi, val delay: Int, val hasCard: Boolean, val hasBubble: Boolean, val toBubble: Boolean) {
    var from by mutableStateOf<Geo?>(null)
    var to by mutableStateOf<Geo?>(null)
    /** Size of the message (for laying out the file bubble's content). */
    var bubble: Size? = null
}

private class Captured(val key: Any, val x: Float, val y: Float, val above: Boolean, val layer: GraphicsLayer?)

/**
 * Expanding or collapsing one batch. Phases: [capturing] (rows draw into snapshot layers, positions
 * are read), then the switch to the new layout, then [t] runs from 0 to [total] ms. Meanwhile
 * ghosts stand in for the batch's items, the batch's real content is hidden, rows that stay on
 * screen slide from where they were (a translation read from the list layout in the draw phase, so
 * the first frame of the new layout never shows them in place) and rows pushed off screen are
 * drawn from their snapshots.
 */
internal class BatchMorph(val batch: String, val expanding: Boolean, val items: List<MessageUi>, val anchor: Int, val stackKey: Any) {
    var capturing by mutableStateOf(true)
    val t = Animatable(0f)
    var total = MORPH_MS
    val ghosts = mutableStateListOf<Ghost>()
    val layers = HashMap<Any, GraphicsLayer>()
    private val before = HashMap<Any, Float>()
    private val captured = ArrayList<Captured>()
    /** Container y of a list item = its layout offset + [k]. */
    var k = 0f
    var height = 0f
    /** Rows of this batch after the switch (keys); the header row slides from [headerFrom]. */
    var batchKeys: Set<Any> = emptySet()
    var headerKey: Any? = null
    var headerFrom: Float? = null
    /** List index of the batch's first row after the switch. */
    var batchIndex = 0

    val self get() = items.first().self
    fun isBatchRow(key: Any) = key == stackKey || items.any { it.id == key }
    fun rowProgress() = EaseStandard.transform((t.value / MORPH_MS).coerceIn(0f, 1f))

    /** Reads positions before the switch (no row is translated yet). */
    fun capture(info: LazyListLayoutInfo, registry: MorphRegistry, batchFirstIndex: Int): Boolean {
        val container = registry.container?.takeIf { it.isAttached } ?: return false
        height = container.size.height.toFloat()
        var haveK = false
        for (item in info.visibleItemsInfo) {
            val coords = registry.rows[item.key]?.takeIf { it.isAttached } ?: continue
            val at = container.localPositionOf(coords, Offset.Zero)
            if (!haveK) { k = at.y - item.offset; haveK = true }
            before[item.key] = at.y
            if (!isBatchRow(item.key)) captured += Captured(item.key, at.x, at.y, item.index < batchFirstIndex, layers[item.key])
        }
        return haveK
    }

    fun beforeY(key: Any) = before[key]

    private fun rowY(info: LazyListLayoutInfo, key: Any) = info.visibleItemsInfo.firstOrNull { it.key == key }?.let { it.offset + k }

    /** Untranslated rect of [coords] (inside row [rowKey]) in container px, from the current list layout. */
    fun rectIn(info: LazyListLayoutInfo, registry: MorphRegistry, rowKey: Any, coords: LayoutCoordinates?): Rect? {
        val row = registry.rows[rowKey]?.takeIf { it.isAttached } ?: return null
        val node = coords?.takeIf { it.isAttached } ?: return null
        val container = registry.container?.takeIf { it.isAttached } ?: return null
        val y = rowY(info, rowKey) ?: return null
        val inRow = row.localPositionOf(node, Offset.Zero)
        val x = container.localPositionOf(node, Offset.Zero).x
        return Rect(Offset(x, y + inRow.y), Size(node.size.width.toFloat(), node.size.height.toFloat()))
    }

    /** Translation of a row in the draw phase: from where it was (or from just outside the list) to where it is. */
    fun rowShift(key: Any, info: LazyListLayoutInfo): Float {
        if (capturing) return 0f
        if (key != headerKey && isBatchRow(key)) return 0f
        val items = info.visibleItemsInfo
        val item = items.firstOrNull { it.key == key } ?: return 0f
        val shift = knownShift(item) ?: if (item.index < batchIndex || key == headerKey) {
            // New above the batch (and a header that was off screen): comes in over the top edge,
            // and never in front of the first row below it that was on screen before.
            val group = items.filter { (it.index < batchIndex || it.key == headerKey) && knownShift(it) == null }
            val lowest = group.maxOfOrNull { it.offset + it.size + k } ?: return 0f
            val below = items.firstOrNull { it.index > group.maxOf { g -> g.index } && knownShift(it) != null }?.let(::knownShift)
            minOf(below ?: 0f, -lowest)
        } else {
            // New below: comes up from the bottom edge, never in front of the last row above it that was on screen.
            val group = items.filter { it.index > batchIndex && !isBatchRow(it.key) && knownShift(it) == null }
            val highest = group.minOfOrNull { it.offset + k } ?: return 0f
            val above = items.lastOrNull { it.index < group.minOf { g -> g.index } && knownShift(it) != null }?.let(::knownShift)
            maxOf(above ?: 0f, height - highest)
        }
        return shift * (1f - rowProgress())
    }

    /** How far a row that was on screen before (or the batch's header row) has to come from; null for new rows. */
    private fun knownShift(item: LazyListItemInfo): Float? {
        val start = (if (item.key == headerKey) headerFrom else before[item.key]) ?: return null
        return start - (item.offset + k)
    }

    /** Rows that were on screen and are no longer laid out: their snapshots slide out of the list. */
    fun drawDeparting(scope: DrawScope, info: LazyListLayoutInfo) {
        if (capturing) return
        val visible = info.visibleItemsInfo.mapTo(HashSet()) { it.key }
        val gone = captured.filter { it.key !in visible && it.layer != null }
        if (gone.isEmpty()) return
        // How far the rows that stay moved, per side; departing ones move at least that far and off screen.
        fun moved(above: Boolean) = captured.firstOrNull { it.above == above && it.key in visible }?.let { c -> rowY(info, c.key)?.minus(c.y) }
        val e = rowProgress()
        val firstBelow = gone.filter { !it.above }.minOfOrNull { it.y }
        val lastAbove = gone.filter { it.above }.maxOfOrNull { it.y + (it.layer?.size?.height ?: 0) }
        for (g in gone) {
            val layer = g.layer ?: continue
            val shift = if (g.above) minOf(moved(true) ?: 0f, -(lastAbove ?: 0f)) else maxOf(moved(false) ?: 0f, height - (firstBelow ?: height))
            scope.translate(g.x, g.y + shift * e) { drawLayer(layer) }
        }
    }

    // -- ghosts --

    private fun cardGeo(box: Offset, m: MessageUi, aspect: Float, pose: Pose, density: Float): Geo {
        val s = contain(aspect)
        return Geo(box.x + (CARD_WF - s.width) / 2f * density, box.y + (CARD_HF - s.height) / 2f * density, s.width * density, s.height * density,
            pose.tx * density, pose.s, pose.r, pose.o, pose.d, card = true)
    }
    private fun hiddenAt(box: Offset, m: MessageUi, aspect: Float, like: Geo, density: Float) =
        cardGeo(box, m, aspect, Pose(0f, .9f, 0f, 0f, 0f, 0f), density).copy(card = like.card)
    private fun bubbleGeo(r: Rect) = Geo(r.left, r.top, r.width, r.height, card = false)
    private fun delayOf(i: Int) = min(ringDistance(items.size, i, anchor), 6) * MORPH_STAGGER_MS

    /** Expand, before the switch: ghosts start on the cards (or hidden in the stack's centre). */
    fun startExpand(stackBox: Offset, aspects: (MessageUi) -> Float, density: Float) {
        val roles = stackRoles(items.size, anchor)
        // Farthest first: the top card's ghost is drawn last, on top.
        for (i in items.indices.sortedByDescending { ringDistance(items.size, it, anchor) }) {
            val m = items[i]
            val role = roles[i]
            val g = Ghost(m, delayOf(i), hasCard = role != null && poseOf(role).o > 0f, hasBubble = true, toBubble = true)
            if (role != null && poseOf(role).o > 0f) g.from = cardGeo(stackBox, m, aspects(m), poseOf(role), density)
            ghosts += g
        }
        total = MORPH_MS + (ghosts.maxOfOrNull { it.delay } ?: 0)
    }

    /** Expand, after the switch: the messages' rects (items laid out off screen head that way and fade). */
    fun finishExpand(info: LazyListLayoutInfo, registry: MorphRegistry, stackBox: Offset, aspects: (MessageUi) -> Float, density: Float) {
        val rects = items.associate { it.id to rectIn(info, registry, it.id, registry.bubbles[it.id]) }
        val topRect = rects[items[anchor].id]
        for (g in ghosts) {
            val i = items.indexOf(g.m)
            val r = rects[g.m.id]
            val onScreen = r != null && r.bottom > -40f * density && r.top < height + 40f * density
            if (r != null) g.bubble = r.size
            when {
                onScreen -> {
                    val to = bubbleGeo(r!!)
                    if (g.from == null) g.from = hiddenAt(stackBox, g.m, aspects(g.m), to, density)
                    g.to = to
                }
                g.from != null -> {
                    val f = g.from!!
                    val size = r?.size ?: if (g.m.kind == "image") contain(aspects(g.m), BATCH_MEDIA_WF, BATCH_MEDIA_HF).let { Size(it.width * density, it.height * density) }
                        else Size(BATCH_FILE_W.value * density, 70f * density)
                    if (g.bubble == null) g.bubble = size
                    val x = topRect?.let { if (self) it.right - size.width else it.left } ?: f.x
                    val above = r?.let { it.top < 0f } ?: (i < anchor)
                    val y = if (above) -size.height - 24f * density else height + 24f * density
                    g.to = Geo(x, y, size.width, size.height, o = 0f, card = false)
                }
            }
        }
    }

    /** Collapse, before the switch: ghosts start on the messages that are on screen. */
    fun startCollapse(info: LazyListLayoutInfo, registry: MorphRegistry, density: Float) {
        val roles = stackRoles(items.size, anchor)
        for (i in items.indices.sortedByDescending { ringDistance(items.size, it, anchor) }) {
            val m = items[i]
            val r = rectIn(info, registry, m.id, registry.bubbles[m.id])
            val onScreen = r != null && r.bottom > 0f && r.top < height
            val inStack = roles[i]?.let { poseOf(it).o > 0f } == true
            if (!onScreen && !inStack) continue
            val g = Ghost(m, delayOf(i), hasCard = inStack, hasBubble = onScreen, toBubble = false)
            if (onScreen) { g.from = bubbleGeo(r!!); g.bubble = r.size }
            ghosts += g
        }
        total = MORPH_MS + (ghosts.maxOfOrNull { it.delay } ?: 0)
    }

    /** The tapped message's top (container px), where the stack box will land. */
    fun anchorTop(info: LazyListLayoutInfo, registry: MorphRegistry) = rectIn(info, registry, items[anchor].id, registry.bubbles[items[anchor].id])?.top

    /** Collapse, after the switch: the cards' places around the tapped item. */
    fun finishCollapse(stackBox: Offset, aspects: (MessageUi) -> Float, density: Float) {
        val roles = stackRoles(items.size, anchor)
        for (g in ghosts) {
            val i = items.indexOf(g.m)
            val role = roles[i]
            val to = role?.let { poseOf(it) }?.takeIf { it.o > 0f }?.let { cardGeo(stackBox, g.m, aspects(g.m), it, density) }
            when {
                g.from != null && to != null -> g.to = to
                g.from != null -> g.to = hiddenAt(stackBox, g.m, aspects(g.m), g.from!!, density)
                to != null -> { g.from = to.copy(o = 0f, s = to.s * .94f); g.to = to }
            }
        }
    }

    fun release(graphics: GraphicsContext) {
        layers.values.forEach { graphics.releaseGraphicsLayer(it) }
        layers.clear()
    }
}

/** Overlay over the list while a morph runs: departing rows' snapshots, then the ghosts. */
@Composable
internal fun MorphOverlay(morph: BatchMorph, info: () -> LazyListLayoutInfo, modifier: Modifier) {
    Box(modifier.drawWithContent { morph.drawDeparting(this, info()); drawContent() }) {
        if (!morph.capturing) for (g in morph.ghosts) GhostView(g, morph)
    }
}

@Composable
private fun GhostView(g: Ghost, morph: BatchMorph) {
    val from = g.from ?: return
    val c = Oink.colors
    val density = LocalDensity.current
    val original = if (g.m.kind == "image") rememberImage(g.m.imagePath, 720) else null
    val thumbnail = if (g.m.kind == "image") rememberImage(g.m.thumbnailPath.takeIf { original == null }, 720) else null
    val image = original ?: thumbnail
    val e = if (morph.capturing) 0f else EaseStandard.transform(((morph.t.value - g.delay) / MORPH_MS).coerceIn(0f, 1f))
    val to = g.to ?: from
    val media = g.m.kind == "image"
    // Files are radius-12 cards both in the stack and as messages; pictures go from 9 to the bubble's corners.
    fun radii(geo: Geo): FloatArray {
        val r = if (!media) floatArrayOf(12f, 12f, 12f, 12f) else if (geo.card) floatArrayOf(9f, 9f, 9f, 9f)
            else if (g.m.self) floatArrayOf(18f, 4f, 18f, 18f) else floatArrayOf(4f, 18f, 18f, 18f)
        return FloatArray(4) { r[it] * density.density }
    }
    val ra = radii(from); val rb = radii(to)
    val shape = RoundedCornerShape(lerp(ra[0], rb[0], e), lerp(ra[1], rb[1], e), lerp(ra[2], rb[2], e), lerp(ra[3], rb[3], e))
    // A file keeps the card's surface and edge throughout; only the stack's shadow fades (file-bubble SPEC 1.5).
    val plate = if (media) Color.Transparent else c.stackCard
    val x = lerp(from.x, to.x, e); val y = lerp(from.y, to.y, e)
    val w = lerp(from.w, to.w, e); val h = lerp(from.h, to.h, e)
    val shadow = lerp(if (from.card) 1f else 0f, if (to.card) 1f else 0f, e)
    val d = lerp(from.d, to.d, e)
    with(density) {
        Box(Modifier.offset { IntOffset(x.roundToInt(), y.roundToInt()) }.size(w.toDp(), h.toDp())
            .graphicsLayer {
                translationX = lerp(from.tx, to.tx, e); scaleX = lerp(from.s, to.s, e); scaleY = scaleX
                rotationZ = lerp(from.r, to.r, e); alpha = lerp(from.o, to.o, e)
            }
            .stackShadow(c, (if (media) 9 else 12).dp, shadow).clip(shape).background(plate)
            .dim { c.stackDim * d }.then(if (media) Modifier.mediaEdge(c, shape, shown = image != null) else Modifier.border(1.dp, c.stackCardEdge, shape))) {
            if (media) {
                if (image != null) Image(image, null, Modifier.fillMaxSize(), contentScale = ContentScale.Crop)
                else Box(Modifier.fillMaxSize().background(c.fill))
            } else {
                // Card content stays put in the top-left corner and fades; the message content fades in (or the reverse).
                val cardAlpha = when { !g.hasCard -> 0f; !g.hasBubble -> 1f; g.toBubble -> 1f - (e / .45f).coerceIn(0f, 1f); else -> ((e - .55f) / .45f).coerceIn(0f, 1f) }
                val bubbleAlpha = when { !g.hasBubble -> 0f; !g.hasCard -> 1f; g.toBubble -> ((e - .3f) / .7f).coerceIn(0f, 1f); else -> 1f - (e / .45f).coerceIn(0f, 1f) }
                if (cardAlpha > 0f) Box(Modifier.wrapContentSize(Alignment.TopStart, unbounded = true).alpha(cardAlpha)) { FileCardContent(g.m) }
                val bubble = g.bubble
                if (bubbleAlpha > 0f && bubble != null) Box(Modifier.wrapContentSize(Alignment.TopStart, unbounded = true)
                    .requiredSize(bubble.width.toDp(), bubble.height.toDp()).alpha(bubbleAlpha)) {
                    FileCardRow(g.m, Modifier.fillMaxSize(), onDownload = null)
                }
            }
        }
    }
}
