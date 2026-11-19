import java.util.Properties

plugins {
    id("com.android.application")
}

/** Repository root: the Cargo workspace and the rootfs from `scripts/fetch-rootfs` are here. */
val repoRoot: Directory = rootProject.layout.projectDirectory.dir("..")

/** Both Zig builds use the local toolchain override before the environment and PATH. */
val zigExecutable = providers.fileContents(rootProject.layout.projectDirectory.file("local.properties"))
    .asText.orElse("")
    .map { text ->
        Properties().apply { load(text.reader()) }
            .getProperty("zig.path")?.trim()?.takeIf { it.isNotEmpty() }
    }
    .orElse(providers.environmentVariable("ZIG"))
    .orElse("zig")

/** Orders file names by the numbers in them, so that 3.10 comes after 3.9 and r10 after r9. */
val byVersion: Comparator<File> = Comparator { a, b ->
    val numbers = { f: File -> Regex("""\d+""").findAll(f.name).map { it.value.toLong() }.toList() }
    val (x, y) = numbers(a) to numbers(b)
    x.zip(y).map { (p, q) -> p.compareTo(q) }.firstOrNull { it != 0 } ?: x.size.compareTo(y.size)
}

/** The newest of the files `scripts/fetch-rootfs` downloaded and verified whose name matches. */
fun newestFetched(name: Regex): File? = repoRoot.dir("target/rootfs").asFile
    .listFiles { f -> f.name.matches(name) }
    ?.maxWithOrNull(byVersion)

/** The newest package for each name, or null when any package is missing. */
fun fetchedPackages(vararg names: String): List<File>? = names
    .map { newestFetched(Regex("""$it-\d.*\.apk""")) }
    .takeIf { null !in it }
    ?.filterNotNull()

/** Alpine minirootfs tarball. */
val rootfsTarball: File? = newestFetched(Regex("""alpine-minirootfs-.*-aarch64\.tar\.gz"""))

/**
 * The doas package bundled with the app, installed offline when a regular user first becomes the
 * login account.
 */
val doasPackage: File? = newestFetched(Regex("""doas-.*\.apk"""))

/**
 * bash and the libraries it needs that the minirootfs lacks, bundled with the app and installed
 * offline at the first login of a system the app imported, whose login shell bash is; null when
 * any of them is missing.
 */
val bashPackages: List<File>? = fetchedPackages("bash", "readline", "libncursesw", "ncurses-terminfo-base")

/** procps-ng and its missing libraries, installed offline on the first login of a new Alpine system. */
val procpsPackages: List<File>? = fetchedPackages(
    "procps-ng", "libproc2", "libintl", "utmps-libs", "skalibs-libs", "libncursesw", "ncurses-terminfo-base",
)

/** bubblewrap and libcap, installed offline independently of the login shell. */
val bubblewrapPackages: List<File>? = fetchedPackages("bubblewrap", "libcap2")

/** For example 3.24.3 → v3.24, written into apk's repository URLs. */
val alpineBranch: String = rootfsTarball
    ?.name?.let { Regex("""alpine-minirootfs-(\d+)\.(\d+)\.""").find(it) }
    ?.let { "v${it.groupValues[1]}.${it.groupValues[2]}" }
    ?: "edge"

android {
    namespace = "io.github.sajadxdp0.tuxport"

    compileSdk = 37
    ndkVersion = "29.0.14206865"

    defaultConfig {
        applicationId = "io.github.sajadxdp0.tuxport"
        minSdk = 30
        targetSdk = 37
        versionCode = 2
        versionName = "1.1.0"
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        buildConfigField("String", "ALPINE_BRANCH", "\"$alpineBranch\"")
    }

    buildFeatures {
        buildConfig = true
    }

    // The release signing key, from local.properties (gitignored). Without the entries the
    // release build falls back to the debug key, so builds on machines without the key keep
    // working; see the README's Development section.
    val signingProps = Properties().apply {
        rootProject.file("local.properties").let { if (it.isFile) it.inputStream().use(::load) }
    }
    val releaseKeystore = signingProps.getProperty("signing.storeFile")

    signingConfigs {
        if (releaseKeystore != null) {
            create("release") {
                storeFile = rootProject.file(releaseKeystore)
                storePassword = signingProps.getProperty("signing.storePassword")
                keyAlias = signingProps.getProperty("signing.keyAlias")
                keyPassword = signingProps.getProperty("signing.keyPassword")
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
            // With the release key installed: the release signature; otherwise the debug key
            signingConfig = if (releaseKeystore != null) signingConfigs.getByName("release")
                else signingConfigs.getByName("debug")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    packaging {
        // Native libraries are stored uncompressed and page-aligned in the APK and mapped directly
        // from it (also on 16K-page devices)
        jniLibs.useLegacyPackaging = false
    }

    lint {
        // A lint warning fails lint (CI runs lintDebug), as `-D warnings` fails clippy.
        warningsAsErrors = true
        // These follow what is published or dated outside the repository (newer libraries, AGP or
        // SDK levels, Play's target SDK deadlines, the Play SDK Index), so as errors they would fail
        // an unchanged tree; they stay in the report.
        informational += listOf(
            "GradleDependency",
            "AndroidGradlePluginVersion",
            "OldTargetApi",
            "ExpiringTargetSdkVersion",
            "OutdatedLibrary",
        )
    }
}

kotlin {
    compilerOptions {
        // A compiler warning fails the build, main and test sources alike.
        allWarningsAsErrors = true
    }
}

dependencies {
    implementation(libs.androidx.biometric)
    implementation(libs.androidx.browser)
    implementation(libs.androidx.core)
    implementation(libs.androidx.preference)
    implementation(libs.androidx.recyclerview)
    implementation(libs.androidx.webkit)
    implementation(libs.material)
    androidTestImplementation(libs.androidx.runner)
    androidTestImplementation(libs.androidx.junit)
}

val rootfsAsset = tasks.register<RootfsAssetTask>("rootfsAsset") {
    rootfsTarball?.let { tarball.set(it) }
    doasPackage?.let { doas.set(it) }
    bashPackages?.let { bash.from(it) }
    procpsPackages?.let { procps.from(it) }
    bubblewrapPackages?.let { bubblewrap.from(it) }
    outputDir.set(layout.buildDirectory.dir("generated/rootfs/assets"))
}

val browserAsset = tasks.register<BrowserAssetTask>("browserAsset") {
    source.set(repoRoot.file("hosts/android/open-url.c"))
    zig.set(zigExecutable)
    outputDir.set(layout.buildDirectory.dir("generated/browser/assets"))
}

androidComponents {
    onVariants { variant ->
        val suffix = variant.name.replaceFirstChar(Char::uppercase)
        val buildRust = tasks.register<CargoNdkTask>("buildRust$suffix") {
            workspace.set(repoRoot)
            zig.set(zigExecutable)
            ndkDirectory.set(androidComponents.sdkComponents.ndkDirectory.map { it.asFile.path })
            when (variant.buildType) {
                "debug" -> cargoArgs.addAll("--profile", "release", "--features", "test-hooks,borrow-check")
                "release" -> cargoArgs.addAll("--profile", "android-release")
                else -> cargoArgs.addAll("--profile", "release")
            }
            outputDir.set(layout.buildDirectory.dir("generated/rust/${variant.name}/jniLibs"))
        }
        variant.sources.jniLibs?.addGeneratedSourceDirectory(buildRust, CargoNdkTask::outputDir)
        variant.sources.assets?.addGeneratedSourceDirectory(rootfsAsset, RootfsAssetTask::outputDir)
        variant.sources.assets?.addGeneratedSourceDirectory(browserAsset, BrowserAssetTask::outputDir)
    }
}

/** A static Linux launcher: no guest curl, netcat, shell extensions or libc installation needed. */
abstract class BrowserAssetTask : DefaultTask() {
    @get:InputFile
    abstract val source: RegularFileProperty

    @get:Input
    abstract val zig: Property<String>

    @get:OutputDirectory
    abstract val outputDir: DirectoryProperty

    @get:Inject
    abstract val exec: ExecOperations

    init {
        // Zig's compilation cache also tracks the compiler and its bundled musl sources.
        outputs.upToDateWhen { false }
    }

    @TaskAction
    fun build() {
        val out = outputDir.get().asFile.apply { mkdirs() }
        exec.exec {
            commandLine(zig.get(), "cc", "-target", "aarch64-linux-musl", "-static", "-O2", "-s",
                "-std=c11", "-Wall", "-Wextra", "-Werror", source.get().asFile.path,
                "-o", File(out, "tuxport-open-url").path)
        }
    }
}

/**
 * Builds libtuxport.so with cargo-ndk. Both APKs need an optimized interpreter.
 * The debug APK enables kernel test hooks and borrow checks for instrumentation.
 */
abstract class CargoNdkTask : DefaultTask() {
    @get:Internal
    abstract val workspace: DirectoryProperty

    @get:Input
    abstract val zig: Property<String>

    @get:Input
    abstract val ndkDirectory: Property<String>

    @get:Input
    abstract val cargoArgs: ListProperty<String>

    @get:OutputDirectory
    abstract val outputDir: DirectoryProperty

    @get:Inject
    abstract val exec: ExecOperations

    init {
        // cargo does its own incremental builds
        outputs.upToDateWhen { false }
    }

    @TaskAction
    fun build() {
        val root = workspace.get().asFile
        exec.exec {
            workingDir = root
            executable = cargo()
            args(
                "ndk", "-t", "arm64-v8a", "--platform", "30",
                "-o", outputDir.get().asFile.path,
                "build", "-p", "tuxport-android",
            )
            args(cargoArgs.get())
            // Set both: with only one set, when the other one in the environment points at another
            // NDK (GitHub's runners preset ANDROID_NDK_ROOT), cargo-ndk warns, and which one it
            // uses depends on its order of precedence.
            environment("ANDROID_NDK_HOME", ndkDirectory.get())
            environment("ANDROID_NDK_ROOT", ndkDirectory.get())
            environment("ZIG", zig.get())
            environment("CARGO_TARGET_DIR", File(root, "target/android").path)
        }
    }

    /** The cargo under `~/.cargo/bin` when there is one, else the one in `PATH`. */
    private fun cargo(): String = File(System.getProperty("user.home"), ".cargo/bin/cargo")
        .takeIf { it.canExecute() }?.path ?: "cargo"
}

/**
 * Puts the rootfs tarball into the assets (imported on first start, the android/setup map), the
 * doas package (for the login account) as `doas.apk`, and bash's packages (the login shell of the
 * systems the app imports) under `bash/`, procps-ng with its libraries under `procps/`, and
 * bubblewrap with libcap under `bubblewrap/`.
 */
abstract class RootfsAssetTask : DefaultTask() {
    @get:InputFile
    @get:Optional
    abstract val tarball: RegularFileProperty

    @get:InputFile
    @get:Optional
    abstract val doas: RegularFileProperty

    @get:InputFiles
    abstract val bash: ConfigurableFileCollection

    @get:InputFiles
    abstract val procps: ConfigurableFileCollection

    @get:InputFiles
    abstract val bubblewrap: ConfigurableFileCollection

    @get:OutputDirectory
    abstract val outputDir: DirectoryProperty

    @TaskAction
    fun copy() {
        val source = tarball.orNull?.asFile
            ?: throw GradleException("Alpine minirootfs not found: run scripts/fetch-rootfs first")
        val out = outputDir.get().asFile
        out.deleteRecursively()
        out.mkdirs()
        source.copyTo(File(out, "rootfs.tgz"))
        val doasFile = doas.orNull?.asFile
            ?: throw GradleException("doas package not found: run scripts/fetch-rootfs first")
        doasFile.copyTo(File(out, "doas.apk"))
        if (bash.isEmpty) throw GradleException("bash packages not found: run scripts/fetch-rootfs first")
        for (apk in bash) apk.copyTo(File(out, "bash/${apk.name}"))
        if (procps.isEmpty) throw GradleException("procps-ng packages not found: run scripts/fetch-rootfs first")
        for (apk in procps) apk.copyTo(File(out, "procps/${apk.name}"))
        if (bubblewrap.isEmpty) throw GradleException("bubblewrap packages not found: run scripts/fetch-rootfs first")
        for (apk in bubblewrap) apk.copyTo(File(out, "bubblewrap/${apk.name}"))
    }
}
