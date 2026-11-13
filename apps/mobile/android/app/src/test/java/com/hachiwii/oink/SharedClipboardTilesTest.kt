package com.hachiwii.oink

import android.content.ClipboardManager
import android.content.Context
import android.os.Looper
import java.lang.reflect.Proxy
import java.util.concurrent.CopyOnWriteArrayList
import kotlinx.coroutines.async
import kotlinx.coroutines.cancel
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.receiveAsFlow
import org.junit.After
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import uniffi.oink_mobile_ffi.CardOs
import uniffi.oink_mobile_ffi.TransferOnlyDevice
import uniffi.oink_mobile_ffi.TransferTarget
import uniffi.oink_mobile_ffi.TransferTargetKind
import uniffi.oink_mobile_ffi.TransferTargetsView

/**
 * The repository side of the notification tap and the 复制 / 粘贴 tiles (user 2026-11-11), on a core
 * that answers only what these paths ask.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], application = android.app.Application::class)
class SharedClipboardTilesTest {
    private val context: Context get() = RuntimeEnvironment.getApplication()
    private lateinit var repository: OinkRepository
    private val events = Channel<CoreEvent>(Channel.UNLIMITED)
    /** Methods the repository called on the core, by name. */
    private val calls = CopyOnWriteArrayList<String>()
    private val shared = CopyOnWriteArrayList<ShareDraft>()
    private var current: ReceivedContent? = null
    private var helperText: ClipPaste.Text? = null

    private val pc = Device("pc", "Windows PC", true, computer = true, connection = DeviceConnection.ONLINE)
    private val targets = TransferTargetsView(1uL, listOf(TransferTarget("pc", "Windows PC", TransferTargetKind.COMPUTER, CardOs.WINDOWS, false, 1uL)),
        1u, TransferOnlyDevice("pc", "Windows PC", TransferTargetKind.COMPUTER, CardOs.WINDOWS, true), false, null)

    private val core: CoreBridge = Proxy.newProxyInstance(CoreBridge::class.java.classLoader, arrayOf(CoreBridge::class.java)) { proxy, method, args ->
        calls += method.name
        when (method.name) {
            "getEvents" -> events.receiveAsFlow()
            "start", "stop", "automaticText", "reportPublish", "nativeClipboardChanged", "transferForeground" -> Unit
            "localDeviceName" -> DeviceName("Phone", "Phone", false)
            "transferTargets" -> targets
            "receive" -> current
            "publishPrivileged" -> com.hachiwii.oink.clipboard.NativeClipboardWrite.Unavailable
            "validateReceived" -> true
            "privilegedPasteText" -> helperText
            "share" -> { shared += args!![0] as ShareDraft; listOf(DeliveryResult(args[2] as String, true, "已发送")) }
            "toString" -> "FakeCore"
            "hashCode" -> System.identityHashCode(proxy)
            "equals" -> proxy === args?.get(0)
            else -> throw UnsupportedOperationException(method.name)
        }
    } as CoreBridge

    @Before fun start() {
        repository = OinkRepository(context)
        repository.createCore = { core }
        pump { repository.startCore() }
        events.trySend(CoreEvent.Snapshot(CoreSnapshot(devices = listOf(Device("me", "Phone", true, isSelf = true), pc))))
        eventually { repository.state.value.snapshot.devices.size == 2 }
    }

    @After fun stop() { repository.scope.cancel() }

    /** Repository work hops to Main, the Robolectric test thread: run it elsewhere and pump the main looper. */
    private fun <T> pump(block: suspend () -> T): T {
        val job = repository.scope.async { block() }
        eventually { job.isCompleted }
        return kotlinx.coroutines.runBlocking { job.await() }
    }
    private fun eventually(condition: () -> Boolean) {
        val deadline = System.nanoTime() + 5_000_000_000
        while (!condition()) {
            check(System.nanoTime() < deadline) { "timed out" }
            shadowOf(Looper.getMainLooper()).idle()
            Thread.sleep(5)
        }
    }

    @Test fun aCopiedSharedClipboardIsOnThePhoneAndTheTitleSaysSo() {
        current = ReceivedContent(text = "来自电脑", operationId = 1uL)
        assertEquals(CopyOutcome.Copied, pump { repository.copySharedClipboard().await() })
        val clip = context.getSystemService(ClipboardManager::class.java).primaryClip
        assertEquals("来自电脑", clip?.getItemAt(0)?.text?.toString())
        assertEquals("已复制", persistentNotice(repository.state.value).title)
    }

    @Test fun nothingToCopyIsItsOwnOutcome() {
        current = null
        assertEquals(CopyOutcome.Nothing, pump { repository.copySharedClipboard().await() })
        assertFalse(repository.state.value.noticeCopied)
        assertNull(context.getSystemService(ClipboardManager::class.java).primaryClip)
    }

    /** User 2026-11-11: a manual paste always sends; 「自动读取」 only decides whether copies go by themselves. */
    @Test fun privilegedPasteSendsWithAutoReadOff() {
        LocalSettings(context).autoReadClipboard = false
        helperText = ClipPaste.Text("手机上的文字")
        val text = repository.privilegedPasteText()
        assertEquals(ClipPaste.Text("手机上的文字"), text)
        val result = java.util.concurrent.atomic.AtomicReference<String?>("pending")
        repository.pasteClipboard(text!!) { result.set(it) }
        eventually { result.get() != "pending" }
        assertNull(result.get())
        assertEquals(listOf("手机上的文字"), shared.map { it.text })
        // Neither the auto-read switch nor its broadcast path was involved.
        assertFalse("privilegedAutoRead" in calls)
        assertFalse("privilegedReadAndBroadcast" in calls)
    }

    @Test fun anEmptyClipboardOnlyPrompts() {
        var result: String? = "pending"
        repository.pasteClipboard(ClipPaste.Empty) { result = it }
        assertEquals(PASTE_EMPTY, result)
        repository.pasteClipboard(ClipPaste.Unsupported) { result = it }
        assertEquals(PASTE_UNSUPPORTED, result)
        assertTrue(shared.isEmpty())
    }
}
