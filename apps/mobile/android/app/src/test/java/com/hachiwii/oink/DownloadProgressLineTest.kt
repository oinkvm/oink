package com.hachiwii.oink

import android.content.Context
import kotlinx.coroutines.cancel
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config

/** The persistent notification shows `AppState.progress` over the status: it must end with its download. */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], application = android.app.Application::class)
class DownloadProgressLineTest {
    private val context: Context get() = RuntimeEnvironment.getApplication()
    private val core = TestCore()
    private lateinit var repository: OinkRepository
    private val operation = ByteArray(16)

    @Before
    fun setUp() {
        repository = OinkRepository(context)
        repository.createCore = { core.bridge }
        repository.pumping { repository.startCore() }
    }

    @After
    fun tearDown() { repository.scope.cancel() }

    private fun send(event: TransferCoreEvent) { core.events.trySend(CoreEvent.Transfer(event)) }
    private fun downloading() {
        send(TransferCoreEvent.DownloadProgress(operation, 400, 1000, 7))
        eventually { repository.state.value.progress != null }
        assertEquals("400 / 1000 字节", repository.state.value.progress)
    }

    @Test
    fun completedDownloadClearsTheLine() {
        downloading()
        send(TransferCoreEvent.DownloadProgress(operation, 1000, 1000, 7))
        eventually { repository.state.value.progress == null }
        assertNull(repository.state.value.progress)
    }

    @Test
    fun failedDownloadClearsTheLine() {
        downloading()
        send(TransferCoreEvent.Error(operation, "link lost", 7))
        eventually { repository.state.value.progress == null }
        assertNull(repository.state.value.progress)
    }

    @Test
    fun endedPhoneReceiveClearsTheLine() {
        downloading()
        // A failed or cancelled phone-to-phone receive: no final progress, only its end.
        send(TransferCoreEvent.DirectEnded(listOf(7)))
        eventually { repository.state.value.progress == null }
        assertNull(repository.state.value.progress)
    }
}
