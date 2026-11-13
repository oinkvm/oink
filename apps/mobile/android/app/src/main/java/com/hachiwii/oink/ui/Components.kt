package com.hachiwii.oink.ui

import android.graphics.BlurMaskFilter
import android.view.HapticFeedbackConstants
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.CubicBezierEasing
import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.animateColorAsState
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.keyframes
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.interaction.collectIsPressedAsState
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicText
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Shape
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.graphics.drawscope.rotate
import androidx.compose.ui.graphics.nativeCanvas
import androidx.compose.ui.graphics.drawscope.drawIntoCanvas
import androidx.compose.ui.graphics.toArgb
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.text.PlatformTextStyle
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.LineHeightStyle
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

val EaseOutExpo = CubicBezierEasing(.22f, 1f, .36f, 1f)

/**
 * The mockup's weights (400, 500, 550, 600, 650, 700) on the system font. Xiaomi HyperOS's MiSans
 * renders every weight above 600 as regular through Typeface.create(…, weight) (verified on the
 * Phone, Android 15: 650 and 700 come out at 400 while 600 is bold), so the heavy weights use
 * 600, which reads as the mockup's semibold/bold on every system font; 550 uses 500.
 */
fun systemWeight(css: Int) = FontWeight(when { css >= 600 -> 600; css > 500 -> 500; else -> css })

/** CSS text: size and line height in px (= dp / sp), weight 100–900, letter spacing in px. */
fun textStyle(size: Float, color: Color, weight: Int = 400, lineHeight: Float = size * 1.4f, letterSpacing: Float = 0f,
              align: TextAlign = TextAlign.Unspecified, tabular: Boolean = false) = TextStyle(
    color = color, fontSize = size.sp, fontWeight = systemWeight(weight), lineHeight = lineHeight.sp,
    letterSpacing = letterSpacing.sp, textAlign = align, fontFeatureSettings = if (tabular) "tnum" else null,
    lineHeightStyle = LineHeightStyle(LineHeightStyle.Alignment.Center, LineHeightStyle.Trim.None),
    platformStyle = PlatformTextStyle(includeFontPadding = false),
)

@Composable
fun T(text: String, size: Float, color: Color, modifier: Modifier = Modifier, weight: Int = 400, lineHeight: Float = size * 1.4f,
      letterSpacing: Float = 0f, align: TextAlign = TextAlign.Unspecified, maxLines: Int = Int.MAX_VALUE,
      ellipsis: Boolean = false, tabular: Boolean = false) {
    // CSS may break a line after a hyphen even before a digit ("DESKTOP-" / "9AO8GPM"); ICU does not,
    // so a zero-width space after each hyphen gives Android the same break opportunity.
    BasicText(text.replace("-", "-\u200B"), modifier, textStyle(size, color, weight, lineHeight, letterSpacing, align, tabular),
        overflow = if (ellipsis) TextOverflow.Ellipsis else TextOverflow.Clip, maxLines = if (ellipsis && maxLines == Int.MAX_VALUE) 1 else maxLines,
        softWrap = !(ellipsis && maxLines == Int.MAX_VALUE))
}

/** Haptics per docs/settings-and-pairing.md 4.6: tap 10 ms → CLOCK_TICK/CONTEXT_CLICK, long/err → LONG_PRESS/REJECT. */
@Composable
fun rememberHaptics(): (Boolean) -> Unit {
    val view = LocalView.current
    return remember(view) { { long -> view.performHapticFeedback(if (long) HapticFeedbackConstants.LONG_PRESS else HapticFeedbackConstants.CONTEXT_CLICK) } }
}

/** Click without ripple; the mockup's :active states are drawn by [pressedColor] when given. */
@Composable
fun Modifier.press(enabled: Boolean = true, shape: Shape? = null, pressedColor: Color? = null, role: Role = Role.Button, onClick: () -> Unit): Modifier {
    val source = remember { MutableInteractionSource() }
    val pressed by source.collectIsPressedAsState()
    val bg = if (pressed && enabled && pressedColor != null) Modifier.background(pressedColor, shape ?: RoundedCornerShape(0.dp)) else Modifier
    return this.then(if (shape != null) Modifier.clip(shape) else Modifier).then(bg)
        .clickable(source, indication = null, enabled = enabled, role = role, onClick = onClick)
}

/** CSS box-shadow drawn outside the element only (CSS never paints a shadow under its box). */
fun Modifier.boxShadow(color: Color, y: Dp, blur: Dp, radius: Dp, spread: Dp = 0.dp, x: Dp = 0.dp) = drawBehind {
    if (color.alpha == 0f) return@drawBehind
    val sigma = blur.toPx() / 2f
    val r = radius.toPx()
    drawIntoCanvas { canvas ->
        val nc = canvas.nativeCanvas
        nc.save()
        val own = android.graphics.Path().apply { addRoundRect(0f, 0f, size.width, size.height, r, r, android.graphics.Path.Direction.CW) }
        nc.clipOutPath(own)
        val paint = android.graphics.Paint(android.graphics.Paint.ANTI_ALIAS_FLAG).apply {
            this.color = color.toArgb()
            // Skia blur radius → sigma is 0.57735 r + 0.5; CSS blur B has sigma B / 2.
            if (sigma > 0.5f) maskFilter = BlurMaskFilter((sigma - .5f) / .57735f, BlurMaskFilter.Blur.NORMAL)
        }
        val s = spread.toPx(); val dx = x.toPx(); val dy = y.toPx()
        nc.drawRoundRect(-s + dx, -s + dy, size.width + s + dx, size.height + s + dy, r + s, r + s, paint)
        nc.restore()
    }
}

/** Material 3 switch as drawn in the mockup (ui-kit Switch, os android): 52 × 32. */
@Composable
fun OinkSwitch(checked: Boolean, disabled: Boolean = false) {
    val c = Oink.colors
    val t by animateFloatAsState(if (checked) 1f else 0f, tween(250, easing = EaseOutExpo), label = "switch")
    val track by animateColorAsState(if (checked) c.accent else c.chip, tween(220), label = "track")
    val edge by animateColorAsState(if (checked) c.accent else c.outline, tween(220), label = "edge")
    val thumb by animateColorAsState(if (checked) c.thumb else c.outline, tween(200), label = "thumb")
    Box(Modifier.size(52.dp, 32.dp).alpha(if (disabled) .38f else 1f).clip(CircleShape).background(track).border(2.dp, edge, CircleShape)) {
        val d = 16.dp + 8.dp * t
        // Thumb: left 6 inside the 2 px border, 16 → 24 when on, moved 16 px right.
        Box(Modifier.offset(x = 8.dp + 16.dp * t, y = 0.dp).align(Alignment.CenterStart).size(d).clip(CircleShape).background(thumb), contentAlignment = Alignment.Center) {
            if (checked) Lucide(DesignIconData.lucideCheck, 14.dp, c.accent, 3f)
        }
    }
}

@Composable
fun OinkCheckbox(checked: Boolean) {
    val c = Oink.colors
    Box(Modifier.size(20.dp).clip(RoundedCornerShape(5.dp)).background(if (checked) c.accent else Color.Transparent)
        .border(2.dp, if (checked) c.accent else c.outline, RoundedCornerShape(5.dp)), contentAlignment = Alignment.Center) {
        if (checked) Lucide(DesignIconData.lucideCheck, 14.dp, c.onAccent, 3.2f)
    }
}

@Composable
fun OinkRadio(checked: Boolean) {
    val c = Oink.colors
    Box(Modifier.size(20.dp).border(2.dp, if (checked) c.accent else c.outline, CircleShape), contentAlignment = Alignment.Center) {
        if (checked) Box(Modifier.size(10.dp).clip(CircleShape).background(c.accent))
    }
}

/** ui-kit Spinner: 30 % arc of r 9.5 in a 24 viewBox, stroke 2.6, one turn per 0.9 s. */
@Composable
fun Spinner(size: Dp, color: Color = Oink.colors.accent, modifier: Modifier = Modifier) {
    val turn by rememberInfiniteTransition(label = "spin").animateFloat(0f, 360f, infiniteRepeatable(tween(900, easing = LinearEasing)), label = "turn")
    Canvas(modifier.size(size)) {
        val u = this.size.width / 24f
        rotate(turn) {
            // SVG strokes start at 3 o'clock and the dash covers 30 of 100 path units.
            drawArc(color, 0f, 108f, false, topLeft = Offset(2.5f * u, 2.5f * u), size = Size(19f * u, 19f * u), style = Stroke(2.6f * u, cap = StrokeCap.Round))
        }
    }
}

/** ui-kit Dots: three 4 px dots, opacity .25 → 1 → .25 over 1.2 s, staggered .15 s. */
@Composable
fun Dots(color: Color) {
    val tr = rememberInfiniteTransition(label = "dots")
    Row(horizontalArrangement = Arrangement.spacedBy(3.dp), verticalAlignment = Alignment.CenterVertically) {
        repeat(3) { i ->
            val a by tr.animateFloat(.25f, .25f, infiniteRepeatable(keyframes {
                durationMillis = 1200
                .25f at (i * 150)
                1f at (i * 150 + 480)
                .25f at minOf(1199, i * 150 + 960)
            }, RepeatMode.Restart), label = "dot$i")
            Box(Modifier.size(4.dp).alpha(a).clip(CircleShape).background(color))
        }
    }
}

data class ToastData(val text: String, val action: String? = null, val onAction: (() -> Unit)? = null, val id: Long = System.nanoTime())

/** ui-kit Toast: pill at top 72 of its layer, optional action button. */
@Composable
fun BoxScope.TopToast(toast: ToastData?, top: Dp = 72.dp, onAction: () -> Unit = {}) {
    val c = Oink.colors
    if (toast == null) return
    Row(Modifier.align(Alignment.TopCenter).padding(top = top).widthIn(max = 361.dp)
        .boxShadow(c.shadow, 4.dp, 16.dp, 21.dp).clip(RoundedCornerShape(21.dp)).background(c.toast)
        .heightIn(min = 42.dp).padding(start = 16.dp, end = 8.dp, top = 8.dp, bottom = 8.dp),
        verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(14.dp)) {
        T(toast.text, 13f, c.toastInk, Modifier.padding(end = 8.dp), lineHeight = 18.2f)
        if (toast.action != null) Box(Modifier.height(30.dp).clip(RoundedCornerShape(15.dp)).background(c.toastActionBg)
            .press { onAction() }.padding(horizontal = 12.dp), contentAlignment = Alignment.Center) {
            T(toast.action, 13f, c.toastAction, weight = 600, lineHeight = 18.2f)
        }
    }
}

/** Shows [text] for [ms] then clears; a new value restarts the timer. */
@Composable
fun rememberToast(freeze: Boolean = false): MutableState<ToastData?> {
    val state = remember { mutableStateOf<ToastData?>(null) }
    val current = state.value
    LaunchedEffect(current?.id) {
        if (current != null && !freeze) { kotlinx.coroutines.delay(if (current.action != null) 4000 else 2000); if (state.value?.id == current.id) state.value = null }
    }
    return state
}

/** Capsule buttons (settings-pairing.css .sp-capsule-*). */
@Composable
fun PrimaryCapsule(text: String, modifier: Modifier = Modifier, enabled: Boolean = true, leading: (@Composable () -> Unit)? = null, trailing: (@Composable () -> Unit)? = null, horizontalPadding: Dp = 24.dp, onClick: () -> Unit) {
    val c = Oink.colors
    Row(modifier.alpha(if (enabled) 1f else .4f).height(48.dp).clip(RoundedCornerShape(24.dp)).background(c.accentSoft)
        .press(enabled) { onClick() }.padding(horizontal = horizontalPadding), horizontalArrangement = Arrangement.spacedBy(6.dp, Alignment.CenterHorizontally), verticalAlignment = Alignment.CenterVertically) {
        leading?.invoke(); T(text, 15f, c.accentInk, weight = 600, lineHeight = 21f); trailing?.invoke()
    }
}

@Composable
fun SoftCapsule(text: String, modifier: Modifier = Modifier, enabled: Boolean = true, leading: (@Composable () -> Unit)? = null, trailing: (@Composable () -> Unit)? = null, onClick: () -> Unit) {
    val c = Oink.colors
    Row(modifier.alpha(if (enabled) 1f else .4f).height(44.dp).clip(RoundedCornerShape(22.dp)).background(c.chip)
        .press(enabled) { onClick() }.padding(horizontal = 22.dp), horizontalArrangement = Arrangement.spacedBy(6.dp, Alignment.CenterHorizontally), verticalAlignment = Alignment.CenterVertically) {
        leading?.invoke(); T(text, 14f, c.text2, weight = 500, lineHeight = 19.6f); trailing?.invoke()
    }
}

class DialogAction(val label: String, val danger: Boolean = false, val onClick: () -> Unit)

/** Material 3 dialog as drawn in the mockup (ui-kit Dialog, os android). Drawn in the app's own layer tree. */
@Composable
fun OinkDialog(title: String, onDismiss: () -> Unit, actions: List<DialogAction>, content: @Composable ColumnScope.() -> Unit = {}) {
    val c = Oink.colors
    androidx.activity.compose.BackHandler { onDismiss() }
    Box(Modifier.fillMaxSize().background(c.overlay).press(pressedColor = null) { onDismiss() }.windowInsetsPadding(WindowInsets.safeDrawing).padding(24.dp), contentAlignment = Alignment.Center) {
        Column(Modifier.widthIn(max = 320.dp).fillMaxWidth().boxShadow(c.shadow, 12.dp, 40.dp, 28.dp)
            .clip(RoundedCornerShape(28.dp)).background(c.elev).press(pressedColor = null) {}
            .padding(start = 24.dp, end = 24.dp, top = 24.dp, bottom = 16.dp)) {
            T(title, 21f, c.ink, weight = 600, lineHeight = 28.35f)
            Spacer(Modifier.height(14.dp))
            content()
            Row(Modifier.fillMaxWidth().padding(top = 20.dp), horizontalArrangement = Arrangement.spacedBy(4.dp, Alignment.End)) {
                actions.forEach { a ->
                    Box(Modifier.height(40.dp).clip(RoundedCornerShape(20.dp)).press(pressedColor = c.press) { a.onClick() }.padding(horizontal = 14.dp), contentAlignment = Alignment.Center) {
                        T(a.label, 14f, if (a.danger) c.badInk else c.accentInk, weight = 600, lineHeight = 19.6f)
                    }
                }
            }
        }
    }
}

@Composable
fun DialogBody(text: String) = T(text, 14f, Oink.colors.text2, lineHeight = 22.4f)

/** Six one-time-code cells over one hidden numeric field (ui-kit CodeCells). */
@Composable
fun CodeCells(value: String, cellW: Dp = 42.dp, cellH: Dp = 54.dp, fontSize: Float = 26f, start: Boolean = false,
              invalid: Boolean = false, disabled: Boolean = false, showActive: Boolean = true, cellBg: Color = Oink.colors.fill,
              digitsOnly: Boolean = false, success: Boolean = false, focused: Boolean = true, onClick: (() -> Unit)? = null) {
    val c = Oink.colors
    Row(Modifier.then(if (onClick != null) Modifier.press { onClick() } else Modifier), horizontalArrangement = if (start) Arrangement.Start else Arrangement.Center) {
        repeat(6) { i ->
            val active = showActive && focused && i == minOf(value.length, 5) && !disabled && !invalid && !digitsOnly
            val ch = value.getOrNull(i)?.toString() ?: ""
            if (i > 0) Spacer(Modifier.width(if (i == 3) 14.dp else 6.dp))
            val shape = RoundedCornerShape(14.dp)
            Box(Modifier.size(cellW, cellH)
                .then(if (active) Modifier.drawBehind {
                    val r = 14.dp.toPx(); val g = 3.dp.toPx()
                    drawRoundRect(c.accentSoft, Offset(-g, -g), Size(size.width + 2 * g, size.height + 2 * g), androidx.compose.ui.geometry.CornerRadius(r + g))
                } else Modifier)
                .clip(shape).background(if (digitsOnly) Color.Transparent else cellBg)
                .border(1.5.dp, when { digitsOnly -> Color.Transparent; invalid -> c.bad; active -> c.accent; else -> c.track }, shape),
                contentAlignment = Alignment.Center) {
                T(ch, fontSize, when { success -> c.goodInk; invalid -> c.badInk; disabled -> c.muted; else -> c.ink }, weight = 600, lineHeight = fontSize * 1.4f, tabular = true)
            }
        }
    }
}

/** Shake used for invalid input (520 ms, cubic-bezier(.22,1,.36,1)). */
@Composable
fun rememberShake(key: Any?): Float {
    val x = remember { Animatable(0f) }
    LaunchedEffect(key) {
        if (key == null) return@LaunchedEffect
        x.snapTo(0f)
        x.animateTo(0f, keyframes { durationMillis = 520; -7f at 104; 6f at 208; -4f at 312; 2f at 416; 0f at 520 })
    }
    return x.value
}
