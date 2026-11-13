package com.hachiwii.oink.ui

import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.StrokeJoin
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.graphics.drawscope.clipRect
import androidx.compose.ui.graphics.drawscope.withTransform
import androidx.compose.ui.graphics.vector.PathParser
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.foundation.gestures.detectVerticalDragGestures
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.semantics.LiveRegionMode
import androidx.compose.ui.semantics.clearAndSetSemantics
import androidx.compose.ui.semantics.liveRegion
import androidx.compose.ui.text.style.LineBreak
import androidx.compose.foundation.text.BasicText
import androidx.compose.animation.Crossfade
import androidx.compose.animation.animateContentSize
import androidx.compose.animation.core.snap
import com.hachiwii.oink.ShareStatusText
import kotlinx.coroutines.launch
import kotlin.math.PI
import kotlin.math.hypot
import kotlin.math.sin

// Share card stage drawing (ShareCard.jsx, share-anim SPEC); the maths is in ShareAnim.
private fun trailShape(pts: List<Offset>): Pair<Path, Pair<Offset, Offset>>? {
    var len = 0f; for (i in 1 until pts.size) len += hypot(pts[i].x - pts[i - 1].x, pts[i].y - pts[i - 1].y)
    if (len < .5f) return null
    val l = ArrayList<Offset>(); val r = ArrayList<Offset>(); val n = pts.lastIndex
    for (k in 0..n) {
        val a = pts[maxOf(0, k - 1)]; val b = pts[minOf(n, k + 1)]
        val tx = a.x - b.x; val ty = a.y - b.y; val tl = hypot(tx, ty).let { if (it == 0f) 1f else it }
        val nx = -ty / tl; val ny = tx / tl; val h = (3.3f + (1f - 3.3f) * k / n) / 2
        l += Offset(pts[k].x + nx * h, pts[k].y + ny * h); r += Offset(pts[k].x - nx * h, pts[k].y - ny * h)
    }
    return Path().apply { moveTo(l[0].x, l[0].y); l.drop(1).forEach { lineTo(it.x, it.y) }; r.asReversed().forEach { lineTo(it.x, it.y) }; close() } to (pts[0] to pts[n])
}
private const val HEAD = "M18.48 0L0.31 -7.87L-0.31 7.87Q9.78 5.71 18.48 0Z"
private const val LANDED_TAIL = "M20.5 32C20.5 46 31 54.5 45 54.5A9.5 9.5 0 1 0 36.3 41.2"
private const val LANDED_ARROW = "M9 6V39L33.5 29.5Q23.5 15.5 9 6Z"
/** Offline mark (SPEC §3.8): Wi-Fi arcs, dot and slash around the dot centre. */
private const val OFFLINE_ARCS = "M-7.07 -7.07A10 10 0 0 1 7.07 -7.07M-3.89 -3.89A5.5 5.5 0 0 1 3.89 -3.89"
private fun path(d: String) = PathParser().parsePathString(d).toPath()
/** The Oink mark on this phone while waiting (SPEC §3.7). */
private val LOCAL_MARK = Offset(25.8f, 42.8f)
private const val LOCAL_MARK_SCALE = .32f

/** What the stage shows in one frame: the flight (null: no arrow) and the waiting marks' opacities. */
private class StageFrame(val flight: ShareFrame?, val arrowAlpha: Float, val logo: Float, val offline: Float)

/**
 * Share-extension send card over its host (ShareCard.jsx, share-anim SPEC). [host] draws the
 * mockup's gallery host (design scenes only). While sending it offers a small 取消 and, with
 * [onHide], 隐藏 (also a downward swipe): the share then goes on in the background with its
 * progress in a notification (native-rewrite.md 「手机点对点传输」). While it waits for its offline
 * target (SPEC §2.1) the stage stands still with Oink on this phone and the offline mark on the
 * target, the text says so on two lines, and 取消 is the only way out (no 隐藏, no swipe). When the
 * target comes online the card hands over to sending in 300 ms (§5.9). [reduceMotion]: Android
 * 「移除动画」 (SPEC §7).
 */
@Composable
fun ShareCardLayer(s: ShareCardUi, host: Boolean = s.freezeT != null, onHide: (() -> Unit)? = null, onCancel: (() -> Unit)? = null,
                   reduceMotion: Boolean = !android.animation.ValueAnimator.areAnimatorsEnabled()) {
    val c = Oink.colors
    val waiting = s.phase == "waiting"
    val done = s.phase == "done"
    val target = if (s.to != null) ShareTarget.PHONE else ShareTarget.COMPUTER
    val frozen = s.freezeT != null || s.freezeHandoff != null
    // Own frame clock (nanoseconds): the hand-over needs the first round to start 160 ms after the
    // target came online, which an infinite transition starting at 0 cannot do (SPEC §8.3).
    var now by remember { mutableLongStateOf(0L) }
    var flightZero by remember { mutableStateOf<Long?>(null) }
    var onlineAt by remember { mutableStateOf<Long?>(null) }
    var waitAt by remember { mutableStateOf<Long?>(null) }
    val wasWaiting = remember { mutableStateOf<Boolean?>(null) }
    LaunchedEffect(waiting) {
        val before = wasWaiting.value
        wasWaiting.value = waiting
        // Shown waiting from the start, or reduced motion: no transition (SPEC §2.1, §7).
        if (before == null || before == waiting || reduceMotion || frozen) return@LaunchedEffect
        val at = withFrameNanos { it }
        if (waiting) { waitAt = at; onlineAt = null } else { onlineAt = at; waitAt = null; flightZero = at + (ShareAnim.LEAD_MS * 1e6f).toLong() }
    }
    fun msSince(at: Long?) = at?.let { (now - it) / 1e6f }
    val entering = waiting && (msSince(waitAt)?.let { it < ShareAnim.ENTER_WAIT_MS } ?: false)
    val indeterminate = s.phase == "indeterminate"
    // The stage stands still while waiting (no time source, SPEC §5.9) once the 150 ms entry is over.
    val ticking = !frozen && !done && (if (waiting) entering else !reduceMotion || indeterminate)
    LaunchedEffect(ticking) {
        if (ticking) while (true) withFrameNanos { frame -> now = frame; if (flightZero == null) flightZero = frame }
    }
    val handoffMs = s.freezeHandoff ?: msSince(onlineAt)?.takeIf { !waiting && !done && it < ShareAnim.HANDOFF_MS }
    val handoff = handoffMs?.let(ShareAnim::handoff)
    val liveT = flightZero?.let { zero -> if (now < zero) null else (((now - zero) / 1e6f) % ShareAnim.D) / ShareAnim.D } ?: 0f
    val stage = when {
        done -> StageFrame(null, 0f, 0f, 0f)
        waiting && entering -> {
            val x = ShareAnim.enterWait(msSince(waitAt) ?: 0f)
            StageFrame(ShareAnim.frame(if (reduceMotion) ShareAnim.STATIC_T else liveT, target), 1 - x, x, x)
        }
        waiting -> StageFrame(null, 0f, 1f, 1f)
        handoff != null -> StageFrame(handoff.t?.let { ShareAnim.frame(it, target) }, 1f, handoff.logo, handoff.offline)
        s.freezeT != null -> StageFrame(ShareAnim.frame(s.freezeT, target), 1f, 0f, 0f)
        reduceMotion -> StageFrame(ShareAnim.frame(ShareAnim.STATIC_T, target), 1f, 0f, 0f)
        else -> StageFrame(ShareAnim.frame(liveT, target), 1f, 0f, 0f)
    }
    val barT = handoff?.let { it.t ?: 0f } ?: s.freezeT ?: liveT
    val barAlpha = when {
        waiting -> if (entering) 1 - ShareAnim.enterWait(msSince(waitAt) ?: 0f) else 0f
        handoff != null -> handoff.bar
        else -> 1f
    }
    val message = when {
        done -> ShareStatusText.completed(s.elapsed, s.to)
        s.phase == "sending" -> ShareStatusText.sending(s.rate, s.to)
        else -> ShareStatusText.sending(null, s.to)
    }
    val drag = remember { androidx.compose.animation.core.Animatable(0f) }
    val scope = rememberCoroutineScope()
    val canHide = onHide != null && !done && !waiting
    Box(Modifier.fillMaxSize().then(if (host) Modifier.background(c.bg) else Modifier).press(pressedColor = null) {}) {
        if (host) Column(Modifier.fillMaxSize().windowInsetsPadding(WindowInsets.safeDrawing).padding(horizontal = 16.dp)) {
            T("图库", 28f, c.ink, Modifier.padding(start = 4.dp, end = 4.dp, top = 14.dp, bottom = 12.dp), weight = 700, lineHeight = 34f)
            BoxWithConstraints(Modifier.fillMaxWidth()) {
                val cell = (maxWidth - 6.dp) / 3
                Column(verticalArrangement = Arrangement.spacedBy(3.dp)) {
                    repeat(6) { row -> Row(horizontalArrangement = Arrangement.spacedBy(3.dp)) { repeat(3) { col ->
                        val i = row * 3 + col
                        Box(Modifier.size(cell).background(if (i % 3 == 1) c.shareTile2 else c.shareTile).then(if (i == 4) Modifier.drawBehind {
                            val w = 3.dp.toPx(); drawRect(c.systemApp, Offset(w / 2, w / 2), Size(size.width - w, size.height - w), style = Stroke(w))
                        } else Modifier))
                    } } }
                }
            }
        }
        Box(Modifier.fillMaxSize().background(c.shareScrim))
        Column(Modifier.align(Alignment.Center).windowInsetsPadding(WindowInsets.safeDrawing).padding(horizontal = 24.dp).widthIn(max = 272.dp).fillMaxWidth()
            .graphicsLayer { translationY = drag.value }
            .then(if (canHide) Modifier.pointerInput(Unit) {
                // Swiping the card down hides it, like 隐藏.
                detectVerticalDragGestures(onDragEnd = { scope.launch { if (drag.value > 70.dp.toPx()) onHide?.invoke() else drag.animateTo(0f) } },
                    onDragCancel = { scope.launch { drag.animateTo(0f) } }) { _, d -> scope.launch { drag.snapTo((drag.value + d).coerceAtLeast(0f)) } }
            } else Modifier)
            .boxShadow(c.shareShadowFar, 12.dp, 32.dp, 20.dp).boxShadow(c.shareShadowNear, 1.dp, 2.dp, 20.dp)
            .clip(RoundedCornerShape(20.dp)).background(c.shareSurface).border(1.dp, c.shareBorder, RoundedCornerShape(20.dp))
            // 226 (waiting) ↔ 224 (sending) in 200 ms (SPEC §5.9); no transition with reduced motion.
            .then(if (reduceMotion || frozen) Modifier else Modifier.animateContentSize(tween(200, easing = ShareAnim.easeStandard)))
            .padding(horizontal = 16.dp, vertical = 20.dp).semantics { contentDescription = "Oink 正在发送" }, horizontalAlignment = Alignment.CenterHorizontally) {
            Stage(stage, target, done)
            // The waiting text and the status line cross-fade in 150 ms (SPEC §5.9).
            Crossfade(waiting, Modifier.padding(top = 12.dp).fillMaxWidth(),
                animationSpec = if (reduceMotion || frozen) snap() else tween(150, easing = LinearEasing), label = "share-text") { wait ->
                if (wait) BasicText(ShareStatusText.waitingForTarget(),
                    Modifier.fillMaxWidth().semantics { liveRegion = LiveRegionMode.Polite },
                    // Chinese breaks after 「Oink」 by its own newline; English balances its two lines.
                    style = textStyle(13f, c.shareText, lineHeight = 20f, align = TextAlign.Center).copy(lineBreak = LineBreak.Paragraph))
                else T(message, 13f, c.shareText, Modifier.fillMaxWidth().height(20.dp), lineHeight = 20f, align = TextAlign.Center, ellipsis = true, tabular = true)
            }
            val progress = if (done) 100 else s.progress
            val width by animateFloatAsState(progress / 100f, if (reduceMotion) snap() else tween(250, easing = LinearEasing), label = "bar")
            // No bar while waiting, not even its track (SPEC §2.1); it fades in during the hand-over.
            if (!waiting || entering) Canvas(Modifier.padding(top = 12.dp).widthIn(max = 240.dp).fillMaxWidth().height(6.dp)
                .graphicsLayer { alpha = barAlpha }.clip(RoundedCornerShape(3.dp)).background(c.shareTrack)) {
                val r = CornerRadius(3.dp.toPx())
                if (indeterminate && reduceMotion) {
                    // Reduced motion: the whole bar breathes between 0.35 and 0.70 every 2.4 s (SPEC §7).
                    val breath = .525f + .175f * sin(2 * PI * (now / 1e6 % 2400.0) / 2400.0).toFloat()
                    drawRoundRect(c.accentInk.copy(alpha = breath), Offset.Zero, size, r)
                } else if (indeterminate) clipRect {
                    val w = size.width * .35f
                    drawRoundRect(c.accentInk, Offset(size.width * ShareAnim.barLeft(barT), 0f), Size(w, size.height), r)
                } else drawRoundRect(c.accentInk, Offset.Zero, Size(size.width * width, size.height), r)
            }
            if (!done && (canHide || onCancel != null)) Row(Modifier.padding(top = 16.dp), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                if (canHide) CardButton(appText("隐藏", "Hide")) { onHide?.invoke() }
                if (onCancel != null) CardButton(appText("取消", "Cancel"), onCancel)
            }
        }
    }
}

/** sh-cancel: a small soft capsule, 30 high. */
@Composable
private fun CardButton(text: String, onClick: () -> Unit) {
    val c = Oink.colors
    Box(Modifier.height(30.dp).clip(RoundedCornerShape(15.dp)).background(c.chip).press(pressedColor = c.press) { onClick() }.padding(horizontal = 14.dp),
        contentAlignment = Alignment.Center) { T(text, 13f, c.text2, weight = 500, lineHeight = 18.2f) }
}

/**
 * The 240 × 88 stage (SPEC §3): this phone, the target ([target]: a computer or a phone), the flight
 * of [f], the landed mark when [done], and while waiting the Oink mark on this phone and the offline
 * mark on the target.
 */
@Composable
private fun Stage(f: StageFrame, target: ShareTarget, done: Boolean) {
    val c = Oink.colors
    val head = remember { path(HEAD) }; val tail = remember { path(LANDED_TAIL) }; val arrow = remember { path(LANDED_ARROW) }
    val arcs = remember { path(OFFLINE_ARCS) }
    Canvas(Modifier.size(240.dp, 88.dp).clearAndSetSemantics {}) {
        withTransform({ scale(size.width / 240f, size.height / 88f, Offset.Zero) }) {
            val stroke = Stroke(2f, cap = StrokeCap.Butt, join = StrokeJoin.Round)
            drawRoundRect(c.shareDevice, Offset(23f, 30f), Size(26f, 46f), CornerRadius(5.5f), style = stroke)
            drawLine(c.shareDevice, Offset(32.5f, 35.5f), Offset(39.5f, 35.5f), 2f)
            drawLine(c.shareDevice, Offset(32f, 70.5f), Offset(40f, 70.5f), 2f)
            val flight = f.flight
            val nudge = if (done) 1f else flight?.nudge ?: 1f
            withTransform({ scale(nudge, nudge, target.nudgeCenter) }) {
                if (target == ShareTarget.PHONE) {
                    // This phone mirrored about the stage's centre line (SPEC §3.6).
                    drawRoundRect(c.shareDevice, Offset(191f, 30f), Size(26f, 46f), CornerRadius(5.5f), style = stroke)
                    drawLine(c.shareDevice, Offset(200.5f, 35.5f), Offset(207.5f, 35.5f), 2f)
                    drawLine(c.shareDevice, Offset(200f, 70.5f), Offset(208f, 70.5f), 2f)
                } else {
                    drawRoundRect(c.shareDevice, Offset(177f, 36f), Size(46f, 31f), CornerRadius(3f), style = stroke)
                    drawLine(c.shareDevice, Offset(169f, 72f), Offset(231f, 72f), 2f)
                }
            }
            fun mark(at: Offset, k: Float, alpha: Float) = withTransform({ translate(at.x, at.y); scale(k, k, Offset.Zero); translate(0f, 1.5f) }) {
                drawPath(tail, c.accent, alpha = alpha, style = Stroke(5.5f, cap = StrokeCap.Round))
                drawPath(arrow, c.shareArrow, alpha = alpha); drawPath(arrow, c.shareArrow, alpha = alpha, style = Stroke(5f, join = StrokeJoin.Round))
            }
            if (done) mark(target.landed, target.landedScale, 1f)
            if (f.logo > .001f) mark(LOCAL_MARK, LOCAL_MARK_SCALE, f.logo)
            if (f.offline > .001f) withTransform({ translate(target.offlineOrigin.x, target.offlineOrigin.y) }) {
                // Arcs and dot, then the slash cut out of the arcs by a surface-coloured stroke (SPEC §3.8).
                drawPath(arcs, c.faint, alpha = f.offline, style = Stroke(2f, cap = StrokeCap.Round))
                drawCircle(c.faint, 1.6f, Offset.Zero, alpha = f.offline)
                drawLine(c.shareSurface, Offset(-7f, -12.5f), Offset(7f, 1.5f), 4f, StrokeCap.Round, alpha = f.offline)
                drawLine(c.faint, Offset(-7f, -12.5f), Offset(7f, 1.5f), 2f, StrokeCap.Round, alpha = f.offline)
            }
            if (flight == null) return@withTransform
            val opacity = flight.opacity * f.arrowAlpha
            val tr = trailShape(flight.trail)
            if (tr != null && opacity > .001f) drawPath(tr.first, Brush.linearGradient(0f to c.accent, .4f to c.accent.copy(alpha = .8f), 1f to c.accent.copy(alpha = 0f),
                start = tr.second.first, end = tr.second.second), alpha = opacity)
            if (opacity > .001f) withTransform({ translate(flight.p.x, flight.p.y); rotate(flight.angle, Offset.Zero); scale(flight.scale, flight.scale, Offset.Zero) }) {
                drawPath(head, c.shareSurface, alpha = opacity, style = Stroke(6f, join = StrokeJoin.Round))
                drawPath(head, c.shareArrow, alpha = opacity); drawPath(head, c.shareArrow, alpha = opacity, style = Stroke(3f, join = StrokeJoin.Round))
            }
        }
    }
}

/** The share extension: the card alone, over the app that shared. */
@Composable
fun ShareOnly(card: ShareCardUi, prefs: Prefs, systemDark: Boolean, onHide: (() -> Unit)? = null, onCancel: (() -> Unit)? = null) {
    val dark = when (prefs.scheme) { "dark" -> true; "light" -> false; else -> systemDark }
    OinkTheme(remember(dark, prefs.accent) { OinkColors.of(dark, prefs.accent) }) { ShareCardLayer(card, host = false, onHide = onHide, onCancel = onCancel) }
}
