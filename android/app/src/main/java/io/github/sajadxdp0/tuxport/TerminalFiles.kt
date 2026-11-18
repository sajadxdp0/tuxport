package io.github.sajadxdp0.tuxport

import android.app.Application
import android.net.Uri
import android.os.Handler
import android.os.Looper
import android.os.ParcelFileDescriptor
import android.provider.OpenableColumns
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.LiveData
import androidx.lifecycle.MutableLiveData
import java.io.IOException
import java.io.InputStream
import kotlin.concurrent.read
import kotlin.concurrent.thread

/** Owns a picker request and its copy across activity recreation, but never process death. */
internal class TerminalFiles(application: Application) : AndroidViewModel(application) {
    data class Result(val paths: List<String>, val failures: List<String>)

    private val notifications = MutableLiveData(Unit)
    val changes: LiveData<Unit> get() = notifications

    var copying = false
        private set
    var result: Result? = null
        private set
    private var selecting = false
    private var login = ""
    private var selectedUris = emptyList<Uri>()
    private val main = Handler(Looper.getMainLooper())

    val occupied: Boolean get() = selecting || copying || selectedUris.isNotEmpty() || result != null

    fun begin(login: String): Boolean {
        if (occupied) return false
        this.login = login
        selecting = true
        changed()
        return true
    }

    fun selected(uris: List<Uri>) {
        if (!selecting) return
        selecting = false
        selectedUris = uris.distinct()
        changed()
    }

    /** Starts a pending copy once; the caller must be resumed with its privacy gate open. */
    fun importSelected(): Boolean {
        if (selectedUris.isEmpty() || copying) return false
        val uris = selectedUris
        val requestedLogin = login
        selectedUris = emptyList()
        copying = true
        changed()
        thread(name = "terminal-import") {
            val imported = copySelected(uris, requestedLogin)
            main.post {
                copying = false
                result = imported
                changed()
            }
        }
        return true
    }

    private fun copySelected(uris: List<Uri>, login: String): Result {
        val context = getApplication<Application>()
        val resolver = context.contentResolver
        val paths = mutableListOf<String>()
        val failures = mutableListOf<String>()
        try {
            Rootfs.access.read {
                if (!Rootfs.isReady(context)) throw IOException("The filesystem is not ready")
                val store = Rootfs.store(context).path
                val account = Rootfs.loginAccount(context, login)
                for (uri in uris) {
                    var name = context.getString(R.string.import_file_name)
                    try {
                        resolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { cursor ->
                            if (cursor.moveToFirst()) name = cursor.getString(0)?.takeIf(String::isNotEmpty) ?: name
                        }
                        val input = resolver.openInputStream(uri)
                            ?: throw IOException("Cannot open the selected file")
                        paths += copyIntoHome(store, account, name, input)
                    } catch (e: Exception) {
                        failures += context.getString(R.string.import_file_failed, name, e.message ?: e.javaClass.simpleName)
                    }
                }
            }
        } catch (e: Exception) {
            failures += e.message ?: e.javaClass.simpleName
        }
        return Result(paths, failures)
    }

    fun dismissResult() {
        result = null
        changed()
    }

    private fun changed() { notifications.value = Unit }

    companion object {
        /** Owns [input]; a failed copy or close removes only the file it created. */
        internal fun copyIntoHome(store: String, account: Rootfs.Account, displayName: String, input: InputStream): String {
            var createdFile: Pair<String, String>? = null
            return try {
                input.use {
                    val home = Documents.canonical("/" + account.home.trim('/'))?.takeIf { it != "/" }
                        ?: throw IOException("The login account has no home directory")
                    val top = Uri.encode(home, "/")
                    val safeName = displayName.map { if (Character.isISOControl(it)) '_' else it }.joinToString("")
                    val name = Documents.fileName(safeName, "", false)
                    val created = Native.browseCreate(store, top, top, name, false, account.uid, account.gid)
                    val path = Documents.child(top, created)
                    createdFile = top to path
                    val fd = Native.browseOpenWrite(store, top, path, truncate = false, append = false)
                    try {
                        ParcelFileDescriptor.AutoCloseOutputStream(ParcelFileDescriptor.adoptFd(fd)).use { output ->
                            input.copyTo(output)
                            output.flush()
                            output.fd.sync()
                        }
                    } finally {
                        Native.browseWritten(store, top, path)
                    }
                    Documents.child(home, Documents.displayName(created))
                }
            } catch (e: Exception) {
                createdFile?.let { (top, path) ->
                    try {
                        Native.browseRemove(store, top, path)
                    } catch (cleanup: Exception) {
                        e.addSuppressed(cleanup)
                    }
                }
                throw e
            }
        }

        /** Escapes imported absolute paths as shell arguments without appending a newline. */
        fun clipboardPaths(paths: List<String>): String = paths.joinToString(" ") { path ->
            buildString {
                for (char in path) {
                    if (char.isWhitespace() || char in "\\'\"`$&|;<>*?()[]{}!#~^") append('\\')
                    append(char)
                }
            }
        }
    }
}
