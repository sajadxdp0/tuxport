package io.github.sajadxdp0.tuxport

import android.content.ActivityNotFoundException
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.webkit.RenderProcessGoneDetail
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebView
import android.webkit.WebViewClient
import android.widget.FrameLayout
import android.widget.TextView
import android.widget.Toast
import androidx.activity.enableEdgeToEdge
import androidx.appcompat.app.AppCompatActivity
import androidx.browser.customtabs.CustomTabsIntent
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.updatePadding
import androidx.webkit.WebViewAssetLoader
import com.google.android.material.appbar.AppBarLayout
import com.google.android.material.appbar.MaterialToolbar
import com.google.android.material.color.DynamicColors
import java.util.Locale
import kotlin.math.roundToInt

/** An offline HTML reader, with native navigation and the current Material colours. */
class LicenseActivity : AppCompatActivity() {
    private var reader: Reader? = null
    private var restoring = 0f
    private var restorePending = true

    override fun onCreate(savedInstanceState: Bundle?) {
        DynamicColors.applyToActivityIfAvailable(this)
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)
        val entries = LicenseCatalog.read(this)
        val entry = entries.firstOrNull { it.id == intent.getStringExtra(EXTRA_ID) }
        if (entry == null) {
            finish()
            return
        }
        setContentView(R.layout.page)
        findViewById<MaterialToolbar>(R.id.toolbar).apply {
            title = entry.title
            setNavigationOnClickListener { finish() }
        }
        val content = findViewById<FrameLayout>(R.id.content)
        val appBar = findViewById<AppBarLayout>(R.id.app_bar)
        ViewCompat.setOnApplyWindowInsetsListener(findViewById(R.id.root)) { v, insets ->
            val bars = insets.getInsets(WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout())
            v.setPadding(bars.left, 0, bars.right, bars.bottom)
            appBar.updatePadding(top = bars.top)
            WindowInsetsCompat.CONSUMED
        }

        // Resolve colours on the UI thread; asset requests run on WebView's worker threads.
        val colors = themeCss()
        val loader = WebViewAssetLoader.Builder()
            .addPathHandler("/assets/", WebViewAssetLoader.AssetsPathHandler(this))
            .addPathHandler("/theme/") { path ->
                if (path == "colors.css") WebResourceResponse("text/css", "UTF-8", colors.byteInputStream()) else null
            }
            .build()
        val pagePath = "/assets/notices/${entry.file}"
        restoring = savedInstanceState?.getFloat(STATE_POSITION) ?: 0f
        val web = Reader(this).apply {
            id = R.id.license_reader
            setBackgroundColor(themeColor(com.google.android.material.R.attr.colorSurface))
            settings.apply {
                javaScriptEnabled = false
                allowFileAccess = false
                allowContentAccess = false
                blockNetworkLoads = true
                builtInZoomControls = true
                displayZoomControls = false
                textZoom = (resources.configuration.fontScale * 100).roundToInt()
            }
            setOnScrollChangeListener { _, _, y, _, _ -> appBar.setLifted(y > 0) }
            webViewClient = object : WebViewClient() {
                override fun shouldInterceptRequest(view: WebView, request: WebResourceRequest): WebResourceResponse {
                    val uri = request.url
                    if (isLocal(uri) && uri.path in setOf(pagePath, "/assets/notices/style.css", "/theme/colors.css")) {
                        loader.shouldInterceptRequest(uri)?.let { return it }
                    }
                    return WebResourceResponse("text/plain", "UTF-8", 404, "Not Found", emptyMap(), "".byteInputStream())
                }

                override fun shouldOverrideUrlLoading(view: WebView, request: WebResourceRequest): Boolean {
                    val uri = request.url
                    if (!request.isForMainFrame) return true
                    if (isLocal(uri)) {
                        if (uri.path == pagePath) return false
                        entries.firstOrNull { uri.path == "/assets/notices/${it.file}" }?.let {
                            startActivity(Intent(this@LicenseActivity, LicenseActivity::class.java).putExtra(EXTRA_ID, it.id))
                        }
                    } else if (uri.scheme == "https" || uri.scheme == "http") {
                        try {
                            CustomTabsIntent.Builder().build().launchUrl(this@LicenseActivity, uri)
                        } catch (_: ActivityNotFoundException) {
                            Toast.makeText(this@LicenseActivity, R.string.link_no_app, Toast.LENGTH_SHORT).show()
                        }
                    }
                    return true
                }

                override fun onPageFinished(view: WebView, url: String) {
                    if (!restorePending) return
                    restorePending = false
                    view.postVisualStateCallback(0, object : WebView.VisualStateCallback() {
                        override fun onComplete(requestId: Long) {
                            reader?.restorePosition(restoring)
                            restoring = 0f
                        }
                    })
                }

                override fun onRenderProcessGone(view: WebView, detail: RenderProcessGoneDetail): Boolean {
                    content.removeView(view)
                    reader = null
                    view.destroy()
                    content.addView(TextView(this@LicenseActivity).apply {
                        setText(R.string.license_unavailable)
                        setPadding(dp(24), dp(24), dp(24), dp(24))
                    })
                    return true
                }
            }
        }
        reader = web
        content.addView(web, FrameLayout.LayoutParams(FrameLayout.LayoutParams.MATCH_PARENT, FrameLayout.LayoutParams.MATCH_PARENT))
        web.loadUrl("https://$ASSET_HOST$pagePath")
    }

    private fun themeCss(): String {
        fun color(attr: Int) = String.format(Locale.ROOT, "#%06x", themeColor(attr) and 0xffffff)
        return """
            :root {
              --surface: ${color(com.google.android.material.R.attr.colorSurface)};
              --text: ${color(com.google.android.material.R.attr.colorOnSurface)};
              --muted: ${color(com.google.android.material.R.attr.colorOnSurfaceVariant)};
              --primary: ${color(androidx.appcompat.R.attr.colorPrimary)};
              --outline: ${color(com.google.android.material.R.attr.colorOutlineVariant)};
            }
        """.trimIndent()
    }

    override fun onSaveInstanceState(outState: Bundle) {
        outState.putFloat(STATE_POSITION, if (restoring > 0f) restoring else reader?.position ?: 0f)
        super.onSaveInstanceState(outState)
    }

    override fun onDestroy() {
        reader?.let {
            findViewById<FrameLayout>(R.id.content).removeView(it)
            it.destroy()
        }
        reader = null
        super.onDestroy()
    }

    /** A relative position survives reflow when the width or system font size changes. */
    private class Reader(context: Context) : WebView(context) {
        private val range: Int get() = (computeVerticalScrollRange() - height).coerceAtLeast(0)
        val position: Float get() = if (range > 0) scrollY.toFloat() / range else 0f
        fun restorePosition(position: Float) = scrollTo(0, (position.coerceIn(0f, 1f) * range).roundToInt())
    }

    companion object {
        const val EXTRA_ID = "license_id"
        private const val STATE_POSITION = "position"
        private const val ASSET_HOST = "appassets.androidplatform.net"
        private fun isLocal(uri: Uri) = uri.scheme == "https" && uri.authority == ASSET_HOST
    }
}
