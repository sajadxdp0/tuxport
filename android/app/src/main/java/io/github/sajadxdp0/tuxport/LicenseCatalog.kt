package io.github.sajadxdp0.tuxport

import android.content.Context
import org.json.JSONArray

/** The checked-in notices shared by the list and its reader. */
internal object LicenseCatalog {
    data class Entry(
        val id: String,
        val name: String,
        val version: String,
        val license: String,
        val file: String,
        val packageCount: Int,
        val fullText: Boolean,
    ) {
        val title: String get() = if (version.isEmpty()) name else "$name $version"

        fun subtitle(context: Context): String = when {
            packageCount > 0 -> context.resources.getQuantityString(
                R.plurals.license_packages, packageCount, packageCount,
            )
            license == "Public domain" -> context.getString(R.string.license_public_domain)
            else -> license
        }
    }

    fun read(context: Context): List<Entry> {
        val index = context.assets.open("notices/index.json").bufferedReader().use { JSONArray(it.readText()) }
        return List(index.length()) { i ->
            val item = index.getJSONObject(i)
            Entry(
                item.getString("id"), item.getString("name"), item.getString("version"),
                item.getString("license"), item.getString("file"),
                item.optInt("packageCount"), item.optString("kind") == "license",
            )
        }
    }
}
