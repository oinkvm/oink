package com.hachiwii.oink.ui

import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.Immutable
import androidx.compose.runtime.ReadOnlyComposable
import androidx.compose.runtime.compositionLocalOf
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.graphics.Color

/** Theme accents offered in settings; ids match LocalSettings.themeColor. */
enum class Accent(val id: String, val label: String, val base: Long, val deep: Long, val darkInk: Long) {
    Pink("pink", "粉色", 0xFFFF6B8E, 0xFFD42E62, 0xFFFF8FAB),
    Orange("orange", "橙色", 0xFFFF8A3D, 0xFFC2530C, 0xFFFFA463),
    Violet("violet", "紫色", 0xFF8B6CF6, 0xFF6440D8, 0xFFAB95FF),
    Blue("blue", "蓝色", 0xFF3D8BFD, 0xFF1D62D0, 0xFF6FA8FF),
    Teal("teal", "青色", 0xFF14B8A6, 0xFF0B7D70, 0xFF3FD6C4);
    companion object {
        // "purple" was the stored id before the palette followed the design names.
        fun of(id: String?) = entries.firstOrNull { it.id == id } ?: if (id == "purple") Violet else Pink
    }
}

/**
 * Colour tokens of the mobile design (theme.css). Names follow the CSS variables:
 * --c-bg → bg, --accent-ink → accentInk, and so on.
 */
@Immutable
data class OinkColors(
    val dark: Boolean,
    val accentId: Accent,
    val bg: Color, val card: Color, val sheet: Color, val control: Color, val elev: Color, val tile: Color, val field: Color,
    val fill: Color, val chip: Color, val track: Color, val level: Color,
    /** --c-input-disabled: the transfer composer's input pill while it is disabled (its usual fill is [fill]). */
    val inputDisabled: Color,
    val ink: Color, val bubbleInk: Color, val text2: Color, val muted: Color, val faint: Color, val hint: Color, val dotOff: Color, val outline: Color,
    val line: Color, val lineSolid: Color, val border: Color, val handle: Color, val progressTrack: Color,
    val good: Color, val goodSoft: Color, val goodInk: Color, val bad: Color, val badSoft: Color, val badInk: Color,
    val warning: Color, val onWarning: Color,
    val overlay: Color, val press: Color, val shadow: Color, val tabbar: Color, val tabbarSolid: Color, val tabbarEdge: Color,
    val toast: Color, val toastInk: Color, val toastActionBg: Color, val toastAction: Color, val onAccent: Color, val thumb: Color,
    val osWindows: Color, val osAndroid: Color, val file: Color, val folder: Color, val systemApp: Color,
    /** Android privileged-mode crown: fill and the 0.5 dp rim under it (dark needs no darker rim). */
    val crown: Color, val crownEdge: Color,
    val accent: Color, val accentDeep: Color, val accentInk: Color, val accentSoft: Color, val accentSoft2: Color, val accentSoft3: Color, val accentBubble: Color,
    val shareSurface: Color, val shareBorder: Color, val shareTrack: Color, val shareText: Color, val shareDevice: Color, val shareArrow: Color,
    val shareScrim: Color, val shareTile: Color, val shareTile2: Color, val shareShadowNear: Color, val shareShadowFar: Color,
) {
    // Rich text in transfer bubbles (user 2026-11-12, theme.css --c-rich-*). The two translucent ones work on
    // both the grey and the accent bubble.
    /** --c-rich-link: links. */
    val richLink: Color get() = accentInk
    /** --c-rich-marker: list markers, a picture's alt text. */
    val richMarker: Color get() = muted
    /** --c-rich-quote-ink: block quote text. */
    val richQuoteInk: Color get() = text2
    /** --c-rich-rule: the quote bar, horizontal rules, the table header's rule. */
    val richRule: Color get() = if (dark) Color(236, 238, 242).copy(alpha = .22f) else Color(27, 28, 34).copy(alpha = .16f)
    /** --c-rich-code-bg: inline code, code blocks, tables. */
    val richCodeBg: Color get() = if (dark) Color(236, 238, 242).copy(alpha = .08f) else Color(27, 28, 34).copy(alpha = .06f)

    companion object {
        fun of(dark: Boolean, accent: Accent): OinkColors {
            val a = Color(accent.base)
            val bg = if (dark) Color(0xFF14161C) else Color.White
            val card = if (dark) Color(0xFF1C1F27) else Color(0xFFF4F4F6)
            val pink = accent == Accent.Pink && !dark
            // color-mix(in srgb, accent N%, base): resolved here because Compose has no colour mixing.
            fun mix(p: Float, base: Color = bg) = mix(a, base, p)
            val ink = if (dark) Color(0xFFECEEF2) else Color(0xFF1B1C22)
            fun inkA(alpha: Float) = Color(27, 28, 34).copy(alpha = alpha)
            fun lightA(alpha: Float) = Color(236, 238, 242).copy(alpha = alpha)
            return OinkColors(
                dark = dark, accentId = accent,
                bg = bg, card = card,
                sheet = if (dark) Color(0xFF1C1F27) else Color(0xFFF9F9FB),
                control = if (dark) Color(0xFF232731) else Color.White,
                elev = if (dark) Color(0xFF232731) else Color.White,
                tile = if (dark) Color(0xFF2D323E) else Color.White,
                field = if (dark) Color(0xFF1A1D24) else Color(0xFFF9F9FB),
                fill = if (dark) Color(0xFF232731) else Color(0xFFF1F1F4),
                inputDisabled = if (dark) Color(0xFF191C23) else Color(0xFFE4E4EA),
                chip = if (dark) Color(0xFF2D323E) else Color(0xFFECECEF),
                track = if (dark) Color(0xFF2D323E) else Color(0xFFE2E2E7),
                level = if (dark) Color(0xFF4A5161) else Color.White,
                ink = ink,
                bubbleInk = if (dark) Color(0xFFDFE2E8) else Color(0xFF34353D),
                text2 = if (dark) Color(0xFFA7ADBA) else Color(0xFF5F606A),
                muted = if (dark) Color(0xFF868D9D) else Color(0xFF7B7D88),
                faint = if (dark) Color(0xFF767D8E) else Color(0xFF989AA4),
                hint = if (dark) Color(0xFF6F7687) else Color(0xFFA3A5AE),
                dotOff = if (dark) Color(0xFF4A505E) else Color(0xFFB8B9C0),
                outline = if (dark) Color(0xFF6F7687) else Color(0xFF7B7D88),
                line = if (dark) lightA(.08f) else inkA(.07f),
                lineSolid = if (dark) Color(0xFF262A34) else Color(0xFFEEEEF2),
                border = if (dark) Color(0xFF2D323E) else Color(0xFFE9E9ED),
                handle = if (dark) Color(0xFF4A505E) else Color(0xFFC7C8CE),
                progressTrack = if (dark) Color(0xFF2D323E) else Color(0xFFDEDEE5),
                good = Color(0xFF2FBF71),
                goodSoft = if (dark) mix(Color(0xFF2FBF71), Color(0xFF14161C), .18f) else Color(0xFFE2F6EB),
                goodInk = if (dark) Color(0xFF5FD39A) else Color(0xFF1C8F50),
                bad = Color(0xFFF04452),
                badSoft = if (dark) mix(Color(0xFFF04452), Color(0xFF14161C), .18f) else Color(0xFFFDE8EA),
                badInk = if (dark) Color(0xFFFF8D7E) else Color(0xFFD42F3D),
                warning = Color(0xFFF5A524), onWarning = Color(0xFF1B1C22),
                overlay = if (dark) Color.Black.copy(alpha = .55f) else inkA(.24f),
                press = if (dark) lightA(.06f) else inkA(.05f),
                shadow = if (dark) Color.Black.copy(alpha = .45f) else inkA(.12f),
                tabbar = if (dark) Color(35, 39, 49).copy(alpha = .86f) else Color(245, 245, 247).copy(alpha = .88f),
                tabbarSolid = if (dark) Color(0xFF232731) else Color(0xFFF4F4F6),
                tabbarEdge = if (dark) Color.White.copy(alpha = .06f) else Color.White.copy(alpha = .9f),
                toast = if (dark) Color(0xFFECEEF2) else Color(0xFF292A31),
                toastInk = if (dark) Color(0xFF14161C) else Color.White,
                toastActionBg = if (dark) Color(20, 22, 28).copy(alpha = .07f) else Color.White.copy(alpha = .1f),
                // The toast inverts in dark mode, so its action uses the deep accent (light-surface text).
                toastAction = if (dark) Color(accent.deep) else a,
                onAccent = Color.White, thumb = Color.White,
                osWindows = if (dark) Color(0xFF4CA6EE) else Color(0xFF0078D4),
                // The green of Google's own android-head_flat.svg, both themes.
                osAndroid = Color(0xFF34A853),
                crown = if (dark) Color(0xFFF5B400) else Color(0xFFF0B400),
                crownEdge = if (dark) Color(0xFFF5B400) else Color(0xFFA87500),
                file = Color(0xFF8698B4), folder = Color(0xFFD5A348), systemApp = Color(0xFF3D8BFD),
                accent = a, accentDeep = Color(accent.deep),
                accentInk = if (dark) Color(accent.darkInk) else Color(accent.deep),
                accentSoft = when { pink -> Color(0xFFFFE7ED); dark -> mix(.20f); else -> mix(.15f) },
                accentSoft2 = when { pink -> Color(0xFFFFE1EA); dark -> mix(.24f); else -> mix(.19f) },
                accentSoft3 = when { pink -> Color(0xFFFFDAE5); dark -> mix(.30f); else -> mix(.25f) },
                accentBubble = when { pink -> Color(0xFFFFE9EF); dark -> mix(.22f, card); else -> mix(.14f) },
                shareSurface = if (dark) Color(0xFF1C1F27) else Color.White,
                shareBorder = if (dark) Color(0xFF2D323E) else Color.Transparent,
                shareTrack = if (dark) Color(0xFF2D323E) else Color(0xFFECEEF2),
                shareText = if (dark) Color(0xFFA7ADBA) else Color(0xFF555B6A),
                shareDevice = if (dark) Color(0xFFA7ADBA) else Color(0xFF555B6A),
                shareArrow = if (dark) Color(0xFFF2F3F5) else Color(0xFF1B1E28),
                shareScrim = if (dark) Color.Black.copy(alpha = .48f) else Color(20, 22, 28).copy(alpha = .32f),
                shareTile = if (dark) Color(0xFF232731) else Color(0xFFE3E5EA),
                shareTile2 = if (dark) Color(0xFF2D323E) else Color(0xFFD5D8DF),
                shareShadowNear = if (dark) Color.Black.copy(alpha = .3f) else Color.Black.copy(alpha = .06f),
                shareShadowFar = if (dark) Color.Black.copy(alpha = .4f) else Color(20, 22, 28).copy(alpha = .16f),
            )
        }
        /** CSS color-mix(in srgb, a p, b): component-wise on the encoded sRGB values. */
        fun mix(a: Color, b: Color, p: Float) = Color(
            red = a.red * p + b.red * (1 - p), green = a.green * p + b.green * (1 - p),
            blue = a.blue * p + b.blue * (1 - p), alpha = 1f)
    }
}

val LocalOinkColors = staticCompositionLocalOf { OinkColors.of(false, Accent.Pink) }
/** The colour a badge "cuts out" of (CSS --cutout): the surface behind the icon. */
val LocalCutout = compositionLocalOf { Color.White }

object Oink {
    val colors: OinkColors @Composable @ReadOnlyComposable get() = LocalOinkColors.current
}

@Composable
fun OinkTheme(colors: OinkColors, content: @Composable () -> Unit) {
    CompositionLocalProvider(LocalOinkColors provides colors, LocalCutout provides colors.bg, content = content)
}
