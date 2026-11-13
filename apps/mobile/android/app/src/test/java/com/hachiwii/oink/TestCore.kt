package com.hachiwii.oink

import android.os.Looper
import java.lang.reflect.Proxy
import java.util.concurrent.CopyOnWriteArrayList
import kotlinx.coroutines.async
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.receiveAsFlow
import kotlinx.coroutines.runBlocking
import org.robolectric.Shadows.shadowOf

/**
 * A [CoreBridge] with only what [OinkRepository.startCore] touches: tests feed [events] and read
 * [automaticText]. Everything else throws, like a failing core (audio settings and targets are read
 * inside runCatching and reported).
 */
internal class TestCore {
    val events = Channel<CoreEvent>(Channel.UNLIMITED)
    val automaticText = CopyOnWriteArrayList<Boolean>()
    val bridge: CoreBridge = Proxy.newProxyInstance(
        CoreBridge::class.java.classLoader, arrayOf(CoreBridge::class.java),
    ) { proxy, method, args ->
        when (method.name) {
            "getEvents" -> events.receiveAsFlow()
            "automaticText" -> { automaticText += args!![0] as Boolean; Unit }
            "start", "stop", "pairingWindow", "transferForeground" -> Unit
            "localDeviceName" -> DeviceName("Test", "Test", false)
            "toString" -> "TestCore"
            "hashCode" -> System.identityHashCode(proxy)
            "equals" -> proxy === args?.get(0)
            else -> throw UnsupportedOperationException(method.name)
        }
    } as CoreBridge
}

/** Repository calls hop to Main, which is the Robolectric test thread: run them elsewhere and pump the main looper. */
internal fun <T> OinkRepository.pumping(block: suspend () -> T): T {
    val job = scope.async { block() }
    val deadline = System.nanoTime() + 5_000_000_000
    while (!job.isCompleted) {
        check(System.nanoTime() < deadline) { "timed out" }
        shadowOf(Looper.getMainLooper()).idle()
        Thread.sleep(5)
    }
    return runBlocking { job.await() }
}

/** Waits (off Main) until [condition] holds; the repository handles core events on its IO scope. */
internal fun eventually(condition: () -> Boolean) {
    val deadline = System.nanoTime() + 5_000_000_000
    while (!condition()) {
        check(System.nanoTime() < deadline) { "timed out" }
        Thread.sleep(5)
    }
}
