package com.hachiwii.oink

import android.content.Context
import androidx.core.content.edit
import kotlinx.coroutines.cancel
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config

/** 「自动写入剪贴板」 has one source of truth (LocalSettings) and reaches the core on every start. */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], application = android.app.Application::class)
class AutoWriteClipboardTest {
    private val context: Context get() = RuntimeEnvironment.getApplication()
    private val core = TestCore()
    private lateinit var repository: OinkRepository

    @Before
    fun setUp() {
        repository = OinkRepository(context)
        repository.createCore = { core.bridge }
    }

    @After
    fun tearDown() { repository.scope.cancel() }

    @Test
    fun storedSettingReachesTheCoreOnStartWithoutTouchingTheSwitch() {
        LocalSettings(context).autoWriteClipboard = true
        // The state seen on the user's phone: the removed second copy still says false.
        context.getSharedPreferences("oink", Context.MODE_PRIVATE).edit { putBoolean("automaticText", false) }

        repository.pumping { repository.startCore() }

        assertEquals(listOf(true), core.automaticText.toList())
    }

    @Test
    fun switchStoresTheSettingEvenWithoutARunningCore() {
        LocalSettings(context).autoWriteClipboard = true

        repository.setAutomatic(false)
        assertFalse(LocalSettings(context).autoWriteClipboard)
        repository.pumping { repository.startCore() }

        assertEquals(listOf(false), core.automaticText.toList())
    }

    @Test
    fun switchReachesARunningCore() {
        repository.pumping { repository.startCore() }
        repository.setAutomatic(true)
        eventually { core.automaticText.size == 2 }

        assertEquals(listOf(false, true), core.automaticText.toList())
    }
}
