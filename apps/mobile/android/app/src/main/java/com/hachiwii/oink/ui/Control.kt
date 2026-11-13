package com.hachiwii.oink.ui

import android.app.Activity
import android.content.Context
import android.content.ContextWrapper
import android.content.pm.ActivityInfo
import android.view.HapticFeedbackConstants
import android.view.KeyCharacterMap
import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.VectorConverter
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInVertically
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.focusable
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.gestures.waitForUpOrCancellation
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.composed
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.draw.scale
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.geometry.RoundRect
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.BlendMode
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.ImageShader
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.PathOperation
import androidx.compose.ui.graphics.ShaderBrush
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.TileMode
import androidx.compose.ui.graphics.TransformOrigin
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.graphics.drawscope.DrawScope
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.graphics.drawscope.clipPath
import androidx.compose.ui.graphics.drawscope.withTransform
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.input.key.onPreviewKeyEvent
import androidx.compose.ui.input.key.KeyEventType
import androidx.compose.ui.input.key.type
import androidx.compose.ui.input.pointer.AwaitPointerEventScope
import androidx.compose.ui.input.pointer.changedToDown
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.input.pointer.positionChanged
import androidx.compose.ui.layout.Layout
import androidx.compose.ui.layout.boundsInRoot
import androidx.compose.ui.layout.layout
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.layout.positionInRoot
import androidx.compose.ui.layout.positionInWindow
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.semantics.*
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.toSize
import androidx.compose.ui.window.Popup
import androidx.compose.ui.window.PopupProperties
import androidx.lifecycle.compose.LifecycleResumeEffect
import com.hachiwii.oink.control.*
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlin.math.max
import kotlin.math.roundToInt

/** Design-scene state of the control page (docs/control-page.md §8); null in the app. */
data class ControlInit(
    /** adjust; adjust-swipe / -layout (调节 with that second menu open over it); focus, lock, lock-game */
    val menu: String? = null,
    /** Opens the keyboard with this remembered kind (for both orientations). */
    val keyboard: KeyboardKind? = null,
    /** Landscape emulated keyboard: opens expanded to the whole screen. */
    val expanded: Boolean = false,
    /** Screenshot/test IME geometry: remaining screen height including the status bar. */
    val systemRemaining: Float? = null,
    /**
     * Landscape floating keyboard frozen mid-gesture (user 2026-11-12 mockups): left / mid / right (moved
     * there), dismiss (dragged 70 down), pressed (handle held down).
     */
    val float: String? = null,
    val symbols: Boolean = false,
    /** Modifiers tapped once (held) or twice (locked) after opening. */
    val mods: Map<Mod, Latch> = emptyMap(),
    val fn: Latch? = null,
    /** Finger at this fraction of the pad (press highlight), or one of the edge-lock demos: half, full, corner, locked, relay. */
    val touch: Pair<Float, Float>? = null,
    val edgeDemo: String? = null,
    /** Key preview (§3.1.4): a finger frozen on this key (its slide id, e.g. "vk-g", "copy"), others holding [held]. */
    val preview: String? = null,
    val held: List<String> = emptyList(),
    /** Landscape held with the cutout on the right (pad and keyboard swap sides); null: from the display cutout. */
    val flip: Boolean? = null,
)

/**
 * How the control page is laid out (docs/control-page.md §1, 用户 2026-11-12 确认):
 * [PORTRAIT] keyboard button beside the narrowed tab bar, 全屏 / 切换键盘 in the header's top-right
 * slot, 收起键盘 in the control keys; [LANDSCAPE] (phone held sideways = the full screen, 用户
 * 2026-11-12) the touchpad covers the whole screen with the top bar floating over it, the emulated
 * keyboard floats in a container or fills the screen; [SIDE] (large landscape windows with the side
 * tab bar) the header keeps its keyboard toggle and the keyboard is full width.
 */
enum class ControlLayout { PORTRAIT, LANDSCAPE, SIDE }

/** Colours of the control page that theme.css keeps in control.css (pad, keyboard). */
private class ControlColors(dark: Boolean) {
    val padHi = if (dark) Color(0xFF3A3D45) else Color(0xFFFBFBFC)
    val padMid = if (dark) Color(0xFF2C2F36) else Color(0xFFEFF0F3)
    val padLo = if (dark) Color(0xFF25272D) else Color(0xFFE5E6EA)
    val padGrain = if (dark) .40f else .32f
    val padHighlight = if (dark) Color.White.copy(alpha = .09f) else Color.White.copy(alpha = .95f)
    val padEdge = if (dark) Color.White.copy(alpha = .035f) else Color.White.copy(alpha = .5f)
    val padInner = if (dark) Color.Black.copy(alpha = .28f) else INK.copy(alpha = .035f)
    val padOutline = if (dark) Color.Black.copy(alpha = .55f) else INK.copy(alpha = .10f)
    val padDrop = if (dark) Color.Black.copy(alpha = .30f) else INK.copy(alpha = .05f)
    val padGlow = if (dark) Color.White.copy(alpha = .07f) else Color.White.copy(alpha = .55f)
    val padPress = if (dark) Color.Black.copy(alpha = .25f) else INK.copy(alpha = .05f)
    val padHint = if (dark) Color.White.copy(alpha = .2f) else INK.copy(alpha = .24f)
    val kbBg = if (dark) Color(0xFF1C1E24) else Color(0xFFD6D8DE)
    val kbKey = if (dark) Color(0xFF454A56) else Color.White
    val kbKey2 = if (dark) Color(0xFF30343D) else Color(0xFFB5B9C3)
    val kbInk = if (dark) Color(0xFFECEEF2) else Color(0xFF1B1C22)
    val kbShadow = if (dark) Color.Black.copy(alpha = .5f) else Color.Black.copy(alpha = .28f)
    /** Key preview bubble (§3.1.4): one step above the character keys in dark, white in light. */
    val callout = if (dark) Color(0xFF5B606D) else Color.White
    val calloutEdge = if (dark) Color.White.copy(alpha = .08f) else INK.copy(alpha = .12f)
    val calloutShadow = if (dark) Color.Black.copy(alpha = .62f) else INK.copy(alpha = .26f)
    companion object { val INK = Color(27, 28, 34) }
}

private val KbEase = androidx.compose.animation.core.CubicBezierEasing(.2f, 0f, 0f, 1f)
private const val KB_MS = 285
private val ACCESSORY_H = 134.dp
/** Landscape IME height before one was seen (Gboard / Sogou: about half the screen, candidates included, above the nav bar). */
private val LAND_IME_H = 220.dp
private const val LONG_PRESS_MS = 450L
private const val NO_TARGET = "请先选择要控制的电脑"

/**
 * Everything the control page keeps while the app runs: the remembered settings, the open session,
 * the keyboard and touchpad models, and where the keyboard currently is (the pad makes room for it).
 */
@Stable
class ControlHolder(val init: ControlInit?, prefs: ControlPrefs, repeat: RepeatClock = RepeatClock.NONE, private val save: (ControlPrefs) -> Unit) {
    var prefs by mutableStateOf(prefs); private set
    var session by mutableStateOf<ControlSession?>(null)
    /**
     * The system keyboard's hidden field starts afresh ([RemoteInputView.restartField]; set by the page
     * while the field exists): it mirrors the text in front of the focus computer's caret, so whatever
     * else types there or moves that caret — the page's keys, a touchpad click, a hardware key,
     * another text field or computer — ends what it can edit.
     */
    var restartField: (() -> Unit)? = null
    /** The field's own Backspace is going out (it does not restart the field). */
    private var fieldKey = false
    val keyboard = KeyboardModel(prefs.keyboard, repeat = repeat, text = { session?.text(it); restartField?.invoke() },
        typedChar = { layout, mac, ch, mods -> session?.typedChar(layout, mac, ch, mods) }) { code, down ->
        session?.key(code, down)
        if (down && !fieldKey && !Hid.keepsCaret(code)) restartField?.invoke()
    }
    /** Backspace from the hidden field: with the modifiers held on the page, like the page's own keys. */
    fun fieldBackspace() {
        fieldKey = true
        try { keyboard.click(Hid.BACKSPACE) } finally { fieldKey = false }
    }
    val pad = TouchpadModel(object : PadSink {
        override fun move(dx: Float, dy: Float, t: Long, since: Long) { session?.move(dx, dy, t, since) }
        override fun scroll(dx: Float, dy: Float, phase: PadScrollPhase, t: Long, since: Long) { session?.scroll(dx, dy, phase, t, since) }
        override fun gesture(gesture: PadGesture) { session?.gesture(gesture) }
        override fun touches(t: Long, fingers: List<PadFinger>, width: Float, height: Float, cancelled: Boolean, multiFinger: Boolean) =
            session?.touches(t, fingers, width, height, cancelled, multiFinger) ?: CoreFrame.UNCLAIMED
        // A click or drag carries the modifiers that are down; held ones let go once its button is up.
        // A click also places the computer's caret: the hidden field starts afresh.
        override fun button(button: ControlButton, down: Boolean) {
            session?.button(button, down)
            if (down) restartField?.invoke() else keyboard.releaseHeld()
        }
    })
    var keyboardOpen by mutableStateOf(false); private set
    var menu by mutableStateOf(init?.menu)
    /** The page's layout and, in landscape, whether the pad and keyboard swap sides; set by the page. */
    var layout by mutableStateOf(ControlLayout.PORTRAIT)
    var flip by mutableStateOf(false)
    /** 全屏 / 恢复: the orientation the page asks for (applied by the page while it is shown). */
    val fullscreen = Fullscreen()
    /** The phone's own tilt as last seen (true upright, false sideways, null unknown); read by 恢复. */
    internal var upright: Boolean? = null
    /**
     * This phone's 接收音频 / 共享麦克风 (调节 and the expanded keyboard; the device page's own switches,
     * same state and rules): set by the page from the app model with the app's toggles.
     */
    var forwardOn by mutableStateOf(false); internal set
    var speakerOn by mutableStateOf(false); internal set
    var micOn by mutableStateOf(false); internal set
    internal var toggleSpeaker: () -> Unit = {}
    internal var longPressSpeaker: () -> Unit = {}
    internal var receiveSpeaker: () -> Unit = {}
    internal var toggleMic: () -> Unit = {}
    /**
     * Tops of the drawn keyboard surfaces (root px), null while not drawn; the pad keeps 12 above the
     * higher one. In landscape the keyboard stands beside the pad: there [accessoryTop] is the top of
     * the full-width system keyboard under it and [emulatedTop] stays null.
     */
    var accessoryTop by mutableStateOf<Float?>(null)
    var emulatedTop by mutableStateOf<Float?>(null)
    val keyboardTop: Float? get() = listOfNotNull(accessoryTop, emulatedTop).minOrNull()
    var active by mutableStateOf(false)
    /** Fingers sliding over the keys with a preview bubble (§3.1.4); [feel] is set by the keyboard (haptics). */
    val slider = KeySlider { slide -> feel?.invoke(slide) }
    internal var feel: ((Boolean) -> Unit)? = null
    /** Keys on screen (root px) by composable instance; with what they do when they have a bubble. */
    internal val keySpots = HashMap<Any, KeySpot>()
    /** The keyboard surfaces (accessory bar, emulated keyboard) in root px: leaving them ends a finger. */
    internal val keyAreas = HashMap<Any, androidx.compose.ui.layout.LayoutCoordinates>()

    /** The keyboard kinds are remembered per orientation (user 2026-11-12); true while the landscape one is in use. */
    var landscapeKeys = false; private set
    private var orientationSeen = false
    /** The page's orientation: that orientation's remembered kind takes over (in place if the keyboard is up). */
    fun useOrientation(land: Boolean) {
        if (orientationSeen && land == landscapeKeys) return
        orientationSeen = true
        landscapeKeys = land
        keyboard.restore(if (land) prefs.keyboardLand else prefs.keyboard)
    }
    /** The user switched kinds (the switch key): remembered for the current orientation. */
    fun rememberKind(kind: KeyboardKind) {
        val p = prefs
        if (landscapeKeys) { if (kind != p.keyboardLand) update(p.copy(keyboardLand = kind)) }
        else if (kind != p.keyboard) update(p.copy(keyboard = kind))
    }
    /** The landscape emulated keyboard expanded / floating, remembered for the next time it opens. */
    fun setExpanded(expanded: Boolean) { if (expanded != prefs.expanded) update(prefs.copy(expanded = expanded)) }

    fun update(next: ControlPrefs) {
        val speedChanged = next.speed != prefs.speed
        prefs = next
        save(next)
        pad.oneFingerScroll = next.swipe == "scroll"
        if (speedChanged) session?.let { applySettings?.invoke(it) }
        if (textFocus.autoPopup != next.keyboardAutoPopup) {
            textFocus.autoPopup = next.keyboardAutoPopup
            reevaluateTextFocus()
        }
    }
    internal var applySettings: ((ControlSession) -> Unit)? = null

    /** The keyboard button: a keyboard opened here never hides with the computer's focus; hiding it is the user's. */
    fun toggleKeyboard() { if (keyboardOpen) closeKeyboard() else { textFocus.userOpened(); keyboardOpen = true } }

    /** The focus computer's text fields pop the keyboard up and hide it (docs/control-page.md §9, [PcTextFocusGate]). */
    private val textFocus = PcTextFocusGate()
    /** Uptime ms (replaced in tests). */
    internal var now: () -> Long = { android.os.SystemClock.uptimeMillis() }
    /** When a text-focus change waiting for the touchpad to be quiet is due ([PcTextFocusGate.retryAt]). */
    var textFocusRetryAt by mutableStateOf<Long?>(null); private set
    /** The computer's text focus and the focus computer the hidden field typed for (a change restarts it). */
    private var fieldFocus: PcTextFocusReport? = null
    private var fieldDevice: String? = null
    fun pcTextFocus(report: PcTextFocusReport) {
        if (report != fieldFocus) { fieldFocus = report; restartField?.invoke() }
        textFocus.record(report)
        reevaluateTextFocus()
    }
    /** After a report, a change of the focus computer or the page's visibility, and at [textFocusRetryAt]. */
    fun reevaluateTextFocus() {
        val device = session?.state?.focusDevice
        if (device != fieldDevice) { fieldDevice = device; restartField?.invoke() }
        follow(textFocus.decide(session?.state?.focusDevice, active && session != null, keyboardOpen, now()))
        textFocusRetryAt = textFocus.retryAt()
    }
    /** The touchpad is in use (a finger down, the locked drag held) or just stopped: text focus waits for it. */
    fun touchpadBusy(busy: Boolean) {
        textFocus.touchpad(busy, now())
        textFocusRetryAt = textFocus.retryAt()
    }
    private fun follow(action: PcTextFocusAction) {
        when (action) {
            PcTextFocusAction.None -> {}
            is PcTextFocusAction.Open -> { keyboard.secure = action.secure; keyboardOpen = true }
            is PcTextFocusAction.Follow -> keyboard.secure = action.secure
            PcTextFocusAction.Close -> closeKeyboard(byUser = false)
        }
    }

    /** [byUser]: the keyboard button or the system's own dismissal (no pop-up again during this input focus). */
    fun closeKeyboard(byUser: Boolean = true) {
        if (!keyboardOpen) return
        if (byUser) textFocus.userHid(session?.state?.focusDevice)
        keyboardOpen = false
        slider.reset()
        keyboard.closed()
    }

    /** Hardware keyboard: real key codes and modifiers, forwarded as they are. */
    fun hardwareKey(keyCode: Int, down: Boolean): Boolean {
        val code = Hid.fromAndroid(keyCode) ?: return false
        session?.key(code, down)
        if (down && !Hid.keepsCaret(code)) restartField?.invoke()
        return true
    }

    init {
        pad.oneFingerScroll = prefs.swipe == "scroll"
        textFocus.autoPopup = prefs.keyboardAutoPopup
    }
}

@Composable
fun rememberControlHolder(init: UiInit): ControlHolder {
    val context = LocalContext.current
    return remember(init.control, init.freeze) {
        val scene = init.freeze
        val prefs = if (scene) init.control.let { s ->
            val kind = s?.keyboard ?: KeyboardKind.SYSTEM
            ControlPrefs(keyboard = kind, keyboardLand = s?.keyboard ?: KeyboardKind.EMULATED, expanded = s?.expanded == true)
        } else ControlPrefs.load(context)
        // Design scenes hold keys down without end: no repeat timers there.
        ControlHolder(init.control, prefs, if (scene) RepeatClock.NONE else RepeatClock.main()) { if (!scene) ControlPrefs.save(context, it) }
    }
}

private tailrec fun Context.activity(): Activity? = when (this) { is Activity -> this; is ContextWrapper -> baseContext.activity(); else -> null }

/**
 * The control tab: header and touchpad. The keyboard is drawn above the whole app by [ControlKeyboardLayer].
 * [headerTabs]: the app's tabs, drawn in the landscape top bar (the tab bar is hidden then).
 * [canFullscreen]: a landscape window would get the phone's landscape layout (the 全屏 button shows).
 */
@Composable
fun ControlPage(holder: ControlHolder, model: AppModel, actions: AppActions, active: Boolean, layout: ControlLayout, toast: MutableState<ToastData?>,
                headerTabs: @Composable () -> Unit = {}, canFullscreen: Boolean = false) {
    val c = Oink.colors
    val cc = remember(c.dark) { ControlColors(c.dark) }
    val context = LocalContext.current
    val land = layout == ControlLayout.LANDSCAPE
    // Held the other way round (cutout on the right, 用户 2026-11-12): the floating keyboard starts on
    // the left and the cutout side keeps its safe area. The top bar does not swap.
    val cutout = WindowInsets.displayCutout
    val density = LocalDensity.current
    val direction = androidx.compose.ui.platform.LocalLayoutDirection.current
    val flip = land && (holder.init?.flip ?: (cutout.getRight(density, direction) > cutout.getLeft(density, direction)))
    // The panel stays composed under other tabs (TabTrack): "active" says whether it is shown; leaving composition ends it too.
    SideEffect {
        holder.active = active; holder.layout = layout; holder.flip = flip
        holder.useOrientation(land)
        // 接收音频 / 共享麦克风: this phone's device settings, with the device page's toggles (same rules, same failures).
        holder.forwardOn = model.forwardOn; holder.speakerOn = model.speakerOn && !model.forwardOn; holder.micOn = model.micOn
        holder.toggleSpeaker = actions.toggleSpeaker; holder.receiveSpeaker = actions.receiveSpeaker ?: actions.toggleSpeaker; holder.longPressSpeaker = actions.longPressSpeaker; holder.toggleMic = actions.toggleMic
    }
    DisposableEffect(holder) { onDispose { holder.active = false } }

    // One session while the page is on screen and the app is in the foreground (§14.6).
    val opener = actions.control
    LifecycleResumeEffect(active, opener) {
        val session = if (active && opener != null) {
            ControlSession(opener, onFailure = { toast.value = ToastData(it) }, textFocused = holder::pcTextFocus)
        } else null
        if (session != null) {
            val m = context.resources.displayMetrics
            val apply: (ControlSession) -> Unit = { s -> s.settings(holder.prefs.speed, m.xdpi / 25.4f / m.density) }
            holder.applySettings = apply
            apply(session)
        }
        holder.session = session
        onPauseOrDispose {
            // A pressed (or locked) left button and held keys go up before the session closes.
            holder.pad.releaseAll(android.os.SystemClock.uptimeMillis())
            holder.closeKeyboard(byUser = false)
            holder.menu = null
            holder.session = null
            session?.close()
        }
    }
    val state = holder.session?.state ?: ControlState()
    // The emulated keyboard types for the focus computer's layout (调节 › 键盘布局) and system: the
    // OS comes from the core's control state (the focus target's DeviceFacts OS), as on iOS.
    SideEffect {
        holder.keyboard.layout = holder.prefs.layout
        holder.keyboard.targetMac = state.focus?.platform == "mac"
    }
    LaunchedEffect(state.noTarget) { if (state.noTarget && active) toast.value = ToastData(NO_TARGET) }
    LaunchedEffect(state.focusDevice, active, holder.session) { holder.reevaluateTextFocus() }
    // Keyboard changes from the computer's text focus wait until the pad has been idle (TouchpadModel.TOUCHPAD_IDLE_DELAY_MS).
    LaunchedEffect(holder) {
        snapshotFlow { holder.pad.pressed != null || holder.pad.locked != null }.collect { holder.touchpadBusy(it) }
    }
    LaunchedEffect(holder.textFocusRetryAt) {
        val at = holder.textFocusRetryAt ?: return@LaunchedEffect
        delay((at - holder.now()).coerceAtLeast(0))
        holder.reevaluateTextFocus()
    }

    FullscreenEffects(holder, active, land)
    LaunchedEffect(holder.keyboard) { snapshotFlow { holder.keyboard.preferred }.collect { holder.rememberKind(it) } }
    LaunchedEffect(holder.init) {
        val s = holder.init ?: return@LaunchedEffect
        if (s.keyboard != null) {
            holder.toggleKeyboard()
            if (s.symbols) holder.keyboard.symbols = true
            s.mods.forEach { (mod, latch) -> if (latch == Latch.LOCKED) holder.keyboard.lockModifier(mod) else holder.keyboard.tapModifier(mod) }
            s.fn?.let { holder.keyboard.tapFn(); if (it == Latch.LOCKED) holder.keyboard.tapFn() }
        }
    }

    // Hardware keyboard while no on-screen keyboard has focus. Its auto-repeat (further downs of a key
    // that is down) goes out too: the core sends those to the focus computer as key repeats.
    val focus = remember { FocusRequester() }
    LaunchedEffect(active, holder.keyboardOpen) { if (active && !holder.keyboardOpen) runCatching { focus.requestFocus() } }
    Box(Modifier.fillMaxSize().background(c.bg).focusRequester(focus).focusable()
        .onPreviewKeyEvent { e ->
            val n = e.nativeKeyEvent
            if (n.deviceId == KeyCharacterMap.VIRTUAL_KEYBOARD) false
            else holder.hardwareKey(n.keyCode, e.type == KeyEventType.KeyDown)
        }) {
        var pageOrigin by remember { mutableStateOf(Offset.Zero) }
        var padOrigin by remember { mutableStateOf(Offset.Zero) }
        if (land) Box(Modifier.fillMaxSize().onGloballyPositioned { pageOrigin = it.positionInRoot() }) {
            // Full screen (用户 2026-11-12): the pad reaches every screen edge, under the status bar and the
            // safe areas (the screen's own corners round it); the top bar floats over it. With the system
            // keyboard the overlay chooses two rows or a left pad / right controls from remaining height.
            val systemUp = holder.keyboardOpen && holder.keyboard.shown == KeyboardKind.SYSTEM
            val safe = WindowInsets.systemBars.union(WindowInsets.displayCutout)
            if (!systemUp) Touchpad(holder, cc, Modifier.outset(safe.getLeft(density, direction), safe.getTop(density), safe.getRight(density, direction), safe.getBottom(density))
                .onGloballyPositioned { padOrigin = it.positionInRoot() }, flat = true, bottomInset = with(density) { safe.getBottom(density).toDp() }) {
                Box(Modifier.fillMaxSize().padding(safe.asPaddingValues())) {
                    LandscapeBar(holder, model, state, headerTabs)
                }
            }
            else LandscapeBar(holder, model, state, headerTabs)
        } else Column(Modifier.fillMaxSize().onGloballyPositioned { pageOrigin = it.positionInRoot() }) {
            ControlHeader(holder, model, state, layout, headerTabs, canFullscreen)
            // The pad's bottom margin: 92 for the floating tab bar (side layout 16), or keyboard
            // height + 12 while a keyboard is up under it.
            var pageBottom by remember { mutableStateOf(0f) }
            val kbTop = holder.keyboardTop
            val rest = if (layout == ControlLayout.PORTRAIT) 92.dp else 16.dp
            val bottom = if (kbTop == null) rest else with(density) { max(rest.toPx(), pageBottom - kbTop + 12.dp.toPx()).toDp() }
            Box(Modifier.weight(1f).fillMaxWidth().onGloballyPositioned { pageBottom = it.positionInRoot().y + it.size.height }) {
                Touchpad(holder, cc, Modifier.fillMaxSize().padding(start = 16.dp, end = 16.dp, top = 2.dp, bottom = bottom)
                    .onGloballyPositioned { padOrigin = it.positionInRoot() })
            }
        }
        // The glow reaches past the pad's edge, so it is drawn on the page.
        if (!(land && holder.keyboardOpen && holder.keyboard.shown == KeyboardKind.SYSTEM)) {
            holder.pad.edge?.let { e -> EdgeGlow(e, padOrigin - pageOrigin) }
        }
    }
    BackHandlerFor(holder)
}

/** CSS negative margins: the child is [left] … [bottom] px larger than this box on each side and drawn that far out. */
private fun Modifier.outset(left: Int, top: Int, right: Int, bottom: Int): Modifier = layout { measurable, constraints ->
    val w = constraints.maxWidth; val h = constraints.maxHeight
    val p = measurable.measure(androidx.compose.ui.unit.Constraints.fixed(w + left + right, h + top + bottom))
    layout(w, h) { p.place(-left, -top) }
}

/**
 * 全屏 / 恢复 ([Fullscreen], 用户 2026-11-12): the orientation the page asks the activity for while it is
 * shown; leaving the page (another tab, or the app going to the background) gives it back to the
 * system and ends a 恢复 that holds portrait. The phone's own tilt is watched in landscape (恢复 needs
 * it) and while a 恢复 after 全屏 waits for the phone to be upright.
 */
@Composable
private fun FullscreenEffects(holder: ControlHolder, active: Boolean, land: Boolean) {
    val context = LocalContext.current
    val fs = holder.fullscreen
    val request = if (active) fs.request else Fullscreen.Request.SYSTEM
    SideEffect {
        context.activity()?.requestedOrientation = when (request) {
            Fullscreen.Request.LANDSCAPE -> ActivityInfo.SCREEN_ORIENTATION_USER_LANDSCAPE
            Fullscreen.Request.PORTRAIT -> ActivityInfo.SCREEN_ORIENTATION_USER_PORTRAIT
            Fullscreen.Request.SYSTEM -> ActivityInfo.SCREEN_ORIENTATION_UNSPECIFIED
        }
    }
    DisposableEffect(Unit) { onDispose { context.activity()?.requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_UNSPECIFIED } }
    LaunchedEffect(active) { if (!active) fs.left() }
    androidx.lifecycle.compose.LifecycleEventEffect(androidx.lifecycle.Lifecycle.Event.ON_STOP) { fs.left() }
    if (active && (land || fs.watchesTilt)) DisposableEffect(Unit) {
        val listener = object : android.view.OrientationEventListener(context) {
            override fun onOrientationChanged(degrees: Int) {
                val upright = Fullscreen.upright(degrees) ?: return
                holder.upright = upright
                if (fs.watchesTilt) fs.tilt(upright)
            }
        }
        if (listener.canDetectOrientation()) listener.enable()
        onDispose { listener.disable(); holder.upright = null }
    }
}

@Composable
private fun BackHandlerFor(holder: ControlHolder) {
    val imeVisible = WindowInsets.ime.getBottom(LocalDensity.current) > 0
    // The visible system IME owns Back (including predictive Back) and its inset-close observer
    // in ControlKeyboardLayer closes our state. Do not compete for that same gesture.
    // Before the IME appears, the dispatcher can close the pending keyboard request.
    val imeOwnsBack = holder.keyboardOpen && holder.keyboard.shown == KeyboardKind.SYSTEM && imeVisible
    val restore = !holder.keyboardOpen && !imeVisible && holder.layout == ControlLayout.LANDSCAPE && holder.fullscreen.enteredByButton
    androidx.activity.compose.BackHandler(enabled = holder.active && !imeOwnsBack && (holder.keyboardOpen || restore)) {
        if (holder.keyboardOpen) holder.closeKeyboard()
        else holder.fullscreen.restore(holder.upright)
    }
}

// ---------- Header ----------

private enum class LockLook { UNLOCKED, LOCKED, GAME_ON, GAME_AUTO }

private fun ControlState.lockLook() = when {
    gameMode == ControlGameMode.ON -> LockLook.GAME_ON
    gameMode == ControlGameMode.AUTO && gameActive -> LockLook.GAME_AUTO
    locked -> LockLook.LOCKED
    else -> LockLook.UNLOCKED
}

/**
 * The surface of the landscape header group (tabs, focus capsule, lock; 用户 2026-11-12): the tab
 * bar's look — its frosted fill (over the header's plain page background that comes out as the fill
 * laid over the background, so nothing behind needs blurring), a 0.5 outer ring in the line colour,
 * a 1 inner edge highlight and the shadow 0 3 12. [fill] replaces the fill (the lock when on).
 */
internal fun Modifier.headerSurface(c: OinkColors, radius: Dp, fill: Color = c.tabbar): Modifier =
    boxShadow(c.shadow, 3.dp, 12.dp, radius).drawBehind {
        val r = radius.toPx(); val d = density
        drawRoundRect(c.bg, cornerRadius = CornerRadius(r))
        drawRoundRect(fill, cornerRadius = CornerRadius(r))
        drawRoundRect(c.line, Offset(-.5f * d, -.5f * d), Size(size.width + d, size.height + d), CornerRadius(r + .5f * d), style = Stroke(.5f * d))
        drawRoundRect(c.tabbarEdge, Offset(.5f * d, .5f * d), Size(size.width - d, size.height - d), CornerRadius(r - .5f * d), style = Stroke(d))
    }

/**
 * Header for the portrait and side layouts (docs/control-page.md §1–2, 用户 2026-11-12 确认): 64 high;
 * 调节 left, focus capsule and lock centred. Portrait: a 48 slot at the top right that holds 全屏 while
 * no keyboard is up ([canFullscreen]) and 切换键盘 while one is (the keyboard opens from the button
 * beside the tab bar). Side layout: the keyboard toggle at the top right. Landscape: [LandscapeBar].
 */
@Composable
private fun ControlHeader(holder: ControlHolder, model: AppModel, state: ControlState, layout: ControlLayout, headerTabs: @Composable () -> Unit,
                          canFullscreen: Boolean) {
    val c = Oink.colors
    val haptic = rememberHaptics()
    val focus = state.focus
    val height = 64.dp
    Box(Modifier.fillMaxWidth().height(height).background(c.bg)) {
      Box(Modifier.fillMaxSize().padding(horizontal = 16.dp)) {
        IconButton48(Modifier.align(Alignment.CenterStart), "调节", onClick = {
            haptic(false); holder.menu = if (holder.menu?.startsWith("adjust") == true) null else "adjust"
        }) { Lucide(ControlIcons.sliders, 23.dp, c.ink) }
        Row(Modifier.align(Alignment.Center).padding(horizontal = 56.dp), horizontalArrangement = Arrangement.spacedBy(8.dp),
            verticalAlignment = Alignment.CenterVertically) {
            FocusCapsule(focus, false, Modifier.weight(1f, fill = false)) { haptic(false); holder.menu = if (holder.menu == "focus") null else "focus" }
            LockButton(holder, state, false)
        }
        if (layout == ControlLayout.PORTRAIT) {
            // The top-right slot (用户 2026-11-12): 切换键盘 while a keyboard is up (pops in 200 ms), else 全屏.
            if (holder.keyboardOpen) SwitchKeyboardButton(holder, Modifier.align(Alignment.CenterEnd))
            else if (canFullscreen) IconButton48(Modifier.align(Alignment.CenterEnd), "全屏", onClick = { haptic(false); holder.fullscreen.enter() }) {
                Lucide(ControlIcons.maximize, 22.dp, c.ink)
            }
        } else {
            val kbShown = holder.keyboardOpen
            Box(Modifier.align(Alignment.CenterEnd).size(48.dp).clip(RoundedCornerShape(16.dp))
                .background(if (kbShown) c.accentSoft2 else Color.Transparent)
                .press { haptic(false); holder.toggleKeyboard() }
                .semantics { contentDescription = if (kbShown) "收起键盘" else "弹出键盘"; selected = kbShown },
                contentAlignment = Alignment.Center) {
                SvgIcon(DesignIconData.pcKeyboard, 24.dp, tint = if (kbShown) c.accentInk else c.ink, stroke = 1.4f)
            }
        }
      }
        // The menus open 6 above the header's bottom edge.
        ControlMenus(holder, model, state, height - 6.dp, land = false)
    }
}

@Composable
private fun ControlMenus(holder: ControlHolder, model: AppModel, state: ControlState, top: Dp, land: Boolean) {
    when (val m = holder.menu) {
        null -> {}
        "focus" -> FocusMenu(holder, model, state, top)
        "lock", "lock-game" -> LockMenu(holder, state, m, top)
        else -> AdjustMenu(holder, m, top, land)
    }
}

/**
 * Whether the landscape top bar takes touches (it does not while hidden, 用户 2026-11-12): read by its
 * items, including the app's tabs ([HeaderTabs]). A hidden bar keeps no pointer input at all, so the
 * pad under it gets every touch there.
 */
internal val LocalBarLive = compositionLocalOf { true }

/**
 * The landscape top bar (full screen, 用户 2026-11-12): the 48-high row floats over the full-screen pad,
 * 8 below the status bar, 16 inside the safe area: 调节 left; the app's tabs, the focus capsule and the
 * lock centred; the keyboard toggle and 恢复 right. Every item is a 48-high shape on the tab bar's
 * surface; the row itself has no fill and no pointer input, so touches between the items go to the pad
 * (which is under the bar): touches are split by where they go down.
 *
 * It stays visible during pad use. The pad is its ancestor: an idle down on a button is the
 * button's, but once the pad owns a gesture it consumes every contact in Initial, before buttons.
 * Keyboard visibility still hides the bar (an open menu keeps it up), with the existing fade.
 */
@Composable
private fun LandscapeBar(holder: ControlHolder, model: AppModel, state: ControlState, headerTabs: @Composable () -> Unit) {
    val c = Oink.colors
    val haptic = rememberHaptics()
    val keyboardOpen = holder.keyboardOpen
    val covered = keyboardOpen && (holder.keyboard.shown == KeyboardKind.SYSTEM || holder.prefs.expanded)
    val busy = covered || (holder.menu == null && keyboardOpen)
    val hidden = busy
    val shown by animateFloatAsState(if (hidden) 0f else 1f,
        if (hidden) tween(160, easing = androidx.compose.animation.core.EaseIn) else tween(240, easing = EaseOutExpo), label = "bar")
    // Omit the original bar under a full-screen keyboard.
    if (covered) return
    val live = !hidden
    Box(Modifier.fillMaxWidth().height(BAR_TOP + 48.dp)
        .graphicsLayer { alpha = shown; translationY = -(1f - shown) * 10.dp.toPx() }
        .then(if (live) Modifier else Modifier.clearAndSetSemantics { })) {
        CompositionLocalProvider(LocalBarLive provides live) {
            Box(Modifier.fillMaxWidth().padding(start = 16.dp, end = 16.dp, top = BAR_TOP).height(48.dp)) {
                BarButton(Modifier.align(Alignment.CenterStart), "调节") {
                    haptic(false); holder.menu = if (holder.menu?.startsWith("adjust") == true) null else "adjust"
                }
                Row(Modifier.align(Alignment.Center).padding(horizontal = 116.dp), horizontalArrangement = Arrangement.spacedBy(10.dp),
                    verticalAlignment = Alignment.CenterVertically) {
                    headerTabs()
                    FocusCapsule(state.focus, true, Modifier.weight(1f, fill = false)) { haptic(false); holder.menu = if (holder.menu == "focus") null else "focus" }
                    LockButton(holder, state, true)
                }
                Row(Modifier.align(Alignment.CenterEnd), horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                    // Keyboard on the left, 恢复 at the far right (用户 2026-11-12).
                    val kbShown = holder.keyboardOpen
                    BarButton(Modifier, if (kbShown) "收起键盘" else "弹出键盘", selected = kbShown) { haptic(false); holder.toggleKeyboard() }
                    BarButton(Modifier, "恢复") { haptic(false); holder.fullscreen.restore(holder.upright) }
                }
            }
        }
        // The menus open 4 below the row.
        ControlMenus(holder, model, state, BAR_TOP + 48.dp + 4.dp, land = true)
    }
}

/** The landscape row's top: 8 below the status bar (用户 2026-11-12). */
private val BAR_TOP = 8.dp

/**
 * A round 48 item of the landscape top bar on the tab bar's surface; [selected] (the keyboard toggle
 * while a keyboard is up) has the accent fill and accent glyph. Its icon comes from its [label].
 */
@Composable
private fun BarButton(modifier: Modifier, label: String, selected: Boolean = false, onClick: () -> Unit) {
    val c = Oink.colors
    val live = LocalBarLive.current
    val ink = if (selected) c.accentInk else c.ink
    Box(modifier.size(48.dp).headerSurface(c, 24.dp, if (selected) c.accentSoft2 else c.tabbar).clip(CircleShape)
        .then(if (live) Modifier.press(pressedColor = c.press) { onClick() } else Modifier)
        .semantics { contentDescription = label; if (selected) this.selected = true }, contentAlignment = Alignment.Center) {
        when (label) {
            "调节" -> Lucide(ControlIcons.sliders, 23.dp, ink)
            "恢复" -> Lucide(ControlIcons.minimize, 22.dp, ink)
            else -> SvgIcon(DesignIconData.pcKeyboard, 24.dp, tint = ink, stroke = 1.4f)
        }
    }
}

/** 切换键盘 in the portrait header's top-right slot: the switch glyph 24 in ink, 48 target, no fill; scales in from 0.7 and fades in over 200 ms. */
@Composable
private fun SwitchKeyboardButton(holder: ControlHolder, modifier: Modifier) {
    val c = Oink.colors
    val haptic = rememberHaptics()
    val shown = remember { Animatable(0f) }
    LaunchedEffect(Unit) { shown.animateTo(1f, tween(200, easing = EaseOutExpo)) }
    val toEmulated = holder.keyboard.shown == KeyboardKind.SYSTEM
    IconButton48(modifier.graphicsLayer { alpha = shown.value; val s = .7f + .3f * shown.value; scaleX = s; scaleY = s },
        if (toEmulated) "切换键盘：换到模拟键盘（发送原始键码）" else "切换键盘：换到系统键盘（手机输入法）",
        onClick = { haptic(false); holder.keyboard.switchKind() }) {
        SvgIcon(ControlIcons.switchKeyboard, 24.dp, tint = c.ink, stroke = 1.6f)
    }
}

/**
 * Focus capsule (item 9, 用户 2026-11-12 确认): the space from the capsule's left edge to the drawn
 * platform logo equals the space from the chevron to the right edge — 16 in portrait (40 high), 20 in
 * the landscape group (48 high). The logo sits in a 26 × 26 slot (the iOS frame) with −2 on each side,
 * drawn at 0.62, so it is 4.6 inside the slot; with the chevron 4 inside its 16 box the paddings are
 * left 12.5 / right 12 (landscape 16.5 / 16).
 */
@Composable
private fun FocusCapsule(focus: ControlTarget?, land: Boolean, modifier: Modifier, onClick: () -> Unit) {
    val c = Oink.colors
    val h = if (land) 48.dp else 40.dp
    val shape = RoundedCornerShape(h / 2)
    Row(modifier.height(h).then(if (land) Modifier.headerSurface(c, h / 2) else Modifier.clip(shape).background(c.chip))
        .clip(shape).then(if (LocalBarLive.current) Modifier.press(pressedColor = c.press) { onClick() } else Modifier)
        .padding(start = if (focus != null) (if (land) 16.5.dp else 12.5.dp) else (if (land) 18.dp else 14.dp), end = if (land) 16.dp else 12.dp)
        .semantics { contentDescription = focus?.let { "焦点设备：${it.name}" } ?: "选择设备" },
        horizontalArrangement = Arrangement.spacedBy(6.dp), verticalAlignment = Alignment.CenterVertically) {
        // The name rises and fades in when the focus moves (200 ms).
        val rise = with(LocalDensity.current) { 6.dp.roundToPx() }
        AnimatedContent(focus, Modifier.weight(1f, fill = false), transitionSpec = {
            (fadeIn(tween(200, easing = EaseOutExpo)) + slideInVertically(tween(200, easing = EaseOutExpo)) { rise })
                .togetherWith(fadeOut(tween(0)))
        }, contentKey = { it?.device }, label = "focus") { f ->
            Row(horizontalArrangement = Arrangement.spacedBy(6.dp), verticalAlignment = Alignment.CenterVertically) {
                if (f != null) Box(Modifier.horizontalMargin((-2).dp).size(26.dp), contentAlignment = Alignment.Center) {
                    PlatformIcon(f.platform, Modifier.wrapContentSize(unbounded = true), scale = .62f)
                }
                T(f?.name ?: "选择设备", 15f, c.ink, weight = 600, lineHeight = 20f, ellipsis = true)
            }
        }
        Lucide(ControlIcons.chevronDown, 16.dp, c.muted)
    }
}

/** CSS `margin: 0 m` (negative allowed): the box takes its width + 2m in the row and draws m in from that. */
private fun Modifier.horizontalMargin(m: Dp): Modifier = layout { measurable, constraints ->
    val p = measurable.measure(constraints)
    val px = m.roundToPx()
    layout((p.width + 2 * px).coerceAtLeast(0), p.height) { p.place(px, 0) }
}

/**
 * Lock / game button (item 8, 用户 2026-11-12 确认): game mode shows only while it is actually on
 * (开, or 自动 and on now), with the gamepad alone — no AUTO tag, the two look the same; a tap turns
 * game mode off. 自动 while off shows the lock state as usual.
 */
@Composable
private fun LockButton(holder: ControlHolder, state: ControlState, land: Boolean) {
    val c = Oink.colors
    val haptic = rememberHaptics()
    val look = state.lockLook()
    val on = look != LockLook.UNLOCKED
    val label = when (look) { LockLook.UNLOCKED -> "指针未锁定"; LockLook.LOCKED -> "指针已锁定"; LockLook.GAME_ON -> "游戏模式已开启"; LockLook.GAME_AUTO -> "游戏模式已开启（自动）" }
    val bg by androidx.compose.animation.animateColorAsState(if (on) c.accentSoft2 else if (land) c.tabbar else c.chip, tween(200), label = "lockBg")
    val ink by androidx.compose.animation.animateColorAsState(if (on) c.accentInk else c.text2, tween(200), label = "lockInk")
    val live = LocalBarLive.current
    Box(Modifier.size(if (land) 48.dp else 40.dp).then(if (land) Modifier.headerSurface(c, 24.dp, bg) else Modifier.background(bg, CircleShape))
        .then(if (!live) Modifier else Modifier.pointerInput(look) {
            awaitEachGesture {
                awaitFirstDown().consume()
                val released = withTimeoutOrNull(LONG_PRESS_MS) { waitForUpOrCancellation() != null }
                if (released == null) {
                    haptic(true); holder.menu = "lock"
                    waitForUpOrCancellation()
                } else if (released) {
                    haptic(false)
                    val s = holder.session ?: return@awaitEachGesture
                    when (look) {
                        LockLook.UNLOCKED -> s.lock(true)
                        LockLook.LOCKED -> s.lock(false)
                        else -> { s.game(ControlGameMode.OFF); s.lock(false) }
                    }
                }
            }
        })
        .semantics {
            contentDescription = "$label，点按${when (look) { LockLook.UNLOCKED -> "锁定"; LockLook.LOCKED -> "解锁"; else -> "关闭游戏模式" }}，长按更多"
            role = Role.Button
            onClick {
                haptic(false)
                holder.session?.let { s -> when (look) { LockLook.UNLOCKED -> s.lock(true); LockLook.LOCKED -> s.lock(false); else -> { s.game(ControlGameMode.OFF); s.lock(false) } } }
                true
            }
            onLongClick("更多") { haptic(true); holder.menu = "lock"; true }
        }, contentAlignment = Alignment.Center) {
        when (look) {
            LockLook.UNLOCKED -> SvgIcon(DesignIconData.actionUnlock, 21.dp, tint = ink, stroke = 1.8f)
            LockLook.LOCKED -> Lucide(DesignIconData.lucideLockKeyhole, 20.dp, ink)
            else -> SvgIcon(DesignIconData.pcGame, 22.dp, tint = ink, stroke = 1.4f)
        }
    }
}


// ---------- Menus (Material 3 look: elevated surface, radius 16, rows 48, values and checks at the end) ----------

/** A single-card menu (the focus list) [top] below the header's top, 12 from the left or centred. */
@Composable
private fun ControlMenu(holder: ControlHolder, centered: Boolean, top: Dp, content: @Composable ColumnScope.() -> Unit) {
    val density = LocalDensity.current
    val offset = with(density) { IntOffset(0, top.roundToPx()) }
    Popup(alignment = if (centered) Alignment.TopCenter else Alignment.TopStart, offset = offset,
        onDismissRequest = { holder.menu = null }, properties = PopupProperties(focusable = true)) {
        BoxWithConstraints {
            Column(Modifier.padding(start = 12.dp, end = 12.dp, bottom = 24.dp)
                .menuCard(if (centered) .5f else 0f, Modifier.widthIn(min = 236.dp, max = maxWidth - 24.dp).width(IntrinsicSize.Max)), content = content)
        }
    }
}

/**
 * The menu card shared by all control menus: elevated surface, radius 16, border, shadow, 8 above
 * and below the rows; it appears from its top edge at [originX] (0 = left, .5 = centre), scaling
 * 0.9 → 1 and fading in over 260 ms. [size] goes between the animation and the surface.
 */
@Composable
private fun Modifier.menuCard(originX: Float, size: Modifier): Modifier {
    val c = Oink.colors
    val shown = remember { Animatable(0f) }
    LaunchedEffect(Unit) { shown.animateTo(1f, tween(260, easing = EaseOutExpo)) }
    return graphicsLayer {
        val s = .9f + .1f * shown.value
        scaleX = s; scaleY = s; alpha = shown.value
        transformOrigin = TransformOrigin(originX, 0f)
    }
        .then(size)
        .boxShadow(c.shadow, 6.dp, 24.dp, 16.dp)
        .clip(RoundedCornerShape(16.dp)).background(c.elev).border(1.dp, c.border, RoundedCornerShape(16.dp))
        .padding(vertical = 8.dp)
}

/** One menu row: optional leading icon, the label, a value at the end (accent in 调节), a check when chosen. */
@Composable
private fun MenuItem(label: String, value: String? = null, height: Dp = 48.dp, accentValue: Boolean = false, checked: Boolean? = null,
                     leading: (@Composable () -> Unit)? = null, onClick: () -> Unit) {
    val c = Oink.colors
    val haptic = rememberHaptics()
    Row(Modifier.fillMaxWidth().heightIn(min = height).press(pressedColor = c.press, role = if (checked != null) Role.RadioButton else Role.Button) { haptic(false); onClick() }
        .padding(horizontal = 16.dp).semantics(mergeDescendants = true) { if (checked != null) selected = checked },
        horizontalArrangement = Arrangement.spacedBy(14.dp), verticalAlignment = Alignment.CenterVertically) {
        leading?.invoke()
        // The name never shortens; a long value ends in an ellipsis.
        if (value == null) T(label, 15f, c.ink, Modifier.weight(1f), lineHeight = 21f, ellipsis = true)
        else {
            T(label, 15f, c.ink, lineHeight = 21f, maxLines = 1)
            T(value, 14f, if (accentValue) c.accentInk else c.muted, Modifier.weight(1f), weight = if (accentValue) 500 else 400,
                lineHeight = 20f, ellipsis = true, align = TextAlign.End)
        }
        if (checked == true) Lucide(ControlIcons.check, 18.dp, c.accentInk, 2.4f)
    }
}


/**
 * A menu card with an optional second card (调节 and the lock menu, 用户 2026-11-11 / 2026-11-12):
 * [first] opens [top] below the header's top, 12 from the left or centred; when [open] names one of
 * its rows (recorded in [rows] with [anchor]), [second] opens as another card anchored to that row
 * ([SubMenu]; [over]: on the row itself, landscape) while the first stays open.
 *
 * Both cards live in one popup window that reaches from the menu's top down to the bottom of the
 * screen: the second card then always lies above the first and is placed in the same coordinates
 * as the row. (Two popup windows of the same type are stacked by the order they were added in,
 * which neither the code nor the screenshot tests can see.) A tap outside the cards, or Back,
 * runs [back] (the caller closes the second card first, then the menu). The popup also keeps every
 * touch below the menu's top from the landscape pad under it.
 */
@Composable
private fun TwoCardMenu(top: Dp, centered: Boolean, open: String?, rows: Map<String, Rect>, back: () -> Unit, over: Boolean = false,
                        first: @Composable ColumnScope.() -> Unit, second: @Composable ColumnScope.() -> Unit) {
    val density = LocalDensity.current
    val view = LocalView.current
    // From the menu's top to the bottom of the window (the popup's height).
    var below by remember { mutableIntStateOf(0) }
    val menuTop = with(density) { top.roundToPx() }
    Spacer(Modifier.fillMaxSize().onGloballyPositioned { below = (view.height - it.positionInWindow().y.roundToInt() - menuTop).coerceAtLeast(0) })
    Popup(alignment = Alignment.TopStart, offset = IntOffset(0, menuTop), onDismissRequest = back, properties = PopupProperties(focusable = true)) {
        BoxWithConstraints(Modifier.fillMaxWidth().then(if (below > 0) Modifier.height(with(density) { below.toDp() }) else Modifier).pointerInput(Unit) { detectTapGestures { back() } }) {
            Column(Modifier.align(if (centered) Alignment.TopCenter else Alignment.TopStart).padding(start = 12.dp, end = 12.dp, bottom = 24.dp)
                .menuCard(if (centered) .5f else 0f, Modifier.widthIn(min = 236.dp, max = maxWidth - 24.dp).width(IntrinsicSize.Max)).swallowTaps(), content = first)
            val row = open?.let { rows[it] }
            if (row != null) SubMenu(row, over, second)
        }
    }
}

/** Records where a row of a [TwoCardMenu] sits in its popup, under [key]. */
private fun Modifier.anchor(rows: MutableMap<String, Rect>, key: String) = onGloballyPositioned { rows[key] = Rect(it.positionInRoot(), it.size.toSize()) }

/**
 * 调节 (§2.1, 用户 2026-11-11 / 2026-11-12): 指针速度 with its slider always shown; the switches
 * 键盘自动弹出 ([ControlPrefs.keyboardAutoPopup]), 接收音频 and 共享麦克风 (this phone's device settings,
 * the device page's switches: same state, same rules and failure notices); then one-line rows (name
 * left, value right in the accent colour) 键盘布局 and 单指划动. A row opens its options as a second
 * menu card over this one, anchored to the row (`menu` = adjust-layout / -swipe); picking one closes
 * only that card and the row shows the new value. 屏幕方向 is gone (全屏 replaces it).
 *
 * Landscape ([land]): too short for one column, so two columns 250 wide with a 1 line between them —
 * speed and the switches left, the two choices right; the second card lies on its row (first option
 * on the row, left edges together) and lists more than four options in two columns (four rows), so
 * nothing runs off the screen.
 */
@Composable
private fun AdjustMenu(holder: ControlHolder, menu: String, top: Dp, land: Boolean) {
    val c = Oink.colors
    val p = holder.prefs
    val options = listOf(
        AdjustOption("adjust-layout", "键盘布局", ControlPrefs.LAYOUTS, p.layout) { holder.prefs.copy(layout = it) },
        AdjustOption("adjust-swipe", "单指划动", ControlPrefs.SWIPE, p.swipe) { holder.prefs.copy(swipe = it) },
    )
    val open = options.firstOrNull { it.key == menu }
    val rows = remember { mutableStateMapOf<String, Rect>() }
    @Composable fun speedAndSwitches() {
        SpeedRow(p.speed) { holder.update(holder.prefs.copy(speed = it)) }
        SwitchMenuRow("键盘自动弹出", p.keyboardAutoPopup) { holder.update(holder.prefs.copy(keyboardAutoPopup = it)) }
        SwitchMenuRow("接收音频", holder.speakerOn, enabled = !holder.forwardOn) { holder.receiveSpeaker() }
        SwitchMenuRow("共享麦克风", holder.micOn) { holder.toggleMic() }
    }
    @Composable fun choices() = options.forEach { o ->
        Box(Modifier.anchor(rows, o.key)) {
            MenuItem(o.name, ControlPrefs.label(o.options, o.current), 56.dp, accentValue = true) { holder.menu = o.key }
        }
    }
    TwoCardMenu(top, centered = false, open = open?.key, rows = rows, back = { holder.menu = if (holder.menu != "adjust") "adjust" else null }, over = land, first = {
        if (land) Row(Modifier.height(IntrinsicSize.Min)) {
            Column(Modifier.width(250.dp)) { speedAndSwitches() }
            Box(Modifier.fillMaxHeight().width(1.dp).background(c.line))
            Column(Modifier.width(250.dp)) { choices() }
        } else {
            speedAndSwitches()
            choices()
        }
    }) {
        val list = open?.options.orEmpty()
        @Composable fun item(id: String, text: String) = MenuItem(text, checked = open?.current == id) { open?.let { holder.update(it.pick(id)) }; holder.menu = "adjust" }
        if (land && list.size > 4) Row {
            val half = (list.size + 1) / 2
            Column(Modifier.width(IntrinsicSize.Max).widthIn(min = 200.dp)) { list.take(half).forEach { (id, text) -> item(id, text) } }
            Column(Modifier.width(IntrinsicSize.Max).widthIn(min = 200.dp)) { list.drop(half).forEach { (id, text) -> item(id, text) } }
        } else list.forEach { (id, text) -> item(id, text) }
    }
}

/** A switch row of 调节 (用户 2026-11-12): name left, the Material switch right, the whole row is the target; 56 high. */
@Composable
private fun SwitchMenuRow(label: String, checked: Boolean, enabled: Boolean = true, onChange: (Boolean) -> Unit) {
    val c = Oink.colors
    val haptic = rememberHaptics()
    Row(Modifier.fillMaxWidth().alpha(if (enabled) 1f else .4f).heightIn(min = 56.dp).press(enabled = enabled, pressedColor = c.press, role = Role.Switch) { haptic(false); onChange(!checked) }
        .padding(horizontal = 16.dp).semantics(mergeDescendants = true) { toggleableState = androidx.compose.ui.state.ToggleableState(checked) },
        horizontalArrangement = Arrangement.spacedBy(12.dp), verticalAlignment = Alignment.CenterVertically) {
        T(label, 15f, c.ink, Modifier.weight(1f), lineHeight = 21f, maxLines = 1)
        OinkSwitch(checked)
    }
}

/** A row of 调节 with a second menu: its `menu` key, name, options (id to label), current id, and the prefs after picking one. */
private class AdjustOption(val key: String, val name: String, val options: List<Pair<String, String>>, val current: String, val pick: (String) -> ControlPrefs)

/**
 * The second menu card of a [TwoCardMenu], anchored to [row] (in the same popup): under the row, 24
 * in from its left edge, or above the row when it does not fit below. [over] (landscape): on the row
 * itself — its first option over the row (the card's top padding above it), left edges together —
 * moved up just enough to stay 8 above the screen's bottom edge.
 */
@Composable
private fun BoxScope.SubMenu(row: Rect, over: Boolean, content: @Composable ColumnScope.() -> Unit) {
    Layout({ Column(Modifier.menuCard(0f, Modifier.widthIn(min = 196.dp).width(IntrinsicSize.Max)).swallowTaps(), content = content) },
        Modifier.matchParentSize()) { measurables, constraints ->
        val card = measurables.single().measure(constraints.copy(minWidth = 0, minHeight = 0))
        val margin = 8.dp.roundToPx(); val gap = 2.dp.roundToPx()
        val maxLeft = (constraints.maxWidth - margin - card.width).coerceAtLeast(margin)
        val left: Int
        var top: Int
        if (over) {
            left = row.left.roundToInt().coerceIn(margin, maxLeft)
            top = row.top.roundToInt() - 8.dp.roundToPx()
            if (top + card.height > constraints.maxHeight - margin) top = (constraints.maxHeight - margin - card.height).coerceAtLeast(0)
        } else {
            left = (row.left.roundToInt() + 24.dp.roundToPx()).coerceIn(margin, maxLeft)
            top = row.bottom.roundToInt() + gap
            if (top + card.height > constraints.maxHeight - margin) top = (row.top.roundToInt() - gap - card.height).coerceAtLeast(0)
        }
        layout(constraints.maxWidth, constraints.maxHeight) { card.place(left, top) }
    }
}

/** A menu card keeps taps on its padding from reaching the dismiss area behind it. */
private fun Modifier.swallowTaps(): Modifier = pointerInput(Unit) {
    awaitEachGesture { awaitFirstDown(requireUnconsumed = false).consume() }
}

/**
 * 指针速度 (用户 2026-11-11): the name and the number, the slider always under them (track 4, accent
 * thumb 20, 慢 / 快 under the ends). Touching or dragging along the slider sets the value, with a
 * tick per step; screen readers get an ordinary adjustable slider.
 */
@Composable
private fun SpeedRow(value: Int, onChange: (Int) -> Unit) {
    val c = Oink.colors
    val view = LocalView.current
    var trackW by remember { mutableStateOf(1f) }
    val current by rememberUpdatedState(value)
    val change by rememberUpdatedState(onChange)
    Column(Modifier.fillMaxWidth().padding(start = 16.dp, end = 16.dp, top = 14.dp, bottom = 10.dp)
        .semantics(mergeDescendants = true) {
            contentDescription = "指针速度"
            progressBarRangeInfo = ProgressBarRangeInfo(value.toFloat(), 1f..10f, 8)
            setProgress { v -> onChange(v.roundToInt().coerceIn(1, 10)); true }
        }) {
        Row(Modifier.fillMaxWidth().heightIn(min = 24.dp), verticalAlignment = Alignment.CenterVertically) {
            T("指针速度", 15f, c.ink, Modifier.weight(1f), lineHeight = 21f)
            T("$value", 15f, c.accentInk, weight = 600, lineHeight = 21f, tabular = true)
        }
        // The 36-high strip around the track takes the touches; the track is 12 in on each side.
        Box(Modifier.fillMaxWidth().height(36.dp).pointerInput(Unit) {
            val inset = 12.dp.toPx()
            fun set(x: Float) {
                val v = (1 + ((x - inset) / trackW).coerceIn(0f, 1f) * 9).roundToInt()
                if (v != current) { view.performHapticFeedback(HapticFeedbackConstants.CLOCK_TICK); change(v) }
            }
            awaitEachGesture {
                val down = awaitFirstDown()
                down.consume()
                set(down.position.x)
                while (true) {
                    val e = awaitPointerEvent()
                    val ch = e.changes.firstOrNull { it.id == down.id } ?: break
                    if (!ch.pressed) break
                    ch.consume()
                    set(ch.position.x)
                }
            }
        }) {
            Box(Modifier.align(Alignment.Center).padding(horizontal = 12.dp).fillMaxWidth().height(4.dp)
                .onGloballyPositioned { trackW = it.size.width.toFloat().coerceAtLeast(1f) }) {
                val f = (value - 1) / 9f
                Box(Modifier.fillMaxSize().clip(RoundedCornerShape(2.dp)).background(c.track))
                Box(Modifier.fillMaxHeight().fillMaxWidth(f).clip(RoundedCornerShape(2.dp)).background(c.accent))
                // (The menu measures its intrinsic width, so no BoxWithConstraints here.)
                // requiredSize lets the 20 thumb overflow the 4-high track, centred on it vertically.
                Box(Modifier.offset { IntOffset((trackW * f - 10.dp.toPx()).roundToInt(), 0) }.requiredSize(20.dp).clip(CircleShape).background(c.accent))
            }
        }
        Row(Modifier.fillMaxWidth().padding(horizontal = 4.dp)) {
            T("慢", 11f, c.faint, Modifier.weight(1f), lineHeight = 15f)
            T("快", 11f, c.faint, lineHeight = 15f)
        }
    }
}


@Composable
private fun FocusMenu(holder: ControlHolder, model: AppModel, state: ControlState, top: Dp) {
    val c = Oink.colors
    val focus = state.focus?.device
    ControlMenu(holder, centered = true, top = top) {
        T("可控制的在线设备", 12.5f, c.muted, Modifier.padding(start = 16.dp, end = 16.dp, top = 2.dp, bottom = 6.dp), lineHeight = 17f)
        val online = state.targets.filter { it.online }
        if (online.isEmpty()) T("没有在线的电脑", 15f, c.text2, Modifier.padding(horizontal = 16.dp, vertical = 12.dp), lineHeight = 21f)
        online.forEach { t ->
            val latency = model.peers.firstOrNull { it.id == t.device }?.latency
            Row(Modifier.fillMaxWidth().heightIn(min = 48.dp).press(pressedColor = c.press, role = Role.RadioButton) {
                holder.menu = null; holder.session?.select(t.device)
            }.padding(horizontal = 16.dp, vertical = 6.dp).semantics(mergeDescendants = true) { selected = t.device == focus },
                horizontalArrangement = Arrangement.spacedBy(14.dp), verticalAlignment = Alignment.CenterVertically) {
                PlatformIcon(t.platform, Modifier.size(24.dp, 26.dp).wrapContentSize(unbounded = true), scale = .62f)
                Column(Modifier.weight(1f)) {
                    T(t.name, 15f, c.ink, lineHeight = 21f, ellipsis = true)
                    if (latency != null) T("${latency.toInt()} ms", 13f, c.muted, lineHeight = 18f)
                }
                if (t.device == focus) Lucide(ControlIcons.check, 18.dp, c.accentInk, 2.4f)
            }
        }
    }
}

/**
 * Long press on the lock button (item 8, 用户 2026-11-12 确认): the same card as 调节 — rows 56,
 * name left, value right in the accent colour (weight 500) — keeping the leading icons (lock 17 /
 * unlock 18 / gamepad 18 in text2, in a 20 slot). 游戏模式 opens a second card under its row
 * (自动 / 开 / 关, check at the end) while the first stays open; no 「‹ 游戏模式」 back row.
 */
@Composable
private fun LockMenu(holder: ControlHolder, state: ControlState, menu: String, top: Dp) {
    val c = Oink.colors
    val game = when (state.gameMode) { ControlGameMode.AUTO -> "auto"; ControlGameMode.ON -> "on"; ControlGameMode.OFF -> "off" }
    val label = mapOf("auto" to "自动", "on" to "开", "off" to "关")
    val rows = remember { mutableStateMapOf<String, Rect>() }
    @Composable fun glyph(content: @Composable () -> Unit) = Box(Modifier.size(20.dp), contentAlignment = Alignment.Center) { content() }
    TwoCardMenu(top, centered = true, open = if (menu == "lock-game") "game" else null, rows = rows,
        back = { holder.menu = if (holder.menu == "lock-game") "lock" else null }, first = {
        MenuItem(if (state.locked) "解锁指针" else "锁定指针", height = 56.dp, leading = {
            glyph { if (state.locked) SvgIcon(DesignIconData.actionUnlock, 18.dp, tint = c.text2, stroke = 1.8f) else Lucide(DesignIconData.lucideLockKeyhole, 17.dp, c.text2) }
        }) {
            holder.menu = null
            val s = holder.session
            // Game mode off first: turning it off may also drop the lock it implies.
            if (state.locked) s?.lock(false) else { s?.game(ControlGameMode.OFF); s?.lock(true) }
        }
        Box(Modifier.anchor(rows, "game")) {
            MenuItem("游戏模式", label[game], 56.dp, accentValue = true, leading = { glyph { SvgIcon(DesignIconData.pcGame, 18.dp, tint = c.text2, stroke = 1.4f) } }) {
                holder.menu = "lock-game"
            }
        }
    }) {
        listOf("auto" to ControlGameMode.AUTO, "on" to ControlGameMode.ON, "off" to ControlGameMode.OFF).forEach { (id, mode) ->
            MenuItem(label.getValue(id), checked = game == id) { holder.menu = null; holder.session?.game(mode) }
        }
    }
}

// ---------- Touchpad ----------

/** 160 × 160 grain tile (CSS fractal noise stand-in): fixed seed so every launch and screenshot looks the same. */
private fun grainTile(density: Float): ImageBitmap {
    val cell = density.roundToInt().coerceAtLeast(1)
    val n = 160
    val bmp = android.graphics.Bitmap.createBitmap(n * cell, n * cell, android.graphics.Bitmap.Config.ARGB_8888)
    val rnd = java.util.Random(7)
    // The mockup's noise is mid grey with noisy alpha under soft-light, so it only nudges the surface:
    // values stay close to the neutral 128.
    val v = Array(n) { IntArray(n) { 112 + rnd.nextInt(33) } }
    val px = IntArray(n * cell * n * cell)
    for (y in 0 until n * cell) for (x in 0 until n * cell) {
        // Two octaves: the cell value plus a softer 2 × 2 average, like baseFrequency .9 / 2 octaves.
        val a = v[y / cell][x / cell]
        val b = v[(y / cell / 2 * 2) % n][(x / cell / 2 * 2) % n]
        val g = ((a * 2 + b) / 3).coerceIn(0, 255)
        px[y * n * cell + x] = (0xFF shl 24) or (g shl 16) or (g shl 8) or g
    }
    bmp.setPixels(px, 0, n * cell, 0, 0, n * cell, n * cell)
    return bmp.asImageBitmap()
}

/** Touchpad diagnostics to logcat (tag OinkPad), at most [PER_SECOND] lines a second; drops are counted. */
internal class PadLog {
    private var windowStart = 0L
    private var lines = 0
    private var dropped = 0
    fun line(text: String) {
        val now = android.os.SystemClock.uptimeMillis()
        if (now - windowStart >= 1000) {
            if (dropped > 0) android.util.Log.i("OinkPad", "($dropped lines dropped)")
            windowStart = now; lines = 0; dropped = 0
        }
        if (lines >= PER_SECOND) { dropped++; return }
        lines++
        android.util.Log.i("OinkPad", text)
    }
    private companion object { const val PER_SECOND = 40 }
}

/**
 * The touchpad. [flat]: the landscape full screen (用户 2026-11-12) — the pad is the whole screen, so it
 * has no corners, outline or drop shadow of its own (the screen's corners round it); [bottomInset]
 * keeps the 释放 capsule above the navigation bar it reaches under.
 */
@Composable
private fun Touchpad(holder: ControlHolder, cc: ControlColors, modifier: Modifier, flat: Boolean = false, bottomInset: Dp = 0.dp, overlay: @Composable BoxScope.() -> Unit = {}) {
    val c = Oink.colors
    val pad = holder.pad
    val density = LocalDensity.current
    val view = LocalView.current
    val grain = remember(density.density) { grainTile(density.density) }
    val pressed = pad.pressed
    var glowAt by remember { mutableStateOf(Offset.Zero) }
    if (pressed != null) glowAt = Offset(pressed.first, pressed.second)
    val glow by animateFloatAsState(if (pressed != null) 1f else 0f, tween(if (pressed != null) 120 else 300, easing = androidx.compose.animation.core.FastOutSlowInEasing), label = "glow")
    val hint by animateFloatAsState(if (holder.keyboardOpen) 0f else 1f,
        if (holder.keyboardOpen) tween(160, easing = androidx.compose.animation.core.EaseIn) else tween(240, 120, EaseOutExpo), label = "hint")

    pad.onEdgeFull = { view.performHapticFeedback(HapticFeedbackConstants.CONFIRM) }
    // Each click (single, double, right) ticks like the iOS pad's selection haptic, through the
    // view so the system's touch-feedback setting applies.
    pad.onClick = { view.performHapticFeedback(HapticFeedbackConstants.CONTEXT_CLICK) }
    pad.onDragTick = { view.performHapticFeedback(if (android.os.Build.VERSION.SDK_INT >= 34)
        HapticFeedbackConstants.SEGMENT_TICK else HapticFeedbackConstants.CLOCK_TICK) }
    // Touchpad diagnostics to logcat (tag OinkPad): finger count changes, multi-finger decisions,
    // gesture begin / end. At most 40 lines a second, so a long session cannot flood the log.
    val padLog = remember { PadLog() }
    val padCoords = remember { arrayOfNulls<androidx.compose.ui.layout.LayoutCoordinates>(1) }
    pad.log = padLog::line
    // A tap's deferred button-up (tap-and-drag): due unless the next finger lands first.
    LaunchedEffect(pad.tapUpAt) {
        val at = pad.tapUpAt ?: return@LaunchedEffect
        delay((at - android.os.SystemClock.uptimeMillis()).coerceAtLeast(0))
        pad.expireTap(android.os.SystemClock.uptimeMillis())
    }
    // A two-finger tap's held-back right click: due unless a second two-finger tap (smart zoom) comes first.
    LaunchedEffect(pad.rightTapAt) {
        val at = pad.rightTapAt ?: return@LaunchedEffect
        delay((at + TouchpadModel.TWO_FINGER_DOUBLE_TAP_MS - android.os.SystemClock.uptimeMillis()).coerceAtLeast(0))
        pad.expireRightTap(android.os.SystemClock.uptimeMillis())
    }
    // Design scenes: put the finger / edge-lock demo in place once the pad has its size.
    val scene = holder.init
    Box(modifier.then(if (flat) Modifier else Modifier.boxShadow(cc.padDrop, 1.dp, 3.dp, 32.dp))
        .onGloballyPositioned { padCoords[0] = it; with(density) { pad.width = it.size.width.toDp().value; pad.height = it.size.height.toDp().value } }
        .pointerInput(pad) {
            awaitEachGesture {
              try {
                // In full screen the top bar is a child, so its idle buttons get first refusal.
                // Once the pad starts, Initial consumes even new fingers before child hit targets
                // see them; removing button handlers after recomposition alone misses rapid downs.
                val first = awaitFirstDown(requireUnconsumed = true, pass = androidx.compose.ui.input.pointer.PointerEventPass.Final)
                first.consume()
                holder.menu = null
                val d = density.density
                // A change's position is measured from the pad's current layout, so a layout change
                // during a touch (keyboard shown / hidden, rotation, the floating bar) makes resting
                // fingers appear to jump. The pad's origin in the root is followed per event and the
                // model rebased by its change; no motion is sent for the layout change itself.
                fun padOrigin() = padCoords[0]?.takeIf { it.isAttached }?.positionInRoot()
                var origin = padOrigin()
                pad.down(first.id.value, first.position.x / d, first.position.y / d, first.uptimeMillis)
                val down = HashSet<Long>().apply { add(first.id.value) }
                while (down.isNotEmpty()) {
                    val e = awaitPointerEvent(androidx.compose.ui.input.pointer.PointerEventPass.Initial)
                    val now = padOrigin()
                    val before = origin
                    if (now != null && before != null && now != before) pad.shift((before.x - now.x) / d, (before.y - now.y) / d)
                    if (now != null) origin = now
                    // The system took the touch away (ACTION_CANCEL; on Xiaomi phones the three-finger
                    // screenshot gesture does this as soon as a third finger lands). Compose does not
                    // throw for it: it synthesizes one event with every finger up and the up already
                    // consumed. Treated as lifts, a scroll would glide on and a Dock swipe would
                    // complete; it cancels instead (contacts Cancel, scroll / gesture Cancelled).
                    if (e.changes.any { it.id.value in down } &&
                        e.changes.all { !it.pressed && (it.id.value !in down || it.isConsumed) }) {
                        padLog.line("system cancel: pointers=${e.changes.size} down=${down.size}")
                        pad.cancel(android.os.SystemClock.uptimeMillis())
                        e.changes.forEach { it.consume() }
                        break
                    }
                    // Fingers already down move in one call per event, so the pointer moves once
                    // per event (several calls microseconds apart read as a huge finger speed).
                    val moved = HashMap<Long, Pair<Float, Float>>()
                    var movedAt = Long.MIN_VALUE
                    for (ch in e.changes) {
                        if (ch.pressed && ch.id.value in down) {
                            moved[ch.id.value] = ch.position.x / d to ch.position.y / d
                            movedAt = maxOf(movedAt, ch.uptimeMillis)
                        }
                    }
                    if (moved.isNotEmpty()) pad.move(moved, movedAt)
                    for (ch in e.changes) {
                        val id = ch.id.value
                        when {
                            ch.pressed && id !in down -> { down += id; pad.down(id, ch.position.x / d, ch.position.y / d, ch.uptimeMillis) }
                            !ch.pressed && id in down -> { down -= id; pad.up(id, ch.uptimeMillis) }
                        }
                        ch.consume()
                    }
                }
              } catch (e: kotlinx.coroutines.CancellationException) { pad.cancel(android.os.SystemClock.uptimeMillis()); throw e }
            }
        }
        .semantics { contentDescription = "触摸板" }) {
        // Keep the animated texture in a separate draw node from the floating toolbar. A glow
        // invalidation must redraw the pad, without replacing its sibling toolbar's cached layer.
        Box(Modifier.matchParentSize().drawBehind { drawPad(cc, grain, glow, glowAt, density.density, if (flat) 0f else 32f) })
        // Hint in the middle; hidden while a keyboard is up.
        Column(Modifier.align(Alignment.Center).alpha(hint).clearAndSetSemantics { }, horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.spacedBy(1.dp)) {
            T("触摸移动", 11.5f, cc.padHint, weight = 500, lineHeight = 15f, letterSpacing = .3f)
            T("轻触单击", 11.5f, cc.padHint, weight = 500, lineHeight = 15f, letterSpacing = .3f)
        }
        pad.ripple?.let { r -> key(r.id) { ClickRipple(r) } }
        pad.edge?.let { e -> EdgeBadge(e) }
        if (pad.locked != null) {
            // Screen readers hear 手指已锁定 when the lock starts.
            Box(Modifier.size(1.dp).semantics { liveRegion = LiveRegionMode.Polite; contentDescription = "手指已锁定" })
            ReleaseCapsule(Modifier.align(Alignment.BottomCenter).padding(bottom = 14.dp + bottomInset)) { pad.releaseLock() }
        }
        if (scene?.edgeDemo != null || scene?.touch != null) EdgeDemo(holder, scene)
        overlay()
    }
}

/** Places a scene's finger on the pad through the model, as a real touch would. */
@Composable
private fun EdgeDemo(holder: ControlHolder, scene: ControlInit) {
    val pad = holder.pad
    LaunchedEffect(scene) {
        while (pad.width <= 0f) androidx.compose.runtime.withFrameMillis { }
        val w = pad.width; val h = pad.height
        fun at(x: Float, y: Float) = x to y
        val path = when (scene.edgeDemo) {
            "half" -> at(w - 40, h * .42f); "full", "locked", "relay" -> at(w - 12, h * .42f); "corner" -> at(w - 34, h - 30)
            else -> scene.touch?.let { at(w * it.first, h * it.second) }
        } ?: return@LaunchedEffect
        // The edge lock belongs to a drag: tap, then press again and move (tap-and-drag).
        val drag = scene.edgeDemo != null
        val t = if (drag) { pad.down(899, w * .5f, h * .42f, 0); pad.up(899, 40); 100L } else 0L
        pad.down(900, w * .5f, h * .42f, t)
        pad.move(900, (w * .5f + path.first) / 2, (h * .42f + path.second) / 2, t + 16)
        pad.move(900, path.first, path.second, t + 32)
        if (scene.edgeDemo == "locked" || scene.edgeDemo == "relay") pad.up(900, t + 48)
        if (scene.edgeDemo == "relay") pad.down(901, w * .35f, h * .55f, t + 400)
    }
}

private fun DrawScope.drawPad(cc: ControlColors, grain: ImageBitmap, glow: Float, at: Offset, dp: Float, radius: Float) {
    val r = radius * dp
    val w = size.width; val h = size.height
    val shape = Path().apply { addRoundRect(RoundRect(0f, 0f, w, h, CornerRadius(r))) }
    // Outer 0.5 outline (none on the full-screen pad).
    if (radius > 0f) drawRoundRect(cc.padOutline, Offset(-.5f * dp, -.5f * dp), Size(w + dp, h + dp), CornerRadius(r + .5f * dp), style = Stroke(.5f * dp))
    clipPath(shape) {
        // radial-gradient(120% 85% at 28% 0%): an ellipse drawn as a circle squeezed vertically.
        val rx = 1.2f * w; val ry = .85f * h
        withTransform({ scale(1f, ry / rx, Offset(.28f * w, 0f)) }) {
            drawRect(Brush.radialGradient(0f to cc.padHi, .58f to cc.padMid, 1f to cc.padLo, center = Offset(.28f * w, 0f), radius = rx),
                size = Size(w, h * rx / ry))
        }
        drawRect(ShaderBrush(ImageShader(grain, TileMode.Repeated, TileMode.Repeated)), alpha = cc.padGrain, blendMode = BlendMode.Softlight)
        // Bottom inner shadow (inset 0 −10 24) and, while pressed, the top one (inset 0 2 10).
        drawRect(Brush.verticalGradient(listOf(Color.Transparent, cc.padInner), startY = h - 34f * dp, endY = h), topLeft = Offset(0f, h - 34f * dp))
        if (glow > 0f) {
            drawRect(Brush.verticalGradient(listOf(cc.padPress, Color.Transparent), startY = 0f, endY = 12f * dp), alpha = glow)
            drawCircle(Brush.radialGradient(0f to cc.padGlow, .7f to Color.Transparent, center = at * dp, radius = 150f * dp), 150f * dp, at * dp, alpha = glow)
        }
        // Inner hairline edge and the 1 dp top highlight.
        drawRoundRect(cc.padEdge, cornerRadius = CornerRadius(r), style = Stroke(2f * dp))
        val shifted = Path().apply { addRoundRect(RoundRect(0f, dp, w, h + dp, CornerRadius(r))) }
        drawPath(Path.combine(PathOperation.Difference, shape, shifted), cc.padHighlight)
    }
}

/** Click feedback: a theme ring (two for a double click, dashed for a right click), 500 ms, 0.4 → 1.25. */
@Composable
private fun ClickRipple(r: Ripple) {
    val c = Oink.colors
    val a = remember { Animatable(0f) }
    val b = remember { Animatable(0f) }
    LaunchedEffect(Unit) { a.animateTo(1f, tween(500, easing = EaseOutExpo)) }
    LaunchedEffect(Unit) { if (r.kind == RippleKind.DOUBLE) { delay(120); b.animateTo(1f, tween(500, easing = EaseOutExpo)) } }
    val label = when (r.kind) { RippleKind.CLICK -> "单击"; RippleKind.DOUBLE -> "双击"; RippleKind.RIGHT -> "右键" }
    Canvas(Modifier.fillMaxSize().semantics { liveRegion = LiveRegionMode.Polite; contentDescription = label }) {
        fun ring(t: Float) {
            if (t <= 0f || t >= 1f) return
            val radius = 22.dp.toPx() * (.4f + .85f * t)
            drawCircle(c.accent.copy(alpha = .7f * (1f - t)), radius, Offset(r.x.dp.toPx(), r.y.dp.toPx()),
                style = Stroke(2.dp.toPx(), pathEffect = if (r.kind == RippleKind.RIGHT) androidx.compose.ui.graphics.PathEffect.dashPathEffect(floatArrayOf(5.dp.toPx(), 4.dp.toPx())) else null))
        }
        ring(a.value); ring(b.value)
    }
}

/** Lock badge (36): lock icon in a progress ring; solid accent and 1.08 when full. */
@Composable
private fun BoxScope.EdgeBadge(e: EdgeLock) {
    val c = Oink.colors
    val full = e.full
    val bg by androidx.compose.animation.animateColorAsState(if (full) c.accent else c.elev, tween(120), label = "badgeBg")
    val ink by androidx.compose.animation.animateColorAsState(if (full) c.onAccent else c.accentInk, tween(120), label = "badgeInk")
    val s by animateFloatAsState(if (full) 1.08f else 1f, tween(120), label = "badgeScale")
    Box(Modifier.offset { IntOffset((e.lx.dp - 18.dp).roundToPx(), (e.ly.dp - 18.dp).roundToPx()) }.size(36.dp).scale(s)
        .boxShadow(c.shadow, 2.dp, 8.dp, 18.dp).clip(CircleShape).background(bg).clearAndSetSemantics { },
        contentAlignment = Alignment.Center) {
        Canvas(Modifier.fillMaxSize()) {
            val r = 15.dp.toPx(); val sw = 3.dp.toPx()
            val tl = Offset(center.x - r, center.y - r)
            if (!full) drawCircle(c.line, r, style = Stroke(sw))
            drawArc(if (full) c.onAccent else c.accent, -90f, 360f * e.progress, false, tl, Size(2 * r, 2 * r), style = Stroke(sw, cap = StrokeCap.Round))
        }
        Lucide(DesignIconData.lucideLockKeyhole, 14.dp, ink, 2.2f)
    }
}

/** Edge glow: a theme-colour light centred on the edge (or corner), larger and brighter as progress rises. */
@Composable
private fun EdgeGlow(e: EdgeLock, padOrigin: Offset) {
    val c = Oink.colors
    Canvas(Modifier.fillMaxSize().clearAndSetSemantics { }) {
        val p = e.progress
        val cx = padOrigin.x + e.cx.dp.toPx(); val cy = padOrigin.y + e.cy.dp.toPx()
        val (gw, gh) = when {
            e.corner -> (70f + 90f * p).let { it to it }
            e.vertical -> (28f + 56f * p) to (150f + 110f * p)
            else -> (150f + 110f * p) to (28f + 56f * p)
        }
        val rx = gw.dp.toPx() / 2; val ry = gh.dp.toPx() / 2
        val brush = Brush.radialGradient(0f to c.accent, .45f to c.accent.copy(alpha = .45f), 1f to Color.Transparent, center = Offset(cx, cy), radius = rx)
        withTransform({ scale(1f, ry / rx, Offset(cx, cy)) }) {
            drawCircle(brush, rx, Offset(cx, cy), alpha = .2f + .6f * p)
        }
    }
}

/** 释放: lifts the locked finger. */
@Composable
private fun ReleaseCapsule(modifier: Modifier, onClick: () -> Unit) {
    val c = Oink.colors
    val haptic = rememberHaptics()
    val shown = remember { Animatable(0f) }
    LaunchedEffect(Unit) { shown.animateTo(1f, tween(200, easing = androidx.compose.animation.core.FastOutSlowInEasing)) }
    Row(modifier.graphicsLayer { alpha = shown.value; translationY = (1f - shown.value) * 8.dp.toPx() }
        .boxShadow(c.shadow, 4.dp, 14.dp, 20.dp).height(40.dp).clip(RoundedCornerShape(20.dp)).background(c.elev)
        .border(.5.dp, c.border, RoundedCornerShape(20.dp)).press { haptic(false); onClick() }
        .padding(start = 12.dp, end = 16.dp).semantics(mergeDescendants = true) { contentDescription = "释放：抬起锁定的手指" },
        horizontalArrangement = Arrangement.spacedBy(6.dp), verticalAlignment = Alignment.CenterVertically) {
        SvgIcon(DesignIconData.actionUnlock, 18.dp, tint = c.accentInk, stroke = 1.8f)
        T("释放", 15f, c.accentInk, weight = 600, lineHeight = 20f)
    }
}

// ---------- Keyboards ----------

/**
 * The control page's keyboard, above the whole app (it covers the tab bar): the system keyboard with
 * Oink's accessory bar riding on it, or the emulated keyboard. Both rise from the bottom edge with
 * the IME curve (285 ms, ≈ cubic-bezier(.2, 0, 0, 1)); switching between them happens in place:
 * the emulated keyboard is drawn where the IME is and the IME goes down underneath, or it stays on
 * top until the IME has risen and then fades out (120 ms).
 *
 * Landscape (the full screen, 用户 2026-11-12): the emulated keyboard floats over the full-screen pad
 * in a rounded container, or fills the screen when expanded ([FloatingKeyboard]). The system keyboard
 * stays full width at the bottom. H < 135 gives two full-width control rows; H ≥ 135 gives a left
 * touchpad and three control rows on the right. H includes the status bar; controls draw below it.
 */
@OptIn(ExperimentalLayoutApi::class)
@Composable
fun ControlKeyboardLayer(holder: ControlHolder) {
    val c = Oink.colors
    val cc = remember(c.dark) { ControlColors(c.dark) }
    val density = LocalDensity.current
    val kb = holder.keyboard
    val open = holder.keyboardOpen && holder.active
    val ime = WindowInsets.ime.getBottom(density)
    val imeTarget = WindowInsets.imeAnimationTarget.getBottom(density)
    val nav = WindowInsets.navigationBars.getBottom(density)
    val imeUp = imeTarget > 0 && ime >= imeTarget
    val land = holder.layout == ControlLayout.LANDSCAPE
    val flip = holder.flip
    // Where the system keyboard is, only to lay the control keys out on and above it: its target height
    // while it moves, else the last one seen in this orientation during this run (it places the keys
    // while the IME rises and sinks), else a typical one (portrait the emulated keys below, landscape 220).
    // The portrait emulated keyboard (user 2026-11-12) takes the system keyboard's key area only while that
    // is a normal height ([EmulatedKeyboardHeight]: within 12 % of the measured baseline); a tall panel such
    // as Sogou's 超级联想 or a floating IME leaves it at the last normal height, or the baseline.
    var landIme by remember { mutableIntStateOf(0) }
    var portraitIme by remember { mutableIntStateOf(0) }
    LaunchedEffect(imeUp, land) {
        if (!imeUp) return@LaunchedEffect
        if (land) { landIme = imeTarget; return@LaunchedEffect }
        portraitIme = imeTarget
        val keys = (imeTarget - nav) / density.density
        if (EmulatedKeyboardHeight.inRange(keys) && kotlin.math.abs(keys - holder.prefs.systemKeys) > .5f) holder.update(holder.prefs.copy(systemKeys = keys))
    }
    val emulatedKeys = EmulatedKeyboardHeight.keys(holder.prefs.systemKeys).dp
    val imeH = with(density) {
        val seen = if (land) landIme else portraitIme
        (if (imeTarget > 0) imeTarget else if (seen > 0) seen else (if (land) LAND_IME_H else emulatedKeys).roundToPx() + nav).toFloat()
    }
    val shown = kb.shown
    val system = open && shown == KeyboardKind.SYSTEM
    val emulated = open && shown == KeyboardKind.EMULATED

    // The hidden field has focus (system keyboard up) only while the system keyboard is the one shown.
    val context = LocalContext.current
    val input = remember(context) {
        RemoteInputView(context, object : RemoteInputSink {
            override fun text(text: String) { holder.session?.text(text); kb.releaseHeld() }
            override fun backspace() = holder.fieldBackspace()
            override fun key(code: Int, down: Boolean) { holder.session?.key(code, down) }
            override fun hardwareKey(keyCode: Int, down: Boolean) = holder.hardwareKey(keyCode, down)
        })
    }
    DisposableEffect(holder, input) {
        holder.restartField = input::restartField
        onDispose { holder.restartField = null }
    }
    LaunchedEffect(system) { if (system) input.showKeyboard() else input.hideKeyboard() }
    // The system closed the IME itself (back gesture): the page's keyboard closes with it.
    var imeSeen by remember { mutableStateOf(false) }
    LaunchedEffect(system, imeUp, imeTarget == 0) {
        if (!system) imeSeen = false
        else if (imeUp) imeSeen = true
        else if (imeSeen && imeTarget == 0) holder.closeKeyboard()
    }
    // The accessory bar stays while the IME sinks after the system keyboard was closed.
    var lastSystem by remember { mutableStateOf(false) }
    if (open) lastSystem = shown == KeyboardKind.SYSTEM

    val rise = remember { Animatable(0f) }
    val cover = remember { Animatable(1f) }
    var swapped by remember { mutableStateOf(false) }
    LaunchedEffect(emulated) {
        if (emulated) {
            cover.snapTo(1f)
            swapped = ime > 0
            if (swapped) rise.snapTo(1f) else rise.animateTo(1f, tween(KB_MS, easing = KbEase))
        } else if (!open) rise.animateTo(0f, tween(KB_MS, easing = KbEase))
    }
    LaunchedEffect(system && rise.value > 0f, imeUp) {
        if (system && rise.value > 0f && imeUp) { cover.animateTo(0f, tween(120)); rise.snapTo(0f); cover.snapTo(1f) }
    }

    // Sliding between keys (§3.1.4): haptics, the keys under the fingers change with Fn / 123, and a
    // switch or close of the keyboard cancels every finger.
    val view = LocalView.current
    SideEffect {
        holder.feel = { slide ->
            view.performHapticFeedback(if (!slide) HapticFeedbackConstants.KEYBOARD_TAP
                else if (android.os.Build.VERSION.SDK_INT >= 34) HapticFeedbackConstants.SEGMENT_TICK else HapticFeedbackConstants.CLOCK_TICK)
        }
    }
    LaunchedEffect(kb) { snapshotFlow { kb.fn to kb.symbols }.collect { holder.slider.relayout() } }
    LaunchedEffect(kb) { snapshotFlow { kb.shown }.collect { holder.slider.reset() } }
    val near = with(density) { 8.dp.toPx() }
    val stick = with(density) { 5.dp.toPx() }
    PreviewScene(holder)

    var layerOrigin by remember { mutableStateOf(Offset.Zero) }
    var layerSize by remember { mutableStateOf(IntSize.Zero) }
    // Landscape: the cutout side keeps its safe area; the other side only a navigation bar there.
    val direction = androidx.compose.ui.platform.LocalLayoutDirection.current
    val safe = WindowInsets.systemBars.union(WindowInsets.displayCutout)
    val bars = WindowInsets.navigationBars
    val safeL = safe.getLeft(density, direction); val safeR = safe.getRight(density, direction)
    val navL = bars.getLeft(density, direction); val navR = bars.getRight(density, direction)
    val insets = LandInsets(top = safe.getTop(density), bottom = safe.getBottom(density), cutoutLeft = safeL, cutoutRight = safeR, freeLeft = navL, freeRight = navR,
        left = if (flip) navL else safeL, right = if (flip) safeR else navR)
    androidx.compose.runtime.CompositionLocalProvider(LocalKeyBoard provides holder) {
    Box(Modifier.fillMaxSize().onGloballyPositioned { layerOrigin = it.positionInRoot(); layerSize = it.size }) {
        androidx.compose.ui.viewinterop.AndroidView({ input }, Modifier.size(1.dp).alpha(0f))
        if (system || (!open && lastSystem && ime > 0)) {
            // H includes the status bar; only the drawing area subtracts its top inset.
            val remaining = holder.init?.systemRemaining?.let { it * density.density }
                ?: (layerSize.height - imeH).coerceAtLeast(0f)
            val split = land && LandscapeSystemLayout.split(remaining / density.density)
            val top = insets.top
            val bar = if (land) (remaining - top).coerceAtLeast(0f) else with(density) { ACCESSORY_H.toPx() }
            val progress = if (holder.init?.systemRemaining != null) 1f else (ime / imeH).coerceIn(0f, 1f)
            val y = if (holder.init?.systemRemaining != null) layerSize.height - remaining else ime - (1f - progress) * bar
            val usableWidth = (layerSize.width - insets.left - insets.right).coerceAtLeast(0)
            val controlsWidth = if (split) usableWidth / 2f else usableWidth.toFloat()
            DisposableEffect(Unit) { onDispose { holder.accessoryTop = null } }
            SideEffect { if (land) holder.accessoryTop = null }
            Column(Modifier.align(Alignment.BottomStart).offset { IntOffset(if (land) insets.left + (if (split) usableWidth / 2 else 0) else 0, (-y).roundToInt()) }
                .then(if (land) Modifier.width(with(density) { controlsWidth.toDp() }) else Modifier.fillMaxWidth())
                .height(with(density) { bar.toDp() })
                .background(cc.kbBg).then(if (land) Modifier else Modifier.onGloballyPositioned { holder.accessoryTop = it.positionInRoot().y })
                .drawBehind { drawRect(cc.kbShadow, Offset(0f, size.height - .5f * density.density), Size(size.width, .5f * density.density)) }
                .keyTouches(holder, near, stick)
                .padding(horizontal = 6.dp, vertical = 7.dp)) {
                if (land) LandscapeSystemKeys(holder, cc, remaining / density.density)
                else ControlKeys(holder, cc, Modifier.height(120.dp), rightOfUp = if (holder.layout == ControlLayout.PORTRAIT) RightOfUp.HIDE else RightOfUp.SWITCH)
            }
            if (split && system) {
                var padOrigin by remember { mutableStateOf(Offset.Zero) }
                var boxOrigin by remember { mutableStateOf(Offset.Zero) }
                Box(Modifier.align(Alignment.BottomStart).offset { IntOffset(insets.left, (-y).roundToInt()) }
                    .size(with(density) { controlsWidth.toDp() }, with(density) { bar.toDp() })
                    .onGloballyPositioned { boxOrigin = it.positionInRoot() }) {
                    Touchpad(holder, cc, Modifier.fillMaxSize().padding(start = 12.dp, end = 6.dp, top = 6.dp, bottom = 6.dp)
                        .onGloballyPositioned { padOrigin = it.positionInRoot() })
                    holder.pad.edge?.let { EdgeGlow(it, padOrigin - boxOrigin) }
                }
            }
        }
        if (rise.value > 0f) {
            DisposableEffect(Unit) { onDispose { holder.emulatedTop = null } }
            // Turned to landscape with the keyboard up: the portrait top no longer applies.
            SideEffect { if (land) holder.emulatedTop = null }
            if (land) FloatingKeyboard(holder, cc, rise.value, cover.value, swapped, layerSize, insets, near, stick)
            else {
                // The keys' height above the nav bar (see emulatedKeys) plus the control keys.
                val full = with(density) { emulatedKeys.toPx() + nav + ACCESSORY_H.toPx() }
                Column(Modifier.align(Alignment.BottomStart).offset { IntOffset(0, ((1f - rise.value) * full).roundToInt()) }
                    .alpha(cover.value)
                    .fillMaxWidth()
                    .height(with(density) { full.toDp() }).background(cc.kbBg)
                    .onGloballyPositioned { holder.emulatedTop = it.positionInRoot().y }
                    .drawBehind { drawRect(cc.kbShadow, size = Size(size.width, .5f * density.density)) }
                    // Touches between keys stay on the keyboard.
                    .pointerInput(Unit) { awaitEachGesture { awaitFirstDown(requireUnconsumed = false) } }
                    .keyTouches(holder, near, stick)
                    .padding(start = 3.dp, end = 3.dp, top = 7.dp, bottom = 10.dp + with(density) { nav.toDp() }),
                    verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    Box(Modifier.padding(horizontal = 3.dp)) {
                        ControlKeys(holder, cc, Modifier.height(120.dp), rightOfUp = if (holder.layout == ControlLayout.PORTRAIT) RightOfUp.HIDE else RightOfUp.SWITCH)
                    }
                    key(swapped) { EmulatedKeys(kb, cc, Modifier.weight(1f).fillMaxWidth().fadeIn(swapped)) }
                }
            }
        }
        KeyBubbleLayer(holder, cc, layerOrigin, layerSize.width.toFloat())
    }
    }
}

/**
 * Screen insets (px) of the landscape keyboards: the status bar [top], the bottom bar, the safe area
 * on each side ([cutoutLeft] / [cutoutRight]), a navigation bar on each side ([freeLeft] / [freeRight]),
 * and what the control keys over the system keyboard keep free on each side ([left] / [right]: the
 * cutout side's safe area, else a navigation bar there).
 */
private class LandInsets(val top: Int, val bottom: Int, val cutoutLeft: Int, val cutoutRight: Int, val freeLeft: Int, val freeRight: Int, val left: Int, val right: Int)

/**
 * The landscape emulated keyboard (用户 2026-11-12, numbers in [FloatMotion]).
 *
 * Floating: a container of the reference phone’s portrait width (clamped inside safe areas) with proportional radius, the keyboard colour, a 0.5 edge and a 0 12 36 shadow,
 * from 8 below the status bar down to the bottom bar (at least 8 from the screen bottom); the handle
 * on top (a 40 × 5 bar in a 22-high strip), then the control keys (切换键盘 right of ↑) and
 * the five rows of keys, sharing the remaining height 1:3. It starts on the side away from the cutout, 8 from that screen edge; touches
 * in it go to the keys, touches outside to the pad.
 *
 * The handle: sideways it moves the container (1:1, a third past the bounds; a flick rests at the
 * projection, snapped to a bound within 24, with the 0.35 s critically damped spring); down it shrinks
 * toward the centre and fades, and a projected 80 dismisses it (200 ms ease-in to 0.6 / 0, then the
 * keyboard closes). While held the handle darkens. Double-tapping expands to the whole screen;
 * upward drags do not expand. The same spring morphs the container during expansion and restore.
 *
 * Expanded: the whole screen below the status bar ([ExpandedKeys]), remembered for this orientation
 * ([ControlPrefs.expanded]); 还原 fades the floating layout in at full size (120 ms) and springs back
 * into the floating container. The pad is completely covered then.
 */
@Composable
private fun BoxScope.FloatingKeyboard(holder: ControlHolder, cc: ControlColors, rise: Float, cover: Float, fadeIn: Boolean, screen: IntSize,
                                      insets: LandInsets, near: Float, stick: Float) {
    val c = Oink.colors
    val density = LocalDensity.current
    val d = density.density
    val scope = rememberCoroutineScope()
    val flip = holder.flip
    val pose = holder.init?.float
    val view = LocalView.current
    val sw = screen.width / d; val sh = screen.height / d
    val safeLeft = maxOf(insets.cutoutLeft, insets.freeLeft) / d
    val safeRight = maxOf(insets.cutoutRight, insets.freeRight) / d
    val width = FloatMotion.width(sw, safeLeft, safeRight)
    val bounds = FloatMotion.Bounds.of(sw, width, safeLeft, safeRight, safeLeft, safeRight, flip)
    val home = when (pose) { "left" -> bounds.lo; "right" -> bounds.hi; "mid" -> (bounds.lo + bounds.hi) / 2; else -> bounds.home(flip) }
    val expanded = holder.prefs.expanded
    // Gesture state (dp and morph progress); x is NaN until the handle first moves the container.
    var x by remember { mutableFloatStateOf(Float.NaN) }
    var q by remember { mutableFloatStateOf(if (expanded) 1f else 0f) }
    var down by remember { mutableFloatStateOf(if (pose == "dismiss") 70f else 0f) }
    var finish by remember { mutableFloatStateOf(0f) }
    var handlePressed by remember { mutableStateOf(pose == "pressed") }
    /** ms into a layout swap's fades (expanding: full layout in, icon out; 还原: floating layout in), null when none runs. */
    var swap by remember { mutableStateOf<Float?>(null) }
    var restoring by remember { mutableStateOf(false) }
    val job = remember { arrayOfNulls<kotlinx.coroutines.Job>(1) }
    val left = if (x.isNaN()) home else x
    val current by rememberUpdatedState(bounds)
    val currentLeft by rememberUpdatedState(left)
    val springSpec = androidx.compose.animation.core.spring<Float>(dampingRatio = 1f, stiffness = FloatMotion.STIFFNESS)
    fun swapFades(ms: Int, after: suspend () -> Unit = {}) {
        job[0] = scope.launch {
            swap = 0f
            androidx.compose.animation.core.animate(0f, ms.toFloat(), animationSpec = tween(ms, easing = LinearEasing)) { v, _ -> swap = v }
            swap = null
            after()
        }
    }
    fun restore() {
        job[0]?.cancel()
        holder.setExpanded(false)
        // From this frame on the floating layout fades in (no frame with it at full opacity).
        swap = 0f; restoring = true; q = 1f
        swapFades(FloatMotion.RESTORE_FADE_MS) {
            androidx.compose.animation.core.animate(1f, 0f, 0f, springSpec) { v, _ -> q = v }
            restoring = false
        }
    }
    fun expand() {
        job[0]?.cancel()
        job[0] = scope.launch {
            androidx.compose.animation.core.animate(q, 1f, 0f, springSpec) { v, _ -> q = v }
            swap = 0f
            holder.setExpanded(true)
            swapFades(FloatMotion.EXPAND_FADE_MS)
        }
    }
    fun release(axis: Int, vx: Float, vy: Float) {
        when {
            axis == 2 && down > 0f -> job[0] = scope.launch {
                if (FloatMotion.dismisses(down, vy)) {
                    androidx.compose.animation.core.animate(0f, 1f, animationSpec = tween(FloatMotion.DISMISS_FINISH_MS, easing = androidx.compose.animation.core.Easing { t -> t * t })) { v, _ -> finish = v }
                    holder.closeKeyboard()
                } else androidx.compose.animation.core.animate(down, 0f, vy, springSpec) { v, _ -> down = v }
            }
            axis == 1 -> {
                val from = x
                val to = current.rest(from, vx)
                job[0] = scope.launch { androidx.compose.animation.core.animate(from, to, vx, springSpec) { v, _ -> x = v } }
            }
        }
    }

    // The box: between the floating container and the whole screen below the status bar.
    val qb = if (expanded && !restoring) 1f else minOf(1f, q)
    val floatTop = insets.top / d + 8f
    val floatBottom = sh - maxOf(insets.bottom / d, 8f)
    val boxL = FloatMotion.lerp(left, 0f, qb)
    val boxR = FloatMotion.lerp(left + width, sw, qb)
    val boxT = FloatMotion.lerp(floatTop, insets.top / d, qb)
    val boxB = FloatMotion.lerp(floatBottom, sh, qb)
    val floatingScale = FloatMotion.floatingScale(width)
    val radius = FloatMotion.lerp(24f * floatingScale, 0f, qb).dp
    val (scale, fade) = FloatMotion.dismissLook(down, finish)
    // What it shows (用户 2026-11-12): floating keys, the icon, or the full-screen layout.
    val morph = FloatMotion.morphLook(q)
    val floatingO = if (restoring) swap?.let { minOf(1f, it / FloatMotion.RESTORE_FADE_MS) } ?: 1f else morph.floating
    val iconO = when { expanded -> swap?.let { 1f - minOf(1f, it / FloatMotion.ICON_FADE_MS) } ?: 0f; restoring -> 0f; else -> morph.icon }
    val fullO = if (expanded) swap?.let { minOf(1f, it / FloatMotion.EXPAND_FADE_MS) } ?: 1f else 0f
    val full = expanded
    val fullTop = (8 * FloatMotion.SCALE).dp
    val shape = RoundedCornerShape(radius)
    val side = (maxOf(insets.cutoutLeft, insets.cutoutRight, insets.freeLeft, insets.freeRight) / d).coerceAtLeast(8f).dp
    // Scale fixed dp/sp metrics together; weighted letter rows still share the screen-bound height.
    val keyboardDensity = androidx.compose.ui.unit.Density(d * (if (full) FloatMotion.SCALE else floatingScale), density.fontScale)
    Box(Modifier
        .offset { IntOffset((boxL * d).roundToInt(), (boxT * d).roundToInt()) }
        .size((boxR - boxL).coerceAtLeast(0f).dp, (boxB - boxT).coerceAtLeast(0f).dp)
        .graphicsLayer {
            translationY = (1f - rise) * (sh - boxT) * d
            scaleX = scale; scaleY = scale
            alpha = fade * cover
        }
        .then(if (qb < 1f) Modifier.boxShadow(c.shadow, 12.dp, 36.dp, radius) else Modifier)
        .clip(shape).background(cc.kbBg).then(if (qb < 1f) Modifier.border(.5.dp, cc.kbShadow, shape) else Modifier)
        // Touches between keys stay on the keyboard.
        .pointerInput(Unit) { awaitEachGesture { awaitFirstDown(requireUnconsumed = false) } }
        .keyTouches(holder, near, stick)) {
        if (full) Box(Modifier.fillMaxSize().alpha(fullO).padding(start = side, end = side, top = fullTop, bottom = (insets.bottom / d).dp + (4 * FloatMotion.SCALE).dp)) {
            CompositionLocalProvider(LocalDensity provides keyboardDensity) {
                ExpandedKeys(holder, cc, onHide = { holder.closeKeyboard() }, onRestore = { restore() })
            }
        } else CompositionLocalProvider(LocalDensity provides keyboardDensity) {
            Column(Modifier.fillMaxSize().alpha(floatingO).padding(start = 6.dp, end = 6.dp, top = 22.dp, bottom = 8.dp),
                verticalArrangement = Arrangement.spacedBy(6.dp)) {
                ControlKeys(holder, cc, Modifier.weight(1f), gap = 3.dp, fs = 12f, rightOfUp = RightOfUp.SWITCH)
                key(fadeIn) { EmulatedKeys(holder.keyboard, cc, Modifier.weight(3f).fillMaxWidth().fadeIn(fadeIn), small = true) }
            }
        }
        if (iconO > .001f) Box(Modifier.align(Alignment.Center).alpha(iconO * .55f).clearAndSetSemantics { }) {
            SvgIcon(DesignIconData.pcKeyboard, 44.dp, tint = cc.kbInk, stroke = 1.4f)
        }
        // The handle stays mounted through movement and the expansion animation.
        if (!expanded) CompositionLocalProvider(LocalDensity provides keyboardDensity) {
            val at = remember { arrayOfNulls<androidx.compose.ui.layout.LayoutCoordinates>(1) }
            Box(Modifier.align(Alignment.TopCenter).fillMaxWidth().height(22.dp)
                .onGloballyPositioned { at[0] = it }
                .pointerInput(Unit) {
                    var lastTapUp = Long.MIN_VALUE
                    var lastTapPosition = Offset.Zero
                    awaitEachGesture {
                        val first = awaitFirstDown()
                        first.consume()
                        view.performHapticFeedback(HapticFeedbackConstants.KEYBOARD_TAP)
                        val coords = at[0]?.takeIf { it.isAttached } ?: return@awaitEachGesture
                        job[0]?.cancel()
                        q = 0f; restoring = false; swap = null
                        val start = coords.localToRoot(first.position)
                        val x0 = currentLeft
                        val tracker = androidx.compose.ui.input.pointer.util.VelocityTracker()
                        tracker.addPosition(first.uptimeMillis, start)
                        var axis = 0
                        var up: androidx.compose.ui.input.pointer.PointerInputChange? = null
                        handlePressed = true
                        try {
                            while (true) {
                                val e = awaitPointerEvent()
                                val ch = e.changes.firstOrNull { it.id == first.id } ?: break
                                ch.consume()
                                if (!ch.pressed) { up = ch; break }
                                val p = coords.localToRoot(ch.position)
                                tracker.addPosition(ch.uptimeMillis, p)
                                val dx = (p.x - start.x) / d; val dy = (p.y - start.y) / d
                                if (axis == 0) {
                                    if (kotlin.math.hypot(dx, dy) < FloatMotion.SLOP) continue
                                    axis = if (kotlin.math.abs(dy) > kotlin.math.abs(dx)) 2 else 1
                                }
                                if (axis == 1) x = current.rubber(x0 + dx)
                                else down = maxOf(0f, dy)
                            }
                        } finally {
                            handlePressed = false
                        }
                        val lifted = up
                        if (axis == 0 && lifted != null && lifted.uptimeMillis - first.uptimeMillis < viewConfiguration.longPressTimeoutMillis) {
                            val gap = first.uptimeMillis - lastTapUp
                            val nearby = (start - lastTapPosition).getDistance() <= viewConfiguration.touchSlop * 2
                            if (lastTapUp != Long.MIN_VALUE && gap in viewConfiguration.doubleTapMinTimeMillis..viewConfiguration.doubleTapTimeoutMillis && nearby) {
                                lastTapUp = Long.MIN_VALUE
                                expand()
                            } else {
                                lastTapUp = lifted.uptimeMillis
                                lastTapPosition = start
                            }
                        } else {
                            lastTapUp = Long.MIN_VALUE
                            val v = tracker.calculateVelocity()
                            release(axis, v.x / d, v.y / d)
                        }
                    }
                }
                .semantics {
                    contentDescription = "拖动键盘"
                    stateDescription = if (handlePressed) "已按下" else "未按下"
                    onClick(label = "展开全屏键盘") { expand(); true }
                }) {
                Box(Modifier.align(Alignment.Center).size(40.dp, 5.dp).clip(RoundedCornerShape(3.dp)).background(if (handlePressed) c.handle.copy(red = c.handle.red * .7f, green = c.handle.green * .7f, blue = c.handle.blue * .7f) else c.handle))
            }
        }
    }
}

/**
 * The expanded landscape keyboard (用户 2026-11-12): laid out like a laptop. Two function rows (34
 * high), each nine equal columns plus a 64-wide column, so the rows line up:
 * Esc, copy, paste, ⏪, ⏯, ⏩, mute, volume − / + | 收起 (icon only);
 * Home, End, PgUp, PgDn, Del, PrtSc, 接收音频, 共享麦克风, 切换键盘 | 还原 (icon only).
 * Then five rows in key units (15 each): ` 1–0 - = ⌫(2) / Tab(1.5) q–p [ ] \(1.5) / Caps(1.75) a–l ; '
 * ↵(2.25) / ⇧(2.25) z–m , . / ⇧(2.75) / Fn(1) Ctrl Win Alt(1.25 each) space(6) Alt(1.25) ←↑↓→(3; Mac:
 * Fn ⌃ ⌥ ⌘ space ⌘). Every symbol is on its real key and Shift gives the upper one (the faces follow),
 * so there is no symbol layer; with Fn the number row becomes F1–F12. Both Shifts (and both Alts / ⌘)
 * are one modifier. Caps follows the real touch ([KeyboardModel.capsLock]); PrtSc sends Print Screen
 * (also to a Mac, which has no use for it). 接收音频 / 共享麦克风 are switches with 调节's state.
 */
@Composable
private fun ExpandedKeys(holder: ControlHolder, cc: ControlColors, onHide: () -> Unit, onRestore: () -> Unit) {
    val c = Oink.colors
    val kb = holder.keyboard
    val mac = holder.session?.state?.focus?.platform == "mac"
    val shift = kb.shiftDown
    val fn = kb.fn != null
    val r = 9.dp
    fun textFace(t: String, size: Float = 20f) = BubbleFace(false) { T(t, size, it, weight = 500, lineHeight = size * 1.25f, maxLines = 1) }
    fun iconFace(draw: @Composable (Color) -> Unit) = BubbleFace(false, draw)
    @Composable fun RowScope.text(id: String, text: String, name: String, code: Int, w: Float) =
        Key(cc, OnRelease { kb.click(code) }, name, Modifier.weight(w).fillMaxHeight(), radius = r, slideId = id, bubble = textFace(text)) { KeyText(text, 13f, it) }
    @Composable fun RowScope.held(id: String, name: String, code: Int, glyph: @Composable (Color, Boolean) -> Unit, media: Boolean) =
        Key(cc, Hold({ kb.press(id, code, combines = !media) }, { kb.release(id) }), name, Modifier.weight(1f).fillMaxHeight(), radius = r,
            slideId = id, bubble = iconFace { glyph(it, true) }) { glyph(it, false) }
    @Composable fun RowScope.char(ch: Char, w: Float) {
        val label = when { !shift -> ch.toString(); ch.isLetter() -> ch.uppercaseChar().toString(); else -> (Hid.US_SHIFT_LABEL[ch] ?: ch).toString() }
        val id = "vk-$ch"
        Key(cc, Hold({ kb.pressChar(id, ch) }, { kb.release(id) }), label, Modifier.weight(w).fillMaxHeight(), function = false, radius = r,
            slideId = id, bubble = BubbleFace(true) { T(label, 32f, it, lineHeight = 38f) }) { ink -> T(label, if (ch.isDigit()) 18f else 20f, ink, lineHeight = 26f) }
    }
    @Composable fun RowScope.mod(mod: Mod, text: String, name: String, w: Float) =
        Key(cc, OnRelease(run = { kb.tapModifier(mod) }, long = { kb.lockModifier(mod) }), name, Modifier.weight(w).fillMaxHeight(), latch = kb.mods[mod], radius = r) { ink ->
            if (text.isEmpty()) SvgIcon(ControlIcons.winLogo, 14.dp, tint = ink) else KeyText(text, if (mac && text.length == 1) 18f else 14f, ink)
        }
    @Composable fun RowScope.audio(speaker: Boolean) {
        val on = if (speaker) holder.speakerOn else holder.micOn
        Key(cc, OnRelease(long = if (speaker) holder.longPressSpeaker else null, run = { if (speaker) holder.toggleSpeaker() else holder.toggleMic() }), if (speaker) "接收音频" else "共享麦克风",
            Modifier.weight(1f).fillMaxHeight(), radius = r, toggled = on, enabled = !speaker || !holder.forwardOn, longHaptic = !speaker, longLabel = "切换扬声器模式") { ink ->
            CompositionLocalProvider(LocalCutout provides if (on) c.accentSoft2 else cc.kbKey2) {
                DevIcon(if (speaker) speakerGlyph(3) else DesignIconData.pcMicrophone, 18.dp, ink, badge = if (!on) null else if (speaker) Badge.Receive else Badge.Send)
            }
        }
    }
    val digits = "1234567890-="
    Column(Modifier.fillMaxSize(), verticalArrangement = Arrangement.spacedBy(8.dp)) {
        Column(verticalArrangement = Arrangement.spacedBy(5.dp)) {
        Row(Modifier.fillMaxWidth().height(34.dp), horizontalArrangement = Arrangement.spacedBy(5.dp)) {
            text("esc", "Esc", "Esc", Hid.ESCAPE, 1f)
            Key(cc, OnRelease { kb.shortcut(Hid.C, mac) }, "复制（${if (mac) "⌘C" else "Ctrl+C"}）", Modifier.weight(1f).fillMaxHeight(), radius = r,
                slideId = "copy", bubble = iconFace { Lucide(DesignIconData.lucideCopy, 26.dp, it) }) { Lucide(DesignIconData.lucideCopy, 17.dp, it) }
            Key(cc, OnRelease { kb.shortcut(Hid.V, mac) }, "粘贴（${if (mac) "⌘V" else "Ctrl+V"}）", Modifier.weight(1f).fillMaxHeight(), radius = r,
                slideId = "paste", bubble = iconFace { Lucide(ControlIcons.clipboardPaste, 26.dp, it) }) { Lucide(ControlIcons.clipboardPaste, 17.dp, it) }
            held("prev", "快退 / 上一首", Hid.PREVIOUS, { ink, big -> Lucide(ControlIcons.rewind, if (big) 26.dp else 17.dp, ink) }, media = true)
            Key(cc, OnRelease { kb.media(Hid.PLAY_PAUSE) }, "播放 / 暂停", Modifier.weight(1f).fillMaxHeight(), radius = r, slideId = "play",
                bubble = iconFace { SvgIcon(ControlIcons.playPause, 28.dp, tint = it) }) { SvgIcon(ControlIcons.playPause, 19.dp, tint = it) }
            held("next", "快进 / 下一首", Hid.NEXT, { ink, big -> Lucide(ControlIcons.fastForward, if (big) 26.dp else 17.dp, ink) }, media = true)
            Key(cc, OnRelease { kb.media(Hid.MUTE) }, "静音", Modifier.weight(1f).fillMaxHeight(), radius = r, slideId = "mute",
                bubble = iconFace { SvgIcon(speakerGlyph(0), 27.dp, tint = it, stroke = 1.4f) }) { SvgIcon(speakerGlyph(0), 18.dp, tint = it, stroke = 1.4f) }
            held("vol-", "音量-", Hid.VOLUME_DOWN, { ink, big -> SvgIcon(speakerGlyph(1), if (big) 27.dp else 18.dp, tint = ink, stroke = 1.4f) }, media = true)
            held("vol+", "音量+", Hid.VOLUME_UP, { ink, big -> SvgIcon(speakerGlyph(3), if (big) 27.dp else 18.dp, tint = ink, stroke = 1.4f) }, media = true)
            Key(cc, OnRelease { onHide() }, "收起键盘", Modifier.width(64.dp).fillMaxHeight(), radius = r) { SvgIcon(ControlIcons.hideKeyboard, 22.dp, tint = it, stroke = 1.6f) }
        }
        Row(Modifier.fillMaxWidth().height(34.dp), horizontalArrangement = Arrangement.spacedBy(5.dp)) {
            text("home", "Home", "Home", Hid.HOME, 1f)
            text("end", "End", "End", Hid.END, 1f)
            text("pgup", "PgUp", "Page Up", Hid.PAGE_UP, 1f)
            text("pgdn", "PgDn", "Page Down", Hid.PAGE_DOWN, 1f)
            if (mac) Key(cc, OnRelease { kb.click(Hid.DELETE) }, "向前删除", Modifier.weight(1f).fillMaxHeight(), radius = r, slideId = "del", bubble = textFace("⌦", 28f)) { KeyText("⌦", 18f, it) }
            else text("del", "Del", "Delete", Hid.DELETE, 1f)
            text("prtsc", "PrtSc", "Print Screen", Hid.PRINT_SCREEN, 1f)
            audio(speaker = true)
            audio(speaker = false)
            Key(cc, OnRelease { kb.switchKind() }, "切换键盘：换到系统键盘（手机输入法）", Modifier.weight(1f).fillMaxHeight(), radius = r) {
                SvgIcon(ControlIcons.switchKeyboard, 22.dp, tint = it, stroke = 1.6f)
            }
            Key(cc, OnRelease { onRestore() }, "还原：回到浮动键盘", Modifier.width(64.dp).fillMaxHeight(), radius = r) { Lucide(ControlIcons.minimize, 18.dp, it) }
        }
        }
        Column(Modifier.weight(1f).fillMaxWidth(), verticalArrangement = Arrangement.spacedBy(6.dp)) {
            val row = Modifier.weight(1f).fillMaxWidth()
            Row(row, horizontalArrangement = Arrangement.spacedBy(5.dp)) {
                char('`', 1f)
                digits.forEachIndexed { i, ch ->
                    val n = i + 1
                    if (fn) Key(cc, OnRelease { kb.fKey(n) }, "F$n", Modifier.weight(1f).fillMaxHeight(), radius = r, slideId = "f$n", bubble = textFace("F$n")) { KeyText("F$n", 13f, it) }
                    else char(ch, 1f)
                }
                Key(cc, Hold({ kb.press("backspace", Hid.BACKSPACE) }, { kb.release("backspace") }), "退格", Modifier.weight(2f).fillMaxHeight(), radius = r,
                    slideId = "backspace", bubble = iconFace { Lucide(ControlIcons.delete, 26.dp, it) }) { Lucide(ControlIcons.delete, 20.dp, it) }
            }
            Row(row, horizontalArrangement = Arrangement.spacedBy(5.dp)) {
                text("tab", "Tab", "Tab", Hid.TAB, 1.5f)
                "qwertyuiop[]".forEach { char(it, 1f) }
                char('\\', 1.5f)
            }
            Row(row, horizontalArrangement = Arrangement.spacedBy(5.dp)) {
                Key(cc, Hold({ kb.capsLock(true) }, { kb.capsLock(false) }), "Caps Lock", Modifier.weight(1.75f).fillMaxHeight(), radius = r, slideId = "caps", bubble = textFace("Caps")) { KeyText("Caps", 13f, it) }
                "asdfghjkl;'".forEach { char(it, 1f) }
                Key(cc, Hold({ kb.press("return", Hid.RETURN) }, { kb.release("return") }), "回车", Modifier.weight(2.25f).fillMaxHeight(), radius = r,
                    slideId = "return", bubble = iconFace { Lucide(ControlIcons.cornerDownLeft, 26.dp, it) }) { Lucide(ControlIcons.cornerDownLeft, 20.dp, it) }
            }
            Row(row, horizontalArrangement = Arrangement.spacedBy(5.dp)) {
                mod(Mod.SHIFT, if (mac) "⇧" else "Shift", "Shift", 2.25f)
                "zxcvbnm,./".forEach { char(it, 1f) }
                mod(Mod.SHIFT, if (mac) "⇧" else "Shift", "Shift", 2.75f)
            }
            Row(row, horizontalArrangement = Arrangement.spacedBy(5.dp)) {
                Key(cc, OnRelease { kb.tapFn() }, "Fn", Modifier.weight(1f).fillMaxHeight(), latch = kb.fn, radius = r) { KeyText("Fn", 14f, it) }
                val mods = if (mac) listOf(Triple(Mod.CTRL, "⌃", "control"), Triple(Mod.ALT, "⌥", "option"), Triple(Mod.META, "⌘", "command"))
                else listOf(Triple(Mod.CTRL, "Ctrl", "Ctrl"), Triple(Mod.META, "", "Windows"), Triple(Mod.ALT, "Alt", "Alt"))
                mods.forEach { (m, t, n) -> mod(m, t, n, 1.25f) }
                Key(cc, Hold({ kb.press("space", Hid.SPACE) }, { kb.release("space") }), "空格", Modifier.weight(6f).fillMaxHeight(), function = false, radius = r,
                    slideId = "space", bubble = BubbleFace(false) { T("空格", 20f, it, weight = 500, lineHeight = 25f) }) { T("空格", 13f, it, lineHeight = 18f) }
                mods.last().let { (m, t, n) -> mod(m, t, n, 1.25f) }
                Row(Modifier.weight(3f).fillMaxHeight(), horizontalArrangement = Arrangement.spacedBy(5.dp)) {
                    @Composable fun RowScope.arrow(id: String, name: String, code: Int, glyph: IconData) =
                        Key(cc, Hold({ kb.press(id, code) }, { kb.release(id) }), name, Modifier.weight(1f).fillMaxHeight(), radius = r, slideId = id, bubble = iconFace { Lucide(glyph, 26.dp, it) }) { Lucide(glyph, 17.dp, it) }
                    arrow("left", "左", Hid.LEFT, ControlIcons.arrowLeft)
                    Column(Modifier.weight(1f).fillMaxHeight(), verticalArrangement = Arrangement.spacedBy(3.dp)) {
                        Key(cc, Hold({ kb.press("up", Hid.UP) }, { kb.release("up") }), "上", Modifier.weight(1f).fillMaxWidth(), radius = r, slideId = "up",
                            bubble = iconFace { Lucide(ControlIcons.arrowUp, 26.dp, it) }) { Lucide(ControlIcons.arrowUp, 15.dp, it) }
                        Key(cc, Hold({ kb.press("down", Hid.DOWN) }, { kb.release("down") }), "下", Modifier.weight(1f).fillMaxWidth(), radius = r, slideId = "down",
                            bubble = iconFace { Lucide(ControlIcons.arrowDown, 26.dp, it) }) { Lucide(ControlIcons.arrowDown, 15.dp, it) }
                    }
                    arrow("right", "右", Hid.RIGHT, ControlIcons.arrowRight)
                }
            }
        }
    }
}

/** Design states with a key preview (§8): fingers put down on [ControlInit.held] and then [ControlInit.preview]. */
@Composable
private fun PreviewScene(holder: ControlHolder) {
    val scene = holder.init ?: return
    val key = scene.preview ?: return
    LaunchedEffect(holder.keyboardOpen) {
        if (!holder.keyboardOpen) return@LaunchedEffect
        fun spot(id: String) = holder.keySpots.values.firstOrNull { it.attached && it.slide?.id == id }
        fun find(id: String) = spot(id)?.slide
        // Wait until the key is laid out and has stopped moving (the keyboard rises first), and
        // past the Fn / 123 relayout of the opening frames.
        var last: Rect? = null
        var still = 0
        var tries = 0
        while (still < 3 && tries++ < 240) {
            androidx.compose.runtime.withFrameNanos { }
            val r = spot(key)?.rect
            if (r != null && r == last) still++ else still = 0
            last = r
        }
        scene.held.forEachIndexed { i, id -> find(id)?.let { holder.slider.press(100L + i, it) } }
        find(key)?.let { holder.slider.press(1L, it) }
    }
}

/** The lower half fades in (120 ms) when it replaces the system keyboard in place. */
@Composable
private fun Modifier.fadeIn(enabled: Boolean): Modifier {
    val a = remember { Animatable(if (enabled) 0f else 1f) }
    LaunchedEffect(Unit) { a.animateTo(1f, tween(120, easing = androidx.compose.animation.core.LinearOutSlowInEasing)) }
    return this.alpha(a.value)
}

/**
 * How a key acts: once on release (finger still on it), or down on press and up on release / slide-off.
 * [OnRelease.long] (modifiers, user 2026-11-12): held for [LONG_PRESS_MS] it runs instead, at once,
 * and the release then does nothing. [OnRelease.long] comes first so that `OnRelease { … }` (the
 * usual key) passes its trailing lambda to [OnRelease.run]; keys with both name them.
 */
private sealed interface KeyAct
private class OnRelease(val long: (() -> Unit)? = null, val run: () -> Unit) : KeyAct
private class Hold(val down: () -> Unit, val up: () -> Unit) : KeyAct

/** A key's preview bubble (§3.1.4): character keys get the tall one, the others the short one; [face] is drawn in it. */
internal class BubbleFace(val char: Boolean, val face: @Composable (Color) -> Unit)

/** A key on screen for the slide tracker: where it is (root px), and for keys with a bubble what it does and shows. */
internal class KeySpot(private val coordinates: androidx.compose.ui.layout.LayoutCoordinates, val slide: SlideKey?, val bubble: BubbleFace?) {
    val attached: Boolean get() = coordinates.isAttached
    // graphicsLayer translates/scales the keyboard during entry and expansion without a new
    // layout callback. A cached boundsInRoot() then stays at the off-screen entry position and
    // misses actual touches. Resolve the current transform when hit-testing or placing a bubble.
    val rect: Rect get() = coordinates.boundsInRoot()
}

/** The control page's keyboard state for its keys (registry, slide tracker); null outside the keyboard. */
private val LocalKeyBoard = androidx.compose.runtime.staticCompositionLocalOf<ControlHolder?> { null }

/**
 * One key. Press feedback swaps the key and function-key tones (80 ms); a modifier's [latch] draws the
 * held (soft accent, accent ring) or locked (solid accent, bar under the label) look.
 *
 * A key with a [bubble] (§3.1.4, user 2026-11-11) has no touch handling of its own: the keyboard's
 * slide tracker presses it (by [slideId]), shows the bubble and lets fingers slide between such
 * keys; it is drawn pressed while a finger is on it. Keys without one (modifiers, Fn, the switch
 * key, 123 / ABC) stay plain buttons: sliding off cancels a release key and lifts a held key.
 */
@Composable
private fun Key(cc: ControlColors, act: KeyAct, label: String, modifier: Modifier, function: Boolean = true, latch: Latch? = null,
                radius: Dp = 6.dp, slideId: String? = null, bubble: BubbleFace? = null, toggled: Boolean? = null, enabled: Boolean = true, longHaptic: Boolean = true, longLabel: String? = null, content: @Composable BoxScope.(Color) -> Unit) {
    val c = Oink.colors
    val view = LocalView.current
    val board = LocalKeyBoard.current
    var touched by remember { mutableStateOf(false) }
    val slide = if (slideId != null && bubble != null) when (act) {
        is Hold -> SlideKey(slideId, hold = true, down = act.down, up = act.up)
        is OnRelease -> SlideKey(slideId, hold = false, fire = act.run)
    } else null
    val pressed = if (slide != null) board?.slider?.down?.contains(slide.id) == true else touched
    val base = if (function) cc.kbKey2 else cc.kbKey
    val pressedBg = if (function) cc.kbKey else cc.kbKey2
    // A switch key that is on (接收音频 / 共享麦克风 on the expanded keyboard): accent fill and ink, no ring.
    val on = toggled == true
    val bg by androidx.compose.animation.animateColorAsState(when (latch) { Latch.HELD -> c.accentSoft2; Latch.LOCKED -> c.accent; null -> if (on) c.accentSoft2 else if (pressed) pressedBg else base },
        tween(80), label = "key")
    val ink = when (latch) { Latch.HELD -> c.accentInk; Latch.LOCKED -> c.onAccent; null -> if (on) c.accentInk else cc.kbInk }
    val shape = RoundedCornerShape(radius)
    val current by rememberUpdatedState(act)
    val token = remember { Any() }
    if (board != null) DisposableEffect(board, token) { onDispose { board.keySpots.remove(token) } }
    Box(modifier.alpha(if (enabled) 1f else .4f).clip(shape).background(bg)
        .then(if (latch == Latch.HELD) Modifier.border(1.5.dp, c.accent, shape) else Modifier)
        .then(if (board != null) Modifier.onGloballyPositioned { board.keySpots[token] = KeySpot(it, slide, bubble) } else Modifier)
        .then(if (!enabled) Modifier else if (slide != null) Modifier else Modifier.pointerInput(enabled) {
            awaitEachGesture {
                val down = awaitFirstDown()
                down.consume()
                val a = current
                touched = true
                view.performHapticFeedback(HapticFeedbackConstants.KEYBOARD_TAP)
                if (a is Hold) a.down()
                // Sliding off cancels a release key and lifts a held key at once (nothing stays pressed).
                var live = true
                var longDone = false
                fun end(send: Boolean) {
                    if (!live) return
                    live = false; touched = false
                    when (a) { is Hold -> a.up(); is OnRelease -> if (send && !longDone) a.run() }
                }
                suspend fun AwaitPointerEventScope.follow() {
                    while (live) {
                        val e = awaitPointerEvent()
                        val ch = e.changes.firstOrNull { it.id == down.id } ?: break
                        ch.consume()
                        val within = ch.position.x in 0f..size.width.toFloat() && ch.position.y in 0f..size.height.toFloat()
                        if (!within) end(false) else if (!ch.pressed) end(true)
                    }
                }
                try {
                    val long = (a as? OnRelease)?.long
                    if (long == null) follow()
                    else {
                        withTimeoutOrNull(LONG_PRESS_MS) { follow() }
                        // Still down on the key after the long-press time: the long action, now.
                        if (live) {
                            longDone = true
                            if (longHaptic) view.performHapticFeedback(HapticFeedbackConstants.LONG_PRESS)
                            long()
                            follow()
                        }
                    }
                } finally { end(false) }
            }
        })
        .semantics {
            contentDescription = label + when (latch) { Latch.HELD -> "（按住）"; Latch.LOCKED -> "（锁定）"; null -> "" }
            role = if (toggled != null) Role.Switch else Role.Button
            if (toggled != null) toggleableState = androidx.compose.ui.state.ToggleableState(toggled)
            if (latch != null) selected = true
            if (!enabled) disabled()
            if (enabled) onClick {
                view.performHapticFeedback(HapticFeedbackConstants.KEYBOARD_TAP)
                when (val a = current) { is OnRelease -> a.run(); is Hold -> try { a.down() } finally { a.up() } }
                true
            }
            if (enabled) (act as? OnRelease)?.long?.let { long -> onLongClick(longLabel ?: if (latch == Latch.LOCKED) "解锁" else "锁定") { view.performHapticFeedback(HapticFeedbackConstants.LONG_PRESS); long(); true } }
        }, contentAlignment = Alignment.Center) {
        content(ink)
        if (latch == Latch.LOCKED) Box(Modifier.align(Alignment.BottomCenter).padding(bottom = 4.dp).size(12.dp, 2.dp).clip(RoundedCornerShape(1.dp)).background(ink))
    }
}

/**
 * Touches on a keyboard surface go to the slide tracker (§3.1.4): a finger landing on a key with a
 * bubble presses it; the tracker then follows that finger anywhere (this surface keeps getting its
 * events) — over other keys, over keys without a bubble (nothing), or off the keyboard (the finger
 * is done). Keys without a bubble consume their own touches first, so those never reach it.
 */
private fun Modifier.keyTouches(holder: ControlHolder, near: Float, stick: Float): Modifier = composed {
    val token = remember { Any() }
    val coords = remember { arrayOfNulls<androidx.compose.ui.layout.LayoutCoordinates>(1) }
    DisposableEffect(token) { onDispose { holder.keyAreas.remove(token) } }
    onGloballyPositioned { coords[0] = it; holder.keyAreas[token] = it }
        .pointerInput(holder) {
            awaitPointerEventScope {
                while (true) {
                    val e = awaitPointerEvent()
                    val at = coords[0]?.takeIf { it.isAttached } ?: continue
                    for (ch in e.changes) {
                        val id = ch.id.value
                        val p = at.localToRoot(ch.position)
                        val spots = holder.keySpots.values.filter { it.attached }.map { it.rect to it.slide }
                        val areas = holder.keyAreas.values.filter { it.isAttached }.map { it.boundsInRoot() }
                        when {
                            ch.changedToDown() -> {
                                if (ch.isConsumed) continue
                                // Only a finger landing on a key presses it (a gap between keys does nothing).
                                val hit = KeySlider.locate(p.x, p.y, null, spots, areas, near = 0.5f, stick = 0f)
                                if (hit is Hit.On) { holder.slider.press(id, hit.key); ch.consume() }
                            }
                            !holder.slider.tracks(id) -> {}
                            !ch.pressed -> { holder.slider.lift(id); ch.consume() }
                            else -> {
                                if (ch.positionChanged()) holder.slider.move(id, KeySlider.locate(p.x, p.y, holder.slider.keyOf(id), spots, areas, near, stick))
                                ch.consume()
                            }
                        }
                    }
                }
            }
        }
}

/**
 * The preview bubble (§3.1.4), above both keyboard surfaces and the pad: a Gboard-like rounded
 * rectangle floating over the key (character keys (kw + 16) × 58, its bottom 6 into the key, radius
 * 12; the others max(kw + 14, 46) × 44 up to 96 wide, 4 into the key, radius 10), 3 inside the
 * screen edges. Appears in 70 ms (fade, 0.9 → 1 from the key centre), glides to the next key in
 * 100 ms from wherever it is, swaps the content at once (fading it in from 0.3 over 80 ms), and
 * fades out in 100 ms after at least 90 ms on screen. Reduced motion: no animation. Not drawn while
 * TalkBack explores by touch (keys are then activated by double tap).
 */
@Composable
private fun KeyBubbleLayer(holder: ControlHolder, cc: ControlColors, origin: Offset, screenW: Float) {
    val context = LocalContext.current
    val density = LocalDensity.current
    val a11y = remember(context) { context.getSystemService(android.view.accessibility.AccessibilityManager::class.java) }
    val explore = a11y?.isTouchExplorationEnabled == true
    val still = remember(context) {
        android.provider.Settings.Global.getFloat(context.contentResolver, android.provider.Settings.Global.ANIMATOR_DURATION_SCALE, 1f) == 0f
    }
    val target = holder.slider.bubble
    var face by remember { mutableStateOf<BubbleFace?>(null) }
    var faceKey by remember { mutableStateOf<String?>(null) }
    val geo = remember { Animatable(Rect.Zero, Rect.VectorConverter) }
    val fade = remember { Animatable(0f) }
    val scale = remember { Animatable(1f) }
    val ink = remember { Animatable(1f) }
    var shownAt by remember { mutableStateOf(0L) }
    var keyCentre by remember { mutableStateOf(Offset.Zero) }
    LaunchedEffect(target, explore) {
        if (target == null || explore) {
            if (fade.value == 0f) return@LaunchedEffect
            val wait = 90 - (System.currentTimeMillis() - shownAt)
            if (wait > 0) kotlinx.coroutines.delay(wait)
            if (still) fade.snapTo(0f) else fade.animateTo(0f, tween(100, easing = androidx.compose.animation.core.EaseIn))
            return@LaunchedEffect
        }
        val spot = holder.keySpots.values.firstOrNull { it.attached && it.slide?.id == target.key.id } ?: return@LaunchedEffect
        val r = spot.rect
        val char = spot.bubble?.char == true
        val w = with(density) { if (char) r.width + 16.dp.toPx() else minOf(maxOf(r.width + 14.dp.toPx(), 46.dp.toPx()), 96.dp.toPx()) }
        val h = with(density) { if (char) 58.dp.toPx() else 44.dp.toPx() }
        val bottom = r.top + with(density) { if (char) 6.dp.toPx() else 4.dp.toPx() }
        val edge = with(density) { 3.dp.toPx() }
        val left = (r.center.x - w / 2).coerceIn(edge, maxOf(edge, screenW - edge - w))
        val to = Rect(left, bottom - h, left + w, bottom)
        face = spot.bubble; faceKey = target.key.id
        keyCentre = r.center
        val gliding = target.glide && fade.value > 0f && !still
        if (gliding) {
            kotlinx.coroutines.coroutineScope {
                launch { geo.animateTo(to, tween(100, easing = androidx.compose.animation.core.CubicBezierEasing(.2f, .9f, .3f, 1f))) }
                ink.snapTo(.3f)
                ink.animateTo(1f, tween(80, easing = androidx.compose.animation.core.EaseOut))
            }
        } else {
            geo.snapTo(to); ink.snapTo(1f)
            shownAt = System.currentTimeMillis()
            if (still) { fade.snapTo(1f); scale.snapTo(1f); return@LaunchedEffect }
            fade.snapTo(0f); scale.snapTo(.9f)
            kotlinx.coroutines.coroutineScope {
                launch { fade.animateTo(1f, tween(70, easing = EaseOutExpo)) }
                scale.animateTo(1f, tween(70, easing = EaseOutExpo))
            }
        }
    }
    val f = face ?: return
    if (fade.value <= 0f) return
    val g = geo.value
    val char = f.char
    val shape = RoundedCornerShape(if (char) 12.dp else 10.dp)
    Box(Modifier
        .offset { IntOffset((g.left - origin.x).roundToInt(), (g.top - origin.y).roundToInt()) }
        .size(with(density) { g.width.toDp() }, with(density) { g.height.toDp() })
        .graphicsLayer {
            alpha = fade.value
            scaleX = scale.value; scaleY = scale.value
            val cx = ((keyCentre.x - g.left) / g.width.coerceAtLeast(1f))
            val cy = ((keyCentre.y - g.top) / g.height.coerceAtLeast(1f))
            transformOrigin = TransformOrigin(cx, cy)
        }
        .boxShadow(cc.calloutShadow, 4.dp, 12.dp, if (char) 12.dp else 10.dp)
        .clip(shape).background(cc.callout).border(.5.dp, cc.calloutEdge, shape)
        .clearAndSetSemantics { },
        contentAlignment = Alignment.Center) {
        Box(Modifier.alpha(ink.value)) { key(faceKey) { f.face(cc.kbInk) } }
    }
}

@Composable
private fun KeyText(text: String, size: Float, ink: Color) = T(text, size, ink, lineHeight = size * 1.25f, maxLines = 1)


/**
 * The three rows of control keys shared by both keyboards (§3.1). Left of the 120-wide arrow block
 * the keys form one 5-column grid; the top row's media keys share the rest of their row; Fn swaps
 * everything right of Esc for F1–F12 (180 ms cross-fade). Names follow the focus computer's OS.
 * The rows share [modifier]'s height with [gap] between them (portrait 120 → rows 36; the floating
 * landscape keyboard rows 32 with gap 5, text [fs] 12; over the landscape system keyboard stretched
 * to fill, text 15).
 *
 * The middle row starts with Caps Lock (用户 2026-11-12; Mac ⇪): a plain key, no state shown, that sends
 * Caps Lock on touch down/up ([KeyboardModel.capsLock]); the letters' own Shift is the only Shift now.
 *
 * Right of ↑ ([rightOfUp], 用户 2026-11-12 确认): 收起键盘 in portrait (切换键盘 is in the header's
 * top-right slot) and over the landscape system keyboard; 切换键盘 on the floating landscape keyboard
 * (its handle hides it) and in the side layout (the header has the toggle). Landscape system IME
 * controls use [LandscapeSystemKeys].
 */
@Composable
private fun ControlKeys(holder: ControlHolder, cc: ControlColors, modifier: Modifier, gap: Dp = 6.dp, fs: Float = 13f, rightOfUp: RightOfUp) {
    val kb = holder.keyboard
    val mac = holder.session?.state?.focus?.platform == "mac"
    val fnShown by animateFloatAsState(if (kb.fn != null) 1f else 0f, tween(180, easing = androidx.compose.animation.core.LinearOutSlowInEasing), label = "fn")
    // Preview bubbles (§3.1.4): text 20 (weight 500), icons 26, Mac ⌦ 28.
    fun textFace(t: String, size: Float = 20f) = BubbleFace(false) { T(t, size, it, weight = 500, lineHeight = size * 1.25f, maxLines = 1) }
    fun iconFace(draw: @Composable (Color) -> Unit) = BubbleFace(false, draw)
    @Composable fun KeyLabel(text: String, ink: Color) = KeyText(text, fs, ink)
    BoxWithConstraints(modifier.fillMaxWidth()) {
        val w = maxWidth
        val cw = (w - 120.dp - 6.dp - 12.dp) / 5
        val arrowsW = 120.dp
        val g = 3.dp
        val toEmulated = kb.shown == KeyboardKind.SYSTEM
        @Composable fun SwitchKey(m: Modifier) = Key(cc, OnRelease { kb.switchKind() },
            if (toEmulated) "切换键盘：换到模拟键盘（发送原始键码）" else "切换键盘：换到系统键盘（手机输入法）", m) { SvgIcon(ControlIcons.switchKeyboard, 22.dp, tint = it, stroke = 1.6f) }
        Column(Modifier.fillMaxSize(), verticalArrangement = Arrangement.spacedBy(gap)) {
            // Top row.
            Row(Modifier.weight(1f), horizontalArrangement = Arrangement.spacedBy(g)) {
                Key(cc, OnRelease { kb.click(Hid.ESCAPE) }, "Esc", Modifier.width(cw).fillMaxHeight(), slideId = "esc", bubble = textFace("Esc")) { KeyLabel("Esc", it) }
                Box(Modifier.weight(1f).fillMaxHeight()) {
                    if (fnShown < 1f) Row(Modifier.fillMaxSize().alpha(1f - fnShown), horizontalArrangement = Arrangement.spacedBy(g)) {
                        Key(cc, OnRelease { kb.shortcut(Hid.C, mac) }, "复制（${if (mac) "⌘C" else "Ctrl+C"}）", Modifier.width(cw).fillMaxHeight(),
                            slideId = "copy", bubble = iconFace { Lucide(DesignIconData.lucideCopy, 26.dp, it) }) { Lucide(DesignIconData.lucideCopy, 17.dp, it) }
                        Key(cc, OnRelease { kb.shortcut(Hid.V, mac) }, "粘贴（${if (mac) "⌘V" else "Ctrl+V"}）", Modifier.width(cw).fillMaxHeight(),
                            slideId = "paste", bubble = iconFace { Lucide(ControlIcons.clipboardPaste, 26.dp, it) }) { Lucide(ControlIcons.clipboardPaste, 17.dp, it) }
                        val held = { code: Int, id: String -> Hold({ kb.press(id, code, combines = false) }, { kb.release(id) }) }
                        Key(cc, held(Hid.PREVIOUS, "prev"), "快退 / 上一首", Modifier.weight(1f).fillMaxHeight(), slideId = "prev", bubble = iconFace { Lucide(ControlIcons.rewind, 26.dp, it) }) { Lucide(ControlIcons.rewind, 17.dp, it) }
                        Key(cc, OnRelease { kb.media(Hid.PLAY_PAUSE) }, "播放 / 暂停", Modifier.weight(1f).fillMaxHeight(), slideId = "play", bubble = iconFace { SvgIcon(ControlIcons.playPause, 28.dp, tint = it) }) { SvgIcon(ControlIcons.playPause, 19.dp, tint = it) }
                        Key(cc, held(Hid.NEXT, "next"), "快进 / 下一首", Modifier.weight(1f).fillMaxHeight(), slideId = "next", bubble = iconFace { Lucide(ControlIcons.fastForward, 26.dp, it) }) { Lucide(ControlIcons.fastForward, 17.dp, it) }
                        Key(cc, OnRelease { kb.media(Hid.MUTE) }, "静音", Modifier.weight(1f).fillMaxHeight(), slideId = "mute", bubble = iconFace { SvgIcon(speakerGlyph(0), 27.dp, tint = it, stroke = 1.4f) }) { SvgIcon(speakerGlyph(0), 18.dp, tint = it, stroke = 1.4f) }
                        Key(cc, held(Hid.VOLUME_DOWN, "vol-"), "音量-", Modifier.weight(1f).fillMaxHeight(), slideId = "vol-", bubble = iconFace { SvgIcon(speakerGlyph(1), 27.dp, tint = it, stroke = 1.4f) }) { SvgIcon(speakerGlyph(1), 18.dp, tint = it, stroke = 1.4f) }
                        Key(cc, held(Hid.VOLUME_UP, "vol+"), "音量+", Modifier.weight(1f).fillMaxHeight(), slideId = "vol+", bubble = iconFace { SvgIcon(speakerGlyph(3), 27.dp, tint = it, stroke = 1.4f) }) { SvgIcon(speakerGlyph(3), 18.dp, tint = it, stroke = 1.4f) }
                    }
                    if (fnShown > 0f) Row(Modifier.fillMaxSize().alpha(fnShown), horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                        for (group in 0 until 3) Row(Modifier.weight(1f).fillMaxHeight(), horizontalArrangement = Arrangement.spacedBy(g)) {
                            for (i in 1..4) {
                                val n = group * 4 + i
                                Key(cc, OnRelease { kb.fKey(n) }, "F$n", Modifier.weight(1f).fillMaxHeight(), slideId = "f$n", bubble = textFace("F$n")) { KeyLabel("F$n", it) }
                            }
                        }
                    }
                }
            }
            // Middle row: Caps, Home End PgUp PgDn | Fn ↑ hide / switch.
            Row(Modifier.weight(1f), horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                Row(horizontalArrangement = Arrangement.spacedBy(g)) {
                    Key(cc, Hold({ kb.capsLock(true) }, { kb.capsLock(false) }), "Caps Lock", Modifier.width(cw).fillMaxHeight(), slideId = "caps",
                        bubble = if (mac) textFace("⇪", 28f) else textFace("Caps")) { KeyText(if (mac) "⇪" else "Caps", if (mac) 18f else fs, it) }
                    listOf(Triple("Home", "Home", Hid.HOME), Triple("End", "End", Hid.END), Triple("PgUp", "Page Up", Hid.PAGE_UP), Triple("PgDn", "Page Down", Hid.PAGE_DOWN)).forEach { (text, name, code) ->
                        Key(cc, OnRelease { kb.click(code) }, name, Modifier.width(cw).fillMaxHeight(), slideId = text.lowercase(), bubble = textFace(text)) { KeyLabel(text, it) }
                    }
                }
                Row(Modifier.width(arrowsW), horizontalArrangement = Arrangement.spacedBy(g)) {
                    Key(cc, OnRelease { kb.tapFn() }, "Fn", Modifier.weight(1f).fillMaxHeight(), latch = kb.fn) { KeyLabel("Fn", it) }
                    Key(cc, Hold({ kb.press("up", Hid.UP) }, { kb.release("up") }), "上", Modifier.weight(1f).fillMaxHeight(), slideId = "up", bubble = iconFace { Lucide(ControlIcons.arrowUp, 26.dp, it) }) { Lucide(ControlIcons.arrowUp, 15.dp, it) }
                    if (rightOfUp == RightOfUp.HIDE) {
                        Key(cc, OnRelease { holder.closeKeyboard() }, "收起键盘", Modifier.weight(1f).fillMaxHeight()) {
                            SvgIcon(ControlIcons.hideKeyboard, 22.dp, tint = it, stroke = 1.6f)
                        }
                    } else SwitchKey(Modifier.weight(1f).fillMaxHeight())
                }
            }
            // Bottom row: modifiers in keyboard order, Tab, forward delete | ← ↓ →.
            Row(Modifier.weight(1f), horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                Row(horizontalArrangement = Arrangement.spacedBy(g)) {
                    val mods = if (mac) listOf(Triple(Mod.CTRL, "⌃", "control"), Triple(Mod.ALT, "⌥", "option"), Triple(Mod.META, "⌘", "command"))
                    else listOf(Triple(Mod.CTRL, "Ctrl", "Ctrl"), Triple(Mod.META, "", "Windows"), Triple(Mod.ALT, "Alt", "Alt"))
                    mods.forEach { (mod, text, name) ->
                        Key(cc, OnRelease(run = { kb.tapModifier(mod) }, long = { kb.lockModifier(mod) }), name, Modifier.width(cw).fillMaxHeight(), latch = kb.mods[mod]) { ink ->
                            if (text.isEmpty()) SvgIcon(ControlIcons.winLogo, 14.dp, tint = ink) else KeyText(text, if (mac) 18f else fs, ink)
                        }
                    }
                    Key(cc, OnRelease { kb.click(Hid.TAB) }, "Tab", Modifier.width(cw).fillMaxHeight(), slideId = "tab", bubble = textFace("Tab")) { KeyLabel("Tab", it) }
                    Key(cc, OnRelease { kb.click(Hid.DELETE) }, if (mac) "向前删除" else "Delete", Modifier.width(cw).fillMaxHeight(), slideId = "del",
                        bubble = if (mac) textFace("⌦", 28f) else textFace("Del")) { KeyText(if (mac) "⌦" else "Del", if (mac) 18f else fs, it) }
                }
                Row(Modifier.width(arrowsW), horizontalArrangement = Arrangement.spacedBy(g)) {
                    listOf(Triple("left", "左", Hid.LEFT), Triple("down", "下", Hid.DOWN), Triple("right", "右", Hid.RIGHT)).forEach { (id, name, code) ->
                        val glyph = when (id) { "left" -> ControlIcons.arrowLeft; "down" -> ControlIcons.arrowDown; else -> ControlIcons.arrowRight }
                        Key(cc, Hold({ kb.press(id, code) }, { kb.release(id) }), name, Modifier.weight(1f).fillMaxHeight(), slideId = id, bubble = iconFace { Lucide(glyph, 26.dp, it) }) { ink ->
                            Lucide(glyph, 15.dp, ink)
                        }
                    }
                }
            }
        }
    }
}

/** The 26 landscape system-keyboard controls; Fn replaces the entire first-row middle. */
@Composable
private fun LandscapeSystemKeys(holder: ControlHolder, cc: ControlColors, remainingHeight: Float) {
    val kb = holder.keyboard
    val mac = holder.session?.state?.focus?.platform == "mac"
    val rows = LandscapeSystemLayout.rows(remainingHeight, kb.fn != null, mac)
    val split = LandscapeSystemLayout.split(remainingHeight)
    val fs = if (split) 12f else 13f
    @Composable fun face(id: String, m: Modifier) {
        val mod = when (id) { "ctrl" -> Mod.CTRL; "alt" -> Mod.ALT; "meta" -> Mod.META; else -> null }
        val text = when (id) {
            "esc" -> "Esc"; "home" -> "Home"; "end" -> "End"; "pgup" -> "PgUp"; "pgdn" -> "PgDn"
            "caps" -> if (mac) "⇪" else "Caps"; "ctrl" -> if (mac) "⌃" else "Ctrl"
            "alt" -> if (mac) "⌥" else "Alt"; "meta" -> if (mac) "⌘" else ""
            "tab" -> "Tab"; "del" -> if (mac) "⌦" else "Del"; "fn" -> "Fn"
            else -> if (id.startsWith("f") && id.drop(1).toIntOrNull() != null) id.uppercase() else null
        }
        val name = when (id) {
            "copy" -> "复制（${if (mac) "⌘C" else "Ctrl+C"}）"; "paste" -> "粘贴（${if (mac) "⌘V" else "Ctrl+V"}）"
            "caps" -> "Caps Lock"; "ctrl" -> if (mac) "control" else "Ctrl"
            "alt" -> if (mac) "option" else "Alt"; "meta" -> if (mac) "command" else "Windows"
            "pgup" -> "Page Up"; "pgdn" -> "Page Down"; "del" -> if (mac) "向前删除" else "Delete"
            "prev" -> "快退 / 上一首"; "play" -> "播放 / 暂停"; "next" -> "快进 / 下一首"
            "mute" -> "静音"; "vol-" -> "音量-"; "vol+" -> "音量+"
            "left" -> "左"; "up" -> "上"; "down" -> "下"; "right" -> "右"
            "switch" -> "切换键盘：换到模拟键盘（发送原始键码）"; "hide" -> "收起键盘"
            else -> text ?: id
        }
        val code = when (id) {
            "esc" -> Hid.ESCAPE; "home" -> Hid.HOME; "end" -> Hid.END; "pgup" -> Hid.PAGE_UP; "pgdn" -> Hid.PAGE_DOWN
            "tab" -> Hid.TAB; "del" -> Hid.DELETE; "prev" -> Hid.PREVIOUS; "next" -> Hid.NEXT
            "vol-" -> Hid.VOLUME_DOWN; "vol+" -> Hid.VOLUME_UP
            "left" -> Hid.LEFT; "up" -> Hid.UP; "down" -> Hid.DOWN; "right" -> Hid.RIGHT
            else -> null
        }
        val held = id in listOf("prev", "next", "vol-", "vol+", "left", "up", "down", "right")
        val action = when {
            mod != null -> OnRelease(run = { kb.tapModifier(mod) }, long = { kb.lockModifier(mod) })
            id == "caps" -> Hold({ kb.capsLock(true) }, { kb.capsLock(false) })
            held -> Hold({ kb.press(id, code!!, combines = id in listOf("left", "up", "down", "right")) }, { kb.release(id) })
            else -> OnRelease {
                when (id) {
                    "copy" -> kb.shortcut(Hid.C, mac); "paste" -> kb.shortcut(Hid.V, mac)
                    "fn" -> kb.tapFn(); "play" -> kb.media(Hid.PLAY_PAUSE); "mute" -> kb.media(Hid.MUTE)
                    "switch" -> kb.switchKind(); "hide" -> holder.closeKeyboard()
                    else -> if (code != null) kb.click(code) else kb.fKey(id.drop(1).toInt())
                }
            }
        }
        @Composable fun glyph(ink: Color, big: Boolean) {
            val size = if (big) 26.dp else if (split) 15.dp else 17.dp
            when (id) {
                "meta" -> if (mac) KeyText("⌘", if (big) 28f else 18f, ink) else SvgIcon(ControlIcons.winLogo, if (big) 26.dp else 14.dp, tint = ink)
                "copy" -> Lucide(DesignIconData.lucideCopy, size, ink)
                "paste" -> Lucide(ControlIcons.clipboardPaste, size, ink)
                "prev" -> Lucide(ControlIcons.rewind, size, ink); "next" -> Lucide(ControlIcons.fastForward, size, ink)
                "play" -> SvgIcon(ControlIcons.playPause, size, tint = ink)
                "mute", "vol-", "vol+" -> SvgIcon(speakerGlyph(if (id == "mute") 0 else if (id == "vol-") 1 else 3), size, tint = ink, stroke = 1.4f)
                "switch", "hide" -> SvgIcon(if (id == "switch") ControlIcons.switchKeyboard else ControlIcons.hideKeyboard, if (big) 26.dp else 22.dp, tint = ink, stroke = 1.6f)
                "left", "up", "down", "right" -> Lucide(when (id) { "left" -> ControlIcons.arrowLeft; "up" -> ControlIcons.arrowUp; "down" -> ControlIcons.arrowDown; else -> ControlIcons.arrowRight }, size, ink)
                else -> KeyText(text!!, if (big) 20f else if (mac && id in listOf("caps", "ctrl", "alt", "del")) 18f else fs, ink)
            }
        }
        Key(cc, action, name, m, latch = mod?.let { kb.mods[it] } ?: if (id == "fn") kb.fn else null,
            slideId = if (mod == null && id !in listOf("fn", "switch", "hide")) id else null,
            bubble = if (mod == null && id !in listOf("fn", "switch", "hide")) BubbleFace(false) { glyph(it, true) } else null) { glyph(it, false) }
    }
    Column(Modifier.fillMaxSize(), verticalArrangement = Arrangement.spacedBy(3.dp)) {
        rows.forEach { row ->
            Row(Modifier.weight(1f).fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(3.dp)) {
                row.forEach { id -> key(id) { face(id, Modifier.weight(1f).fillMaxHeight()) } }
            }
        }
    }
}

/** What the control keys have right of ↑ ([ControlKeys]). */
private enum class RightOfUp { HIDE, SWITCH }

private val LETTERS = listOf("qwertyuiop", "asdfghjkl", "zxcvbnm")
private const val DIGITS = "1234567890"
private val SYMBOLS = listOf("-/:;()$&@\"", "[]{}#%^*+=", "_\\|~<>?!'")

/**
 * The emulated keyboard below the control keys (§3.1.2, item 2 用户 2026-11-12 确认): the number row
 * 1–0 always on top (it shows ! @ # … while Shift is down), then the letters (QWERTY, Shift and
 * backspace on the third row) or three rows of symbols (backspace at the end of the last), then the
 * same bottom row on both layers: #+= / ABC (1.3 kw), `,`, space, `.`, return (2.2 kw). Both layers
 * have five rows of one height sharing the keyboard (switching does not move anything). [small]:
 * landscape (rows 6 apart, characters 19, bottom-row text 13).
 */
@Composable
private fun EmulatedKeys(kb: KeyboardModel, cc: ControlColors, modifier: Modifier, small: Boolean = false) {
    val symbols = kb.symbols
    val shift = kb.shiftDown
    BoxWithConstraints(modifier.padding(horizontal = 3.dp)) {
        val kg = 6.dp
        val kw = (maxWidth - kg * 9) / 10
        Column(Modifier.fillMaxSize(), verticalArrangement = Arrangement.spacedBy(if (small) 6.dp else 10.dp)) {
            @Composable fun charKey(ch: Char, m: Modifier) {
                // With Shift down: letters in capitals, the rest as Shift + that key types on a US layout.
                val label = when {
                    !shift -> ch.toString()
                    ch.isLetter() -> ch.uppercaseChar().toString()
                    else -> (Hid.US_SHIFT_LABEL[ch] ?: ch).toString()
                }
                val id = "vk-$ch"
                Key(cc, Hold({ kb.pressChar(id, ch) }, { kb.release(id) }), label, m, function = false, radius = 8.dp,
                    slideId = id, bubble = BubbleFace(true) { T(label, 32f, it, lineHeight = 38f) }) { ink ->
                    T(label, if (small || ch.isDigit()) 19f else 22f, ink, lineHeight = 26f)
                }
            }
            @Composable fun backspace(m: Modifier) = Key(cc, Hold({ kb.press("backspace", Hid.BACKSPACE) }, { kb.release("backspace") }), "退格", m, radius = 8.dp,
                slideId = "backspace", bubble = BubbleFace(false) { Lucide(ControlIcons.delete, 26.dp, it) }) {
                Lucide(ControlIcons.delete, 19.dp, it)
            }
            Row(Modifier.weight(1f).fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(kg, Alignment.CenterHorizontally)) {
                DIGITS.forEach { ch -> charKey(ch, Modifier.width(kw).fillMaxHeight()) }
            }
            val rows = if (symbols) SYMBOLS else LETTERS
            rows.forEachIndexed { i, r ->
                Row(Modifier.weight(1f).fillMaxWidth().padding(horizontal = if (!symbols && i == 1) kw / 2 + kg / 2 else 0.dp),
                    horizontalArrangement = Arrangement.spacedBy(kg, Alignment.CenterHorizontally)) {
                    if (!symbols && i == 2) {
                        Key(cc, OnRelease(run = { kb.tapModifier(Mod.SHIFT) }, long = { kb.lockModifier(Mod.SHIFT) }), "Shift", Modifier.weight(1f).fillMaxHeight().padding(end = 3.6.dp), latch = kb.mods[Mod.SHIFT], radius = 8.dp) {
                            Lucide(ControlIcons.arrowUp, 18.dp, it)
                        }
                    }
                    r.forEach { ch -> charKey(ch, if (!symbols && i == 1) Modifier.weight(1f).fillMaxHeight() else Modifier.width(kw).fillMaxHeight()) }
                    if (i == rows.size - 1) backspace(Modifier.weight(1f).fillMaxHeight().padding(start = if (symbols) 0.dp else 3.6.dp))
                }
            }
            Row(Modifier.weight(1f).fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(kg)) {
                Key(cc, OnRelease { kb.symbols = !symbols }, if (symbols) "字母" else "符号", Modifier.width(kw * 1.3f).fillMaxHeight(), radius = 8.dp) {
                    T(if (symbols) "ABC" else "#+=", if (small) 13f else 14f, it, lineHeight = 18f)
                }
                charKey(',', Modifier.width(kw).fillMaxHeight())
                Key(cc, Hold({ kb.press("space", Hid.SPACE) }, { kb.release("space") }), "空格", Modifier.weight(1f).fillMaxHeight(), function = false, radius = 8.dp,
                    slideId = "space", bubble = BubbleFace(false) { T("空格", 20f, it, weight = 500, lineHeight = 25f) }) {
                    T("空格", if (small) 13f else 15f, it, lineHeight = 20f)
                }
                charKey('.', Modifier.width(kw).fillMaxHeight())
                Key(cc, Hold({ kb.press("return", Hid.RETURN) }, { kb.release("return") }), "回车", Modifier.width(kw * 2.2f).fillMaxHeight(), radius = 8.dp,
                    slideId = "return", bubble = BubbleFace(false) { Lucide(ControlIcons.cornerDownLeft, 26.dp, it) }) {
                    Lucide(ControlIcons.cornerDownLeft, 19.dp, it)
                }
            }
        }
    }
}
