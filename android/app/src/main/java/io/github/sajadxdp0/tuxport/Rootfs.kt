package io.github.sajadxdp0.tuxport

import android.annotation.SuppressLint
import android.content.Context
import android.system.ErrnoException
import android.system.Os
import android.system.OsConstants
import android.util.Log
import androidx.core.content.edit
import io.github.sajadxdp0.tuxport.Rootfs.BASH_HOOK
import io.github.sajadxdp0.tuxport.Rootfs.RESUMES
import io.github.sajadxdp0.tuxport.Rootfs.UNMARKED_PROFILE
import io.github.sajadxdp0.tuxport.Rootfs.configure
import io.github.sajadxdp0.tuxport.Rootfs.fetch
import io.github.sajadxdp0.tuxport.Rootfs.writeProfile
import java.io.File
import java.io.FileOutputStream
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL
import java.security.MessageDigest
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.locks.ReentrantReadWriteLock
import kotlin.concurrent.write

/**
 * Importing and configuring the rootfs (the android/setup map): the first run imports the Alpine in
 * the APK, and Settings can switch to Debian (downloaded only when chosen; the version and sha256
 * are fixed here, and as with `scripts/fetch-rootfs` only a tarball whose digest matches is
 * accepted). After the import the configuration files are written, mirror speeds are probed in the
 * background, and the fastest one is written into the package manager's sources, once: from then on
 * the sources are the user's to edit.
 */
internal object Rootfs {
    private const val TAG = "Rootfs"

    /**
     * A tarball to download: `urls` are several sources of the same file; the digest is fixed, so
     * any of them is equally trustworthy.
     */
    data class Download(val urls: List<String>, val sha256: String, val size: Long)

    /**
     * The distros to choose from. `mirrors` is the built-in mirror list (the first one, the
     * official one, is used when probing fails), and `probe` is the file whose beginning is
     * downloaded when probing (relative to the mirror).
     */
    enum class Distro(
        val label: String,
        val download: Download?,
        val mirrors: List<String>,
        val probe: String,
    ) {
        ALPINE(
            "Alpine Linux ${BuildConfig.ALPINE_BRANCH.removePrefix("v")}",
            null,
            listOf(
                "https://dl-cdn.alpinelinux.org/alpine",
                "https://mirrors.tuna.tsinghua.edu.cn/alpine",
                "https://mirrors.ustc.edu.cn/alpine",
                "https://mirrors.aliyun.com/alpine",
            ),
            "${BuildConfig.ALPINE_BRANCH}/main/aarch64/APKINDEX.tar.gz",
        ),

        /**
         * `debian:trixie` built by debuerreotype (the same build the official Docker image uses),
         * pinned to one commit. GitHub is the only source, and a direct connection from China is
         * very slow; no mirror site carries this build, and we do not host it ourselves for now.
         */
        DEBIAN(
            "Debian 13",
            Download(
                listOf(
                    "https://raw.githubusercontent.com/debuerreotype/docker-debian-artifacts/" +
                        "e36653f9967231373aeba7cff2102fd004ac4f4a/trixie/oci/blobs/rootfs.tar.gz",
                ),
                "dfb68c024d5cdf4872bb7f0be8d63874aa6d0b7a0050f1b06169573e34ea543a",
                49_776_179,
            ),
            listOf(
                "deb.debian.org",
                "mirrors.tuna.tsinghua.edu.cn",
                "mirrors.ustc.edu.cn",
                "mirrors.aliyun.com",
            ),
            "debian/dists/trixie/InRelease",
        ),
    }

    /** Thrown when a download is aborted ([download]'s `cancelled` returns true). */
    class Cancelled : IOException("cancelled")

    /** The server sent more than the pinned length ([fetch]); resuming would not help. */
    private class Oversize(message: String) : IOException(message)

    /** An account in `/etc/passwd`. */
    data class Account(val name: String, val uid: Int, val gid: Int, val home: String)

    /** The first argument that makes `login.sh` only rewrite the NOPASSWD rules ([dirsChanged]). */
    private const val REFRESH_RULES = "--refresh-rules"

    /** The most bytes and time a probe downloads. */
    private const val PROBE_BYTES = 512 * 1024
    private const val PROBE_TIMEOUT_MS = 5000

    /** Download timeout: for connecting and between two reads. */
    private const val DOWNLOAD_TIMEOUT_MS = 30_000

    /** The most times a download on one source continues after breaking off. */
    private const val RESUMES = 5

    /**
     * The record (SharedPreferences): the installed distro (while there is no store, the one an
     * import was last started for).
     */
    private const val KEY_DISTRO = "distro"

    /**
     * The store's read-write lock. The kernel must be the only user of the store when it opens it;
     * only then does it do the boot cleanup (emptying `/dev/shm` and `/run`, deleting objects that
     * were not committed last time); the DocumentsProvider may open the store separately at any
     * time on binder threads, holding the read lock each time, and starting the kernel or deleting
     * the store holds the write lock.
     */
    val access = ReentrantReadWriteLock()

    fun store(context: Context): File = File(context.filesDir, "rootfs")

    /** The store has been imported and configured. */
    fun isReady(context: Context): Boolean = isReady(store(context))

    private fun isReady(store: File) = File(store, "meta.db").exists()

    private fun prefs(context: Context) = context.getSharedPreferences("rootfs", Context.MODE_PRIVATE)

    /**
     * The distro installed in the app's store, null while there is no store. Recorded at import
     * (the record [importing] reads); stores imported before round 190 have no record and count as
     * Alpine (the app had not been released yet, so only development installs have such stores).
     */
    fun installed(context: Context): Distro? = if (isReady(context)) importing(context) else null

    /**
     * The distro to import when the app's store is missing: the one an import was last started for
     * (an import or distro switch the app was killed in, [prepare]), Alpine on the first run.
     */
    fun importing(context: Context): Distro {
        val name = prefs(context).getString(KEY_DISTRO, null)
        return Distro.entries.firstOrNull { it.name == name } ?: Distro.ALPINE
    }

    /**
     * Imports [distro] into store [store] (by default the one the app uses). It is first imported
     * into a temporary directory and renamed when done, so a kill midway means starting over next
     * time, with the same distro ([importing]). The tarball to download stays in the cache
     * directory once verified ([download]) and is deleted only once the store is in place, so
     * retrying a failed or killed import needs no new download. Blocks; call it on a background
     * thread.
     */
    @SuppressLint("ApplySharedPref") // The record must be on disk before the import starts
    @Throws(IOException::class)
    fun prepare(
        context: Context,
        distro: Distro,
        progress: (Long, Long) -> Unit = { _, _ -> },
        store: File = store(context),
    ) {
        if (isReady(store)) return
        // The next launch resumes this distro if the app is killed before the store is in place,
        // and once the store is there, the record is its own. Only the app's own store has one (the
        // instrumentation tests import into others)
        if (store == store(context)) prefs(context).edit(commit = true) { putString(KEY_DISTRO, distro.name) }
        val tmp = File(store.parentFile, "${store.name}.tmp")
        tmp.deleteRecursively()
        val tarball = tarball(context, distro)
        if (distro.download == null) {
            context.assets.open("rootfs.tgz").use { input ->
                tarball.outputStream().use { input.copyTo(it) }
            }
        } else {
            download(context, distro, progress)
        }
        Native.importRootfs(tmp.path, tarball.path)
        configure(context, tmp.path, distro)
        // Sync before the rename: otherwise after a power loss the store could be in place while
        // its files are empty; after the rename, sync the directory too, so what is later written
        // into the store does not fall back into rootfs.tmp on a power loss and get deleted as an
        // unfinished import
        Native.syncRootfs(tmp.path)
        if (!tmp.renameTo(store)) throw IOException("cannot move the rootfs into place")
        syncDir(store.parentFile!!)
        tarball.delete()
    }

    private fun tarball(context: Context, distro: Distro) = File(context.cacheDir, "${distro.name.lowercase()}.tar.gz")

    @Throws(IOException::class)
    private fun configure(context: Context, store: String, distro: Distro) {
        val (sources, content) = sources(distro, distro.mirrors[0])
        write(store, sources, content)
        write(store, "/etc/hostname", "localhost\n")
        if (distro == Distro.ALPINE) {
            write(store, "/etc/hosts", "127.0.0.1\tlocalhost\n::1\tlocalhost\n", overwrite = false)
            useBash(context, store)
            cachePackages(context, store, "procps")
            cachePackages(context, store, "bubblewrap")
        } else {
            // Debian's has no hosts, and resolv.conf points at 1.1.1.1. DNS must go through the
            // kernel layer's stub (the kernel/net map) for the system's Private DNS and VPNs to
            // take effect
            write(store, "/etc/hosts", "127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n")
            write(store, "/etc/resolv.conf", "nameserver 127.0.0.53\n")
        }
        writeProfile(context, store, distro)
        val (packages, install) = when (distro) {
            Distro.ALPINE -> "apk" to "apk add"
            Distro.DEBIAN -> "apt" to "apt install"
        }
        write(
            store, "/etc/motd",
            """
            Welcome to Tuxport!
            ${distro.label}, running on Android.

            Working with packages:
            - Update index:     $packages update
            - Search packages:  $packages search <package>
            - Install packages: $install <package>
            - Show help:        $packages --help

            For example, install Codex:
            $packages update
            $install nodejs npm
            npm install -g @openai/codex

            Report issues at:
            https://github.com/sajadxdp0/tuxport/issues
            """.trimIndent(),
        )
    }

    /** The package manager's sources: which file they are written in and what the content is. */
    private fun sources(distro: Distro, mirror: String): Pair<String, String> = when (distro) {
        Distro.ALPINE -> "/etc/apk/repositories" to
            "$mirror/${BuildConfig.ALPINE_BRANCH}/main\n$mirror/${BuildConfig.ALPINE_BRANCH}/community\n"
        // The rootfs has no ca-certificates, so apt uses http (the packages are signed)
        Distro.DEBIAN -> "/etc/apt/sources.list.d/debian.sources" to
            """
            Types: deb
            URIs: http://$mirror/debian
            Suites: trixie trixie-updates
            Components: main
            Signed-By: /usr/share/keyrings/debian-archive-keyring.pgp

            Types: deb
            URIs: http://$mirror/debian-security
            Suites: trixie-security
            Components: main
            Signed-By: /usr/share/keyrings/debian-archive-keyring.pgp

            """.trimIndent()
    }

    /** Stages a bundled package set only at import; login removes it after a successful install. */
    @Throws(IOException::class)
    private fun cachePackages(context: Context, store: String, bundle: String): Boolean {
        val packages = context.assets.list(bundle).orEmpty()
        for (name in packages) {
            val data = context.assets.open("$bundle/$name").use { it.readBytes() }
            write(store, "/var/cache/tuxport/$bundle/$name", data)
        }
        return packages.isNotEmpty()
    }

    /**
     * Makes bash the login shell of a system imported from Alpine instead of busybox ash: puts the
     * packages bundled with the app (bash and the libraries it needs that the minirootfs lacks) into
     * `/var/cache/tuxport/bash/`, from which the login script installs them offline at the first
     * login (using `/bin/sh` until they are installed), and has root's entry in `/etc/passwd` name
     * `/bin/bash`. Accounts made later with busybox's `adduser` get bash too, as it gives them the
     * shell of the account running it (`$SHELL`, which the login script sets). Systems imported
     * before keep their shells.
     */
    @Throws(IOException::class)
    private fun useBash(context: Context, store: String) {
        if (!cachePackages(context, store, "bash")) return
        val passwd = Native.readFile(store, "/etc/passwd") ?: return
        val lines = String(passwd).split('\n').map { line ->
            val fields = line.split(':')
            if (fields.size == 7 && fields[0] == "root" && fields[6] == "/bin/sh") {
                (fields.dropLast(1) + "/bin/bash").joinToString(":")
            } else {
                line
            }
        }
        write(store, "/etc/passwd", lines.joinToString("\n"))
    }

    /**
     * `/etc/profile.d/tuxport.sh`, the assets' `profile.sh`: shows the motd, sets the terminal and
     * language environment variables, and gives bash, busybox ash and zsh a coloured prompt with
     * shell integration, the OSC 133 marks the terminal finds prompts, commands and their output by (the
     * android/vtcore map). ash's marks are in `/etc/profile.d/tuxport.ashrc`, the assets' `ashrc.sh`,
     * which the profile names in `ENV`, so that every interactive ash reads it (a login shell after
     * `~/.profile`); bash's in `/etc/profile.d/tuxport.bashrc`, the assets' `bashrc.sh`, which the
     * profile reads, and on Alpine every interactive bash through `/etc/bash/tuxport.sh`
     * ([BASH_HOOK]); zsh's in `/etc/profile.d/tuxport.zshrc`, the assets' `zshrc.sh`, read by the
     * profile. They are written first, as the profile uses them only when they are there, and
     * their names do not end in `.sh`, so `/etc/profile` does not read them. Which directory the shell starts in is decided by the assets'
     * `login.sh` (restored tabs start in their original directory, the android/service map, new
     * tabs in the current tab's, the android/terminal map).
     */
    @Throws(IOException::class)
    private fun writeProfile(context: Context, store: String, distro: Distro) {
        write(store, "/etc/profile.d/tuxport.ashrc", asset(context, "ashrc.sh"))
        write(store, "/etc/profile.d/tuxport.bashrc", asset(context, "bashrc.sh"))
        write(store, "/etc/profile.d/tuxport.zshrc", asset(context, "zshrc.sh"))
        if (distro == Distro.ALPINE) write(store, "/etc/bash/tuxport.sh", BASH_HOOK)
        write(store, "/etc/profile.d/tuxport.sh", asset(context, "profile.sh"))
    }

    /**
     * `/etc/bash/tuxport.sh` on Alpine, whose `/etc/bash/bashrc` reads every `.sh` file in
     * `/etc/bash` in every interactive bash (a login one too, through
     * `/etc/profile.d/00-bashrc.sh`): bash's prompt
     * marks for a nested bash, which reads no profile. Debian's `/etc/bash.bashrc` reads no such
     * directory, so a nested bash there has no marks.
     */
    private const val BASH_HOOK = """# Written by Tuxport beside /etc/profile.d/tuxport.sh, and replaced together with it: Alpine's
# /etc/bash/bashrc reads this file in every interactive bash, so a nested bash (bash, su without
# -l) marks its prompts too.
if [ -r /etc/profile.d/tuxport.bashrc ]; then . /etc/profile.d/tuxport.bashrc; fi
"""

    private fun asset(context: Context, name: String): String =
        context.assets.open(name).bufferedReader().readText()

    private fun write(store: String, path: String, text: String, overwrite: Boolean = true) =
        write(store, path, text.toByteArray(), overwrite)

    private fun write(store: String, path: String, data: ByteArray, overwrite: Boolean = true) =
        Native.writeFile(store, path, data, 420 /* 0644 */, overwrite)

    /** Makes renames and deletions in directory [dir] durable. */
    private fun syncDir(dir: File) {
        try {
            val fd = Os.open(dir.path, OsConstants.O_RDONLY, 0)
            try {
                Os.fsync(fd)
            } finally {
                Os.close(fd)
            }
        } catch (e: ErrnoException) {
            Log.w(TAG, "cannot sync ${dir.path}", e)
        }
    }

    /**
     * Probes the mirrors of [distro] concurrently and puts the fastest into the package manager's
     * sources ([useMirror]); when every probe fails, nothing changes. Run once, right after an
     * import. Blocks for a few seconds; call it on a background thread. Debian's mirrors are
     * written as host names only, and probed with https.
     */
    fun chooseMirror(context: Context, distro: Distro) {
        val base = { mirror: String -> if (distro == Distro.ALPINE) mirror else "https://$mirror" }
        val speeds = distro.mirrors.zip(probeAll(distro.mirrors.map { "${base(it)}/${distro.probe}" }))
        val best = speeds.filter { it.second != null }.maxByOrNull { it.second!! }?.first ?: return
        Log.i(TAG, "mirror speeds: $speeds, using $best")
        runCatching { useMirror(context, distro, best) }
            .onSuccess { if (!it) Log.i(TAG, "the package sources are not as imported, leaving them alone") }
            .onFailure { Log.w(TAG, "cannot write the package sources", it) }
    }

    /**
     * Makes [mirror] the one in the package manager's sources of [distro], only while the sources
     * file is still exactly what the import wrote ([configure], with the official mirror): once it
     * was changed, by this or by the user, it is left alone, and false is returned. It can be
     * written while the kernel runs too: the store's commit counter changes and the kernel layer's
     * caches are invalidated with it (the kernel/store map).
     */
    @Throws(IOException::class)
    fun useMirror(context: Context, distro: Distro, mirror: String): Boolean {
        val store = store(context).path
        val official = distro.mirrors[0]
        val (path, imported) = sources(distro, official)
        val old = Native.readFile(store, path) ?: return false
        if (String(old) != imported) return false
        if (mirror != official) write(store, path, sources(distro, mirror).second)
        return true
    }

    /**
     * [urls] sorted by measured speed from fast to slow; those that could not be measured follow in
     * their original order. Not probed when there is only one.
     */
    private fun fastest(urls: List<String>): List<String> {
        if (urls.size < 2) return urls
        val speeds = probeAll(urls)
        Log.i(TAG, "download speeds: ${urls.zip(speeds)}")
        return urls.indices.sortedByDescending { speeds[it] ?: -1.0 }.map { urls[it] }
    }

    /**
     * Probes [urls] concurrently and returns each one's speed (bytes per ms), null for failures.
     * Blocks for a few seconds.
     */
    private fun probeAll(urls: List<String>): List<Double?> {
        val pool = Executors.newFixedThreadPool(urls.size)
        val results = urls.map { url -> pool.submit<Double?> { probe(url) } }
        val speeds = results.map {
            runCatching { it.get(PROBE_TIMEOUT_MS * 2L, TimeUnit.MILLISECONDS) }.getOrNull()
        }
        pool.shutdownNow()
        return speeds
    }

    /**
     * Downloads the beginning of [address] and returns the speed (bytes per ms, including
     * connection setup); null when not a single byte arrived. Compared by throughput rather than by
     * the time to finish a small piece: some mirrors are fast for a small piece at the start and
     * are throttled after that.
     */
    private fun probe(address: String): Double? = runCatching {
        val url = URL(address)
        val start = System.nanoTime()
        val elapsed = { (System.nanoTime() - start) / 1_000_000 }
        val conn = url.openConnection() as HttpURLConnection
        conn.connectTimeout = PROBE_TIMEOUT_MS
        conn.readTimeout = PROBE_TIMEOUT_MS
        conn.setRequestProperty("Range", "bytes=0-${PROBE_BYTES - 1}")
        try {
            if (conn.responseCode !in 200..299) return@runCatching null
            val buf = ByteArray(16 * 1024)
            var total = 0
            conn.inputStream.use { input ->
                while (total < PROBE_BYTES && elapsed() < PROBE_TIMEOUT_MS) {
                    val n = input.read(buf)
                    if (n < 0) break
                    total += n
                }
            }
            if (total == 0) null else total.toDouble() / elapsed().coerceAtLeast(1)
        } finally {
            conn.disconnect()
        }
    }.getOrNull()

    /**
     * An `/etc/network/interfaces` with only the loopback interface (written before every kernel
     * start, only when missing; one the user changed is left alone), the same as setup-alpine
     * writes. The network is the host's (the kernel/net map); services that need the network (nginx
     * and so on) have OpenRC bring up networking, which fails without this file, and then the
     * services do not start either. `rc_provide` in rc.conf is not used: it is added to every
     * service, and providing `clock` as well makes a cycle. Without OpenRC and ifupdown nobody
     * reads this file.
     */
    @Throws(IOException::class)
    fun ensureNetworkConfig(context: Context) = write(
        store(context).path, "/etc/network/interfaces",
        "auto lo\niface lo inet loopback\n",
        overwrite = false,
    )

    /**
     * The network rootless podman gives its containers by default, written before every kernel
     * start when that file is missing: the host's. Tuxport has no TUN device, so pasta and
     * slirp4netns, the networks podman makes for an ordinary user, cannot start, and every
     * `podman run` without `--network host` would fail (the kernel/netns map).
     * `containers.rootless.conf.d` is read only for rootless podman, so containers run as root
     * keep their bridge networks; a user's own `~/.config/containers/containers.conf` overrides it.
     * podman's overlay storage driver and docker's overlayfs snapshotter, the packages' defaults,
     * run on the kernel's overlayfs.
     */
    @Throws(IOException::class)
    fun ensureContainersConfig(context: Context) {
        val store = store(context).path
        write(store, "/etc/containers/containers.rootless.conf.d/tuxport.conf", ROOTLESS_CONTAINERS, overwrite = false)
    }

    private const val ROOTLESS_CONTAINERS = """# Written by Tuxport when missing: Tuxport has no TUN device, so pasta and slirp4netns (the
# networks podman makes for an ordinary user) cannot start, and rootless containers use the
# host's network instead: they are not isolated from the host or from each other, and port
# mappings (-p) are dropped. Containers run as root keep their bridge networks, port mappings and
# lookups by container name, so run podman as root for those (sudo podman; doas podman on Alpine).
# Root gives up little here: every user, root included, runs inside the Android app's sandbox.
# To change the default, edit this file or set netns in ~/.config/containers/containers.conf.
[containers]
netns = "host"
"""

    /** Refreshes the bridge before kernel start, preserving user-provided local launchers. */
    @Throws(IOException::class)
    fun ensureBrowser(context: Context, configuration: String, store: String = store(context).path) {
        val binary = context.assets.open("tuxport-open-url").use { it.readBytes() }
        Native.writeFile(store, "/usr/local/libexec/tuxport-open-url", binary, 493 /* 0755 */, true)
        Native.writeFile(store, BrowserBridge.CONFIG_PATH, configuration.toByteArray(), 420 /* 0644 */, true)
        val path = "/usr/local/bin/xdg-open"
        val old = try { Native.readFile(store, path) } catch (_: IOException) { null }
        // Replace the previous Tuxport OSC helper, but leave a user's own launcher alone.
        val replace = old != null && hex(MessageDigest.getInstance("SHA-256").digest(old)) ==
            "abaf8d811e48df794b931f2fd5876e255c2c430464081e0d9fe880b80a3a1d76"
        Native.writeFile(store, path, asset(context, "xdg-open.sh").toByteArray(), 493 /* 0755 */, replace)
    }

    /**
     * On Alpine, puts the doas package bundled with the app into the store
     * (`/var/cache/tuxport/doas.apk`): the login script installs it offline when a regular user
     * first becomes the login account (the android/setup map). Written before every kernel start,
     * so it is replaced by the new one after an app upgrade.
     */
    @Throws(IOException::class)
    fun ensureLoginTools(context: Context) {
        if (installed(context) != Distro.ALPINE) return
        val data = context.assets.open("doas.apk").use { it.readBytes() }
        write(store(context).path, "/var/cache/tuxport/doas.apk", data)
    }

    /**
     * The copy written from round 194 until shell integration came: the coloured prompt for any
     * shell (zsh and dash showed its bash escapes as they are), without prompt marks.
     */
    private val UNMARKED_PROFILE = """
        cat /etc/motd 2>/dev/null
        export TERM=xterm-256color
        export LANG=C.UTF-8
        export NMAP_UNPRIVILEGED=1
        PS1='\[\e[1;32m\]\u@\h\[\e[0m\]:\[\e[1;34m\]\w\[\e[0m\]\$ '

    """.trimIndent()

    /**
     * The copy written before round 194: [UNMARKED_PROFILE] with one more line, `cd "$HOME"`, which
     * took restored tabs back to the home directory.
     */
    private val OLD_PROFILE = UNMARKED_PROFILE.trimEnd('\n') + "\ncd \"${'$'}HOME\" 2>/dev/null\n"

    /**
     * The sha256s of the assets' `profile.sh` as later versions replaced it, whose texts (shell
     * code full of `$`) are not kept here: the first with prompt marks, until taps on the prompt
     * moved the cursor, the one that marked them for bash and busybox ash only, until zsh got
     * its marks, the one with ash's marks in itself, until `ashrc.sh` took them, and the one with
     * bash's and zsh's marks in itself, until `bashrc.sh` and `zshrc.sh` took them. A change to
     * `profile.sh` adds the sha256 of the text it replaces
     * (`git show HEAD:android/app/src/main/assets/profile.sh | shasum -a 256` before committing).
     */
    private val REPLACED_PROFILES = setOf(
        "4a35d44e55e23aaf94543d87dc02deafcc7b324902e90ae18872fe20242642fc",
        "d4daedd69558e1efe41b143f71b74b67708388299584706a9bf55052a7fc3f21",
        "b6dbef9f24826d036570704f8ed15439375032f0f72163e3c6cd219ef5e83336",
        "86785a156574627210ded386f59ca574cb69292672688287c7b7dc7c8e7a4233",
        "747208df567cbf62252e21522712b96acb79564eafa8062ce08a1b159e1e4835",
        "2085a60332217920354a7979b98becad8a114e413724c1180954e3424d7157c5",
        "921a9cbb766e8427950aa5ff7a55e253f7e62b3b25997fcad6410beabb62643c",
        "3257013eafeb90230ae0d6772e73d8af87be62f8d6f74460dd4189ea56271021",
    )

    /**
     * Replaces the profile the app wrote into a previously imported store with the current one
     * ([writeProfile], with the files of ash's, bash's and zsh's marks), if it is an earlier version left
     * unchanged. A change to `ashrc.sh`, `bashrc.sh` or `zshrc.sh` alone reaches no existing store: change
     * `profile.sh` with it.
     */
    @Throws(IOException::class)
    fun updateProfile(context: Context) {
        val store = store(context).path
        val path = "/etc/profile.d/tuxport.sh"
        val old = runCatching { Native.readFile(store, path) }.getOrNull() ?: return
        val earlier = String(old) in listOf(OLD_PROFILE, UNMARKED_PROFILE) ||
            hex(MessageDigest.getInstance("SHA-256").digest(old)) in REPLACED_PROFILES
        if (earlier) writeProfile(context, store, installed(context) ?: Distro.ALPINE)
    }

    /**
     * Run in the guest before every boot (`sh -c`): when OpenRC is installed and none of
     * hwclock/osclock/swclock is in any runlevel, adds osclock to boot. busybox's syslog does
     * `need clock`, and OpenRC provides clock with hwclock by default, which cannot read an RTC or
     * set the host's time, so the boot log always has a failure; osclock means "the operating
     * system manages the time" and does nothing (the kernel/process map). Whichever clock service
     * the user put in place is left alone.
     */
    const val PREFER_OSCLOCK = "[ -x /sbin/rc-update ] && [ -e /etc/init.d/osclock ] || exit 0; " +
        "for s in hwclock osclock swclock; do for l in /etc/runlevels/*/\$s; do " +
        "[ -e \"\$l\" ] || [ -L \"\$l\" ] && exit 0; done; done; " +
        "exec /sbin/rc-update -q add osclock boot >/dev/null 2>&1"

    /**
     * The services that did not start at this boot: OpenRC writes
     * ` * ERROR: <service> failed to start` into `/var/log/boot.log` (the kernel empties this file
     * at every boot, the kernel/process map). Empty without OpenRC or when it cannot be read.
     */
    fun failedServices(context: Context): List<String> {
        val log = runCatching { Native.readFile(store(context).path, "/var/log/boot.log") }.getOrNull()
            ?: return emptyList()
        return Regex("""ERROR: (\S+) failed to start""").findAll(String(log)).map { it.groupValues[1] }.distinct().toList()
    }

    /**
     * Which account the login setting [login] ([ConsolePty.LOGIN_AUTO], [ConsolePty.LOGIN_ROOT] or
     * an account name) actually is: the rules are the same as in `login.sh`; automatic is the first
     * regular user created, or root when there is none, and a named account that no longer exists
     * is root too. When `/etc/passwd` cannot be read, it is root with home directory `/root`.
     */
    fun loginAccount(context: Context, login: String): Account {
        val all = accounts(context).map { it.first }
        val root = all.firstOrNull { it.name == ConsolePty.LOGIN_ROOT } ?: Account(ConsolePty.LOGIN_ROOT, 0, 0, "/root")
        return when (login) {
            ConsolePty.LOGIN_AUTO -> regularAccounts(context).firstOrNull() ?: root
            ConsolePty.LOGIN_ROOT -> root
            else -> all.firstOrNull { it.name == login } ?: root
        }
    }

    /**
     * The regular users that can be the login account: UID 1000–65533 with a login shell (not
     * nologin or false), sorted by UID; the first one is the one logged in "automatically". The
     * rules are the same as in the assets' `login.sh`. Empty when it cannot be read.
     */
    fun regularAccounts(context: Context): List<Account> = accounts(context).mapNotNull { (account, shell) ->
        if (shell.isEmpty() || shell.endsWith("/nologin") || shell.endsWith("/false")) return@mapNotNull null
        if (account.uid !in 1000..65533) return@mapNotNull null
        account
    }.sortedBy { it.uid }

    /** The accounts in `/etc/passwd`, empty when it cannot be read. */
    private fun accounts(context: Context): List<Pair<Account, String>> {
        val passwd = runCatching { Native.readFile(store(context).path, "/etc/passwd") }.getOrNull()
            ?: return emptyList()
        return String(passwd).lines().mapNotNull { line ->
            val f = line.split(':')
            if (f.size < 7) return@mapNotNull null
            val uid = f[2].toIntOrNull() ?: return@mapNotNull null
            val gid = f[3].toIntOrNull() ?: return@mapNotNull null
            Account(f[0], uid, gid, f[5]) to f[6]
        }
    }

    /**
     * The guest changed the entries of [dirs] ([Native.takeChangedDirs], polled by the service):
     * when `/etc` is among them (passwd, chpasswd and adduser replace `/etc/shadow`, addgroup and
     * deluser `/etc/group`), the login's NOPASSWD rules are rewritten from the passwords and groups
     * now, by `login.sh --refresh-rules` run as root on the running [machine], so a password set in
     * a running session counts without a new tab. Returns the refresh's pid (it is not waited for:
     * the guest's init reaps it), or -1 when nothing needed refreshing.
     */
    fun dirsChanged(context: Context, machine: Long, dirs: Array<String>): Int {
        if ("/etc" !in dirs) return -1
        val argv = ConsolePty.argv(ConsolePty.script(context), REFRESH_RULES)
        return Native.spawn(machine, -1, "/bin/sh", argv, ConsolePty.ENVIRONMENT)
    }

    /**
     * Downloads the tarball of [distro] into the cache directory, unless a verified one is already
     * in the cache. While downloading, [progress] receives the bytes downloaded so far and in
     * total; when [cancelled] returns true the download aborts, throwing [Cancelled]. Blocks; call
     * it on a background thread.
     */
    @Throws(IOException::class)
    fun download(
        context: Context,
        distro: Distro,
        progress: (Long, Long) -> Unit = { _, _ -> },
        cancelled: () -> Boolean = { false },
    ) {
        val download = distro.download ?: return
        val tarball = tarball(context, distro)
        if (tarball.length() == download.size && sha256(tarball) == download.sha256) return
        fetch(download, tarball, cancelled, progress)
    }

    /**
     * Downloads [download] to [dest]: with several sources their speed is probed first, and they
     * are tried from the fastest, moving to the next one when one fails.
     * The sha256 is computed while writing, and on a mismatch (or a wrong length) the file is
     * deleted and an exception thrown; it is written to `.part` first and renamed only once
     * verified.
     * When [cancelled] returns true no other source is tried, and [Cancelled] is thrown.
     */
    @Throws(IOException::class)
    fun fetch(
        download: Download,
        dest: File,
        cancelled: () -> Boolean = { false },
        progress: (Long, Long) -> Unit,
    ) {
        var error: IOException? = null
        for (url in fastest(download.urls)) {
            if (cancelled()) throw Cancelled()
            try {
                Log.i(TAG, "downloading $url")
                return fetch(url, download, dest, progress, cancelled)
            } catch (e: Cancelled) {
                throw e
            } catch (e: IOException) {
                Log.w(TAG, "cannot download $url", e)
                error = e
            }
        }
        throw error ?: IOException("nowhere to download from")
    }

    /**
     * Downloads from [url]. When a timeout or a dropped connection interrupts it, it continues from
     * where it broke off with `Range` (counted only when this attempt got data, at most [RESUMES]
     * times); when the server does not support resuming (it replies 200), it starts over. An attempt
     * that got no data ends this source, and so does an error status on a resume.
     */
    @Throws(IOException::class)
    private fun fetch(
        url: String,
        download: Download,
        dest: File,
        progress: (Long, Long) -> Unit,
        cancelled: () -> Boolean,
    ) {
        val part = File(dest.path + ".part")
        var digest = MessageDigest.getInstance("SHA-256")
        var done = 0L
        var reported = 0L
        var resumes = 0
        try {
            while (true) {
                val conn = URL(url).openConnection()
                conn.connectTimeout = DOWNLOAD_TIMEOUT_MS
                conn.readTimeout = DOWNLOAD_TIMEOUT_MS
                val http = conn as? HttpURLConnection
                if (done > 0) conn.setRequestProperty("Range", "bytes=$done-")
                val before = done
                try {
                    if (done > 0) when (val code = http?.responseCode) {
                        HttpURLConnection.HTTP_PARTIAL -> {}
                        HttpURLConnection.HTTP_OK -> {
                            done = 0
                            reported = 0
                            digest = MessageDigest.getInstance("SHA-256")
                        }
                        // Keeps what it has, so this counts as an attempt that got no data
                        else -> throw IOException("$url: HTTP $code on resuming at $done bytes")
                    }
                    conn.getInputStream().use { input ->
                        FileOutputStream(part, done > 0).use { output ->
                            val buf = ByteArray(64 * 1024)
                            while (true) {
                                if (cancelled()) throw Cancelled()
                                val n = input.read(buf)
                                if (n < 0) break
                                // A server sending more than the pinned length is wrong whatever
                                // follows: stop rather than fill the cache
                                if (done + n > download.size) throw Oversize("$url: more than ${download.size} bytes")
                                digest.update(buf, 0, n)
                                output.write(buf, 0, n)
                                done += n
                                // Progress is reported about two hundred times at most
                                if (done - reported >= download.size / 200 || done == download.size) {
                                    reported = done
                                    progress(done, download.size)
                                }
                            }
                        }
                    }
                    break
                } catch (e: IOException) {
                    if (e is Cancelled || e is Oversize || http == null || done == before || ++resumes > RESUMES) throw e
                    // Everything arrived before it broke off: there is nothing left to ask for (a
                    // Range starting at the end is answered 416), only to verify
                    if (done == download.size) break
                    Log.w(TAG, "download of $url broke off at $done bytes, resuming", e)
                } finally {
                    http?.disconnect()
                }
            }
            val got = hex(digest.digest())
            if (done != download.size || got != download.sha256) {
                throw IOException("$url: got $done bytes with sha256 $got")
            }
            if (!part.renameTo(dest)) throw IOException("cannot move the download into place")
        } finally {
            part.delete()
        }
    }

    private fun sha256(file: File): String {
        val digest = MessageDigest.getInstance("SHA-256")
        file.inputStream().use { input ->
            val buf = ByteArray(64 * 1024)
            while (true) {
                val n = input.read(buf)
                if (n < 0) break
                digest.update(buf, 0, n)
            }
        }
        return hex(digest.digest())
    }

    private fun hex(bytes: ByteArray) = bytes.joinToString("") { "%02x".format(it) }

    /** Deletes the app's store (distro switch, rootfs reset). The kernel must be stopped first. */
    fun reset(context: Context) = access.write {
        File(context.filesDir, "rootfs.tmp").deleteRecursively()
        if (!store(context).deleteRecursively()) Log.w(TAG, "cannot delete all of the rootfs")
    }
}
