package io.github.sajadxdp0.tuxport

import android.content.ContentValues
import android.content.Context
import android.net.Uri
import android.provider.MediaStore
import org.json.JSONObject
import java.io.File
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL

/**
 * The update check of the settings screen: the project's newest release on GitHub and its arm64
 * package, fetched through the same [Rootfs.fetch] engine the distro downloads use (resumed,
 * size-checked and sha256-verified against the digest the release API reports). Nothing runs on
 * its own: the user's tap is the only thing that contacts GitHub, and it asks for the release
 * list only; the package then goes to the system's Downloads folder, where the user opens it to
 * install.
 */
internal object Updater {

    /** The newest release: its version (the tag without "v") and its arm64 package. */
    class Release(val version: String, val name: String, val download: Rootfs.Download)

    /**
     * Reads the newest release and its package named `tuxport-<version>-arm64-release.apk`.
     * Blocking; call from a worker thread. [IOException] when the reply is missing either, as a
     * release without a verified package is not one this app can offer.
     */
    @Throws(IOException::class)
    fun latest(): Release {
        val connection = URL(LATEST).openConnection() as HttpURLConnection
        try {
            // GitHub's API asks for a user agent and offers JSON both ways
            connection.setRequestProperty("User-Agent", "Tuxport ${BuildConfig.VERSION_NAME}")
            connection.setRequestProperty("Accept", "application/vnd.github+json")
            connection.connectTimeout = CHECK_TIMEOUT_MS
            connection.readTimeout = CHECK_TIMEOUT_MS
            val code = connection.responseCode
            if (code != HttpURLConnection.HTTP_OK) throw IOException("GitHub answered HTTP $code")
            val root = JSONObject(connection.inputStream.bufferedReader().use { it.readText() })
            val version = root.getString("tag_name").removePrefix("v")
            val assets = root.getJSONArray("assets")
            for (i in 0 until assets.length()) {
                val json = assets.getJSONObject(i)
                val name = json.getString("name")
                if (!name.endsWith("-arm64-release.apk")) continue
                val digest = json.getString("digest")
                if (!digest.startsWith("sha256:")) throw IOException("$name has no sha256 digest")
                val urls = listOf(json.getString("browser_download_url"))
                return Release(version, name, Rootfs.Download(urls, digest.removePrefix("sha256:"), json.getLong("size")))
            }
            throw IOException("release v$version has no arm64 package")
        } finally {
            connection.disconnect()
        }
    }

    /**
     * True when [version] is a newer dotted-number version than [installed], both with an
     * optional leading "v". A part that is not a number answers false: a tag the app cannot
     * compare is not one it can claim to beat.
     */
    fun newer(version: String, installed: String): Boolean {
        val one = parts(version) ?: return false
        val two = parts(installed) ?: return false
        for (i in 0 until maxOf(one.size, two.size)) {
            val a = one.getOrNull(i) ?: 0
            val b = two.getOrNull(i) ?: 0
            if (a != b) return a > b
        }
        return false
    }

    private fun parts(version: String) =
        version.removePrefix("v").split('.').map { it.toIntOrNull() }.takeIf { it.all { n -> n != null } }

    /**
     * Copies [packageFile] (already verified by [Rootfs.fetch]) into the system's Downloads
     * folder as [name] and deletes it, returning where the copy landed. The row is pending
     * until it is complete, so no half package is ever offered.
     */
    @Throws(IOException::class)
    fun intoDownloads(context: Context, packageFile: File, name: String): Uri {
        val resolver = context.contentResolver
        val values = ContentValues().apply {
            put(MediaStore.MediaColumns.DISPLAY_NAME, name)
            put(MediaStore.MediaColumns.MIME_TYPE, PACKAGE_MIME)
            put(MediaStore.MediaColumns.RELATIVE_PATH, "Download")
            put(MediaStore.MediaColumns.IS_PENDING, 1)
        }
        val uri = resolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values)
            ?: throw IOException("the Downloads provider gave no file")
        try {
            resolver.openOutputStream(uri)?.use { output -> packageFile.inputStream().use { it.copyTo(output) } }
                ?: throw IOException("the Downloads provider gave no stream")
            values.clear()
            values.put(MediaStore.MediaColumns.IS_PENDING, 0)
            resolver.update(uri, values, null, null)
        } catch (e: IOException) {
            resolver.delete(uri, null, null)
            throw e
        } finally {
            packageFile.delete()
        }
        return uri
    }

    private const val LATEST = "https://api.github.com/repos/sajadxdp0/tuxport/releases/latest"
    private const val CHECK_TIMEOUT_MS = 15_000
    private const val PACKAGE_MIME = "application/vnd.android.package-archive"
}
