package com.hachiwii.oink.ui

/** Icons of the control page (ControlPage.jsx: lucide-react 0.468 paths and the page's own glyphs). */
internal object ControlIcons {
    private fun s(vararg d: String) = d.map { IconPart(it, 'S', 0f, 0) }
    private fun lucide(vararg d: String) = IconData(0f, 0f, 24f, 24f, s(*d))

    val sliders = lucide("M21 4H14M10 4H3M21 12H12M8 12H3M21 20H16M12 20H3M14 2V6M8 10V14M16 18V22")
    val chevronDown = lucide("m6 9 6 6 6-6")
    /** 全屏 (Lucide maximize-2): two arrows pointing out to opposite corners. */
    val maximize = lucide("M15 3h6v6", "M9 21H3v-6", "M21 3l-7 7", "M3 21l7-7")
    /** 恢复 / 还原 (Lucide minimize-2): two arrows pointing in from opposite corners. */
    val minimize = lucide("M4 14h6v6", "M20 10h-6V4", "M14 10l7-7", "M3 21l7-7")
    val check = lucide("M20 6 9 17l-5-5")
    val arrowUp = lucide("m5 12 7-7 7 7", "M12 19V5")
    val arrowDown = lucide("M12 5v14", "m19 12-7 7-7-7")
    val arrowLeft = lucide("m12 19-7-7 7-7", "M19 12H5")
    val arrowRight = lucide("M5 12h14", "m12 5 7 7-7 7")
    val delete = lucide("M10 5a2 2 0 0 0-1.344.519l-6.328 5.74a1 1 0 0 0 0 1.481l6.328 5.741A2 2 0 0 0 10 19h10a2 2 0 0 0 2-2V7a2 2 0 0 0-2-2z",
        "m12 9 6 6", "m18 9-6 6")
    val cornerDownLeft = lucide("M9 10L4 15L9 20", "M20 4v7a4 4 0 0 1-4 4H4")
    val clipboardPaste = lucide("M15 2H9a1 1 0 0 0-1 1v2c0 .6.4 1 1 1h6c.6 0 1-.4 1-1V3c0-.6-.4-1-1-1Z",
        "M8 4H6a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2M16 4h2a2 2 0 0 1 2 2v2M11 14h10", "m17 10 4 4-4 4")
    /** Lucide polygons drawn with fill="currentColor" (Mac keyboard F7 / F9 keycaps). */
    val rewind = IconData(0f, 0f, 24f, 24f, listOf(
        IconPart("M11 19L2 12L11 5Z", 'F', 0f, 0), IconPart("M11 19L2 12L11 5Z", 'S', 0f, 0),
        IconPart("M22 19L13 12L22 5Z", 'F', 0f, 0), IconPart("M22 19L13 12L22 5Z", 'S', 0f, 0)))
    val fastForward = IconData(0f, 0f, 24f, 24f, listOf(
        IconPart("M13 19L22 12L13 5Z", 'F', 0f, 0), IconPart("M13 19L22 12L13 5Z", 'S', 0f, 0),
        IconPart("M2 19L11 12L2 5Z", 'F', 0f, 0), IconPart("M2 19L11 12L2 5Z", 'S', 0f, 0)))
    /** Play / pause (F8 keycap): solid triangle and two bars, stroke 1.9. */
    val playPause = IconData(0f, 0f, 24f, 24f, listOf(
        IconPart("M4 6.5v11l7.5-5.5z", 'F', 0f, 0), IconPart("M4 6.5v11l7.5-5.5z", 'S', 1.9f, 0),
        IconPart("M15 6.5v11M19.5 6.5v11", 'S', 1.9f, 0)))
    /** Windows logo for the Win modifier (16 viewBox, filled). */
    val winLogo = IconData(0f, 0f, 16f, 16f, listOf(IconPart("M0 0h7.5v7.5H0zM8.5 0H16v7.5H8.5zM0 8.5h7.5V16H0zM8.5 8.5H16V16H8.5z", 'F', 0f, 0)))
    /**
     * 切换键盘: a keyboard with a small circular-arrows badge at its lower right, cut out of the
     * keyboard by a 1.5 gap (the mockup's mask: the clip is the whole box minus that circle, wound
     * the other way so non-zero filling leaves it out). Stroke 1.6, arrows 1.5.
     */
    val switchKeyboard = IconData(0f, 0f, 24f, 24f, listOf(
        IconPart("M3.7 4H17.3A2.2 2.2 0 0 1 19.5 6.2V13.3A2.2 2.2 0 0 1 17.3 15.5H3.7A2.2 2.2 0 0 1 1.5 13.3V6.2A2.2 2.2 0 0 1 3.7 4Z", 'S', 0f, 0, clipped = true),
        IconPart("M5 7.8h.01M8.2 7.8h.01M11.4 7.8h.01M14.6 7.8h.01M6.5 11.8h8", 'S', 0f, 0, clipped = true),
        IconPart("M14.4 17.2a3.6 3.6 0 0 1 6.5-2.1M21.6 17.8a3.6 3.6 0 0 1-6.5 2.1", 'S', 1.5f, 0),
        IconPart("M21.2 12.9v2.4h-2.4M14.8 22.1v-2.4h2.4", 'S', 1.5f, 0)),
        clip = "M0 0H24V24H0Z M11.6 17.5A6.4 6.4 0 0 0 24.4 17.5A6.4 6.4 0 0 0 11.6 17.5Z")
    /**
     * 收起键盘 (用户 2026-11-12): a keyboard with a chevron under it, like SF Symbol
     * `keyboard.chevron.compact.down` (ControlPage.jsx HideKeyboardIcon). Stroke 1.6, chevron 1.8.
     */
    val hideKeyboard = IconData(0f, 0f, 24f, 24f, listOf(
        IconPart("M4.9 2.5H19.1A2.4 2.4 0 0 1 21.5 4.9V12.6A2.4 2.4 0 0 1 19.1 15H4.9A2.4 2.4 0 0 1 2.5 12.6V4.9A2.4 2.4 0 0 1 4.9 2.5Z", 'S', 0f, 0),
        IconPart("M6 6.5h.01M9.4 6.5h.01M12.8 6.5h.01M16.2 6.5h.01M7.5 11h9", 'S', 0f, 0),
        IconPart("M8.5 18.6l3.5 2.9 3.5-2.9", 'S', 1.8f, 0)))
}
