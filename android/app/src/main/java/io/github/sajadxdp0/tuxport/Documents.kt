package io.github.sajadxdp0.tuxport

import android.content.Context
import android.content.Intent
import android.database.Cursor
import android.database.MatrixCursor
import android.net.Uri
import android.os.CancellationSignal
import android.os.Handler
import android.os.HandlerThread
import android.os.ParcelFileDescriptor
import android.os.SystemClock
import android.os.storage.StorageManager
import android.provider.DocumentsContract
import android.provider.DocumentsContract.Document
import android.provider.DocumentsContract.Root
import android.provider.DocumentsProvider
import android.util.Log
import android.webkit.MimeTypeMap
import java.io.ByteArrayOutputStream
import java.io.FileNotFoundException
import java.io.IOException
import kotlin.concurrent.read

/**
 * Tuxport in the system file picker (the android/documents map): file access is off by default.
 * Once enabled in Settings, the login account's home directory is one root and the whole system
 * (`/`) is another, read-only outside the home.
 *
 * Document IDs are the guest's absolute paths (written as described in [Native]) and are looked up
 * directly in the store ([Native.browseList] and the like), without the kernel running: while it
 * runs, what is seen is what it last committed, and mounts such as `/proc`, `/dev` and `/run` are
 * not visible (they are empty directories in the store). Symbolic links are followed. Only
 * directories and regular files; device nodes, FIFOs and sockets are not listed.
 *
 * Inside the home directory files can be created, written, deleted, renamed and moved (the home
 * directory itself only allows creating inside it), and new ones belong to the login account;
 * everywhere else (the whole-system root) is read-only. Confined by real path: writing out of the
 * home directory through a symbolic link is not allowed. After a change, the Files app is told to
 * refresh that directory, and the running kernel is told as well, so programs watching in the guest
 * receive inotify events ([Native.browseWritten]). The other way round, when the guest changes
 * directory entries (create, delete, rename), the service takes them periodically
 * ([Native.takeChangedDirs]) and tells the Files app to refresh that directory ([dirsChanged]).
 *
 * With file access turned off, no roots or documents are accessible, including through old URIs.
 * Disabling also revokes URI grants; already opened file descriptors and copied data remain usable.
 */
class Documents : DocumentsProvider() {
    /** What a `file:` link names ([fileLink]). */
    sealed interface FileLink {
        /** A document shown now, and its MIME type. */
        data class Found(val uri: Uri, val mime: String) : FileLink

        /** A file on another machine: a program there (over ssh) wrote the link. */
        data class Remote(val host: String) : FileLink

        /** A file in the store while file access is disabled. */
        data object Hidden : FileLink

        /** No directory or regular file there in the store (or no store). */
        data object Missing : FileLink
    }

    /**
     * The login account, its UID and GID, and the home directory (as a document ID). null while
     * there is no store or no home directory; a home of `/` counts as none, since it would make the
     * whole system writable (the native side refuses it too).
     */
    private data class Home(val account: String, val uid: Int, val gid: Int, val path: String)

    override fun onCreate(): Boolean = true

    override fun queryRoots(projection: Array<out String>?): Cursor {
        val result = MatrixCursor(projection ?: ROOT_COLUMNS)
        val context = context ?: return result
        if (!Settings.fileAccess(context) || !Rootfs.isReady(context)) return result
        val distro = Rootfs.installed(context)?.label.orEmpty()
        // Free space includes caches the system can clear, matching what Settings shows
        val free = runCatching {
            val storage = context.getSystemService(StorageManager::class.java)
            storage.getAllocatableBytes(storage.getUuidForPath(context.filesDir))
        }.getOrNull()
        // Without a home directory there is no home root, but the whole system is still shown when
        // it is turned on
        val home = home(context)
        if (home != null) result.newRow().apply {
            add(Root.COLUMN_ROOT_ID, ROOT_HOME)
            add(Root.COLUMN_DOCUMENT_ID, home.path)
            add(Root.COLUMN_TITLE, context.getString(R.string.app_name))
            add(Root.COLUMN_SUMMARY, context.getString(R.string.documents_home, home.account, distro))
            // Creating is supported: so it also appears in other apps' "Save to"
            val flags = Root.FLAG_LOCAL_ONLY or Root.FLAG_SUPPORTS_IS_CHILD or Root.FLAG_SUPPORTS_SEARCH or
                Root.FLAG_SUPPORTS_CREATE
            add(Root.COLUMN_FLAGS, flags)
            add(Root.COLUMN_ICON, R.mipmap.ic_launcher)
            add(Root.COLUMN_AVAILABLE_BYTES, free)
        }
        result.newRow().apply {
            add(Root.COLUMN_ROOT_ID, ROOT_SYSTEM)
            add(Root.COLUMN_DOCUMENT_ID, "/")
            add(Root.COLUMN_TITLE, context.getString(R.string.app_name))
            add(Root.COLUMN_SUMMARY, context.getString(R.string.documents_system, distro))
            add(Root.COLUMN_FLAGS, Root.FLAG_LOCAL_ONLY or Root.FLAG_SUPPORTS_IS_CHILD or Root.FLAG_SUPPORTS_SEARCH)
            add(Root.COLUMN_ICON, R.mipmap.ic_launcher)
            add(Root.COLUMN_AVAILABLE_BYTES, free)
        }
        result.setNotificationUri(context.contentResolver, DocumentsContract.buildRootsUri(authority(context)))
        return result
    }

    private fun home(context: Context): Home? {
        if (!Rootfs.isReady(context)) return null
        val login = Settings.login(context)
        cache?.let { (time, key, home) ->
            if (key == login && SystemClock.elapsedRealtime() - time < CACHE_MS) return home
        }
        val home = Rootfs.access.read {
            val account = Rootfs.loginAccount(context, login)
            val path = canonical("/" + account.home.trim('/'))?.takeIf { it != "/" } ?: return@read null
            // No root is listed when the home directory does not exist (the account has none)
            runCatching { Native.browseStat(Rootfs.store(context).path, path) }.getOrNull() ?: return@read null
            Home(account.name, account.uid, account.gid, path)
        }
        cache = Triple(SystemClock.elapsedRealtime(), login, home)
        return home
    }

    /**
     * The home directory computed last: one browse queries several times, so /etc/passwd need not
     * be read every time.
     */
    @Volatile
    private var cache: Triple<Long, String, Home?>? = null

    override fun queryDocument(documentId: String, projection: Array<out String>?): Cursor {
        val context = attached()
        val path = visible(context, documentId)
        val meta = browse { Native.browseStat(it, path) }
        val texts = sniff(listOf(path to meta))
        return MatrixCursor(projection ?: DOCUMENT_COLUMNS).also { row(it, path, meta, home(context), texts) }
    }

    override fun queryChildDocuments(
        parentDocumentId: String,
        projection: Array<out String>?,
        sortOrder: String?,
    ): Cursor {
        val context = attached()
        val dir = visible(context, parentDocumentId)
        val entries = browse { Native.browseList(it, dir) }
            .map { child(dir, it.substringBefore('\t')) to it.substringAfter('\t') }
        val result = MatrixCursor(projection ?: DOCUMENT_COLUMNS)
        val home = home(context)
        val texts = sniff(entries)
        for ((path, meta) in entries) row(result, path, meta, home, texts)
        result.setNotificationUri(
            context.contentResolver,
            DocumentsContract.buildChildDocumentsUri(authority(context), parentDocumentId),
        )
        return result
    }

    override fun querySearchDocuments(rootId: String, query: String, projection: Array<out String>?): Cursor {
        val context = attached()
        requireFileAccess(context)
        val dir = when (rootId) {
            ROOT_HOME -> home(context)?.path
            ROOT_SYSTEM -> "/"
            else -> null
        } ?: throw FileNotFoundException("no root $rootId")
        val result = MatrixCursor(projection ?: DOCUMENT_COLUMNS)
        val home = home(context)
        val found = browse { Native.browseSearch(it, dir, query, SEARCH_LIMIT) }
            .map { it.substringBefore('\t') to it.substringAfter('\t') }
        val texts = sniff(found)
        for ((path, meta) in found) row(result, path, meta, home, texts)
        return result
    }

    /** ContentProvider's requireContext needs API 30. */
    private fun attached(): Context = context ?: throw IllegalStateException("the provider is not attached")

    /**
     * Returns the document ID when it is canonical and inside a root shown now; otherwise treated
     * as nonexistent.
     */
    private fun visible(context: Context, documentId: String): String {
        requireFileAccess(context)
        return canonical(documentId) ?: throw FileNotFoundException("bad document $documentId")
    }

    private fun requireFileAccess(context: Context) {
        if (!Settings.fileAccess(context)) throw FileNotFoundException("file access is disabled")
    }

    private fun <T> browse(call: (String) -> T): T {
        val store = Rootfs.store(attached()).path
        return try {
            Rootfs.access.read { call(store) }
        } catch (e: FileNotFoundException) {
            throw e
        } catch (e: IOException) {
            throw FileNotFoundException(e.message)
        }
    }

    /**
     * The regular files in [docs] (paths and metadata) whose type the extension cannot tell and
     * whose content looks like text ([Native.browseSniff], all checked with one opening of the
     * store). That makes `/etc/profile`, `.bashrc`, scripts and `Makefile` text/plain, so tapping
     * them opens an editor that can handle them.
     */
    private fun sniff(docs: List<Pair<String, String>>): Set<String> {
        val unknown = docs.filter { (path, meta) -> meta.startsWith("f\t") && guessType(displayName(path)) == null }
            .map { it.first }
        if (unknown.isEmpty()) return emptySet()
        val texts = runCatching { browse { Native.browseSniff(it, unknown.toTypedArray()) } }.getOrNull()
            ?: return emptySet()
        return unknown.filterIndexed { i, _ -> texts.getOrElse(i) { false } }.toSet()
    }

    private fun row(cursor: MatrixCursor, path: String, meta: String, home: Home?, texts: Set<String>) {
        val f = meta.split('\t')
        if (f.size < 4) return
        val dir = f[0] == "d"
        val name = displayName(path)
        val mime = when {
            dir -> Document.MIME_TYPE_DIR
            path in texts -> "text/plain"
            else -> mimeType(name)
        }
        cursor.newRow().apply {
            add(Document.COLUMN_DOCUMENT_ID, path)
            add(Document.COLUMN_DISPLAY_NAME, name)
            add(Document.COLUMN_MIME_TYPE, mime)
            add(Document.COLUMN_SIZE, if (dir) null else f[1].toLongOrNull())
            add(Document.COLUMN_LAST_MODIFIED, f[2].toLongOrNull())
            add(Document.COLUMN_FLAGS, flags(path, dir, home?.path))
        }
    }

    /**
     * What a document allows: inside the home directory, deleting, renaming and moving, writing for
     * files, creating inside for directories; the home directory itself only allows creating inside
     * it; everywhere else is read-only. Judged by the literal path; leading out through a symbolic
     * link is refused by the native library.
     */
    private fun flags(path: String, dir: Boolean, home: String?): Int {
        if (home == null) return 0
        if (path == home) return if (dir) Document.FLAG_DIR_SUPPORTS_CREATE else 0
        if (!isChildDocument(home, path)) return 0
        val common = Document.FLAG_SUPPORTS_DELETE or Document.FLAG_SUPPORTS_RENAME or Document.FLAG_SUPPORTS_MOVE
        return common or if (dir) Document.FLAG_DIR_SUPPORTS_CREATE else Document.FLAG_SUPPORTS_WRITE
    }

    override fun isChildDocument(parentDocumentId: String, documentId: String): Boolean {
        if (!Settings.fileAccess(attached())) return false
        val parent = canonical(parentDocumentId) ?: return false
        val doc = canonical(documentId) ?: return false
        return doc != parent && within(parent, doc)
    }

    /**
     * The documents from a root (or [parentDocumentId]) down to a document: the Files app needs it
     * to open a directory it is sent to (a `file:` link to one), and without it shows its default
     * place. The home root is preferred for what lies in both.
     */
    override fun findDocumentPath(parentDocumentId: String?, childDocumentId: String): DocumentsContract.Path {
        val context = attached()
        val child = visible(context, childDocumentId)
        val home = home(context)?.path
        val (rootId, top) = when {
            parentDocumentId != null -> null to visible(context, parentDocumentId)
            home != null && within(home, child) -> ROOT_HOME to home
            else -> ROOT_SYSTEM to "/"
        }
        if (!within(top, child)) throw FileNotFoundException("$childDocumentId is not under $top")
        val path = mutableListOf(child)
        while (path.last() != top) path.add(parent(path.last()))
        return DocumentsContract.Path(rootId, path.reversed())
    }

    override fun openDocument(documentId: String, mode: String, signal: CancellationSignal?): ParcelFileDescriptor {
        val context = attached()
        val flags = ParcelFileDescriptor.parseMode(mode)
        if (flags == ParcelFileDescriptor.MODE_READ_ONLY) {
            val path = visible(context, documentId)
            return ParcelFileDescriptor.adoptFd(browse { Native.browseOpen(it, path) })
        }
        val home = writableHome(context)
        val path = writable(home, documentId)
        val truncate = flags and ParcelFileDescriptor.MODE_TRUNCATE != 0
        val append = flags and ParcelFileDescriptor.MODE_APPEND != 0
        val fd = browse { Native.browseOpenWrite(it, home.path, path, truncate, append) }
        // The other side writes the host object directly, and the kernel sees neither what was
        // written nor when it is closed: hand the fd out with a close callback, and tell the
        // running kernel on close (programs watching in the guest receive IN_MODIFY and
        // IN_CLOSE_WRITE)
        return try {
            ParcelFileDescriptor.wrap(ParcelFileDescriptor.adoptFd(fd), closer) {
                runCatching { browse { Native.browseWritten(it, home.path, path) } }
            }
        } catch (e: IOException) {
            // wrap lets go of the fd before it creates the socket pair that carries the close, so
            // the fd is still open here: hand it out without the close events
            Log.w(TAG, "cannot watch $documentId for closing", e)
            ParcelFileDescriptor.adoptFd(fd)
        }
    }

    /** Close callbacks for files handed out for writing run on this thread ([openDocument]). */
    private val closer: Handler by lazy { Handler(HandlerThread("documents-close").apply { start() }.looper) }

    private fun writableHome(context: Context): Home {
        requireFileAccess(context)
        return home(context) ?: throw FileNotFoundException("no home directory")
    }

    /**
     * A document to modify: canonical and inside the home directory (with [top], also the home
     * directory itself); otherwise read-only.
     */
    private fun writable(home: Home, documentId: String, top: Boolean = false): String {
        val path = canonical(documentId) ?: throw FileNotFoundException("bad document $documentId")
        if ((top && path == home.path) || isChildDocument(home.path, path)) return path
        throw UnsupportedOperationException("$documentId is read-only")
    }

    override fun createDocument(parentDocumentId: String, mimeType: String, displayName: String): String {
        val context = attached()
        val home = writableHome(context)
        val dir = writable(home, parentDocumentId, top = true)
        val directory = mimeType == Document.MIME_TYPE_DIR
        val name = fileName(displayName, mimeType, directory)
        val created = browse { Native.browseCreate(it, home.path, dir, name, directory, home.uid, home.gid) }
        childrenChanged(context, dir)
        return child(dir, created)
    }

    override fun deleteDocument(documentId: String) {
        val context = attached()
        val home = writableHome(context)
        val path = writable(home, documentId)
        browse { Native.browseRemove(it, home.path, path) }
        revokeTree(context, documentId, path)
        childrenChanged(context, parent(path))
    }

    override fun renameDocument(documentId: String, displayName: String): String {
        val context = attached()
        val home = writableHome(context)
        val path = writable(home, documentId)
        val name = browse { Native.browseRename(it, home.path, path, fileName(displayName, "", true)) }
        val renamed = child(parent(path), name)
        if (renamed != path) revokeTree(context, documentId, path)
        childrenChanged(context, parent(path))
        return renamed
    }

    override fun moveDocument(
        sourceDocumentId: String,
        sourceParentDocumentId: String,
        targetParentDocumentId: String,
    ): String {
        val context = attached()
        val home = writableHome(context)
        val path = writable(home, sourceDocumentId)
        val target = writable(home, targetParentDocumentId, top = true)
        browse { Native.browseMove(it, home.path, path, target) }
        revokeTree(context, sourceDocumentId, path)
        childrenChanged(context, parent(path))
        childrenChanged(context, target)
        return child(target, path.substringAfterLast('/'))
    }

    /** Something in directory [dir] changed: the Files app lists it again. */
    private fun childrenChanged(context: Context, dir: String) {
        context.contentResolver.notifyChange(DocumentsContract.buildChildDocumentsUri(authority(context), dir), null)
    }

    /**
     * Document [documentId] (canonical [path]) was deleted, renamed or moved: the grants on it go,
     * and so do the persisted grants other apps hold on documents below it. The framework revokes
     * a URI together with the URIs under it by path segment, and an ID is one segment, so a grant
     * on a descendant would outlive the directory and, as IDs are paths, cover a file created at
     * the same path later. Transient grants below it cannot be listed; they end with the activity
     * that holds them.
     */
    private fun revokeTree(context: Context, documentId: String, path: String) {
        revokeDocumentPermission(documentId)
        val held = runCatching { context.contentResolver.outgoingPersistedUriPermissions }.getOrDefault(emptyList())
        grantedUnder(held.map { it.uri }, authority(context), path).forEach(::revokeDocumentPermission)
    }

    companion object {
        const val ROOT_HOME = "home"
        const val ROOT_SYSTEM = "system"

        private const val TAG = "Documents"

        /** The most results one search gives. */
        private const val SEARCH_LIMIT = 200

        private const val CACHE_MS = 5_000L

        private val ROOT_COLUMNS = arrayOf(
            Root.COLUMN_ROOT_ID, Root.COLUMN_DOCUMENT_ID, Root.COLUMN_TITLE, Root.COLUMN_SUMMARY,
            Root.COLUMN_FLAGS, Root.COLUMN_ICON, Root.COLUMN_AVAILABLE_BYTES,
        )
        private val DOCUMENT_COLUMNS = arrayOf(
            Document.COLUMN_DOCUMENT_ID, Document.COLUMN_DISPLAY_NAME, Document.COLUMN_MIME_TYPE,
            Document.COLUMN_SIZE, Document.COLUMN_LAST_MODIFIED, Document.COLUMN_FLAGS,
        )

        fun authority(context: Context) = "${context.packageName}.documents"

        /**
         * The login account, the store or file access changed: the Files app reads the
         * roots again.
         */
        fun rootsChanged(context: Context) {
            context.contentResolver.notifyChange(DocumentsContract.buildRootsUri(authority(context)), null)
        }

        /** Revoke document and tree grants, persisted and transient, across this provider. */
        fun revokeAccess(context: Context) {
            val uri = Uri.Builder().scheme("content").authority(authority(context)).build()
            context.revokeUriPermission(uri, Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION)
        }

        /**
         * The guest changed something in these directories ([Native.takeChangedDirs]): the Files
         * app lists them again if it is looking at them.
         */
        fun dirsChanged(context: Context, dirs: Array<String>) {
            if (dirs.isEmpty()) return
            val authority = authority(context)
            // One call for all of them (each notifyChange is a call into the system server)
            val uris = dirs.map { DocumentsContract.buildChildDocumentsUri(authority, it) }
            context.contentResolver.notifyChange(uris, null, 0)
        }

        /**
         * The document a `file:` URI names, for a program's OSC 8 link (`ls --hyperlink`): only on
         * this machine (no host, `localhost` or the name in `/etc/hostname`), and only where the
         * provider shows documents now, so opening one from a link reaches no more than the Files
         * app does. Reads the store: not on the main thread.
         */
        fun fileLink(context: Context, link: Uri): FileLink {
            val host = link.host.orEmpty()
            if (host.isNotEmpty() && !host.equals("localhost", ignoreCase = true) &&
                !host.equals(hostname(context), ignoreCase = true)
            ) {
                return FileLink.Remote(host)
            }
            val id = link.encodedPath?.let { linkPath(it) } ?: return FileLink.Missing
            val uri = DocumentsContract.buildDocumentUri(authority(context), id)
            // Through the provider itself, which applies what is shown now and types the file
            val mime = runCatching { context.contentResolver.getType(uri) }.getOrNull()
            if (mime != null) return FileLink.Found(uri, mime)
            val exists = runCatching {
                Rootfs.access.read { Native.browseStat(Rootfs.store(context).path, id) }
            }.isSuccess
            return if (exists) FileLink.Hidden else FileLink.Missing
        }

        /** The guest's host name as `/etc/hostname` has it; null when it cannot be read. */
        private fun hostname(context: Context): String? = runCatching {
            Rootfs.access.read { Native.readFile(Rootfs.store(context).path, "/etc/hostname") }
        }.getOrNull()?.let { String(it, Charsets.UTF_8).trim() }

        /**
         * A `file:` URI's percent-encoded path as a canonical document ID: `%XX` turned into bytes,
         * written back as the native side writes paths, with empty and `.` components dropped; null
         * when it is not absolute or has `..` (resolving it by name could differ from the guest's
         * resolution through symbolic links).
         */
        private fun linkPath(encoded: String): String? {
            if (!encoded.startsWith("/")) return null
            val parts = documentId(percentDecode(encoded)).split('/').filter { it.isNotEmpty() && it != "." }
            if (parts.any { it == ".." }) return null
            return canonical("/" + parts.joinToString("/"))
        }

        /**
         * [text] as bytes: each `%XX` is the byte it names, and everything else (a `%` not followed
         * by two hex digits included) is written as UTF-8.
         */
        private fun percentDecode(text: String): ByteArray {
            val out = ByteArrayOutputStream()
            var i = 0
            while (i < text.length) {
                val c = text[i]
                val hex = if (c == '%' && i + 3 <= text.length) text.substring(i + 1, i + 3) else ""
                if (hex.isNotEmpty() && hex.all { it in '0'..'9' || it in 'a'..'f' || it in 'A'..'F' }) {
                    out.write(hex.toInt(16))
                    i += 3
                } else {
                    val end = if (Character.isHighSurrogate(c) && i + 1 < text.length) i + 2 else i + 1
                    out.write(text.substring(i, end).toByteArray(Charsets.UTF_8))
                    i = end
                }
            }
            return out.toByteArray()
        }

        /**
         * A guest path as the native side writes it ([Native]): valid UTF-8 kept, and `%`, control
         * characters and bytes that are not UTF-8 written as `%XX`.
         */
        private fun documentId(path: ByteArray): String {
            val out = StringBuilder()
            var i = 0
            while (i < path.size) {
                val n = utf8Length(path, i)
                val c = if (n == 0) -1 else String(path, i, n, Charsets.UTF_8).codePointAt(0)
                if (c >= 0 && c != '%'.code && !Character.isISOControl(c)) {
                    out.appendCodePoint(c)
                } else {
                    for (k in i until i + maxOf(n, 1)) out.append("%%%02X".format(path[k].toInt() and 0xff))
                }
                i += maxOf(n, 1)
            }
            return out.toString()
        }

        /** The length of the well-formed UTF-8 sequence at [i] (Unicode's table 3-7), 0 when none. */
        private fun utf8Length(b: ByteArray, i: Int): Int {
            fun at(k: Int) = if (i + k < b.size) b[i + k].toInt() and 0xff else -1
            fun cont(k: Int, lo: Int = 0x80, hi: Int = 0xbf) = at(k) in lo..hi
            val b0 = at(0)
            return when {
                b0 < 0x80 -> 1
                b0 in 0xc2..0xdf -> if (cont(1)) 2 else 0
                b0 == 0xe0 -> if (cont(1, 0xa0) && cont(2)) 3 else 0
                b0 == 0xed -> if (cont(1, hi = 0x9f) && cont(2)) 3 else 0
                b0 in 0xe1..0xef -> if (cont(1) && cont(2)) 3 else 0
                b0 == 0xf0 -> if (cont(1, 0x90) && cont(2) && cont(3)) 4 else 0
                b0 in 0xf1..0xf3 -> if (cont(1) && cont(2) && cont(3)) 4 else 0
                b0 == 0xf4 -> if (cont(1, hi = 0x8f) && cont(2) && cont(3)) 4 else 0
                else -> 0
            }
        }

        /**
         * A canonical path: starts with `/`, has no empty, `.` or `..` components, and does not end
         * with `/` (except `/` itself). null when it is not.
         */
        fun canonical(path: String): String? {
            if (path == "/") return path
            if (!path.startsWith("/") || path.endsWith("/")) return null
            val parts = path.substring(1).split('/')
            if (parts.any { it.isEmpty() || it == "." || it == ".." }) return null
            return path
        }

        fun child(dir: String, name: String) = if (dir == "/") "/$name" else "$dir/$name"

        fun parent(path: String) = path.substringBeforeLast('/').ifEmpty { "/" }

        /** Whether canonical [path] is directory [dir] or lies below it. */
        fun within(dir: String, path: String) = path == dir || path.startsWith(if (dir == "/") "/" else "$dir/")

        /**
         * Of the granted URIs [uris], the document IDs of those on this provider ([authority]) that
         * name [path] or a document below it: a tree URI by its tree's document, other URIs by
         * their document ([revokeTree]).
         */
        fun grantedUnder(uris: List<Uri>, authority: String, path: String): List<String> = uris.mapNotNull { uri ->
            if (uri.authority != authority) return@mapNotNull null
            val tree = DocumentsContract.isTreeUri(uri)
            val id = runCatching {
                if (tree) DocumentsContract.getTreeDocumentId(uri) else DocumentsContract.getDocumentId(uri)
            }.getOrNull() ?: return@mapNotNull null
            val doc = canonical(id) ?: return@mapNotNull null
            id.takeIf { within(path, doc) }
        }.distinct()

        /**
         * The name used when creating and renaming: `/` and NUL become `_`, and empty, `.` and `..`
         * become `_`. When creating a file whose name's extension does not match [mime], one is
         * added as Android's own storage does ("text", text/plain → "text.txt"); not for
         * directories or application/octet-stream.
         */
        fun fileName(displayName: String, mime: String, directory: Boolean): String {
            val name = displayName.replace('/', '_').replace('\u0000', '_')
                .let { if (it.isEmpty() || it == "." || it == "..") "_" else it }
            if (directory || mime.isEmpty() || mime == "application/octet-stream") return name
            if (mimeType(name) == mime) return name
            val ext = MimeTypeMap.getSingleton().getExtensionFromMimeType(mime) ?: return name
            return "$name.$ext"
        }

        /**
         * The human-readable name of the last component of a path: `%XX` turned back into bytes and
         * decoded as UTF-8 (bytes that are not UTF-8 show as �).
         */
        fun displayName(path: String): String {
            val last = path.substringAfterLast('/').ifEmpty { return "/" }
            return String(percentDecode(last), Charsets.UTF_8)
        }

        /** Guesses the MIME type by extension; application/octet-stream when it cannot tell. */
        fun mimeType(name: String): String = guessType(name) ?: "application/octet-stream"

        /**
         * Guesses the MIME type by extension; null when it cannot tell (no extension, an unknown
         * extension). Dot files like `.bashrc` have what follows the dot taken as the extension,
         * which is usually unknown.
         */
        fun guessType(name: String): String? {
            val ext = name.substringAfterLast('.', "").lowercase()
            if (ext.isEmpty()) return null
            return MimeTypeMap.getSingleton().getMimeTypeFromExtension(ext)
        }
    }
}
