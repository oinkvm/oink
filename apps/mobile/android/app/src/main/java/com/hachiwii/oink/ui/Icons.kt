package com.hachiwii.oink.ui

import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.requiredSize
import androidx.compose.foundation.layout.size
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.StrokeJoin
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.graphics.vector.PathParser
import androidx.compose.ui.graphics.vector.addPathNodes
import androidx.compose.ui.graphics.vector.rememberVectorPainter
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp

/**
 * One SVG element of a design icon. role: S stroke (currentColor), F fill (currentColor),
 * K knock-out fill (the surface colour behind the icon), W warning fill, M warning mark
 * stroke, D dimmed stroke (PC "dim", 38 %), C fixed fill colour, E crown edge (fill + round
 * stroke of [IconPart.width] in the crown edge colour: the crown's whole footprint), G crown fill.
 */
class IconPart(val d: String, val role: Char, val width: Float, val color: Long, val clipped: Boolean = false)
class IconData(val minX: Float, val minY: Float, val vw: Float, val vh: Float, val parts: List<IconPart>, val clip: String? = null)

private fun build(icon: IconData, sizeW: Dp, sizeH: Dp, tint: Color, stroke: Float, cutout: Color, warning: Color, onWarning: Color,
                  crown: Color, crownEdge: Color): ImageVector {
    val b = ImageVector.Builder(defaultWidth = sizeW, defaultHeight = sizeH, viewportWidth = icon.vw, viewportHeight = icon.vh)
    // The viewBox origin may be negative (badges) or offset (Android head).
    b.addGroup(translationX = -icon.minX, translationY = -icon.minY)
    fun part(p: IconPart) {
        val nodes = addPathNodes(p.d)
        when (p.role) {
            'F' -> b.addPath(nodes, fill = SolidColor(tint))
            'K' -> b.addPath(nodes, fill = SolidColor(cutout))
            'W' -> b.addPath(nodes, fill = SolidColor(warning))
            'C' -> b.addPath(nodes, fill = SolidColor(Color(p.color)))
            'E' -> b.addPath(nodes, fill = SolidColor(crownEdge), stroke = SolidColor(crownEdge), strokeLineWidth = p.width, strokeLineJoin = StrokeJoin.Round)
            'G' -> b.addPath(nodes, fill = SolidColor(crown))
            'M' -> b.addPath(nodes, stroke = SolidColor(onWarning), strokeLineWidth = p.width, strokeLineCap = StrokeCap.Round, strokeLineJoin = StrokeJoin.Round)
            'B' -> b.addPath(nodes, stroke = SolidColor(tint), strokeLineWidth = if (p.width > 0) p.width else stroke, strokeLineCap = StrokeCap.Butt, strokeLineJoin = StrokeJoin.Miter)
            'D' -> b.addPath(nodes, stroke = SolidColor(tint), strokeAlpha = .38f, strokeLineWidth = if (p.width > 0) p.width else stroke, strokeLineCap = StrokeCap.Round, strokeLineJoin = StrokeJoin.Round)
            else -> b.addPath(nodes, stroke = SolidColor(tint), strokeLineWidth = if (p.width > 0) p.width else stroke, strokeLineCap = StrokeCap.Round, strokeLineJoin = StrokeJoin.Round)
        }
    }
    val clip = icon.clip
    if (clip != null) {
        b.addGroup(clipPathData = addPathNodes(clip))
        icon.parts.filter { it.clipped }.forEach(::part)
        b.clearGroup()
        icon.parts.filterNot { it.clipped }.forEach(::part)
    } else icon.parts.forEach(::part)
    b.clearGroup()
    return b.build()
}

/** Draws a design SVG icon at [width] × [height] with the given stroke width (viewBox units). */
@Composable
fun SvgIcon(icon: IconData, width: Dp, height: Dp = width, tint: Color = Oink.colors.ink, stroke: Float = 2f,
            modifier: Modifier = Modifier, cutout: Color = LocalCutout.current) {
    val c = Oink.colors
    val vector = remember(icon, width, height, tint, stroke, cutout, c.warning, c.crown, c.crownEdge) {
        build(icon, width, height, tint, stroke, cutout, c.warning, c.onWarning, c.crown, c.crownEdge)
    }
    Image(rememberVectorPainter(vector), contentDescription = null, modifier = modifier.requiredSize(width, height))
}

/** lucide-react icon: 24 viewBox, default stroke 2 scaled with the size. */
@Composable
fun Lucide(icon: IconData, size: Dp, tint: Color, stroke: Float = 2f, modifier: Modifier = Modifier) =
    SvgIcon(icon, size, size, tint, stroke, modifier)

/** PC Glyph waves for a volume level 0–200 (pc-icons-data volumeWaves). */
fun volumeWaves(v: Int) = if (v <= 0) 0 else if (v <= 33) 1 else if (v <= 66) 2 else if (v <= 100) 3 else 4

fun speakerGlyph(waves: Int, muted: Boolean = false): IconData = with(DesignIconData) {
    when (waves.coerceIn(0, 4)) {
        0 -> if (muted) pcSpeaker0Muted else pcSpeaker0
        1 -> if (muted) pcSpeaker1Muted else pcSpeaker1
        2 -> if (muted) pcSpeaker2Muted else pcSpeaker2
        3 -> if (muted) pcSpeaker3Muted else pcSpeaker3
        else -> if (muted) pcSpeaker4Muted else pcSpeaker4
    }
}

enum class Badge(val data: IconData) {
    Wifi(DesignIconData.badgeWifi), Ban(DesignIconData.badgeBan), Warning(DesignIconData.badgeWarning),
    Receive(DesignIconData.badgeReceive), Send(DesignIconData.badgeSend), Mute(DesignIconData.badgeMute)
}

/**
 * PC device icon (pc-icons.jsx DevIcon): glyph of [size] in an 18:16 box, badge (size × 10/16)
 * at the box's bottom-right corner. Volume variant: box = glyph, badge = size × 12/22, stroke 1.3.
 * [badgeTint] defaults to the icon colour; the receive badge uses the accent unless the icon is good.
 */
@Composable
fun DevIcon(glyph: IconData, size: Dp, tint: Color, badge: Badge? = null, volume: Boolean = false, badgeTint: Color = tint, modifier: Modifier = Modifier) {
    val box = if (volume) size else size * 18f / 16f
    val badgeSize = if (volume) size * 12f / 22f else size * 10f / 16f
    Box(modifier.size(box)) {
        SvgIcon(glyph, size, size, tint, if (volume) 1.3f else 1.4f, Modifier.align(Alignment.TopStart))
        if (badge != null) SvgIcon(badge.data, badgeSize, badgeSize, badgeTint, 1.3f, Modifier.align(Alignment.BottomEnd))
    }
}

/** Receive badge colour rule (pc-icons.css): accent unless the icon itself is good (green). */
@Composable
fun badgeTintFor(badge: Badge?, tint: Color): Color {
    val c = Oink.colors
    return if (badge == Badge.Receive && tint != c.good) c.accent else tint
}

/** PC latency bars (Signal): 3 levels, good / fair / poor by latency. */
@Composable
fun SignalBars(latency: Double?, width: Dp, height: Dp, modifier: Modifier = Modifier) {
    val c = Oink.colors
    val level = if (latency == null) 0 else if (latency < 20) 3 else if (latency <= 50) 2 else 1
    val on = when (level) { 3 -> c.good; 2 -> c.warning; else -> c.bad }
    Box(modifier.size(width, height)) {
        listOf(DesignIconData.signalBar0, DesignIconData.signalBar1, DesignIconData.signalBar2).forEachIndexed { i, bar ->
            SvgIcon(bar, width, height, if (i < level) on else c.track)
        }
    }
}

internal fun Modifier.nudge(x: Dp = 0.dp, y: Dp = 0.dp) = offset(x, y)
