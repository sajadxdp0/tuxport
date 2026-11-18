package io.github.sajadxdp0.tuxport

import android.content.Intent
import android.text.Html
import android.webkit.WebView
import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/** Notice coverage and the offline reader, without starting the kernel or changing its store. */
@RunWith(AndroidJUnit4::class)
class LicenseTest {
    private val context = InstrumentationRegistry.getInstrumentation().targetContext

    @Test
    fun noticesCoverTheApp() {
        val index = LicenseCatalog.read(context)
        assertEquals(index.size, index.map { it.id }.toSet().size)
        fun text(title: String) = index.first { it.title.startsWith(title) }
            .let { context.assets.open("notices/${it.file}").bufferedReader().use { file ->
                Html.fromHtml(file.readText(), Html.FROM_HTML_MODE_LEGACY).toString()
            } }
        val titles = index.map { it.title }
        assertTrue(text("Tuxport").contains("GNU AFFERO GENERAL PUBLIC LICENSE"))
        assertTrue(text("Fuchsia Starnix").let { it.contains("The Fuchsia Authors") && it.contains("Redistribution and use") })
        assertTrue(text("libghostty-vt ").contains("Mitchell Hashimoto"))
        for (component in listOf("uucode ", "simdutf ", "Highway", "Wuffs", "translate-c ", "Zig ")) {
            assertTrue(component, text(component).contains("Permission is hereby granted") || text(component).contains("Redistribution and use"))
        }
        assertTrue(text("JetBrains Mono").contains("SIL OPEN FONT LICENSE"))
        assertFalse(titles.any { it.contains("Termux") })
        val alpine = text("Alpine Linux")
        assertTrue(alpine, Regex("""busybox \S+\s+License: GPL-2.0-only\s+aports: main/busybox at [0-9a-f]{40}""").containsMatchIn(alpine))
        // The packages bundled beside it: doas, bash, procps-ng and bubblewrap with their libraries
        assertTrue(alpine, Regex("""\ndoas \S+\s+License: ISC\s""").containsMatchIn(alpine))
        assertTrue(alpine, Regex("""\nbash \S+\s+License: GPL-3.0-or-later\s+aports: main/bash at [0-9a-f]{40}""").containsMatchIn(alpine))
        assertTrue(alpine, alpine.contains("\nreadline ") && alpine.contains("\nlibncursesw "))
        for (name in listOf("procps-ng", "libproc2", "libintl", "utmps-libs", "skalibs-libs")) {
            assertTrue(alpine, alpine.contains("\n$name "))
        }
        assertEquals(1, Regex("""\nlibncursesw """).findAll(alpine).count())
        assertTrue(alpine, Regex("""\nbubblewrap \S+\s+License: LGPL-2.1-or-later\s+aports: main/bubblewrap at [0-9a-f]{40}""").containsMatchIn(alpine))
        assertTrue(alpine, Regex("""\nlibcap2 \S+\s+License: BSD-3-Clause OR GPL-2.0-only\s+aports: main/libcap at [0-9a-f]{40}""").containsMatchIn(alpine))
        assertTrue(text("GNU Lesser General Public License 2.1").contains("Version 2.1, February 1999"))
        assertTrue(text("Android libraries").contains("androidx.core:core:"))
        assertTrue(text("Android libraries").contains("androidx.webkit:webkit:"))
        assertTrue(text("Android libraries").contains("androidx.biometric:biometric:1.1.0"))
        assertTrue(text("rusqlite ").contains("Permission is hereby granted"))
        assertTrue(titles.any { it.startsWith("SQLite ") })
        assertFalse("proc macros are not in the library", titles.any { it.startsWith("syn ") || it.startsWith("thiserror-impl ") })
        assertTrue(text("GNU General Public License 2.0").contains("Version 2, June 1991"))
        for (row in index) context.assets.open("notices/${row.file}").close()
    }

    @Test
    fun readerReflowsAndRestoresPosition() {
        val intent = Intent(context, LicenseActivity::class.java).putExtra(LicenseActivity.EXTRA_ID, "tuxport")
        ActivityScenario.launch<LicenseActivity>(intent).use { scenario ->
            awaitPage(scenario)
            val dimensions = JSONObject(evaluate(scenario, "JSON.stringify({width: innerWidth, content: document.documentElement.scrollWidth, font: getComputedStyle(document.body).fontSize})"))
            assertEquals(dimensions.getInt("width"), dimensions.getInt("content"))
            assertEquals("16px", dimensions.getString("font"))
            assertTrue(evaluate(scenario, "document.body.innerText").contains("GNU AFFERO GENERAL PUBLIC LICENSE"))
            var position = 0
            scenario.onActivity { activity ->
                val web = activity.findViewById<WebView>(R.id.license_reader)
                assertFalse(web.settings.javaScriptEnabled)
                assertTrue(web.settings.blockNetworkLoads)
                web.scrollTo(0, 1800)
                position = web.scrollY
                assertTrue(position > 0)
            }
            scenario.recreate()
            awaitPage(scenario)
            // The visual-state callback restores after the document has been laid out.
            val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(10)
            var restored = 0
            do {
                scenario.onActivity { restored = it.findViewById<WebView>(R.id.license_reader).scrollY }
                if (kotlin.math.abs(restored - position) <= 2) break
                Thread.sleep(50)
            } while (System.nanoTime() < deadline)
            assertEquals(position.toDouble(), restored.toDouble(), 2.0)
        }
    }

    private fun awaitPage(scenario: ActivityScenario<LicenseActivity>) {
        val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(15)
        do {
            if (evaluate(scenario, "document.readyState") == "complete" &&
                evaluate(scenario, "document.body && document.body.innerText").contains("GNU AFFERO")) return
            Thread.sleep(50)
        } while (System.nanoTime() < deadline)
        throw AssertionError("The offline notice did not finish loading")
    }

    private fun evaluate(scenario: ActivityScenario<LicenseActivity>, script: String): String {
        val done = CountDownLatch(1)
        var result = ""
        scenario.onActivity { activity ->
            val web = activity.findViewById<WebView>(R.id.license_reader)
            // Enable only this test's DOM inspection; the bundled documents contain no scripts.
            web.settings.javaScriptEnabled = true
            web.evaluateJavascript(script) {
                web.settings.javaScriptEnabled = false
                result = org.json.JSONArray("[$it]").optString(0)
                done.countDown()
            }
        }
        assertTrue("WebView did not answer", done.await(10, TimeUnit.SECONDS))
        return result
    }
}
