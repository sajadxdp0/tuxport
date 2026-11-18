package io.github.sajadxdp0.tuxport

import android.Manifest
import android.app.Activity
import android.app.NotificationManager
import android.content.ActivityNotFoundException
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import androidx.activity.result.ActivityResultLauncher
import androidx.core.content.edit
import android.provider.Settings as SystemSettings

/** The banner and Settings share one request history and the same system-settings fallback. */
internal object NotificationPermission {
    // Keep the original activity preference file so upgrades retain earlier attempts.
    private fun preferences(context: Context) =
        context.getSharedPreferences("MainActivity", Context.MODE_PRIVATE)

    fun initialize(context: Context) {
        val preferences = preferences(context)
        if (preferences.contains(REQUESTED)) return
        // Older versions requested on startup without saving the attempt. An upgrade may
        // inherit a permanent denial, indistinguishable from a first request via rationale.
        val installed = context.packageManager.getPackageInfo(context.packageName, 0)
        preferences.edit {
            putBoolean(REQUESTED, installed.lastUpdateTime > installed.firstInstallTime)
        }
    }

    fun enabled(context: Context): Boolean =
        context.getSystemService(NotificationManager::class.java).areNotificationsEnabled()

    fun enable(activity: Activity, request: ActivityResultLauncher<String>) {
        if (Privacy.locked(activity) || enabled(activity)) return
        val preferences = preferences(activity)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            activity.checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED &&
            !preferences.getBoolean(REQUESTED, false) &&
            !activity.shouldShowRequestPermissionRationale(Manifest.permission.POST_NOTIFICATIONS)
        ) {
            // A dismissed dialog also counts as an attempt; a later tap opens settings.
            preferences.edit { putBoolean(REQUESTED, true) }
            request.launch(Manifest.permission.POST_NOTIFICATIONS)
            return
        }
        val intent = Intent(SystemSettings.ACTION_APP_NOTIFICATION_SETTINGS)
            .putExtra(SystemSettings.EXTRA_APP_PACKAGE, activity.packageName)
        try {
            activity.startActivity(intent)
        } catch (_: ActivityNotFoundException) {
            activity.openPackageSettings(SystemSettings.ACTION_APPLICATION_DETAILS_SETTINGS)
        }
    }

    private const val REQUESTED = "requested_notifications"
}
