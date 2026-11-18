package io.github.sajadxdp0.tuxport

import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import java.io.Closeable
import java.io.IOException
import java.net.InetAddress
import java.net.ServerSocket
import java.net.Socket
import java.net.URI
import java.net.URISyntaxException
import java.nio.ByteBuffer
import java.util.UUID
import java.util.concurrent.ExecutionException
import java.util.concurrent.FutureTask
import java.util.concurrent.TimeUnit
import java.util.concurrent.TimeoutException
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.concurrent.thread

/**
 * One kernel's authenticated loopback browser endpoint. Initial-namespace guest AF_INET sockets reach host sockets;
 * AF_UNIX sockets stay inside the emulated kernel. No terminal bytes or inherited tab state are used.
 * The single worker bounds connections and posts only authenticated requests to the UI thread.
 */
internal class BrowserBridge(private val open: (String) -> Boolean) : Closeable {
    private val main = Handler(Looper.getMainLooper())
    private val closed = AtomicBoolean()
    private val server = ServerSocket(0, 4, InetAddress.getByName("127.0.0.1"))
    private val token = UUID.randomUUID().toString()
    val configuration = "${server.localPort}\n$token\n"
    @Volatile private var active: Socket? = null

    init {
        thread(name = "tuxport-browser", isDaemon = true) {
            while (!closed.get()) {
                try {
                    server.accept().use { socket ->
                        active = socket
                        if (!closed.get()) serve(socket)
                    }
                } catch (_: IOException) {
                    // A client may leave mid-request; close also interrupts accept and active I/O.
                } finally {
                    active = null
                }
            }
        }
    }

    private fun serve(socket: Socket) {
        val deadline = SystemClock.elapsedRealtime() + 2000
        val input = socket.getInputStream()
        fun line(limit: Int): String? {
            val bytes = ByteArray(limit)
            var length = 0
            while (true) {
                val remaining = deadline - SystemClock.elapsedRealtime()
                if (remaining <= 0) return null
                socket.soTimeout = remaining.toInt()
                val byte = input.read()
                if (byte < 0) return null
                if (byte == 10) break
                if (length == limit) return null
                bytes[length++] = byte.toByte()
            }
            return try {
                Charsets.UTF_8.newDecoder().decode(ByteBuffer.wrap(bytes, 0, length)).toString()
            } catch (_: CharacterCodingException) {
                null
            }
        }
        val credential = line(64)
        val url = line(MAX_URL_BYTES)
        val result = if (credential != token) {
            "ERROR authentication failed"
        } else {
            when {
                url == null || !isWebUrl(url) -> "ERROR invalid web URL"
                launch(url) -> "OK"
                else -> "ERROR no focused Tuxport terminal or browser available"
            }
        }
        socket.getOutputStream().write("$result\n".toByteArray())
    }

    private fun launch(url: String): Boolean {
        val task = FutureTask { !closed.get() && open(url) }
        if (!main.post(task)) return false
        return try {
            task.get(2, TimeUnit.SECONDS)
        } catch (_: TimeoutException) {
            false
        } catch (_: ExecutionException) {
            false
        } catch (_: InterruptedException) {
            Thread.currentThread().interrupt()
            false
        } finally {
            // A UI thread that was stalled must not open an expired request later.
            task.cancel(false)
            main.removeCallbacks(task)
        }
    }

    override fun close() {
        if (!closed.compareAndSet(false, true)) return
        runCatching { server.close() }
        runCatching { active?.close() }
    }

    companion object {
        const val CONFIG_PATH = "/etc/tuxport/browser"
        const val MAX_URL_BYTES = 8192

        fun isWebUrl(url: String): Boolean {
            if (url.toByteArray().size > MAX_URL_BYTES || url.any { it.isWhitespace() || it.isISOControl() }) return false
            val uri = try { URI(url) } catch (_: URISyntaxException) { return false }
            return (uri.scheme == "http" || uri.scheme == "https") && !uri.host.isNullOrEmpty() && uri.port in -1..65535
        }
    }
}
