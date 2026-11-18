package io.github.sajadxdp0.tuxport

import android.content.Intent
import android.os.Bundle
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.TextView
import androidx.activity.enableEdgeToEdge
import androidx.appcompat.app.AppCompatActivity
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.updatePadding
import androidx.core.widget.NestedScrollView
import com.google.android.material.appbar.AppBarLayout
import com.google.android.material.appbar.MaterialToolbar
import com.google.android.material.color.DynamicColors

/** Lists component notices; standalone license texts are reached through their detail links. */
class LicensesActivity : AppCompatActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        DynamicColors.applyToActivityIfAvailable(this)
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)
        setContentView(R.layout.page)
        findViewById<MaterialToolbar>(R.id.toolbar).apply {
            setTitle(R.string.settings_licenses)
            setNavigationOnClickListener { finish() }
        }
        val rows = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        val entries = LicenseCatalog.read(this).filterNot { it.fullText }
            .sortedWith(compareBy(String.CASE_INSENSITIVE_ORDER) { it.name })
        for (entry in entries) {
            rows.addView(layoutInflater.inflate(R.layout.list_item, rows, false).apply {
                findViewById<TextView>(R.id.title).text = entry.title
                findViewById<TextView>(R.id.summary).text = entry.subtitle(this@LicensesActivity)
                setOnClickListener {
                    startActivity(Intent(this@LicensesActivity, LicenseActivity::class.java)
                        .putExtra(LicenseActivity.EXTRA_ID, entry.id))
                }
            })
        }
        val list = NestedScrollView(this).apply {
            id = R.id.license_list
            clipToPadding = false
            addView(rows)
        }
        findViewById<FrameLayout>(R.id.content).addView(list)
        val appBar = findViewById<AppBarLayout>(R.id.app_bar)
        ViewCompat.setOnApplyWindowInsetsListener(findViewById(R.id.root)) { v, insets ->
            val bars = insets.getInsets(WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout())
            v.setPadding(bars.left, 0, bars.right, 0)
            appBar.updatePadding(top = bars.top)
            list.updatePadding(bottom = bars.bottom)
            WindowInsetsCompat.CONSUMED
        }
    }
}
